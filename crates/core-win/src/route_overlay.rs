//! 路由叠加策略的 Windows 实现：CreateIpForwardEntry2 / DeleteIpForwardEntry2。
//!
//! 叠加采用 `0.0.0.0/1` + `128.0.0.0/1` 两条路由覆盖默认路由：最长前缀匹配
//! 必然优于系统 /0 默认路由，不依赖"路由 metric + 接口跃点"的组合比较
//! （若仅添加 metric 5 的 /0 路由，有效度量 = 接口跃点 + 5，反而劣于
//! DHCP 默认路由，叠加不会生效）。

use std::net::{IpAddr, Ipv4Addr};

use async_trait::async_trait;
use tracing::{info, warn};
use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    CreateIpForwardEntry2, DeleteIpForwardEntry2, FreeMibTable, GetBestRoute2, GetIpForwardTable2,
    MIB_IPFORWARD_ROW2, MIB_IPFORWARD_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows::Win32::Networking::WinSock::{AF_INET, NL_ROUTE_PROTOCOL, SOCKADDR_IN, SOCKADDR_INET};

use core_lib::switch_engine::{ReconcileAction, SwitchStrategy};
use core_lib::{BypassTarget, CoreError, HandleRoute, Result, SwitchHandle, SwitchMode};

use crate::adapters;
use crate::icmp::{in_addr_from, ipv4_from_net_order};

/// 主条目前缀（0.0.0.0/1，覆盖 0.0.0.0 ~ 127.255.255.255）。
pub const MAIN_PREFIX: &str = "0.0.0.0/1";
/// 附加条目前缀（128.0.0.0/1，覆盖 128.0.0.0 ~ 255.255.255.255）。
pub const EXTRA_PREFIX: &str = "128.0.0.0/1";

/// 本策略添加的路由协议标记（NL_ROUTE_PROTOCOL netmgmt = 3）。
const ROUTE_PROTOCOL_NETMGMT: i32 = 3;

/// 路由生命周期"无限"（0xFFFFFFFF，见 MIB_IPFORWARD_ROW2 文档）。
///
/// 必须显式设置：`Default::default()` 得到的是 0 秒，Windows 会把这类条目
/// 视为**已过期**——它仍能被 GetIpForwardTable2 / `route print` 枚举出来
/// （State 照样是 Alive，极具迷惑性），但不会参与最长前缀匹配与转发，
/// 表现为"启用成功、路由在表里，流量却仍走原默认网关"。
const INFINITE_LIFETIME: u32 = u32::MAX;

