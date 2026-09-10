//! 开机自启设置（UI 层）。
//!
//! 设置项存两处：
//!   - 注册表 HKCU\...\Run 键 `RouteToolUI`（开机实际生效的凭据），
//!     值数据固定为 `"<exe>" --tray`：开机只起托盘，不弹设置窗口；
//!   - `%APPDATA%\RouteTool\ui_settings.json`（UI 侧勾选状态的持久化，
//!     核心服务的 config.json 不承载 UI 层设置）。
//!
//! 迁移：v0.1.18 及以前由安装包任务勾选写 Run 键（无 --tray，开机弹窗口）。
//! 首次启动若无设置文件但 Run 键存在，视为用户已选择自启，继承为开启
//! 并把值数据自愈为带 --tray 的托盘启动形式。

use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
    RRF_RT_REG_SZ,
};

/// Run 键子路径。
const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Run 键值名（与安装包历史值名一致，便于迁移继承）。
const RUN_VALUE_NAME: &str = "RouteToolUI";

/// UI 本地设置文件路径：%APPDATA%\RouteTool\ui_settings.json。
fn settings_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        PathBuf::from(appdata)
            .join("RouteTool")
            .join("ui_settings.json"),
    )
}

/// Run 键期望值数据：`"<exe>" --tray`。
fn run_key_data() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "route-tool-ui.exe".to_string());
    format!("\"{exe}\" --tray")
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 读取 Run 键当前值数据（不存在返回 None）。
fn query_run_value() -> anyhow::Result<Option<String>> {
    let subkey = to_wide(RUN_SUBKEY);
    let value = to_wide(RUN_VALUE_NAME);
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * std::mem::size_of::<u16>()) as u32;
    let mut value_type = REG_VALUE_TYPE::default();

    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if err == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if err.is_err() {
        anyhow::bail!("读取 Run 键失败: {}", err.0);
    }
    let len = (size as usize / std::mem::size_of::<u16>()).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..len]);
    Ok(Some(text.trim_end_matches('\0').to_string()))
}

/// 写入或删除 Run 键值。
fn set_run_value(enable: bool) -> anyhow::Result<()> {
    if !enable {
        let subkey = to_wide(RUN_SUBKEY);
        let value = to_wide(RUN_VALUE_NAME);
        let mut hkey = HKEY::default();
        let err = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                Some(0),
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
        };
        if err.is_err() {
            anyhow::bail!("打开 Run 键失败: {}", err.0);
        }
        // 值不存在（FILE_NOT_FOUND）视为已删除，成功。
        let err = unsafe { RegDeleteValueW(hkey, PCWSTR(value.as_ptr())) };
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        if err.is_err() && err != ERROR_FILE_NOT_FOUND {
            anyhow::bail!("删除 Run 键值失败: {}", err.0);
        }
        return Ok(());
    }

    let data = run_key_data();
    let subkey = to_wide(RUN_SUBKEY);
    let value = to_wide(RUN_VALUE_NAME);
    let data_u16 = to_wide(&data);
    // REG_SZ 数据按 UTF-16 字节序列写入（含结尾 NUL）。
    let data_bytes =
        unsafe { std::slice::from_raw_parts(data_u16.as_ptr().cast::<u8>(), data_u16.len() * 2) };

    let mut hkey = HKEY::default();
    let err = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if err.is_err() {
        anyhow::bail!("打开 Run 键失败: {}", err.0);
    }
    let err = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(value.as_ptr()),
            Some(0),
            REG_SZ,
            Some(data_bytes),
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if err.is_err() {
        anyhow::bail!("写入 Run 键值失败: {}", err.0);
    }
    Ok(())
}

/// 读取 UI 设置文件中的自启勾选状态（文件不存在/损坏返回 None）。
fn load_enabled_from_file() -> Option<bool> {
    let path = settings_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("launch_at_startup").and_then(|b| b.as_bool())
}

fn save_enabled_to_file(enabled: bool) -> anyhow::Result<()> {
    let Some(path) = settings_path() else {
        anyhow::bail!("无法定位 APPDATA 目录");
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::json!({ "launch_at_startup": enabled });
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

/// 应用启动时调用：确定初始自启状态并保证注册表与之一致（自愈/迁移）。
///
/// 返回最终状态，供 UI 勾选框回显。
pub fn init_and_migrate() -> anyhow::Result<bool> {
    let from_file = load_enabled_from_file();
    let reg_current = query_run_value()?;

    // 无设置文件时继承安装包时代写入的 Run 键（v0.1.18 及以前的自启任务）。
    let enabled = match from_file {
        Some(v) => v,
        None => reg_current.is_some(),
    };

    let want = run_key_data();
    match (&reg_current, enabled) {
        // 开启但值数据是旧格式（无 --tray）或缺失：写入/自愈为托盘启动形式。
        (Some(existing), true) if existing != &want => set_run_value(true)?,
        (None, true) => set_run_value(true)?,
        // 已关闭但注册表有残留：清理。
        (Some(_), false) => set_run_value(false)?,
        _ => {}
    }

    save_enabled_to_file(enabled)?;
    Ok(enabled)
}

/// 勾选框切换：同步注册表 + 持久化设置文件。
pub fn set_enabled(enable: bool) -> anyhow::Result<()> {
    set_run_value(enable)?;
    save_enabled_to_file(enable)?;
    tracing::info!("开机自启已{}", if enable { "开启" } else { "关闭" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_key_data_has_tray_arg() {
        let data = run_key_data();
        assert!(data.contains("--tray"), "Run 键值必须带 --tray: {data}");
        assert!(data.starts_with('"'), "路径需带引号: {data}");
    }

    #[test]
    fn settings_path_under_appdata() {
        // 测试环境可能没有 APPDATA，自设一个再验证路径拼装。
        unsafe { std::env::set_var("APPDATA", r"C:\Users\test\AppData\Roaming") };
        let p = settings_path().expect("APPDATA 应存在");
        assert_eq!(p.file_name().unwrap(), "ui_settings.json");
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "RouteTool");
    }
}
