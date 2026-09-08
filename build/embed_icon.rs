// 把应用图标编译进 exe 的资源段。
//
// 由 `crates/*/build.rs` 通过 `include!` 引入（两个 bin 共用同一份逻辑）。
//
// ## 为什么自己找 rc.exe
// `winresource` 默认执行 `reg query HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots`
// 来定位 Windows SDK。一旦 `reg.exe` 不可用（受限 CI、被安全策略拦截），
// 它会退化成直接调用裸 `rc.exe`，报 "系统找不到指定的路径。(os error 3)"，
// 构建照样成功但图标静默缺失。所以这里先按已知安装目录扫一遍。
//
// 注意：这里的注释只能用 `//`——`include!` 展开后本文件并不在 crate 开头，
// 写 `//!` 会触发 E0753。

use std::path::{Path, PathBuf};

/// `Windows Kits` 目录的候选位置。
///
/// 不能只依赖 `%ProgramFiles(x86)%`：某些精简环境（CI 沙箱、最小化的
/// shell）根本不暴露这个变量，于是退化到按 `SystemRoot` 的盘符拼默认路径。
fn sdk_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                roots.push(PathBuf::from(v).join("Windows Kits"));
            }
        }
    }

    let system_root = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("SYSTEMROOT"))
        .unwrap_or_else(|_| String::from(r"C:\Windows"));
    // "C:\Windows" -> "C:\"（盘符 + 反斜杠）
    let drive = system_root.get(..2).unwrap_or("C:");
    roots.push(PathBuf::from(format!(r"{}\Program Files (x86)\Windows Kits", drive)));
    roots.push(PathBuf::from(format!(r"{}\Program Files\Windows Kits", drive)));

    roots
}

/// 定位 Windows SDK 的 `rc.exe`。
///
/// 查找顺序：`RC_EXE` 环境变量 → `%ProgramFiles(x86)%` / `%ProgramFiles%`
/// 下的 `Windows Kits\<10|11>\bin\[<版本>\]<arch>\rc.exe`，多版本时取最新的。
fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC_EXE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    let arch_dir = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") | Ok("aarch64") => "x64",
        _ => "x86",
    };

    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for kits in sdk_roots() {
        for major in ["11", "10", "8.1"] {
            let bin = kits.join(major).join("bin");

            // Windows Kits 10 的布局是 bin\<版本>\<arch>\rc.exe，
            // Windows Kits 8.1 及更早是 bin\<arch>\rc.exe，两种都试。
            let mut paths = vec![bin.join(arch_dir).join("rc.exe")];
            if let Ok(entries) = bin.read_dir() {
                paths.extend(
                    entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.path().join(arch_dir).join("rc.exe")),
                );
            }

            for p in paths {
                if !p.is_file() {
                    continue;
                }
                // 用 SDK 版本目录名做排序键（10.0.26100.0 这类），取最新。
                // 老式布局（bin\<arch>\rc.exe）拿到的上层目录名是 "bin"，
                // 不是版本号，一律归到最低优先级，让带版本号的胜出。
                let version = p
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .map(|s| s.to_string_lossy().into_owned())
                    .filter(|s| s.chars().next().map_or(false, |c| c.is_ascii_digit()))
                    .unwrap_or_default();
                candidates.push((version, p));
            }
        }
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, p)| p)
}

/// 把 `assets/app.ico` 嵌进当前 crate 的 exe。
///
/// 失败只打 warning 不让构建中断：图标缺失影响观感，但不该挡住发版。
pub fn embed_app_icon() {
    let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") else {
        println!("cargo:warning=CARGO_MANIFEST_DIR not set, skip icon embedding");
        return;
    };

    let icon = Path::new(&manifest).join("../../assets/app.ico");
    if !icon.is_file() {
        println!("cargo:warning=app icon not found: {}", icon.display());
        return;
    }
    println!("cargo:rerun-if-changed={}", icon.display());

    let mut res = winresource::WindowsResource::new();

    // 显式给出 rc.exe 所在目录，绕开注册表查询。
    match find_rc_exe() {
        Some(rc) => {
            println!("cargo:warning=using rc.exe: {}", rc.display());
            if let Some(dir) = rc.parent() {
                if let Some(dir) = dir.to_str() {
                    res.set_toolkit_path(dir);
                }
            }
        }
        None => println!("cargo:warning=rc.exe not found, icon may not be embedded"),
    }

    res.set_icon(&icon.display().to_string());
    res.set_language(0x0804); // 简体中文，文件属性里的「语言」

    // 文件属性 / 任务管理器里显示的产品名与描述。
    res.set("FileDescription", "RouteTool");
    res.set("ProductName", "RouteTool");

    if let Err(e) = res.compile() {
        println!("cargo:warning=failed to embed app icon: {e}");
    }
}