/// 路由叠加策略：添加两条经旁路由的 /1 路由，覆盖默认路由。
pub struct RouteOverlayStrategy {
    /// 叠加路由使用的 metric（/1 前缀必然胜出，metric 仅作提示）。
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
        dest.Ipv4 = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: 0,
            sin_addr: in_addr_from(bypass_ip),
            sin_zero: [0; 8],
        };

        let mut row: MIB_IPFORWARD_ROW2 = Default::default();
        let mut best_src: SOCKADDR_INET = Default::default();
        let rc = unsafe { GetBestRoute2(None, 0, None, &dest, 0, &mut row, &mut best_src) };
        if rc != NO_ERROR {
            return Err(CoreError::Network(format!(
                "找不到到 {bypass_ip} 的路由（旁路由地址不可达？）: 0x{:x}",
                rc.0
            )));
        }

        Ok((row.InterfaceIndex, unsafe { row.InterfaceLuid.Value }))
    }

    /// 添加一条 /1 叠加路由；若存在残留同名条目（异常退出未清理）则先删后重试。
    async fn add_route(
        &self,
        if_index: u32,
        if_luid: u64,
        dest: Ipv4Addr,
        prefix_len: u8,
        next_hop: Ipv4Addr,
    ) -> Result<()> {
        let row = build_row(if_luid, if_index, dest, prefix_len, next_hop, self.metric);
        let rc = unsafe { CreateIpForwardEntry2(&row) };
        if rc == NO_ERROR {
            return Ok(());
        }
        warn!(
            "CreateIpForwardEntry2 返回 0x{:x}，尝试清理旧条目后重试",
            rc.0
        );
        unsafe {
            let _ = DeleteIpForwardEntry2(&row);
        }
        let rc2 = unsafe { CreateIpForwardEntry2(&row) };
        if rc2 != NO_ERROR {
            return Err(CoreError::Network(format!(
                "CreateIpForwardEntry2 失败: 0x{:x}（Windows 错误码 {}）",
                rc2.0, rc2.0
            )));
        }
        Ok(())
    }

    /// 精确删除一条路由（条目不存在时返回 Err，由调用方决定是否忽略）。
    fn delete_row(
        if_luid: u64,
        if_index: Option<u32>,
        dest: Ipv4Addr,
        prefix_len: u8,
        next_hop: Ipv4Addr,
    ) -> Result<()> {
        let row = build_row(
            if_luid,
            if_index.unwrap_or(0),
            dest,
            prefix_len,
            next_hop,
            0,
        );
        let rc = unsafe { DeleteIpForwardEntry2(&row) };
        if rc != NO_ERROR {
            return Err(CoreError::Network(format!(
                "DeleteIpForwardEntry2 失败: 0x{:x}",
                rc.0
            )));
        }
        Ok(())
    }

    /// 查找当前已存在的叠加路由：两条 /1 都在且接口 Up 时返回可用的句柄。
    /// 供 reconcile 判断"预期启用"是否仍然成立（比按网卡匹配默认路由更精确）。
    pub async fn existing_handle_for(&self, bypass_ip: Ipv4Addr) -> Result<Option<SwitchHandle>> {
        let rows = scan_overlay_rows(bypass_ip)
            .map_err(|e| CoreError::Network(format!("读取路由表失败: {e}")))?;
        let main = rows
            .iter()
            .find(|r| r.prefix_len == 1 && r.prefix.is_unspecified() && is_effective(r));
        let extra = rows.iter().find(|r| {
            r.prefix_len == 1 && r.prefix == Ipv4Addr::new(128, 0, 0, 0) && is_effective(r)
        });
        let (Some(m), Some(e)) = (main, extra) else {
            return Ok(None);
        };
        if !adapters::is_interface_up(m.if_index) || !adapters::is_interface_up(e.if_index) {
            return Ok(None);
        }
        Ok(Some(SwitchHandle {
            mode: SwitchMode::RouteOverlay,
            if_index: Some(m.if_index),
            if_luid: Some(m.if_luid),
            destination_prefix: Some(MAIN_PREFIX.to_string()),
            next_hop: Some(IpAddr::V4(bypass_ip)),
            adapter_id: None,
            extra_routes: vec![HandleRoute {
                destination_prefix: EXTRA_PREFIX.to_string(),
                if_index: e.if_index,
                if_luid: e.if_luid,
            }],
        }))
    }

    /// 清理经 bypass_ip 的叠加路由（/0 与 /1，netmgmt 协议）。
    /// 供 reconcile 在"预期直连"时清除脏路由（崩溃/禁用失败可能遗留），
    /// 也供 enable 失败时回收已添加的部分路由。
    ///
    /// 兼容历史脏数据：0.1.7 及之前版本因字节序 bug 写入的下一跳是反转的
    /// （如 105.123.168.192），这里对两种字节序解读都做匹配，并按路由表中
    /// 的原始字段原样删除。
    pub async fn cleanup_routes_via(&self, bypass_ip: Ipv4Addr) -> Result<usize> {
        let mut removed = 0;
        let rows = unsafe { scan_overlay_rows_raw(bypass_ip) };
        for row in rows {
            let rc = unsafe { DeleteIpForwardEntry2(&row) };
            if rc == NO_ERROR {
                removed += 1;
            } else {
                let p = ipv4_from_net_order(unsafe {
                    row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr
                });
                let len = row.DestinationPrefix.PrefixLength;
                warn!("清理残留路由 {p}/{len} 失败: 0x{:x}", rc.0);
            }
        }
        Ok(removed)
    }
}

/// 解析 "a.b.c.d/len" 形式的前缀。
fn parse_v4_prefix(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, len) = s.split_once('/')?;
    Some((addr.parse().ok()?, len.parse().ok()?))
}

fn sockaddr_inet(ip: Ipv4Addr) -> SOCKADDR_INET {
    let mut s: SOCKADDR_INET = Default::default();
    s.Ipv4 = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: 0,
        sin_addr: in_addr_from(ip),
        sin_zero: [0; 8],
    };
    s
}

