//! ICMP ping（基于 iphlpapi IcmpSendEcho）与网络序地址转换。

use std::net::Ipv4Addr;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY,
};
use windows::Win32::Networking::WinSock::IN_ADDR;

/// 把 IPv4 转为 Windows 网络序字段的 u32 表示（S_un.S_addr / IPAddr 通用）。
///
/// 这些字段在内存中按网络字节序存放八位组；在小端机器上等价于按原生序
/// 直接写入八位组，即 `htonl(点分十进制对应的主机序 u32)` 的结果。
///
/// 注意：**不能**用 `u32::from_be_bytes(octets)` —— 那在小端机会把八位组
/// 反向写进内存（路由表里出现 105.123.168.192 这类反转网关、子网前缀被
/// 内核以 ERROR_INVALID_PARAMETER 拒绝，均为该错误所致）。
pub fn ip_to_net_order(ip: Ipv4Addr) -> u32 {
    u32::from_le_bytes(ip.octets())
}

/// 逆变换：从原始 S_addr u32 还原 IPv4（`ip_to_net_order` 的反函数）。
pub fn ipv4_from_net_order(raw: u32) -> Ipv4Addr {
    Ipv4Addr::from(raw.to_le_bytes())
}

/// 把 IPv4 转为 Windows 的 IN_ADDR（S_un.S_addr 按网络序存放）。
pub fn in_addr_from(ip: Ipv4Addr) -> IN_ADDR {
    let mut a = IN_ADDR::default();
    a.S_un.S_addr = ip_to_net_order(ip);
    a
}

/// 单次 ICMP ping，返回是否可达（收到 Reply 且 Status == IP_SUCCESS）。
/// `timeout_ms` 为等待毫秒。
pub fn ping_ipv4(addr: Ipv4Addr, timeout_ms: u32) -> bool {
    let handle: HANDLE = match unsafe { IcmpCreateFile() } {
        Ok(h) => h,
        Err(_) => return false,
    };
    if handle.is_invalid() {
        return false;
    }

    // IcmpSendEcho 的目标地址是网络序 u32。
    let dest = ip_to_net_order(addr);

    // 少量载荷，避免个别实现拒绝空请求。
    let data: [u8; 1] = [0xFE];

    // 64 位进程使用 ICMP_ECHO_REPLY（32 变体仅用于 WOW64）；缓冲区留足余量。
    let reply_size = (std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 128) as u32;
    let mut reply = vec![0u8; reply_size as usize];

    let n_replies = unsafe {
        IcmpSendEcho(
            handle,
            dest,
            data.as_ptr() as *const core::ffi::c_void,
            data.len() as u16,
            None,
            reply.as_mut_ptr() as *mut core::ffi::c_void,
            reply_size,
            timeout_ms,
        )
    };

    unsafe {
        let _ = IcmpCloseHandle(handle);
    }

    // 关键：返回值 >0 不代表成功（缓冲区不足、超时等也会返回 1），
    // 必须检查 reply.Status == IP_SUCCESS(0)，否则会得到假阳性。
    if n_replies == 0 {
        return false;
    }
    let r = unsafe { &*(reply.as_ptr() as *const ICMP_ECHO_REPLY) };
    r.Status == 0
}
