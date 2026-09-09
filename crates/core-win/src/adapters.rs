//! 网卡列表与路由状态查询（GetAdaptersAddresses / GetIpForwardTable2）。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetAdaptersAddresses, GetIfEntry2, GetIpForwardTable2, IP_ADAPTER_ADDRESSES_LH,
    MIB_IF_ROW2, MIB_IPFORWARD_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::{IfOperStatusUp, NET_LUID_LH};
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR, SOCKADDR_IN};

use ipc_protocol::{AdapterInfo, RouteState};

use crate::icmp::ipv4_from_net_order;

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;

/// 按接口 LUID 收集 IPv4 默认网关（取自路由表）。
///
/// `IP_ADAPTER_ADDRESSES_LH::FirstGatewayAddress` 在现行 Windows 上常常根本不
/// 填充（实测 DHCP 与静态配置的网卡都拿到空链表），直接依赖它会得到空的网关
/// 列表——进而让"网卡直改"策略的一切判定（是否生效、快照恢复、启动一致性）
/// 全部失灵。可靠来源只有路由表：取 `0.0.0.0/0` 且下一跳非 0.0.0.0 的条目。
pub fn default_gateways_by_luid() -> HashMap<u64, Vec<Ipv4Addr>> {
    let mut out: HashMap<u64, Vec<Ipv4Addr>> = HashMap::new();
    let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    unsafe {
        if GetIpForwardTable2(AF_INET, &mut table) != NO_ERROR {
            return out;
        }
    }

    let n = unsafe { (*table).NumEntries };
    let rows = unsafe { core::slice::from_raw_parts((*table).Table.as_ptr(), n as usize) };
    for row in rows {
        if row.DestinationPrefix.PrefixLength != 0 {
            continue;
        }
        let prefix_raw = unsafe { row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr };
        if prefix_raw != 0 {
            continue;
        }
        let hop = ipv4_from_net_order(unsafe { row.NextHop.Ipv4.sin_addr.S_un.S_addr });
        if hop.is_unspecified() {
            continue;
        }
        let list = out.entry(unsafe { row.InterfaceLuid.Value }).or_default();
        if !list.contains(&hop) {
            list.push(hop);
        }
    }
    unsafe { FreeMibTable(table as *const _) };
    out
}

/// 枚举所有 IPv4 网卡（含连接状态、IP、网关、DNS）。
pub fn list_adapters() -> windows::core::Result<Vec<AdapterInfo>> {
    // 先用 16KB，不够再扩大。
    let mut size: u32 = 16 * 1024;
    let mut buffer;
    loop {
        buffer = vec![0u8; size as usize];
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_INET.0 as u32,
                windows::Win32::NetworkManagement::IpHelper::GET_ADAPTERS_ADDRESSES_FLAGS(0),
                None,
                Some(buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            size *= 2;
            continue;
        }
        if rc != NO_ERROR.0 {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                rc as i32,
            )));
        }
        break;
    }

    let mut out = Vec::new();
    // 网关以路由表为准（FirstGatewayAddress 不可靠，见函数注释）。
    let gw_map = default_gateways_by_luid();
    let mut p = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !p.is_null() {
        let aa = unsafe { &*p };

        if aa.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
            // AdapterName 是 ANSI GUID 字符串。
            let id = {
                let name_ptr = aa.AdapterName.0 as *const u8;
                let len = (0..)
                    .take_while(|&i| unsafe { *name_ptr.add(i) } != 0)
                    .count();
                let bytes = unsafe { core::slice::from_raw_parts(name_ptr, len) };
                String::from_utf8_lossy(bytes).to_string()
            };

            let name = pwstr_to_string(aa.FriendlyName);

            let mac = {
                let len = aa.PhysicalAddressLength as usize;
                if len > 0 {
                    aa.PhysicalAddress[..len]
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join("-")
                } else {
                    String::new()
                }
            };

            let mut ipv4 = Vec::new();
            let mut ipv4_prefixes = Vec::new();
            let mut dns = Vec::new();

            let mut ua = aa.FirstUnicastAddress;
            while !ua.is_null() {
                let u = unsafe { &*ua };
                if let Some(ip) = sockaddr_to_ipv4(u.Address.lpSockaddr) {
                    ipv4.push(ip);
                    // 系统报告的在线前缀长度（快照备份/恢复用）。
                    ipv4_prefixes.push(u.OnLinkPrefixLength);
                }
                ua = u.Next;
            }

            let mut sa = aa.FirstDnsServerAddress;
            while !sa.is_null() {
                let s = unsafe { &*sa };
                if let Some(ip) = sockaddr_to_ipv4(s.Address.lpSockaddr) {
                    dns.push(ip);
                }
                sa = s.Next;
            }

            // 网关：路由表条目优先，FirstGatewayAddress 仅作补充（去重）。
            let mut gateway = gw_map
                .get(&unsafe { aa.Luid.Value })
                .cloned()
                .unwrap_or_default();
            let mut ga = aa.FirstGatewayAddress;
            while !ga.is_null() {
                let g = unsafe { &*ga };
                if let Some(ip) = sockaddr_to_ipv4(g.Address.lpSockaddr) {
                    if !gateway.contains(&ip) {
                        gateway.push(ip);
                    }
                }
                ga = g.Next;
            }

            out.push(AdapterInfo {
                id,
                name,
                kind: format!("if_type={}", aa.IfType),
                mac: if mac.is_empty() { None } else { Some(mac) },
                ipv4: ipv4.into_iter().map(IpAddr::V4).collect(),
                ipv4_prefixes,
                gateway: gateway.into_iter().map(IpAddr::V4).collect(),
                dns: dns.into_iter().map(IpAddr::V4).collect(),
                is_connected: aa.OperStatus == IfOperStatusUp,
            });
        }

        p = aa.Next;
    }
    Ok(out)
}

