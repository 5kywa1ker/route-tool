//! Windows 服务宿主与命令行入口逻辑。

use std::ffi::OsString;
use std::time::Duration;

use tracing::{error, info};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

/// 服务名。
pub const SERVICE_NAME: &str = "BypassToolCore";
/// 服务显示名。
pub const SERVICE_DISPLAY: &str = "Bypass Tool Core";

define_windows_service!(ffi_service_main, my_service_main);

/// 服务入口。
pub fn my_service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        error!("服务运行失败: {e}");
    }
}

fn run_service() -> windows_service::Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::mpsc::channel::<()>(1);

    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.blocking_send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    let start_pending = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    };
    status_handle.set_service_status(start_pending)?;

    // 运行核心逻辑（tokio runtime）。
    let rt = tokio::runtime::Runtime::new().expect("创建 tokio runtime 失败");
    rt.block_on(async move {
        crate::run_core(stop_rx).await;
    });

    // 上报 Stopped。
    let stopped = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(1),
        process_id: None,
    };
    let _ = status_handle.set_service_status(stopped);
    info!("服务已停止");
    Ok(())
}

/// 安装服务（需管理员权限）。
pub fn install_service() -> anyhow::Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;
    let exe = std::env::current_exe()?;
    let config = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    manager.create_service(&config, ServiceAccess::ALL_ACCESS)?;
    info!("服务已安装");
    Ok(())
}

/// 卸载服务。
pub fn uninstall_service() -> anyhow::Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE)?;
    service.delete()?;
    info!("服务已卸载");
    Ok(())
}

/// 服务模式入口：启动 SCM 分发（阻塞）。
pub fn dispatch_service() -> anyhow::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}