fn build_row(
    if_luid: u64,
    if_index: u32,
    dest: Ipv4Addr,
    prefix_len: u8,
    next_hop: Ipv4Addr,
    metric: u32,
) -> MIB_IPFORWARD_ROW2 {
    let mut row: MIB_IPFORWARD_ROW2 = MIB_IPFORWARD_ROW2 {
        InterfaceLuid: NET_LUID_LH { Value: if_luid },
        InterfaceIndex: if_index,
        ..Default::default()
    };
    row.DestinationPrefix.Prefix = sockaddr_inet(dest);
    row.DestinationPrefix.PrefixLength = prefix_len;
    row.NextHop = sockaddr_inet(next_hop);
    row.Metric = metric;
    row.Protocol = NL_ROUTE_PROTOCOL(ROUTE_PROTOCOL_NETMGMT);
    // 生命周期置为无限，否则条目一创建即过期、不参与转发。
    row.ValidLifetime = INFINITE_LIFETIME;
    row.PreferredLifetime = INFINITE_LIFETIME;
    row
}

/// 一条扫描到的叠加相关路由。
#[derive(Debug, Clone, Copy)]
struct OverlayRow {
    if_index: u32,
    if_luid: u64,
    prefix: Ipv4Addr,
    prefix_len: u8,
    /// 条目剩余有效期（秒）。0 表示已过期：可被枚举但不参与转发。
    valid_lifetime: u32,
}

/// 该条目是否真正参与转发（生命周期过期的不算，见 INFINITE_LIFETIME 注释）。
fn is_effective(row: &OverlayRow) -> bool {
    row.valid_lifetime != 0
}

/// 扫描路由表：返回所有 next_hop == bypass_ip（含历史反转字节序）、
/// 前缀 /0 或 /1、协议 netmgmt 的路由。
fn scan_overlay_rows(bypass_ip: Ipv4Addr) -> windows::core::Result<Vec<OverlayRow>> {
    let mut out = Vec::new();
    unsafe {
        for row in scan_overlay_rows_raw(bypass_ip) {
            out.push(OverlayRow {
                if_index: row.InterfaceIndex,
                if_luid: row.InterfaceLuid.Value,
                prefix: ipv4_from_net_order(row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr),
                prefix_len: row.DestinationPrefix.PrefixLength,
                valid_lifetime: row.ValidLifetime,
            });
        }
    }
    Ok(out)
}

/// 扫描路由表（原始行）：返回所有下一跳按两种字节序解读均匹配 bypass_ip、
/// 前缀 /0 或 /1、协议 netmgmt 的 MIB 行。返回的行可直接用于
/// DeleteIpForwardEntry2（字段与表内条目一致，避免重建行不匹配）。
/// 表读取失败时返回空（与旧实现一致，由调用方决定是否告警）。
unsafe fn scan_overlay_rows_raw(bypass_ip: Ipv4Addr) -> Vec<MIB_IPFORWARD_ROW2> {
    let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    if GetIpForwardTable2(AF_INET, &mut table) != NO_ERROR {
        return vec![];
    }

    let mut out = Vec::new();
    let n = (*table).NumEntries;
    let rows = core::slice::from_raw_parts((*table).Table.as_ptr(), n as usize);
    for row in rows {
        let prefix_len = row.DestinationPrefix.PrefixLength;
        if prefix_len > 1 {
            continue;
        }
        let raw = row.NextHop.Ipv4.sin_addr.S_un.S_addr;
        // 正确读法 + 历史版本字节序反转读法，二者任一匹配即视为本工具的路由。
        let le = ipv4_from_net_order(raw);
        let swapped = ipv4_from_net_order(raw.swap_bytes());
        if le != bypass_ip && swapped != bypass_ip {
            continue;
        }
        if row.Protocol != NL_ROUTE_PROTOCOL(ROUTE_PROTOCOL_NETMGMT) {
            continue;
        }
        out.push(*row);
    }
    FreeMibTable(table as *const _);
    out
}

