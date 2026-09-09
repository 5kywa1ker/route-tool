//! 网卡直改手动验证测试（需管理员权限 + 真实修改网卡）。
//!
//! 用途：绕过完整 UI/服务链路，直接调用 core-win 的 netsh 封装，
//! 把 WLAN 网卡设为静态 IP 192.168.123.133 / 掩码 255.255.255.0 /
//! 网关 192.168.123.105 / DNS 192.168.123.105，随后立即读回验证。
//!
//! 运行方式（**管理员** PowerShell 或终端）：
//! ```powershell
//! $env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
//! cargo test -p core-win --test adapter_reconfig_manual -- --ignored --nocapture
//! ```
//!
//! 注意：
//! - 默认被 `#[ignore]` 标记，`cargo test` 不会自动执行，避免误改网卡。
//! - 会真实修改 WLAN 网卡配置，验证后需手动恢复（或重跑带 `--restore-dhcp`
//!   的用例，见 `restore_wlan_to_dhcp`）。

use std::net::Ipv4Addr;

use core_win::{adapters, netsh};

const TARGET_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 123, 133);
const TARGET_MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const TARGET_GW: Ipv4Addr = Ipv4Addr::new(192, 168, 123, 105);
const TARGET_DNS: Ipv4Addr = Ipv4Addr::new(192, 168, 123, 105);

/// 定位 WLAN 网卡：优先按名字匹配 "WLAN"，其次按 GUID 匹配。
fn find_wlan() -> (String, String) {
    let list = adapters::list_adapters().expect("枚举网卡失败");
    for a in &list {
        println!(
            "[枚举] id={} name={:?} ipv4={:?} gw={:?} up={}",
            a.id, a.name, a.ipv4, a.gateway, a.is_connected
        );
    }
    let wlan = list
        .iter()
        .find(|a| {
            a.name.eq_ignore_ascii_case("WLAN")
                || a.name.to_ascii_lowercase().contains("wlan")
                || a.name.to_ascii_lowercase().contains("wi-fi")
                || a.id
                    .eq_ignore_ascii_case("{B0D14BA4-5150-4A43-8938-7DEAE4F52EC9}")
        })
        .expect("未找到 WLAN 网卡，请确认无线网卡已启用");
    (wlan.name.clone(), wlan.id.clone())
}

/// 主用例：设置 WLAN 为指定静态 IP + 网关 + DNS，并读回验证。
///
/// 期望：设置后 WLAN 网关 == 192.168.123.105、IP == 192.168.123.133、
/// DNS == 192.168.123.105。若 netsh 静默失败（退出码 0 但网关没变），
/// 这里会直接断言失败，把问题暴露出来。
#[tokio::test]
#[ignore = "需要管理员权限且会真实修改 WLAN 网卡，手动 --ignored 运行"]
async fn set_wlan_static_ip_and_verify() {
    let (name, _id) = find_wlan();
    println!("=== 目标网卡: {name} ===");

    // 设置前状态。
    let before = adapters::list_adapters().expect("枚举网卡失败");
    let b = before.iter().find(|a| a.name == name).unwrap();
    println!(
        "[设置前] ipv4={:?} gw={:?} dns={:?}",
        b.ipv4, b.gateway, b.dns
    );

    // 1) 设置静态 IP + 网关（核心代码同款调用）。
    netsh::set_static_ipv4(&name, TARGET_IP, TARGET_MASK, TARGET_GW)
        .await
        .expect("set_static_ipv4 失败");

    // 2) 设置静态 DNS。
    netsh::set_dns(&name, &[TARGET_DNS])
        .await
        .expect("set_dns 失败");

    // 3) 读回验证。
    let after = adapters::list_adapters().expect("枚举网卡失败");
    let a = after.iter().find(|a| a.name == name).unwrap();
    println!(
        "[设置后] ipv4={:?} gw={:?} dns={:?}",
        a.ipv4, a.gateway, a.dns
    );

    // 关键断言：网关必须已变成旁路由地址。
    assert!(
        a.gateway.contains(&std::net::IpAddr::V4(TARGET_GW)),
        "网关未生效！设置后 gateway={:?}，期望包含 {}。",
        a.gateway,
        TARGET_GW
    );
    // IP 必须已变成目标静态 IP。
    assert!(
        a.ipv4.contains(&std::net::IpAddr::V4(TARGET_IP)),
        "静态 IP 未生效！设置后 ipv4={:?}，期望包含 {}。",
        a.ipv4,
        TARGET_IP
    );

    println!(
        "\n=== 验证通过：{} 已设为 ip={} gw={} dns={} ===",
        name, TARGET_IP, TARGET_GW, TARGET_DNS
    );
}

/// 恢复用例：把 WLAN 恢复为 DHCP（用于验证后还原）。
#[tokio::test]
#[ignore = "需要管理员权限且会真实修改 WLAN 网卡，手动 --ignored 运行"]
async fn restore_wlan_to_dhcp() {
    let (name, _id) = find_wlan();
    println!("=== 恢复网卡 {name} 为 DHCP ===");
    netsh::enable_dhcp(&name).await.expect("enable_dhcp 失败");
    netsh::reset_dns(&name).await.expect("reset_dns 失败");
    let after = adapters::list_adapters().expect("枚举网卡失败");
    let a = after.iter().find(|a| a.name == name).unwrap();
    println!(
        "[恢复后] ipv4={:?} gw={:?} dns={:?}",
        a.ipv4, a.gateway, a.dns
    );
    println!("=== 已恢复 DHCP ===");
}
