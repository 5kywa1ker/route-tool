//! netsh 调用封装：网卡静态 IP / DHCP / DNS 配置。
//!
//! 说明：需求文档建议用 IP Helper（CreateUnicastIpAddressEntry /
//! SetInterfaceDnsSettings）。但 DHCP 开关与静态网关设置在纯 IP Helper 上
//! 需要操作 WMI/注册表，MVP 选择 netsh（Windows 自带、稳定、SYSTEM 可用），
//! 后续如需纯 API 实现可替换此模块。

use std::net::Ipv4Addr;

use tokio::process::Command;

use core_lib::CoreError;

/// 运行 netsh 命令并检查成功。
async fn run_netsh(args: &[&str]) -> Result<(), CoreError> {
    let out = Command::new("netsh")
        .args(args)
        .output()
        .await
        .map_err(|e| CoreError::Network(format!("无法启动 netsh: {e}")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(CoreError::Network(format!(
            "netsh 失败: {}{}",
            stderr.trim(),
            stdout.trim()
        )));
    }
    Ok(())
}

/// 将网卡名转 netsh 引用名（含空格需整体引用，netsh 接口名 = FriendlyName）。
pub struct IfaceRef {
    pub name: String,
}

impl IfaceRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// 启用 DHCP（IPv4）。
pub async fn enable_dhcp(iface: &str) -> Result<(), CoreError> {
    run_netsh(&["interface", "ip", "set", "address", iface, "dhcp"]).await
}

/// 设置静态 IPv4 + 子网掩码 + 网关。
pub async fn set_static_ipv4(
    iface: &str,
    ip: Ipv4Addr,
    mask: Ipv4Addr,
    gateway: Ipv4Addr,
) -> Result<(), CoreError> {
    run_netsh(&[
        "interface",
        "ip",
        "set",
        "address",
        iface,
        "static",
        &ip.to_string(),
        &mask.to_string(),
        &gateway.to_string(),
    ])
    .await
}

/// 设置静态 DNS 列表（第一条为主，其余 add）。
pub async fn set_dns(iface: &str, dns: &[Ipv4Addr]) -> Result<(), CoreError> {
    if dns.is_empty() {
        return Ok(());
    }
    run_netsh(&[
        "interface",
        "ip",
        "set",
        "dns",
        iface,
        "static",
        &dns[0].to_string(),
    ])
    .await?;
    for d in &dns[1..] {
        run_netsh(&[
            "interface",
            "ip",
            "add",
            "dns",
            iface,
            &d.to_string(),
            "index=2",
        ])
        .await?;
    }
    Ok(())
}

/// 清空静态 DNS（恢复 DHCP DNS）。
pub async fn reset_dns(iface: &str) -> Result<(), CoreError> {
    run_netsh(&["interface", "ip", "set", "dns", iface, "dhcp"]).await
}
