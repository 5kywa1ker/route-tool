//! ICMP ping（基于 iphlpapi IcmpSendEcho）。

use std::net::Ipv4Addr;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY32, IP_OPTION_INFORMATION,
};
use windows::Win32::Networking::WinSock::IN_ADDR;

/// 单次 ICMP ping，返回是否可达。`timeout_ms` 为等待毫秒。
pub fn ping_ipv4(addr: Ipv4Addr, timeout_ms: u32) -> bool {
    let handle: HANDLE = match unsafe { IcmpCreateFile() } {
        Ok(h) => h,
        Err(_) => return false,
    };
    if handle.is_invalid() {
        return false;
    }

    // IcmpSendEcho 的目标地址是网络序 u32。
    let dest = u32::from_be_bytes(addr.octets());

    // 少量载荷，避免个别实现拒绝空请求。
    let data: [u8; 1] = [0xFE];

    let reply_size = (std::mem::size_of::<ICMP_ECHO_REPLY32>() + data.len() + 64) as u32;
    let mut reply = vec![0u8; reply_size as usize];

    let opt: IP_OPTION_INFORMATION = Default::default();

    let n_replies = unsafe {
        IcmpSendEcho(
            handle,
            dest,
            data.as_ptr() as *const core::ffi::c_void,
            data.len() as u16,
            Some(&opt),
            reply.as_mut_ptr() as *mut core::ffi::c_void,
            reply_size,
            timeout_ms,
        )
    };

    unsafe {
        let _ = IcmpCloseHandle(handle);
    }

    n_replies > 0
}

/// 把 IPv4 转为 Windows 的 in_addr（S_un.S_addr 按网络序 u32 存放）。
pub fn in_addr_from(ip: Ipv4Addr) -> IN_ADDR {
    let mut a = IN_ADDR::default();
    a.S_un.S_addr = u32::from_be_bytes(ip.octets());
    a
}
