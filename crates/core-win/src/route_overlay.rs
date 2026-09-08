//! 路由叠加策略的 Windows 实现：CreateIpForwardEntry2 / DeleteIpForwardEntry2。

use std::net::{IpAddr, Ipv4Addr};

use async_trait::async_trait;
use tracing::{info, warn};
use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    CreateIpForwardEntry2, DeleteIpForwardEntry2, GetBestRoute2, GetIpForwardTable2, FreeMibTable,
    MIB_IPFORWARD_ROW2,
};
use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows::Win32::Networking::WinSock::{
    NL_ROUTE_PROTOCOL, SOCKADDR_INET,
};

use core_lib::switch_engine::{ReconcileAction, SwitchStrategy};
use core_lib::{BypassTarget, CoreError, Result, SwitchHandle, SwitchMode};

use crate::adapters;
use crate::icmp::in_addr_from;

/// 路由叠加策略：添加 0.0.0.0/0 via bypass_ip，metric 优于系统默认路由。
pub struct RouteOverlayStrategy {
    /// 叠加路由使用的 metric（小值优先）。
    pub metric: u32,
}

impl Default for RouteOverlayStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteOverlayStrategy {
    pub fn new() -> Self {
        Self { metric: 5 }
    }

    /// 找到能到达 bypass_ip 的接口（利用 GetBestRoute2）。
    fn resolve_interface_for(&self, bypass_ip: Ipv4Addr) -> Result<(u32, u64)> {
        let mut dest: SOCKADDR_INET = Default::default();
        dest.Ipv4 = windows::Win32::Networking::WinSock::SOCKADDR_IN {
            sin_family: windows::Win32::Networking::WinSock::AF_INET,
            sin_port: 0,
            sin_addr: in_addr_from(bypass_ip),
            sin_zero: [0; 8],
        };

        let mut row: MIB_IPFORWARD_ROW2 = Default::default();
        let mut best_src: SOCKADDR_INET = Default::default();
        let rc = unsafe {
            GetBestRoute2(None, 0, None, &dest, 0, &mut row, &mut best_src)
        };
        if rc != NO_ERROR {
            return Err(CoreError::Network(format!(
                "找不到到 {bypass_ip} 的路由（旁路由地址不可达？）: 0x{:x}",
                rc.0
            )));
        }

        Ok((row.InterfaceIndex, unsafe { row.InterfaceLuid.Value }))
    }
}

fn prefix_zero() -> SOCKADDR_INET {
    let mut s: SOCKADDR_INET = Default::default();
    s.Ipv4 = windows::Win32::Networking::WinSock::SOCKADDR_IN {
        sin_family: windows::Win32::Networking::WinSock::AF_INET,
        sin_port: 0,
        sin_addr: in_addr_from(Ipv4Addr::UNSPECIFIED),
        sin_zero: [0; 8],
    };
    s
}

fn sockaddr_inet(ip: Ipv4Addr) -> SOCKADDR_INET {
    let mut s: SOCKADDR_INET = Default::default();
    s.Ipv4 = windows::Win32::Networking::WinSock::SOCKADDR_IN {
        sin_family: windows::Win32::Networking::WinSock::AF_INET,
        sin_port: 0,
        sin_addr: in_addr_from(ip),
        sin_zero: [0; 8],
    };
    s
}