/// 查询当前 IPv4 默认路由状态：下一跳列表，以及是否经由指定网卡（按 GUID 字符串匹配）。
pub fn current_route_state(adapter_id: &str) -> windows::core::Result<RouteState> {
    let mut table: *mut windows::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_TABLE2 =
        std::ptr::null_mut();
    unsafe {
        let rc = GetIpForwardTable2(AF_INET, &mut table);
        if rc != NO_ERROR {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                rc.0 as i32,
            )));
        }
    }

    let mut default_hops: Vec<IpAddr> = Vec::new();
    let mut via_target = false;

    let n = unsafe { (*table).NumEntries };
    let rows = unsafe { core::slice::from_raw_parts((*table).Table.as_ptr(), n as usize) };
    for row in rows {
        if row.DestinationPrefix.PrefixLength != 0 {
            continue;
        }
        // 前缀必须是 0.0.0.0。
        let prefix_raw = unsafe { row.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr };
        if prefix_raw != 0 {
            continue;
        }

        let raw = unsafe { row.NextHop.Ipv4.sin_addr.S_un.S_addr };
        let octets = raw.to_le_bytes();
        if octets == [0, 0, 0, 0] {
            continue; // on-link
        }
        default_hops.push(IpAddr::V4(Ipv4Addr::from(octets)));

        // 该行所属网卡 GUID 是否等于 adapter_id。
        let luid = NET_LUID_LH {
            Value: unsafe { row.InterfaceLuid.Value },
        };
        if let Ok(guid) = luid_to_guid(&luid) {
            if format!("{guid:?}")
                .trim_start_matches('{')
                .trim_end_matches('}')
                .eq_ignore_ascii_case(adapter_id)
            {
                via_target = true;
            }
        }
    }
    unsafe { FreeMibTable(table as *const _) };

    Ok(RouteState {
        default_via_bypass: via_target,
        default_next_hops: default_hops,
    })
}

/// 查询指定接口索引是否 Up。
pub fn is_interface_up(if_index: u32) -> bool {
    let mut row: MIB_IF_ROW2 = MIB_IF_ROW2 {
        InterfaceIndex: if_index,
        ..Default::default()
    };
    unsafe {
        if GetIfEntry2(&mut row) != NO_ERROR {
            return false;
        }
    }
    row.OperStatus == IfOperStatusUp
}

fn sockaddr_to_ipv4(sa: *const SOCKADDR) -> Option<Ipv4Addr> {
    if sa.is_null() {
        return None;
    }
    if unsafe { (*sa).sa_family } != AF_INET {
        return None;
    }
    let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
    let raw = unsafe { sin.sin_addr.S_un.S_addr };
    // S_addr 按网络字节序存放，小端机器上用 to_le_bytes 还原八位组。
    Some(ipv4_from_net_order(raw))
}

fn pwstr_to_string(p: windows::core::PWSTR) -> String {
    if p.0.is_null() {
        return String::new();
    }
    let len = (0..).take_while(|&i| unsafe { *p.0.add(i) } != 0).count();
    let wide = unsafe { core::slice::from_raw_parts(p.0, len) };
    String::from_utf16_lossy(wide)
}

fn luid_to_guid(luid: &NET_LUID_LH) -> windows::core::Result<windows::core::GUID> {
    use windows::Win32::NetworkManagement::IpHelper::ConvertInterfaceLuidToGuid;
    let mut guid = windows::core::GUID::zeroed();
    unsafe {
        let rc = ConvertInterfaceLuidToGuid(luid, &mut guid);
        if rc != NO_ERROR {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                rc.0 as i32,
            )));
        }
    }
    Ok(guid)
}
