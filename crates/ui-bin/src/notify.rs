//! Toast 通知（Windows.UI.Notifications，通过 PowerShell 兜底）。

use tracing::warn;

/// 发送 Toast 通知。MVP 实现：powershell 调用 Windows Runtime Toast API。
/// 崩溃或失败不影响主流程（仅记日志）。
pub fn show_toast(title: &str, body: &str) {
    let script = format!(
        r#"
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
$xml = @"
<toast><visual><binding template="ToastText02"><text id="1">{title}</text><text id="2">{body}</text></binding></visual></toast>
"@
$doc = New-Object Windows.Data.Xml.Dom.XmlDocument
$doc.LoadXml($xml)
$toast = New-Object Windows.UI.Notifications.ToastNotification($doc)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier("Bypass Tool").Show($toast)
"#,
        title = xml_escape(title),
        body = xml_escape(body),
    );

    std::thread::spawn(move || {
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output();
        if let Err(e) = out {
            warn!("发送 Toast 失败: {e}");
        }
    });
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// 直接使用 std 的 CommandExt（Windows）。
#[allow(unused_imports)]
use std::os::windows::process::CommandExt as _;
