//! route-tool-ui.exe：托盘 + 设置窗口。

// Windows：以 GUI 子系统链接，避免启动时闪一个黑色控制台窗口。
// debug 构建保留控制台，方便直接看到 panic / 日志输出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc_client;
mod notify;
mod single_instance;
mod tray;

use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::Mutex;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tracing_subscriber::EnvFilter;

use ipc_protocol::{AdapterInfo, AppConfig, HealthStatus, SwitchMode};

slint::include_modules!();

/// 应用版本（来自 Cargo.toml）。
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// 直改模式子网掩码的默认值（用户未填时）。
const DEFAULT_MASK: &str = "255.255.255.0";

/// 日志目录。
const CORE_LOG_DIR: &str = r"C:\ProgramData\RouteTool\logs";

/// UI 日志滚动文件前缀（新名）。
const UI_LOG_PREFIX_NEW: &str = "route-tool-ui";
/// UI 日志滚动文件前缀（旧名，v0.1.13 及以前；用于保留历史日志可读）。
const UI_LOG_PREFIX_OLD: &str = "bypass-ui";
/// 核心日志滚动文件前缀（新名）。
const CORE_LOG_PREFIX_NEW: &str = "route-tool-core";
/// 核心日志滚动文件前缀（旧名）。
const CORE_LOG_PREFIX_OLD: &str = "bypass-core";

