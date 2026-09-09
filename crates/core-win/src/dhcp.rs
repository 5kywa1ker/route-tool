//! DHCP 状态查询（注册表 EnableDHCP）。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use tracing::warn;
use windows::core::PCWSTR;
use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

/// 通过注册表查询指定网卡的 IPv4 DHCP 是否启用。
///
/// `adapter_id` 是 GUID 形式（如 `{B0D14BA4-...}`），对应注册表子项名。
///
/// 历史实现用 `netsh interface ip show address` 解析输出判定，但 netsh 在
/// 中文 Windows 上输出 GBK 编码，`String::from_utf8_lossy` 后全部变成
/// U+FFFD，"是/否/启用" 永远匹配不上，导致 DHCP 恒被误判为静态——快照
/// 随之失真，恢复时走静态分支又拿不到网关，网卡再也改不回来。注册表
/// `EnableDHCP` 是 netsh 写入的同一数据源，直接读它没有编码问题。
pub fn query_dhcp_enabled(adapter_id: &str) -> Option<bool> {
    let subkey = wide(format!(
        r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{adapter_id}"
    ));
    let value = wide("EnableDHCP");
    let subkey = PCWSTR(subkey.as_ptr());
    let value = PCWSTR(value.as_ptr());

    // 常规情况：REG_DWORD。
    let mut dword: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut dword as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if rc == NO_ERROR {
        return Some(dword != 0);
    }

    // 兜底：个别驱动/工具会写成 REG_SZ "0"/"1"。
    let mut buf = [0u16; 16];
    let mut sz = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut sz),
        )
    };
    if rc == NO_ERROR {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let text = String::from_utf16_lossy(&buf[..len]);
        return Some(text.trim() == "1");
    }

    warn!("读取 EnableDHCP 失败（adapter {adapter_id}）: 0x{:x}", rc.0);
    None
}

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}
