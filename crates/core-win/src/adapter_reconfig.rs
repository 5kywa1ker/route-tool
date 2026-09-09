//! 网卡直改策略的 Windows 实现：备份 / 静态化 / 恢复。
//!
//! 启用前的快照备份由 core-bin 控制层写入 state_store；本策略持有同一
//! StateStore，disable（含健康检测触发的自动回退）时按快照恢复网卡，
//! 保证"自动回退 = 调用当前策略的 disable"语义成立。

use std::net::Ipv4Addr;

use async_trait::async_trait;
use tracing::info;

use core_lib::state_store::StateStore;
use core_lib::switch_engine::{ReconcileAction, SwitchStrategy};
use core_lib::{AdapterSnapshot, BypassTarget, CoreError, Result, SwitchHandle, SwitchMode};

use crate::adapters;
use crate::netsh;

/// 网卡直改策略。
pub struct AdapterReconfigStrategy {
    store: StateStore,
}

/// 前缀长度转点分十进制子网掩码。
pub(crate) fn mask_from_prefix(p: u8) -> Ipv4Addr {
    match p {
        0 => Ipv4Addr::UNSPECIFIED,
        1..=31 => Ipv4Addr::from(u32::MAX << (32 - p)),
        _ => Ipv4Addr::new(255, 255, 255, 255),
    }
}

/// 策略默认子网掩码（用户未配置时使用）。
const DEFAULT_MASK_PREFIX: u8 = 24;

impl AdapterReconfigStrategy {
    pub fn new(store: StateStore) -> Self {
        Self { store }
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

    /// 按快照恢复网卡状态（DHCP 或原静态参数）。
    async fn restore_adapter(&self, snap: &AdapterSnapshot) -> Result<()> {
        let list = adapters::list_adapters()
            .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
        let name = list
            .iter()
            .find(|x| x.id == snap.adapter_id)
            .map(|x| x.name.clone())
            .ok_or_else(|| CoreError::Network(format!("网卡 {} 已不存在", snap.adapter_id)))?;

        if snap.is_dhcp_enabled {
            netsh::enable_dhcp(&name).await?;
            netsh::reset_dns(&name).await?;
        } else {
            // 恢复原静态参数（掩码取快照记录的真实前缀，缺项按 /24）。
            // 快照残缺（缺 IP 或网关）时不再硬报错——那种状态下硬报错会把
            // 网卡永远留在旁路由网关上；退回 DHCP 至少保证能恢复联网。
            let ip = snap.static_ipv4.first().and_then(|x| match x {
                std::net::IpAddr::V4(v4) => Some(v4),
                _ => None,
            });
            let gw = snap.gateway.first().and_then(|x| match x {
                std::net::IpAddr::V4(v4) => Some(v4),
                _ => None,
            });
            let (Some(ip), Some(gw)) = (ip, gw) else {
                tracing::warn!(
                    "快照缺少 IP/网关（adapter {}），回退为 DHCP 恢复",
                    snap.adapter_id
                );
                netsh::enable_dhcp(&name).await?;
                netsh::reset_dns(&name).await?;
                info!(
                    "adapter {} restored as dhcp (incomplete snapshot)",
                    snap.adapter_id
                );
                return Ok(());
            };
            let mask = mask_from_prefix(
                snap.static_ipv4_mask
                    .first()
                    .copied()
                    .unwrap_or(DEFAULT_MASK_PREFIX),
            );

            netsh::set_static_ipv4(&name, *ip, mask, *gw).await?;
            if snap.dns.is_empty() {
                netsh::reset_dns(&name).await?;
            } else {
                let dns4: Vec<Ipv4Addr> = snap
                    .dns
                    .iter()
                    .filter_map(|x| match x {
                        std::net::IpAddr::V4(v4) => Some(*v4),
                        _ => None,
                    })
                    .collect();
                netsh::set_dns(&name, &dns4).await?;
            }
        }
        info!("adapter {} restored from snapshot", snap.adapter_id);
        Ok(())
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

        // 掩码：优先用户配置，未配置按 /24。
        let mask = target
            .subnet_mask
            .unwrap_or_else(|| mask_from_prefix(DEFAULT_MASK_PREFIX));

        netsh::set_static_ipv4(&iface, ip, mask, bypass_ip).await?;
        netsh::set_dns(&iface, &dns).await?;

        info!("adapter reconfig enabled: {iface} ip={ip} mask={mask} gw={bypass_ip} dns={dns:?}");

        Ok(SwitchHandle {
            mode: SwitchMode::AdapterReconfig,
            if_index: None,
            if_luid: None,
            destination_prefix: None,
            // 记录旁路由网关，is_active 据此精确校验是否仍生效。
            next_hop: Some(std::net::IpAddr::V4(bypass_ip)),
            adapter_id: Some(target.adapter_id.clone()),
            extra_routes: vec![],
        })
    }

    async fn disable(&self, handle: &SwitchHandle) -> Result<()> {
        let Some(adapter_id) = &handle.adapter_id else {
            return Ok(());
        };
        // 按快照恢复：健康检测的自动回退与本入口共用同一条路径。
        match self.store.load_snapshot()? {
            Some(snap) => {
                self.restore_adapter(&snap).await?;
                self.store.clear_snapshot()?;
                info!("adapter reconfig disabled: {adapter_id} restored from snapshot");
            }
            None => info!("adapter reconfig disable: 无快照，保持当前网卡状态 ({adapter_id})"),
        }
        Ok(())
    }

    async fn is_active(&self, handle: &SwitchHandle) -> Result<bool> {
        let Some(adapter_id) = &handle.adapter_id else {
            return Ok(false);
        };
        // 生效 = 网卡 Up 且默认网关就是旁路由地址。
        // 期望网关记录在句柄的 next_hop 里（enable 时写入）。
        let expected_gw = match handle.next_hop {
            Some(std::net::IpAddr::V4(v4)) => v4,
            // 旧版本句柄没有记录网关；此时无法精确校验，按未生效处理，
            // 由健康检测重建句柄（reenable 后即带 next_hop）。
            _ => return Ok(false),
        };
        let list = adapters::list_adapters()
            .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
        match list.iter().find(|a| a.id == *adapter_id) {
            Some(a) => Ok(a.is_connected && a.gateway.iter().any(|g| *g == expected_gw)),
            None => Ok(false),
        }
    }

    async fn reconcile_on_startup(&self) -> Result<ReconcileAction> {
        // 恢复逻辑需要快照与配置上下文，由 core-bin 控制层处理。
        Ok(ReconcileAction::NoAction)
    }
}