/// 解析 DNS 输入（逗号/分号/空白分隔），空输入返回 None。
fn parse_dns_list(text: &str) -> Result<Option<Vec<std::net::IpAddr>>, String> {
    let items: Vec<&str> = text
        .split([',', ';', '，', '；', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if items.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for it in items {
        let ip: std::net::IpAddr = it.parse().map_err(|_| format!("DNS 地址无效: {it}"))?;
        out.push(ip);
    }
    Ok(Some(out))
}

/// 从 UI 控件读取当前配置，构造 AppConfig（不含 adapter_id，调用方补齐）。
fn build_config_from_ui(app: &AppWindow) -> Result<AppConfig, String> {
    let ip: std::net::IpAddr = app
        .get_bypass_ip_text()
        .parse()
        .map_err(|_| "旁路由 IP 无效".to_string())?;

    let static_ip_text = app.get_static_ip_text().trim().to_string();
    let static_ip = if static_ip_text.is_empty() {
        None
    } else {
        Some(
            static_ip_text
                .parse::<std::net::Ipv4Addr>()
                .map_err(|_| "网卡静态 IP 无效（如 192.168.2.100）".to_string())?,
        )
    };

    let mask_text = app.get_mask_text().trim().to_string();
    let mask = if mask_text.is_empty() {
        Some(DEFAULT_MASK.parse().unwrap())
    } else {
        Some(
            mask_text
                .parse()
                .map_err(|_| "子网掩码无效（如 255.255.255.0）".to_string())?,
        )
    };

    let dns = parse_dns_list(&app.get_dns_text())?;

    let mut cfg = AppConfig {
        bypass_ip: ip,
        switch_mode: if app.get_mode_index() == 1 {
            SwitchMode::AdapterReconfig
        } else {
            SwitchMode::RouteOverlay
        },
        ..AppConfig::default()
    };
    cfg.subnet_mask = mask;
    cfg.static_ip = static_ip;
    cfg.dns_override = dns;
    cfg.health_check_interval_secs = app.get_interval_secs().max(1) as u32;
    cfg.failure_threshold = app.get_failure_threshold().max(1) as u32;
    cfg.auto_reenable_after_recovery = app.get_auto_reenable();
    cfg.notifications_enabled = app.get_notifications();
    Ok(cfg)
}

/// UI 全局状态（连接共享）。
struct UiState {
    client: Option<ipc_client::IpcClient>,
    config: AppConfig,
    /// 上次 list_adapters 结果（下拉框数据源 + id 映射）。
    adapters: Vec<AdapterInfo>,
}

/// 将多个 IPv4 地址格式化为逗号分隔字符串；取前 2 个避免过长。
fn format_ips(ips: &[std::net::IpAddr]) -> String {
    ips.iter()
        .filter(|ip| matches!(ip, std::net::IpAddr::V4(_)))
        .take(2)
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// 网卡下拉框的显示名：未连接的网卡加「（未连接）」后缀。
///
/// 直改模式下对未连接（没插网线 / Wi-Fi 未关联）的网卡执行 netsh 静态化会
/// "退出码 0 但实际不生效"，用户完全无从判断——在下拉项上直接标出来，
/// 避免选到一块正在离线的网卡。
fn adapter_display_names(list: &[AdapterInfo]) -> Vec<SharedString> {
    list.iter()
        .map(|a| {
            if a.is_connected {
                a.name.clone().into()
            } else {
                format!("{}（未连接）", a.name).into()
            }
        })
        .collect()
}

/// 把选中的网卡信息同步到首页展示字段。
fn update_adapter_info_display(app: &AppWindow, adapters: &[AdapterInfo], adapter_id: &str) {
    if let Some(a) = adapters.iter().find(|a| a.id == adapter_id) {
        let ip = if a.ipv4.is_empty() {
            "--".to_string()
        } else {
            format_ips(&a.ipv4)
        };
        let mask = if a.ipv4_prefixes.is_empty() {
            "--".to_string()
        } else {
            a.ipv4_prefixes
                .iter()
                .map(|p| format!("255.255.255.{}", 256 - (1u32 << (32 - p))))
                .take(1)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let gw = if a.gateway.is_empty() {
            "--".to_string()
        } else {
            format_ips(&a.gateway)
        };
        let dns = if a.dns.is_empty() {
            "--".to_string()
        } else {
            format_ips(&a.dns)
        };
        app.set_selected_adapter_ip(ip.into());
        app.set_selected_adapter_mask(mask.into());
        app.set_selected_adapter_gateway(gw.into());
        app.set_selected_adapter_dns(dns.into());
    } else {
        app.set_selected_adapter_ip("--".into());
        app.set_selected_adapter_mask("--".into());
        app.set_selected_adapter_gateway("--".into());
        app.set_selected_adapter_dns("--".into());
    }
}

/// 拉取网卡列表并填充下拉框；同时按配置里的 adapter_id 选中对应项。
async fn refresh_adapters(state: Arc<Mutex<UiState>>, app_weak: slint::Weak<AppWindow>) {
    let (cfg_id, result) = {
        let mut st = state.lock().await;
        let cfg_id = st.config.adapter_id.clone();
        let result = match st.client.as_mut() {
            Some(c) => c.list_adapters().await,
            None => return,
        };
        (cfg_id, result)
    };
    let Ok(list) = result else { return };
    if list.is_empty() {
        return;
    }

    let sel = list.iter().position(|a| a.id == cfg_id);
    let names: Vec<SharedString> = list.iter().map(|a| a.name.clone().into()).collect();
    let idx = sel.map(|i| i as i32).unwrap_or(-1);
    let adapter_id = cfg_id;
    let list_for_ui = list.clone();

    {
        let mut st = state.lock().await;
        st.adapters = list;
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            app.set_adapter_names(ModelRc::from(Rc::new(VecModel::from(names))));
            app.set_adapter_index(idx);
            update_adapter_info_display(&app, &list_for_ui, &adapter_id);
        }
    });
}

/// 把配置同步到 UI 控件（首次连接与重连共用）。
fn apply_config_to_ui(app: &AppWindow, cfg: &AppConfig) {
    app.set_bypass_ip_text(cfg.bypass_ip.to_string().into());
    app.set_mode_index(match cfg.switch_mode {
        SwitchMode::RouteOverlay => 0,
        SwitchMode::AdapterReconfig => 1,
    });
    app.set_interval_secs(cfg.health_check_interval_secs as i32);
    app.set_failure_threshold(cfg.failure_threshold as i32);
    app.set_auto_reenable(cfg.auto_reenable_after_recovery);
    app.set_notifications(cfg.notifications_enabled);
    app.set_mask_text(
        cfg.subnet_mask
            .map(|m| m.to_string())
            .unwrap_or_else(|| DEFAULT_MASK.to_string())
            .into(),
    );
    app.set_static_ip_text(
        cfg.static_ip
            .map(|ip| ip.to_string())
            .unwrap_or_default()
            .into(),
    );
    let dns_text = cfg
        .dns_override
        .as_ref()
        .map(|list| {
            list.iter()
                .filter(|ip| matches!(ip, std::net::IpAddr::V4(_)))
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    app.set_dns_text(dns_text.into());
}

/// 读取最近的日志文件，解析为 Slint 模型。
///
/// 同时读旧名（`bypass-*.log.YYYY-MM-DD`）与新名（`route-tool-*.log.YYYY-MM-DD`），
/// 让 v0.1.13 及以前升级上来的用户仍能看到当日历史日志。
fn read_logs() -> Vec<LogEntry> {
    let mut out = Vec::new();

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // UI 日志。
    let ui_dir = std::env::temp_dir().join("RouteTool");
    for prefix in [UI_LOG_PREFIX_NEW, UI_LOG_PREFIX_OLD] {
        let p = ui_dir.join(format!("{prefix}.log.{today}"));
        if p.exists() {
            read_log_file(&p, &mut out);
        }
    }

    // 核心日志。
    let core_dir = std::path::Path::new(CORE_LOG_DIR);
    for prefix in [CORE_LOG_PREFIX_NEW, CORE_LOG_PREFIX_OLD] {
        let p = core_dir.join(format!("{prefix}.log.{today}"));
        if p.exists() {
            read_log_file(&p, &mut out);
        }
    }

    // 合并后按时间排序并截断。
    out.sort_by(|a, b| a.time.cmp(&b.time));
    out.into_iter()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// 把 tracing 默认格式的 RFC3339 时间戳（带 Z 后缀）格式化为本地时区的 HH:MM:SS。
///
/// 例如：`2026-09-09T08:40:22.123456Z` 在东八区下显示为 `16:40:22`。
/// 解析失败时回退到字符串切片截取（兼容异常格式）。
fn format_local_hms(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| {
            raw.split_once('T')
                .map(|(_, t)| {
                    t.split_once('.')
                        .map(|(h, _)| h.to_string())
                        .unwrap_or(t.to_string())
                })
                .unwrap_or_else(|| raw.to_string())
        })
}

/// 读取单个日志文件，按 tracing 默认格式解析：
/// `2026-09-09T08:40:22.123456Z  INFO module::path: message`
fn read_log_file(path: &std::path::Path, out: &mut Vec<LogEntry>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 尝试按空白拆分：时间、级别、目标:消息
        let mut parts = line.splitn(3, ' ');
        let time_raw = parts.next().unwrap_or("");
        let level_raw = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or(line);

        // 本地时区显示（用户的实际视角），不再是 UTC 字符串切片。
        let time = format_local_hms(time_raw);

        let level = level_raw.trim().to_uppercase();
        // 去掉目标前缀（如 module::path:）
        let message = if let Some((_, msg)) = rest.split_once(':') {
            msg.trim().to_string()
        } else {
            rest.to_string()
        };

        out.push(LogEntry {
            time: time.into(),
            level: level.into(),
            message: message.into(),
        });
    }
}

fn init_logging() {
    let dir = std::env::temp_dir().join("RouteTool");
    let _ = std::fs::create_dir_all(&dir);
    // 同时清理旧名（v0.1.13 及以前的 `bypass-ui.log.YYYY-MM-DD`），
    // 不让两条命名各占一份当天滚动文件。
    core_lib::log_prune::prune_old_logs(
        &dir,
        UI_LOG_PREFIX_OLD,
        core_lib::log_prune::LOG_RETENTION,
    );
    core_lib::log_prune::prune_old_logs(
        &dir,
        UI_LOG_PREFIX_NEW,
        core_lib::log_prune::LOG_RETENTION,
    );
    let appender = tracing_appender::rolling::daily(dir, format!("{UI_LOG_PREFIX_NEW}.log"));
    let (writer, _guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();
    std::mem::forget(_guard);
}

fn main() -> anyhow::Result<()> {
    init_logging();

    let _single_instance = match single_instance::acquire_or_notify()? {
        Some(guard) => guard,
        None => return Ok(()),
    };

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );

    let state: Arc<Mutex<UiState>> = Arc::new(Mutex::new(UiState {
        client: None,
        config: AppConfig::default(),
        adapters: Vec::new(),
    }));

    let app = AppWindow::new()?;
    app.window()
        .on_close_requested(move || slint::CloseRequestResponse::HideWindow);
    let tray = tray::Tray::new()?;
    let _ = tray::SHARED_TRAY.set(tray::StaticTray(tray));

    app.set_version_text(VERSION.into());

    // ---- 连接核心：串行完成 连接 → 拉配置 → 拉网卡列表，一次性同步 UI ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let rt = rt.clone();
        rt.spawn(async move {
            let mut client = None;
            for _ in 0..10 {
                match ipc_client::IpcClient::connect().await {
                    Ok(c) => {
                        client = Some(c);
                        break;
                    }
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
                }
            }
            let Some(mut client) = client else {
                tracing::warn!("启动时未能连接核心服务，等待轮询重连");
                return;
            };

            let cfg = client.get_config().await.ok();
            let adapters = client.list_adapters().await.ok();

            let (cfg_id, adapter_names, adapter_idx, adapter_id_for_info) = {
                let mut st = state.lock().await;
                st.client = Some(client);
                let cfg_id = match &cfg {
                    Some(c) => {
                        st.config = c.clone();
                        c.adapter_id.clone()
                    }
                    None => st.config.adapter_id.clone(),
                };
                match &adapters {
                    Some(list) => {
                        let sel = list.iter().position(|a| a.id == cfg_id);
                        let names: Vec<SharedString> =
                            list.iter().map(|a| a.name.clone().into()).collect();
                        st.adapters = list.clone();
                        (
                            Some(cfg_id.clone()),
                            Some(names),
                            sel.map(|i| i as i32).unwrap_or(-1),
                            cfg_id,
                        )
                    }
                    None => (None, None, -1, cfg_id),
                }
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    if let Some(cfg) = &cfg {
                        apply_config_to_ui(&app, cfg);
                    }
                    if let Some(names) = adapter_names {
                        app.set_adapter_names(ModelRc::from(Rc::new(VecModel::from(names))));
                        app.set_adapter_index(adapter_idx);
                    }
                    update_adapter_info_display(
                        &app,
                        &adapters.unwrap_or_default(),
                        &adapter_id_for_info,
                    );
                    app.set_service_status_text("服务运行中".into());
                }
            });
            let _ = cfg_id;
        });
    }

    // ---- UI 回调：下拉框选择网卡 ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let rt = rt.clone();
        app.on_adapter_changed(move |idx| {
            let state = state.clone();
            let app_weak = app_weak.clone();
            let rt = rt.clone();
            rt.spawn(async move {
                let mut st = state.lock().await;
                if let Some(info) = st.adapters.get(idx as usize) {
                    let adapter_id = info.id.clone();
                    st.config.adapter_id = adapter_id.clone();
                    let cfg = st.config.clone();
                    if let Some(c) = st.client.as_mut() {
                        if let Err(e) = c.update_config(&cfg).await {
                            let app_weak = app_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = app_weak.upgrade() {
                                    app.set_config_error(format!("保存网卡选择失败: {e}").into());
                                }
                            });
                        } else {
                            let app_weak = app_weak.clone();
                            let adapters = st.adapters.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = app_weak.upgrade() {
                                    update_adapter_info_display(&app, &adapters, &adapter_id);
                                }
                            });
                        }
                    }
                }
            });
        });
    }

    // ---- UI 回调：启用 / 禁用 ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let rt = rt.clone();
        app.on_toggle_bypass(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            let enable = !is_enabled_now(&app_weak);
            let rt = rt.clone();

            let mut cfg = match app_weak.upgrade() {
                Some(app) => match build_config_from_ui(&app) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = app_weak.upgrade() {
                                a.set_config_error(e.into());
                            }
                        });
                        return;
                    }
                },
                None => return,
            };

            rt.spawn(async move {
                let adapter_id = {
                    let st = state.lock().await;
                    st.config.adapter_id.clone()
                };
                cfg.adapter_id = adapter_id;

                let result = {
                    let mut st = state.lock().await;
                    match st.client.as_mut() {
                        Some(c) => {
                            if enable {
                                match c.update_config(&cfg).await {
                                    Ok(()) => c.enable().await,
                                    Err(e) => Err(e),
                                }
                            } else {
                                c.disable().await
                            }
                        }
                        None => Err("未连接到核心服务".to_string()),
                    }
                };
                if result.is_ok() {
                    let mut st = state.lock().await;
                    st.config = cfg;
                }
                let succeeded = result.is_ok();
                if let Err(e) = result {
                    // 提前 clone：move 闭包要独占，避免后续 is_ok 分支再用到 app_weak 时已被移动。
                    let app_weak_err = app_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_weak_err.upgrade() {
                            app.set_config_error(e.into());
                        }
                    });
                }
                // 状态切换成功后，主动拉一次最新网卡列表回填首页。
                // 直改模式下启用 → 网关被 netsh 重写为旁路由 IP，即时回填才有意义；
                // 路由叠加则数据无变化，重复一次也无副作用。
                if succeeded {
                    refresh_adapters(state.clone(), app_weak.clone()).await;
                }
            });
        });
    }

    // ---- UI 回调：刷新网卡（首页进入/窗口显示时触发） ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let rt = rt.clone();
        app.on_refresh_adapters(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            let rt = rt.clone();
            rt.spawn(async move {
                refresh_adapters(state.clone(), app_weak.clone()).await;
            });
        });
    }

    // ---- UI 回调：保存配置 ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let rt = rt.clone();
        app.on_save_config(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            let app = match app_weak.upgrade() {
                Some(a) => a,
                None => return,
            };
            let mut cfg = match build_config_from_ui(&app) {
                Ok(c) => c,
                Err(e) => {
                    app.set_config_error(e.into());
                    return;
                }
            };
            drop(app);

            let rt = rt.clone();
            rt.spawn(async move {
                let mut st = state.lock().await;
                cfg.adapter_id = st.config.adapter_id.clone();
                let result = match st.client.as_mut() {
                    Some(c) => c.update_config(&cfg).await,
                    None => Err("未连接到核心服务".to_string()),
                };
                match result {
                    Ok(()) => {
                        st.config = cfg;
                        drop(st);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = app_weak.upgrade() {
                                app.set_config_error("".into());
                            }
                        });
                    }
                    Err(e) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = app_weak.upgrade() {
                                app.set_config_error(format!("保存失败: {e}").into());
                            }
                        });
                    }
                }
            });
        });
    }

    // ---- UI 回调：恢复默认 ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let rt = rt.clone();
        app.on_restore_default(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            rt.spawn(async move {
                let mut st = state.lock().await;
                let cfg = AppConfig {
                    adapter_id: st.config.adapter_id.clone(),
                    ..AppConfig::default()
                };
                let result = match st.client.as_mut() {
                    Some(c) => c.update_config(&cfg).await,
                    None => Err("未连接到核心服务".to_string()),
                };
                match result {
                    Ok(()) => {
                        st.config = cfg.clone();
                        drop(st);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = app_weak.upgrade() {
                                apply_config_to_ui(&app, &cfg);
                                app.set_config_error("已恢复默认配置".into());
                            }
                        });
                    }
                    Err(e) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = app_weak.upgrade() {
                                app.set_config_error(format!("恢复默认失败: {e}").into());
                            }
                        });
                    }
                }
            });
        });
    }

    // ---- UI 回调：打开日志目录 ----
    app.on_open_logs(|| {
        let dir = CORE_LOG_DIR;
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    });

    // ---- UI 回调：刷新日志 ----
    {
        let app_weak = app.as_weak();
        app.on_refresh_logs(move || {
            let entries = read_logs();
            let app_weak = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let model = VecModel::from(entries);
                if let Some(app) = app_weak.upgrade() {
                    app.set_log_entries(ModelRc::from(Rc::new(model)));
                }
            });
        });
    }

    // ---- UI 回调：检查更新 ----
    app.on_check_update(|| {
        let url = "https://github.com/5kywa1ker/route-tool/releases";
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn();
    });

    // ---- 状态轮询定时器（1s）：刷新状态文本 + 托盘 + 日志页 + 服务指示 ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let timer = slint::Timer::default();
        let last_fallback = Arc::new(Mutex::new(false));
        let last_tray_state = Arc::new(Mutex::new((tray::TrayState::Direct, false)));

        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                let state = state.clone();
                let app_weak = app_weak.clone();
                let last_fallback = last_fallback.clone();
                let last_tray_state = last_tray_state.clone();
                let rt = rt.clone();
                rt.spawn(async move {
                    let mut snapshot = {
                        let mut st = state.lock().await;
                        match st.client.as_mut() {
                            Some(c) => c.get_status().await.ok(),
                            None => None,
                        }
                    };

                    // 同步服务指示文字：与 IPC 客户端存活状态对齐。
                    {
                        let st = state.lock().await;
                        let txt = if st.client.is_some() {
                            "服务运行中"
                        } else {
                            "服务断开"
                        };
                        let app_weak2 = app_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = app_weak2.upgrade() {
                                app.set_service_status_text(txt.into());
                            }
                        });
                    }

                    if snapshot.is_none() {
                        let (reconnected, status_ok, cfg_ok, adapters_ok) = {
                            let mut st = state.lock().await;
                            match ipc_client::IpcClient::connect().await {
                                Ok(mut c) => {
                                    tracing::info!("重连核心成功");
                                    let status_ok = c.get_status().await.ok();
                                    let cfg_ok = c.get_config().await.ok();
                                    let adapters_ok = c.list_adapters().await.ok();
                                    st.client = Some(c);
                                    (true, status_ok, cfg_ok, adapters_ok)
                                }
                                Err(e) => {
                                    tracing::warn!("重连失败: {e:?}");
                                    (false, None, None, None)
                                }
                            }
                        };
                        if !reconnected {
                            return;
                        }
                        let (cfg, adapter_list) = {
                            let mut st = state.lock().await;
                            match (status_ok.as_ref(), cfg_ok, adapters_ok) {
                                (Some(_), Some(cfg), Some(list)) => {
                                    st.config = cfg.clone();
                                    st.adapters = list.clone();
                                    (Some(cfg), Some(list))
                                }
                                _ => {
                                    st.client = None;
                                    (None, None)
                                }
                            }
                        };
                        if let (Some(cfg), Some(list)) = (cfg, adapter_list) {
                            let sel = list.iter().position(|a| a.id == cfg.adapter_id);
                            let names = adapter_display_names(&list);
                            let adapter_id = cfg.adapter_id.clone();
                            let app_weak = app_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = app_weak.upgrade() {
                                    apply_config_to_ui(&app, &cfg);
                                    app.set_adapter_names(ModelRc::from(Rc::new(VecModel::from(
                                        names,
                                    ))));
                                    app.set_adapter_index(sel.map(|i| i as i32).unwrap_or(-1));
                                    update_adapter_info_display(&app, &list, &adapter_id);
                                }
                            });
                        }
                        snapshot = status_ok;
                    }

                    if snapshot.is_none() {
                        let mut st = state.lock().await;
                        if let Some(c) = st.client.as_mut() {
                            if c.get_status().await.is_err() {
                                st.client = None;
                            }
                        }
                        return;
                    }
                    let Some(rs) = snapshot else { return };

                    let need_adapter_refresh = {
                        let st = state.lock().await;
                        st.adapters.is_empty()
                    };
                    if need_adapter_refresh {
                        refresh_adapters(state.clone(), app_weak.clone()).await;
                    }

                    let (txt, tray_state, is_fb) = match &rs.health {
                        HealthStatus::Idle => {
                            ("直连（未启用）".to_string(), tray::TrayState::Direct, false)
                        }
                        HealthStatus::Healthy => {
                            ("旁路由生效".to_string(), tray::TrayState::Bypass, false)
                        }
                        HealthStatus::Degraded {
                            consecutive_failures,
                        } => (
                            format!("探测中（失败 {consecutive_failures} 次）"),
                            tray::TrayState::Bypass,
                            false,
                        ),
                        HealthStatus::Fallback => (
                            "异常已自动回退".to_string(),
                            tray::TrayState::Fallback,
                            true,
                        ),
                        HealthStatus::Recovered => {
                            ("检测恢复".to_string(), tray::TrayState::Direct, false)
                        }
                    };
                    let is_enabled = rs.is_enabled;

                    let latency = rs.last_latency_ms.map(|v| v as i32).unwrap_or(-1);
                    let last_check = if rs.last_updated.timestamp() > 0 {
                        let elapsed = (chrono::Utc::now() - rs.last_updated).num_seconds();
                        if elapsed < 60 {
                            "刚刚".to_string()
                        } else {
                            format!("{}分钟前", elapsed / 60)
                        }
                    } else {
                        "--".to_string()
                    };

                    let notifications_enabled = {
                        let st = state.lock().await;
                        st.config.notifications_enabled
                    };
                    let mut lf = last_fallback.lock().await;
                    if is_fb && !*lf && notifications_enabled {
                        notify::show_toast("旁路由异常", "旁路由异常，已自动切回直连");
                    }
                    *lf = is_fb;
                    drop(lf);

                    let tray_changed = {
                        let mut lts = last_tray_state.lock().await;
                        let changed = *lts != (tray_state, is_enabled);
                        *lts = (tray_state, is_enabled);
                        changed
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_weak.upgrade() {
                            app.set_status_text(txt.into());
                            app.set_is_enabled(is_enabled);
                            app.set_is_fallback(is_fb);
                            app.set_latency_ms(latency);
                            app.set_last_check_text(last_check.into());
                        }
                        if tray_changed {
                            if let Some(t) = tray::SHARED_TRAY.get() {
                                t.update_state(tray_state, is_enabled);
                            }
                        }
                    });
                });
            },
        );
        std::mem::forget(timer);
    }

    // ---- 托盘菜单事件 + 二次启动唤起 轮询（200ms）----
    {
        let app_weak = app.as_weak();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(200),
            move || {
                if single_instance::poll_show_request() {
                    if let Some(a) = app_weak.upgrade() {
                        a.window().show().ok();
                    }
                }
                match tray::SHARED_TRAY.get().and_then(|t| t.poll_event()) {
                    Some(tray::TrayAction::Toggle) => {
                        if let Some(a) = app_weak.upgrade() {
                            a.invoke_toggle_bypass();
                        }
                    }
                    Some(tray::TrayAction::OpenSettings) => {
                        if let Some(a) = app_weak.upgrade() {
                            a.window().show().ok();
                        }
                    }
                    Some(tray::TrayAction::OpenLogs) => {
                        let _ = std::process::Command::new("explorer")
                            .arg(CORE_LOG_DIR)
                            .spawn();
                    }
                    Some(tray::TrayAction::Quit) => {
                        if let Some(t) = tray::SHARED_TRAY.get() {
                            t.hide_icon();
                        }
                        slint::quit_event_loop().ok();
                    }
                    None => {}
                }
            },
        );
        std::mem::forget(timer);
    }

    app.show()?;
    slint::run_event_loop_until_quit()?;
    std::process::exit(0);
}

fn is_enabled_now(app: &slint::Weak<AppWindow>) -> bool {
    app.upgrade().map(|a| a.get_is_enabled()).unwrap_or(false)
}