#[async_trait]
impl SwitchStrategy for RouteOverlayStrategy {
    async fn enable(&self, target: &BypassTarget) -> Result<SwitchHandle> {
        let bypass_ip = match target.bypass_ip {
            IpAddr::V4(v4) => v4,
            _ => return Err(CoreError::Network("仅支持 IPv4 旁路由地址".into())),
        };

        let (if_index, if_luid) = self.resolve_interface_for(bypass_ip)?;

        let mut row: MIB_IPFORWARD_ROW2 = MIB_IPFORWARD_ROW2 {
            InterfaceLuid: NET_LUID_LH { Value: if_luid },
            InterfaceIndex: if_index,
            ..Default::default()
        };
        row.DestinationPrefix.Prefix = prefix_zero();
        row.NextHop = sockaddr_inet(bypass_ip);
        row.Metric = self.metric;
        row.Protocol = NL_ROUTE_PROTOCOL(3); // netmgmt

        let rc = unsafe { CreateIpForwardEntry2(&row) };
        if rc != NO_ERROR {
            return Err(CoreError::Network(format!(
                "CreateIpForwardEntry2 失败: 0x{:x}",
                rc.0
            )));
        }

        info!("route overlay enabled: 0.0.0.0/0 via {bypass_ip} on if_index={if_index}");

        Ok(SwitchHandle {
            mode: SwitchMode::RouteOverlay,
            if_index: Some(if_index),
            if_luid: Some(if_luid),
            destination_prefix: Some("0.0.0.0/0".to_string()),
            next_hop: Some(IpAddr::V4(bypass_ip)),
            adapter_id: None,
        })
    }

    async fn disable(&self, handle: &SwitchHandle) -> Result<()> {
        let (Some(if_luid), Some(next_hop)) = (handle.if_luid, handle.next_hop) else {
            return Err(CoreError::Other("句柄缺少路由条目信息".into()));
        };
        let bypass_ip = match next_hop {
            IpAddr::V4(v4) => v4,
            _ => return Err(CoreError::Other("句柄含非 IPv4 下一跳".into())),
        };

        let mut row: MIB_IPFORWARD_ROW2 = MIB_IPFORWARD_ROW2 {
            InterfaceLuid: NET_LUID_LH { Value: if_luid },
            ..Default::default()
        };
        if let Some(idx) = handle.if_index {
            row.InterfaceIndex = idx;
        }
        row.DestinationPrefix.Prefix = prefix_zero();
        row.NextHop = sockaddr_inet(bypass_ip);

        let rc = unsafe { DeleteIpForwardEntry2(&row) };
        if rc != NO_ERROR {
            warn!("DeleteIpForwardEntry2 返回 0x{:x}（条目可能已不存在）", rc.0);
            return Err(CoreError::Network(format!(
                "DeleteIpForwardEntry2 失败: 0x{:x}",
                rc.0
            )));
        }

        info!("route overlay disabled: 0.0.0.0/0 via {bypass_ip}");
        Ok(())
    }

    async fn is_active(&self, handle: &SwitchHandle) -> Result<bool> {
        let Some(next_hop) = handle.next_hop else {
            return Ok(false);
        };
        let bypass_ip = match next_hop {
            IpAddr::V4(v4) => v4,
            _ => return Ok(false),
        };

        let mut table: *mut windows::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_TABLE2 =
            std::ptr::null_mut();
        unsafe {
            if GetIpForwardTable2(AF_INET, &mut table) != NO_ERROR {
                return Ok(false);
            }
        }

        let mut found = false;
        let n = unsafe { (*table).NumEntries };
        let rows = unsafe { core::slice::from_raw_parts((*table).Table.as_ptr(), n as usize) };
        for row in rows {
            if row.DestinationPrefix.PrefixLength != 0 {
                continue;
            }
            let raw = unsafe { row.NextHop.Ipv4.sin_addr.S_un.S_addr };
            if Ipv4Addr::from(raw.to_be_bytes()) != bypass_ip {
                continue;
            }
            if let Some(idx) = handle.if_index {
                if row.InterfaceIndex == idx && adapters::is_interface_up(idx) {
                    found = true;
                    break;
                }
            }
        }
        unsafe { FreeMibTable(table as *const _) };
        Ok(found)
    }

    async fn reconcile_on_startup(&self) -> Result<ReconcileAction> {
        // 一致性修正由 core-bin 控制层结合 state_store 预期状态处理；
        // 策略层只提供路由表读取能力。
        Ok(ReconcileAction::NoAction)
    }
}

use windows::Win32::Networking::WinSock::AF_INET;
