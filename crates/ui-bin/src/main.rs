//! bypass-ui.exe：托盘 + 设置窗口。

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

use ipc_protocol::{AppConfig, HealthStatus, SwitchMode};

slint::include_modules!();

/// UI 全局状态（连接共享）。
struct UiState {
    client: Option<ipc_client::IpcClient>,
    config: AppConfig,
    /// 上次 list_adapters 结果（下拉框数据源 + id 映射）。
    adapters: Vec<ipc_protocol::AdapterInfo>,
}

/// 拉取网卡列表并填充下拉框；同时按配置里的 adapter_id 选中对应项。
///
/// 注意：必须在 client 已连接后调用。启动阶段与连接任务并行调用会因竞态
/// 拿到 None 而空手而归（下拉框一直为空 = "无法选择网卡"），所以启动流程
/// 已改为串行（连接 → 配置 → 网卡），本函数只用于轮询里的自愈补拉。
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

    {
        let mut st = state.lock().await;
        st.adapters = list;
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            app.set_adapter_names(ModelRc::from(Rc::new(VecModel::from(names))));
            app.set_adapter_index(idx);
        }
    });
}

fn init_logging() {
    let dir = std::env::temp_dir().join("RouteTool");
    let _ = std::fs::create_dir_all(&dir);
    // 日志保留巡检（§2：保留 7 天），UI 与 core 各自清理自己的前缀。
    core_lib::log_prune::prune_old_logs(&dir, "bypass-ui.log", core_lib::log_prune::LOG_RETENTION);
    let appender = tracing_appender::rolling::daily(dir, "bypass-ui.log");
    let (writer, _guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();
    // _guard 泄漏以保持日志器存活。
    std::mem::forget(_guard);
}

fn main() -> anyhow::Result<()> {
    init_logging();

    // 单实例：已有 bypass-ui 在跑时直接退出（并通知其弹出设置窗口），
    // 否则双击桌面图标会再起一个进程、托盘上多出一个图标。
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

    // 事件循环由 Slint（winit 后端）独占持有。
    //
    // 这里**不能**再额外创建 tao/winit 的 EventLoop 与窗口：两个 windowing 栈同时
    // 初始化会互相踩踏，实测表现为启动约 3 秒后 0xC0000374 STATUS_HEAP_CORRUPTION
    // 直接退出（窗口闪一下就没了，托盘也不出现）。tray-icon 0.21 本身不依赖 tao，
    // 靠 Slint 的消息泵即可收到菜单事件。
    let app = AppWindow::new()?;
    // 托盘应用：关闭窗口 = 隐藏到托盘，不退出（核心独立运行不受影响）。
    app.window()
        .on_close_requested(move || slint::CloseRequestResponse::HideWindow);
    let tray = tray::Tray::new()?;
    let _ = tray::SHARED_TRAY.set(tray::StaticTray(tray));

    // ---- 连接核心：串行完成 连接 → 拉配置 → 拉网卡列表，一次性同步 UI ----
    //
    // 不能拆成并行的两个任务：拉网卡的分支抢锁时若 client 尚未就绪就会直接
    // 返回，而轮询的健康路径不会再补拉，网卡下拉框将永远是空的。
    // 连接失败（服务未就绪）时重试一段时间；彻底失败则交给 1s 轮询的重连逻辑。
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

            let (cfg_id, adapter_names, adapter_idx) = {
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
                            Some(cfg_id),
                            Some(names),
                            sel.map(|i| i as i32).unwrap_or(-1),
                        )
                    }
                    None => (None, None, -1),
                }
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    if let Some(cfg) = cfg {
                        app.set_bypass_ip_text(cfg.bypass_ip.to_string().into());
                        app.set_mode_index(match cfg.switch_mode {
                            SwitchMode::RouteOverlay => 0,
                            SwitchMode::AdapterReconfig => 1,
                        });
                        app.set_interval_secs(cfg.health_check_interval_secs as i32);
                        app.set_failure_threshold(cfg.failure_threshold as i32);
                        app.set_auto_reenable(cfg.auto_reenable_after_recovery);
                        app.set_notifications(cfg.notifications_enabled);
                        app.set_has_config(true);
                    }
                    if let Some(names) = adapter_names {
                        app.set_adapter_names(ModelRc::from(Rc::new(VecModel::from(names))));
                        app.set_adapter_index(adapter_idx);
                    }
                }
            });
            // cfg_id 当前仅用于填充选中项；消除未使用告警（保留变量便于后续扩展）。
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
                    st.config.adapter_id = info.id.clone();
                    // 立即落盘，避免启用时才发现未保存。
                    let cfg = st.config.clone();
                    if let Some(c) = st.client.as_mut() {
                        if let Err(e) = c.update_config(&cfg).await {
                            let app_weak = app_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = app_weak.upgrade() {
                                    app.set_config_error(format!("保存网卡选择失败: {e}").into());
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
            rt.spawn(async move {
                let result = {
                    let mut st = state.lock().await;
                    match st.client.as_mut() {
                        Some(c) => {
                            if enable {
                                c.enable().await
                            } else {
                                c.disable().await
                            }
                        }
                        None => Err("未连接到核心服务".to_string()),
                    }
                };
                if let Err(e) = result {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_weak.upgrade() {
                            app.set_config_error(e.into());
                        }
                    });
                }
            });
        });
    }

    // ---- UI 回调：测试连通性 ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let rt = rt.clone();
        app.on_test_connectivity(move || {
            let state = state.clone();
            let app_weak = app_weak.clone();
            // 先在 UI 线程取 IP 文本（slint 对象非 Send）。
            let ip_text = app_weak
                .upgrade()
                .map(|a| a.get_bypass_ip_text().to_string())
                .unwrap_or_default();
            let ip: std::net::IpAddr = match ip_text.parse() {
                Ok(ip) => ip,
                Err(_) => {
                    if let Some(app) = app_weak.upgrade() {
                        app.set_test_result("IP 地址无效".into());
                    }
                    return;
                }
            };
            let rt = rt.clone();
            rt.spawn(async move {
                let result = {
                    let mut st = state.lock().await;
                    match st.client.as_mut() {
                        Some(c) => c.test_connectivity(ip).await,
                        None => Err("未连接到核心服务".to_string()),
                    }
                };
                let msg = match result {
                    Ok(true) => "连通性 OK：旁路由可达".to_string(),
                    Ok(false) => "无法连通：请检查旁路由设备".to_string(),
                    Err(e) => format!("测试失败: {e}"),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = app_weak.upgrade() {
                        app.set_test_result(msg.into());
                    }
                });
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
            // 先在 UI 线程同步读取所有 UI 属性，再进异步任务（slint 对象非 Send）。
            let app = match app_weak.upgrade() {
                Some(a) => a,
                None => return,
            };
            let ip: std::net::IpAddr = match app.get_bypass_ip_text().parse() {
                Ok(ip) => ip,
                Err(_) => {
                    app.set_config_error("旁路由 IP 无效".into());
                    return;
                }
            };
            let mut cfg = AppConfig {
                bypass_ip: ip,
                switch_mode: if app.get_mode_index() == 1 {
                    SwitchMode::AdapterReconfig
                } else {
                    SwitchMode::RouteOverlay
                },
                ..AppConfig::default()
            };
            cfg.health_check_interval_secs = app.get_interval_secs().max(1) as u32;
            cfg.failure_threshold = app.get_failure_threshold().max(1) as u32;
            cfg.auto_reenable_after_recovery = app.get_auto_reenable();
            cfg.notifications_enabled = app.get_notifications();
            drop(app);

            let rt = rt.clone();
            rt.spawn(async move {
                let mut st = state.lock().await;
                // UI 不直接编辑 adapter_id，保留原值。
                cfg.adapter_id = st.config.adapter_id.clone();
                cfg.dns_override = st.config.dns_override.clone();
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

    // ---- UI 回调：打开日志目录 ----
    app.on_open_logs(|| {
        let dir = r"C:\ProgramData\RouteTool\logs";
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    });

    // ---- 状态轮询定时器（1s）：刷新状态文本 + 托盘 ----
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        let timer = slint::Timer::default();
        let last_fallback = Arc::new(Mutex::new(false));
        let last_tray_state = Arc::new(Mutex::new(tray::TrayState::Direct));

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
                    // 网络调用在持锁范围内完成，随后释放锁再更新 UI。
                    let mut snapshot = {
                        let mut st = state.lock().await;
                        match st.client.as_mut() {
                            Some(c) => c.get_status().await.ok(),
                            None => None,
                        }
                    };

                    // 未连接 → 尝试重连（核心服务可能晚于 UI 启动或重启过）。
                    if snapshot.is_none() {
                        let (reconnected, status_ok, cfg_ok, adapters_ok) = {
                            let mut st = state.lock().await;
                            match ipc_client::IpcClient::connect().await {
                                Ok(mut c) => {
                                    let status_ok = c.get_status().await.ok();
                                    let cfg_ok = c.get_config().await.ok();
                                    let adapters_ok = c.list_adapters().await.ok();
                                    st.client = Some(c);
                                    (true, status_ok, cfg_ok, adapters_ok)
                                }
                                Err(_) => (false, None, None, None),
                            }
                        };
                        // 重连成功后同步一次 UI（否则界面停在“未连接”）。握手
                        // 失败视为未连上，还原 client = None，等下轮再试。
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
                                    // 握手不完整：回滚连接，保持“未连接”语义。
                                    st.client = None;
                                    (None, None)
                                }
                            }
                        };
                        if let (Some(cfg), Some(list)) = (cfg, adapter_list) {
                            let sel = list.iter().position(|a| a.id == cfg.adapter_id);
                            let names: Vec<SharedString> =
                                list.iter().map(|a| a.name.clone().into()).collect();
                            let app_weak = app_weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = app_weak.upgrade() {
                                    app.set_bypass_ip_text(cfg.bypass_ip.to_string().into());
                                    app.set_mode_index(match cfg.switch_mode {
                                        SwitchMode::RouteOverlay => 0,
                                        SwitchMode::AdapterReconfig => 1,
                                    });
                                    app.set_interval_secs(cfg.health_check_interval_secs as i32);
                                    app.set_failure_threshold(cfg.failure_threshold as i32);
                                    app.set_auto_reenable(cfg.auto_reenable_after_recovery);
                                    app.set_notifications(cfg.notifications_enabled);
                                    app.set_has_config(true);
                                    app.set_adapter_names(ModelRc::from(Rc::new(VecModel::from(
                                        names,
                                    ))));
                                    app.set_adapter_index(sel.map(|i| i as i32).unwrap_or(-1));
                                }
                            });
                        }
                        // 重连当轮就拿到了新状态，直接用它刷新。
                        snapshot = status_ok;
                    }

                    // get_status 连续失败视为连接失效，丢弃 client 触发下轮重连。
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

                    // 网卡列表尚未就绪时补拉一次（自愈启动竞态/首次连接失败）。
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

                    // 回退 Toast（只在状态从非回退变为回退时弹）。
                    let mut lf = last_fallback.lock().await;
                    if is_fb && !*lf {
                        notify::show_toast("旁路由异常", "旁路由异常，已自动切回直连");
                    }
                    *lf = is_fb;
                    drop(lf);

                    // 托盘状态只在变化时更新（重绘图标开销大）。
                    let tray_changed = {
                        let mut lts = last_tray_state.lock().await;
                        let changed = *lts != tray_state;
                        *lts = tray_state;
                        changed
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_weak.upgrade() {
                            app.set_status_text(txt.into());
                            app.set_is_enabled(is_enabled);
                            app.set_is_fallback(is_fb);
                        }
                        if tray_changed {
                            if let Some(t) = tray::SHARED_TRAY.get() {
                                t.update_state(tray_state);
                            }
                        }
                    });
                });
            },
        );
        std::mem::forget(timer); // 保活
    }

    // ---- 托盘菜单事件 + 二次启动唤起 轮询（200ms）----
    {
        let app_weak = app.as_weak();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(200),
            move || {
                // 用户再次双击桌面图标 → 通知事件 → 弹出已有实例的设置窗口。
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
                            .arg(r"C:\ProgramData\RouteTool\logs")
                            .spawn();
                    }
                    Some(tray::TrayAction::Quit) => {
                        // 先显式移除托盘图标再退出：进程结束后 Windows 会残留“幽灵图标”，
                        // 看起来像没有退出干净。
                        if let Some(t) = tray::SHARED_TRAY.get() {
                            t.hide_icon();
                        }
                        slint::quit_event_loop().ok();
                    }
                    None => {}
                }
            },
        );
        std::mem::forget(timer); // 保活
    }

    app.show()?;
    // 托盘常驻：窗口隐藏/无可见 UI 也不退出事件循环，直到显式 quit。
    slint::run_event_loop_until_quit()?;

    // 事件循环退出（托盘“退出”）后立即终止进程：不等任何残留句柄/后台任务，
    // 确保任务管理器里不留 bypass-ui.exe。
    std::process::exit(0);
}

/// 当前是否启用（从 UI 状态读）。
fn is_enabled_now(app: &slint::Weak<AppWindow>) -> bool {
    app.upgrade().map(|a| a.get_is_enabled()).unwrap_or(false)
}
