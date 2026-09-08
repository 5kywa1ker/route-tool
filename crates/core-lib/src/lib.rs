//! core-lib：平台无关的核心逻辑。
//!
//! 包含切换策略 trait、网卡探测 trait、健康检测状态机与状态持久化。
//! 不含任何 Windows 专属实现（实现在 core-win）。

use std::net::IpAddr;

pub mod health_monitor;
pub mod net_inspector;
pub mod state_store;
pub mod switch_engine;

// 平台无关数据类型，随 ipc-protocol 复用给 UI。
pub use ipc_protocol::{
    AdapterInfo, AdapterSnapshot, AppConfig, BypassTarget, HealthEvent, HealthEventType,
    HealthStatus, RouteState, RuntimeState, SwitchHandle, SwitchMode,
};

/// core 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("配置未完成：{0}")]
    ConfigIncomplete(String),
    #[error("网络操作失败：{0}")]
    Network(String),
    #[error("状态持久化失败：{0}")]
    Persistence(String),
    #[error("任务冲突：当前已有活动句柄")]
    AlreadyEnabled,
    #[error("当前未启用旁路由")]
    NotEnabled,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl CoreError {
    pub fn app_error_code(&self) -> i32 {
        1
    }
}

/// 默认旁路由检测目标（通用公网稳定地址），用于首次配置向导连通性校验。
pub const DEFAULT_PROBE_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(223, 5, 5, 5));
