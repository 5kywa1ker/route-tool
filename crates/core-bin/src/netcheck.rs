//! DHCP 状态查询（netsh interface ip show config）。

use tokio::process::Command;

/// 通过 netsh 查询指定网卡（显示名）的 IPv4 DHCP 是否启用。
pub async fn query_dhcp_enabled(iface: &str) -> Option<bool> {
    let out = Command::new("netsh")
        .args(["interface", "ip", "show", "address", iface])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // 中文系统输出形如 "DHCP 启用:               是"，英文 "DHCP enabled: yes"。
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("dhcp") {
            let is_yes = lower.contains("yes")
                || line.contains('是')
                || lower.contains("enabled")
                || line.contains("启用");
            // 行里同时含 enabled/是 才算 true；netsh 输出 "DHCP enabled: No"。
            let no_flag = lower.contains("no") || line.contains('否');
            return Some(is_yes && !no_flag);
        }
    }
    None
}
