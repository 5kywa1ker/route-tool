//! bypass-core.exe：Windows 服务宿主 + Named Pipe IPC server。
//!
//! 用法：
//!   bypass-core.exe                     服务模式（由 SCM 启动）
//!   bypass-core.exe --install-service   安装服务（需管理员）
//!   bypass-core.exe --uninstall-service 卸载服务（需管理员）
//!   bypass-core.exe --console           控制台调试模式（不走 SCM）

mod controller;
mod ipc_server;
mod netcheck;
mod service_host;
mod state;

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use controller::Controller;

use state::ServerState;

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let store = core_lib::state_store::StateStore::default();
    let _ = store.ensure_dirs();
    let dir = store.base_dir().join("logs");
    let appender = tracing_appender::rolling::daily(dir, "bypass-core.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Some(guard)
}

/// 核心运行逻辑：初始化状态 + reconcile + IPC server。
pub async fn run_core(stop_rx: mpsc::Receiver<()>) {
    let store = core_lib::state_store::StateStore::default();
    if let Err(e) = store.ensure_dirs() {
        error!("初始化数据目录失败: {e}");
    }

    let controller = match Controller::new(store) {
        Ok(c) => c,
        Err(e) => {
            error!("初始化控制器失败: {e}");
            return;
        }
    };
    let state = Arc::new(ServerState::new(controller));

    // 启动一致性校验。
    if let Err(e) = state.controller.reconcile_on_startup().await {
        error!("启动校验失败: {e}");
    }

    let (_shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let _ = stop_rx; // TODO: 接入优雅停机
    ipc_server::serve(state.clone(), shutdown_rx).await;

    info!("core 退出");
    drop(stop_rx);
}

#[tokio::main]
async fn main() {
    let _log_guard = init_logging();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--install-service") => {
            match service_host::install_service() {
                Ok(()) => println!("服务安装成功"),
                Err(e) => {
                    eprintln!("服务安装失败: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--uninstall-service") => {
            match service_host::uninstall_service() {
                Ok(()) => println!("服务卸载成功"),
                Err(e) => {
                    eprintln!("服务卸载失败: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--console") => {
            info!("控制台模式启动");
            let (stop_tx, stop_rx) = mpsc::channel(1);
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                info!("收到 Ctrl+C，退出");
                let _ = stop_tx.send(()).await;
            });
            run_core(stop_rx).await;
            return;
        }
        _ => {}
    }

    // 服务模式。
    if let Err(e) = service_host::dispatch_service() {
        error!("服务分发失败: {e}");
        eprintln!("应以 Windows 服务方式运行，或使用 --console 调试。");
        std::process::exit(1);
    }
}
