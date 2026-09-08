//! 切换策略抽象：RouteOverlay 与 AdapterReconfig 实现。

use async_trait::async_trait;

use crate::{BypassTarget, Result, SwitchHandle};

/// 启动时一致性校验的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// 实际状态与预期一致，无需动作。
    NoAction,
    /// 按上次预期状态重新启用（修正脏状态）。
    ReEnabled,
    /// 清理了不在预期中的残留状态（如不应存在但存在的旁路由路由）。
    CleanedUp,
}

/// 切换策略接口。方法签名不泄露 Windows 专属概念。
#[async_trait]
pub trait SwitchStrategy: Send + Sync {
    /// 启用旁路由，返回用于精确 disable 的句柄。
    async fn enable(&self, target: &BypassTarget) -> Result<SwitchHandle>;

    /// 按句柄禁用旁路由。
    async fn disable(&self, handle: &SwitchHandle) -> Result<()>;

    /// 查询句柄对应的旁路由在当前系统中是否仍处于生效状态。
    async fn is_active(&self, handle: &SwitchHandle) -> Result<bool>;

    /// 启动时一致性校验：根据上次预期状态与实际状态比对，修正脏状态。
    async fn reconcile_on_startup(&self) -> Result<ReconcileAction>;
}

/// 旁路由路由条目的默认 metric 值（取较小值，优于系统默认路由）。
pub const ROUTE_METRIC: u32 = 5;
