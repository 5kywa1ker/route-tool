//! 网卡与网络状态探测抽象。

use async_trait::async_trait;
use std::net::IpAddr;

use crate::{AdapterInfo, Result, RouteState};

/// 网卡 / 网络探测接口。平台无关。
#[async_trait]
pub trait NetInspector: Send + Sync {
    /// 列出系统网卡。
    async fn list_adapters(&self) -> Result<Vec<AdapterInfo>>;

    /// 查询指定网卡的当前路由层状态。
    async fn current_route_state(&self, adapter_id: &str) -> Result<RouteState>;

    /// 对给定 IP 做连通性探测（ICMP ping）。
    /// 返回 `Ok(Some(latency_ms))` 表示可达并给出往返毫秒；`Ok(None)` 表示不可达或超时。
    async fn ping(&self, ip: IpAddr, timeout_ms: u32) -> Result<Option<u32>>;
}
