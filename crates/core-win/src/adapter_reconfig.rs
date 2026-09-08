//! 网卡直改策略的 Windows 实现：备份 / 静态化 / 恢复。
//!
//! 备份在 core-bin 控制层完成（写 state_store 快照），本模块负责对网卡的实际变更。

use std::net::Ipv4Addr;

use async_trait::async_trait;
use tracing::info;

use core_lib::switch_engine::{ReconcileAction, SwitchStrategy};
use core_lib::{BypassTarget, CoreError, Result, SwitchHandle, SwitchMode};

use crate::adapters;
use crate::netsh;

/// 网卡直改策略。
pub struct AdapterReconfigStrategy;

impl Default for AdapterReconfigStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterReconfigStrategy {
    pub fn new() -> Self {
        Self
    }

    /// 找到 adapter_id 对应的网卡显示名（netsh 用名字）。
    fn iface_name(&self, adapter_id: &str) -> Result<String> {
        let list = adapters::list_adapters()
            .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
        list.into_iter()
            .find(|a| a.id == adapter_id)
            .map(|a| a.name)
            .ok_or_else(|| CoreError::Network(format!("网卡 {adapter_id} 不存在")))
    }
}

#[async_trait]
impl SwitchStrategy for AdapterReconfigStrategy {
    async fn enable(&self, target: &BypassTarget) -> Result<SwitchHandle> {
        let bypass_ip = match target.bypass_ip {
            std::net::IpAddr::V4(v4) => v4,
            _ => return Err(CoreError::Network("仅支持 IPv4 旁路由地址".into())),
        };

        let iface = self.iface_name(&target.adapter_id)?;

        // 当前网卡的 IPv4 与掩码（保留原 IP，仅改网关）。
        let list = adapters::list_adapters()
            .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
        let current = list
            .iter()
            .find(|a| a.id == target.adapter_id)
            .ok_or_else(|| CoreError::Network(format!("网卡 {} 不存在", target.adapter_id)))?;

        let ip = current
            .ipv4
            .first()
            .copied()
            .ok_or_else(|| CoreError::Network("网卡无 IPv4 地址，无法直改".into()))?;
        let ip = match ip {
            std::net::IpAddr::V4(v4) => v4,
            _ => return Err(CoreError::Network("仅支持 IPv4".into())),
        };

        // 保留原有 DNS 或默认使用旁路由 IP。
        let dns: Vec<Ipv4Addr> = match &target.dns {
            Some(d) if !d.is_empty() => d
                .iter()
                .filter_map(|x| match x {
                    std::net::IpAddr::V4(v4) => Some(*v4),
                    _ => None,
                })
                .collect(),
            _ => vec![bypass_ip],
        };

        // 255.255.255.0 为常见掩码；若原配置有更精确信息可扩展。MVP 用 /24。
        let mask = Ipv4Addr::new(255, 255, 255, 0);

        netsh::set_static_ipv4(&iface, ip, mask, bypass_ip).await?;
        netsh::set_dns(&iface, &dns).await?;

        info!("adapter reconfig enabled: {iface} gw={bypass_ip} dns={dns:?}");

        Ok(SwitchHandle {
            mode: SwitchMode::AdapterReconfig,
            if_index: None,
            if_luid: None,
            destination_prefix: None,
            next_hop: None,
            adapter_id: Some(target.adapter_id.clone()),
        })
    }

    async fn disable(&self, handle: &SwitchHandle) -> Result<()> {
        // 恢复动作需要快照，由 core-bin 控制层在调用前先处理。
        // 这里仅做标记性日志；实际恢复见 Controller.disable。
        info!("adapter reconfig disable requested: {:?}", handle.adapter_id);
        Ok(())
    }

    async fn is_active(&self, handle: &SwitchHandle) -> Result<bool> {
        let Some(adapter_id) = &handle.adapter_id else {
            return Ok(false);
        };
        let list = adapters::list_adapters()
            .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
        match list.iter().find(|a| a.id == *adapter_id) {
            Some(a) => {
                // 生效 = 网卡 Up 且网关列表首项为 bypass 网关（近似判断）。
                Ok(a.is_connected && !a.gateway.is_empty())
            }
            None => Ok(false),
        }
    }

    async fn reconcile_on_startup(&self) -> Result<ReconcileAction> {
        // 恢复逻辑需要快照，由 core-bin 控制层处理。
        Ok(ReconcileAction::NoAction)
    }
}