#[async_trait]
impl SwitchStrategy for RouteOverlayStrategy {
    async fn enable(&self, target: &BypassTarget) -> Result<SwitchHandle> {
        let bypass_ip = match target.bypass_ip {
            IpAddr::V4(v4) => v4,
            _ => return Err(CoreError::Network("仅支持 IPv4 旁路由地址".into())),
        };

        let (if_index, if_luid) = self.resolve_interface_for(bypass_ip)?;
        let extra_dest = Ipv4Addr::new(128, 0, 0, 0);

        // 先清掉残留（异常退出遗留 / 旧版本字节序脏数据），避免
        // CreateIpForwardEntry2 撞上 ERROR_OBJECT_ALREADY_EXISTS。
        self.cleanup_routes_via(bypass_ip).await?;

        if let Err(e) = self
            .add_route(if_index, if_luid, Ipv4Addr::UNSPECIFIED, 1, bypass_ip)
            .await
        {
            self.cleanup_routes_via(bypass_ip).await?;
            return Err(e);
        }
        if let Err(e) = self
            .add_route(if_index, if_luid, extra_dest, 1, bypass_ip)
            .await
        {
            // 部分成功不算成功：回收已添加的路由，不留黑洞脏数据。
            self.cleanup_routes_via(bypass_ip).await?;
            return Err(e);
        }

        info!(
            "route overlay enabled: {MAIN_PREFIX} + {EXTRA_PREFIX} via {bypass_ip} on if_index={if_index}"
        );

        Ok(SwitchHandle {
            mode: SwitchMode::RouteOverlay,
            if_index: Some(if_index),
            if_luid: Some(if_luid),
            destination_prefix: Some(MAIN_PREFIX.to_string()),
            next_hop: Some(IpAddr::V4(bypass_ip)),
            adapter_id: None,
            extra_routes: vec![HandleRoute {
                destination_prefix: EXTRA_PREFIX.to_string(),
                if_index,
                if_luid,
            }],
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

        let main_prefix = handle
            .destination_prefix
            .as_deref()
            .and_then(parse_v4_prefix)
            .unwrap_or((Ipv4Addr::UNSPECIFIED, 1));
        Self::delete_row(
            if_luid,
            handle.if_index,
            main_prefix.0,
            main_prefix.1,
            bypass_ip,
        )?;

        for extra in &handle.extra_routes {
            if let Some((dest, len)) = parse_v4_prefix(&extra.destination_prefix) {
                if let Err(e) =
                    Self::delete_row(extra.if_luid, Some(extra.if_index), dest, len, bypass_ip)
                {
                    warn!("删除附加路由 {} 失败: {e}", extra.destination_prefix);
                }
            }
        }

        info!("route overlay disabled: via {bypass_ip}");
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

        let rows = scan_overlay_rows(bypass_ip)
            .map_err(|e| CoreError::Network(format!("读取路由表失败: {e}")))?;
        let has = |prefix: Ipv4Addr| {
            rows.iter().any(|r| {
                r.prefix_len == 1
                    && r.prefix == prefix
                    && is_effective(r)
                    && handle.if_index.is_none_or(|idx| r.if_index == idx)
                    && adapters::is_interface_up(r.if_index)
            })
        };
        Ok(has(Ipv4Addr::UNSPECIFIED) && has(Ipv4Addr::new(128, 0, 0, 0)))
    }

    async fn reconcile_on_startup(&self) -> Result<ReconcileAction> {
        // 一致性修正由 core-bin 控制层结合 state_store 预期状态处理；
        // 策略层提供 existing_handle_for / cleanup_routes_via 能力。
        Ok(ReconcileAction::NoAction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_lib::switch_engine::ROUTE_METRIC;

    /// 生命周期必须是无限，否则路由一创建即过期、不参与转发（历史 bug）。
    #[test]
    fn build_row_uses_infinite_lifetime() {
        let row = build_row(
            1,
            1,
            Ipv4Addr::UNSPECIFIED,
            1,
            Ipv4Addr::new(192, 168, 1, 1),
            5,
        );
        assert_eq!(row.ValidLifetime, u32::MAX);
        assert_eq!(row.PreferredLifetime, u32::MAX);
    }

    /// 前缀与下一跳按 Windows 网络序写入，读回必须与写入地址一致（历史字节序 bug）。
    #[test]
    fn build_row_roundtrips_addresses() {
        let dest = Ipv4Addr::new(128, 0, 0, 0);
        let next_hop = Ipv4Addr::new(192, 168, 123, 105);
        let row = build_row(1, 1, dest, 1, next_hop, ROUTE_METRIC);

        let got_dest =
            ipv4_from_net_order(unsafe { row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr });
        let got_hop = ipv4_from_net_order(unsafe { row.NextHop.Ipv4.sin_addr.S_un.S_addr });
        assert_eq!(got_dest, dest);
        assert_eq!(got_hop, next_hop);
        assert_eq!(row.DestinationPrefix.PrefixLength, 1);
    }
}
