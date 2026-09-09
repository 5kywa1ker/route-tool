//! NetInspector trait 的 Windows 实现。

use std::net::IpAddr;

use async_trait::async_trait;

use core_lib::net_inspector::NetInspector;
use core_lib::{AdapterInfo, CoreError, Result, RouteState};

use crate::{adapters, icmp};

/// 基于 IP Helper 的网络探测实现。
pub struct WinNetInspector;

#[async_trait]
impl NetInspector for WinNetInspector {
    async fn list_adapters(&self) -> Result<Vec<AdapterInfo>> {
        // 枚举是快速调用，放 spawn_blocking 防止阻塞 runtime。
        tokio::task::spawn_blocking(|| {
            adapters::list_adapters().map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))
        })
        .await
        .map_err(|e| CoreError::Other(format!("任务失败: {e}")))?
    }

    async fn current_route_state(&self, adapter_id: &str) -> Result<RouteState> {
        let id = adapter_id.to_string();
        tokio::task::spawn_blocking(move || {
            adapters::current_route_state(&id)
                .map_err(|e| CoreError::Network(format!("读取路由状态失败: {e}")))
        })
        .await
        .map_err(|e| CoreError::Other(format!("任务失败: {e}")))?
    }

    async fn ping(&self, ip: IpAddr, timeout_ms: u32) -> Result<Option<u32>> {
        match ip {
            IpAddr::V4(v4) => {
                let latency = tokio::task::spawn_blocking(move || icmp::ping_ipv4(v4, timeout_ms))
                    .await
                    .map_err(|e| CoreError::Other(format!("任务失败: {e}")))?;
                Ok(latency)
            }
            IpAddr::V6(_) => Err(CoreError::Network("MVP 不支持 IPv6".into())),
        }
    }
}
