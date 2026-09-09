//! netsh 调用封装：网卡静态 IP / DHCP / DNS 配置。
//!
//! 说明：需求文档建议用 IP Helper（CreateUnicastIpAddressEntry /
//! SetInterfaceDnsSettings）。但 DHCP 开关与静态网关设置在纯 IP Helper 上
//! 需要操作 WMI/注册表，MVP 选择 netsh（Windows 自带、稳定、SYSTEM 可用），
//! 后续如需纯 API 实现可替换此模块。

use std::net::Ipv4Addr;

use tokio::process::Command;
use tracing::{error, info};

use core_lib::CoreError;

use crate::dhcp;

/// netsh 在目标状态已达成时返回 exit 1 并输出的提示语（中英文）。
///
/// 典型场景：网卡本来就是 DHCP，`set address ... dhcp` 仍会得到
/// "已在此接口上启用 DHCP。" —— 这不是失败，重放它是无害的幂等操作。
/// 早期版本把它当硬错误，导致 disable 直接失败、快照不清理、网卡可能
/// 永远留在旁路由网关上。
const IDEMPOTENT_MARKERS: [&str; 4] = [
    "已在此接口上启用 DHCP",
    "DHCP is already enabled",
    "已在此接口上启用",
    "The object is already in the state",
];

/// 运行 netsh 并返回原始输出（供调用方区分成功/幂等/失败）。
async fn run_netsh_raw(args: &[&str]) -> Result<(bool, String), CoreError> {
    let out = Command::new("netsh")
        .args(args)
        .output()
        .await
        .map_err(|e| CoreError::Network(format!("无法启动 netsh: {e}")))?;

    // netsh 输出在中文系统上可能是 GBK，lossy 转换后可读性下降，但退出码与
    // 原始文本仍是排查的第一手证据，必须保留（此前失败毫无痕迹）。
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    )
    .trim()
    .to_string();

    Ok((out.status.success(), text))
}

/// 运行 netsh 命令并检查成功。
async fn run_netsh(args: &[&str]) -> Result<(), CoreError> {
    match run_netsh_raw(args).await? {
        (true, _) => Ok(()),
        (false, text) => {
            error!("netsh {:?} 失败 (exit 1): {}", args, text);
            Err(CoreError::Network(format!("netsh 失败: {text}")))
        }
    }
}

/// 运行 netsh 命令；若目标状态已达成（提示语见 [`IDEMPOTENT_MARKERS`]）
/// 则按成功处理。
async fn run_netsh_idempotent(args: &[&str]) -> Result<(), CoreError> {
    match run_netsh_raw(args).await? {
        (true, _) => Ok(()),
        (false, text) => {
            if IDEMPOTENT_MARKERS.iter().any(|m| text.contains(m)) {
                info!("netsh {:?} 目标状态已达成，按成功处理: {}", args, text);
                return Ok(());
            }
            error!("netsh {:?} 失败 (exit 1): {}", args, text);
            Err(CoreError::Network(format!("netsh 失败: {text}")))
        }
    }
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
///
/// 幂等：接口已经是 DHCP 时 netsh 会返回非 0，这里按成功处理。
pub async fn enable_dhcp(iface: &str) -> Result<(), CoreError> {
    run_netsh_idempotent(&["interface", "ip", "set", "address", iface, "dhcp"]).await
}

/// 确保接口处于 DHCP：已是 DHCP 则直接跳过（注册表 EnableDHCP 为准，
/// 比解析 netsh 输出可靠——netsh 在中文系统上是 GBK，文本匹配可能失效）。
///
/// `adapter_guid` 形如 `{B0D14BA4-...}`，用于查注册表。
pub async fn ensure_dhcp(iface: &str, adapter_guid: &str) -> Result<(), CoreError> {
    match dhcp::query_dhcp_enabled(adapter_guid) {
        Some(true) => {
            info!("网卡 {iface} 已是 DHCP，跳过 netsh 切换");
            Ok(())
        }
        _ => enable_dhcp(iface).await,
    }
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
    run_netsh_idempotent(&["interface", "ip", "set", "dns", iface, "dhcp"]).await
}
