//! JSON-RPC 2.0 风格的行分隔协议类型定义。
//!
//! 该 crate 被 core（服务端）与 ui（客户端）共享，只包含序列化友好的
//! 纯数据模型，不依赖任何 Windows 专属概念。

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 协议版本，用于能力协商。
pub const PROTOCOL_VERSION: u32 = 1;
/// Named Pipe 名称。
pub const PIPE_NAME: &str = r"\\.\pipe\RouteToolCore";

/// 切换模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchMode {
    /// 路由叠加：添加 0.0.0.0/0 指向旁路由。
    RouteOverlay,
    /// 网卡直改：修改网卡静态 IP / 网关 / DNS。
    AdapterReconfig,
}

/// 旁路由目标（平台无关）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BypassTarget {
    pub adapter_id: String,
    pub bypass_ip: IpAddr,
    pub dns: Option<Vec<IpAddr>>,
    /// 网卡直改模式使用的子网掩码；None 时策略层按 255.255.255.0 处理。
    #[serde(default)]
    pub subnet_mask: Option<std::net::Ipv4Addr>,
    /// 网卡直改模式要设置的静态 IP；None 时策略层保留网卡当前 IP。
    ///
    /// 当网卡原本是 DHCP 时，"保留当前 IP"拿到的是 DHCP 租约地址，租约续期/
    /// 网卡重连后地址会漂移，甚至被系统回滚回 DHCP，导致直改"看起来没生效"。
    /// 因此直改模式应允许用户显式指定要写死的静态 IP。
    #[serde(default)]
    pub static_ip: Option<std::net::Ipv4Addr>,
}

/// 网卡信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub id: String,
    /// 显示名（本地化）。
    pub name: String,
    /// 接口类型描述（ether/wireless/...）。
    pub kind: String,
    pub mac: Option<String>,
    pub ipv4: Vec<IpAddr>,
    /// 与 ipv4 一一对应的前缀长度（快照备份/恢复用）。
    #[serde(default)]
    pub ipv4_prefixes: Vec<u8>,
    pub gateway: Vec<IpAddr>,
    pub dns: Vec<IpAddr>,
    pub is_connected: bool,
}

/// 切换后由策略返回的句柄，记录用于精确 disable 的条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchHandle {
    pub mode: SwitchMode,
    /// RouteOverlay 下记录接口索引 + 目标前缀。
    pub if_index: Option<u32>,
    pub if_luid: Option<u64>,
    pub destination_prefix: Option<String>,
    pub next_hop: Option<IpAddr>,
    /// AdapterReconfig 下仅作标记。
    pub adapter_id: Option<String>,
    /// RouteOverlay 的附加路由条目（如 128.0.0.0/1），disable 时一并删除。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_routes: Vec<HandleRoute>,
}

/// 句柄中的附加路由条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandleRoute {
    /// 目标前缀，形如 "128.0.0.0/1"。
    pub destination_prefix: String,
    pub if_index: u32,
    pub if_luid: u64,
}

/// 网络状态（路由层）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteState {
    pub default_via_bypass: bool,
    pub default_next_hops: Vec<IpAddr>,
}

/// 网卡快照（AdapterReconfig 备份/恢复用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterSnapshot {
    pub adapter_id: String,
    pub is_dhcp_enabled: bool,
    pub static_ipv4: Vec<IpAddr>,
    pub static_ipv4_mask: Vec<u8>,
    pub gateway: Vec<IpAddr>,
    pub dns: Vec<IpAddr>,
}

/// 运行状态（与 core-lib 的 RuntimeState 对应，供 UI 直接使用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub is_enabled: bool,
    pub current_mode: Option<SwitchMode>,
    pub health: HealthStatus,
    pub last_updated: DateTime<Utc>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            is_enabled: false,
            current_mode: None,
            health: HealthStatus::Idle,
            last_updated: Utc::now(),
        }
    }
}

/// 健康状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// 未启用（Idle）。
    Idle,
    /// 健康检测正常。
    Healthy,
    /// 健康检测失败计数达到阈值，已自动回退。
    Degraded { consecutive_failures: u32 },
    /// 已触发自动回退，当前处于直连。
    Fallback,
    /// 检测恢复。
    Recovered,
}
/// 应用配置（与 core-lib 的 AppConfig 对应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub adapter_id: String,
    pub bypass_ip: IpAddr,
    pub switch_mode: SwitchMode,
    pub dns_override: Option<Vec<IpAddr>>,
    /// 网卡直改模式的子网掩码；None = 255.255.255.0（路由叠加模式忽略）。
    #[serde(default)]
    pub subnet_mask: Option<std::net::Ipv4Addr>,
    /// 网卡直改模式要设置的静态 IP；None = 保留网卡当前 IP（路由叠加模式忽略）。
    #[serde(default)]
    pub static_ip: Option<std::net::Ipv4Addr>,
    pub health_check_interval_secs: u32,
    pub failure_threshold: u32,
    pub auto_reenable_after_recovery: bool,
    pub notifications_enabled: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            adapter_id: String::new(),
            // 占位，首次配置向导会覆盖。
            bypass_ip: IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            switch_mode: SwitchMode::RouteOverlay,
            dns_override: None,
            subnet_mask: None,
            static_ip: None,
            health_check_interval_secs: 5,
            failure_threshold: 3,
            auto_reenable_after_recovery: false,
            notifications_enabled: true,
        }
    }
}

/// 校验子网掩码合法性（必须为连续 1 前缀，如 255.255.255.0）。
pub fn is_valid_subnet_mask(mask: std::net::Ipv4Addr) -> bool {
    let bits = u32::from(mask);
    let leading = bits.leading_ones();
    bits == if leading == 0 {
        0
    } else {
        u32::MAX << (32 - leading)
    }
}

/// 健康事件（IPC 推送）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthEvent {
    pub event_type: HealthEventType,
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthEventType {
    ProbeFailed { consecutive_count: u32 },
    ProbeRecovered,
    AutoFallbackTriggered { reason: String },
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 消息模型
// ---------------------------------------------------------------------------

/// RPC 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// RPC 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(flatten)]
    pub result: RpcOutcome,
    /// 服务端能力：协议版本（能力协商，§4 预留字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    /// 服务端能力：支持的切换模式（能力协商，§4 预留字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_modes: Option<Vec<SwitchMode>>,
}

impl Default for RpcResponse {
    fn default() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: 0,
            result: RpcOutcome::Ok {
                result: serde_json::Value::Null,
            },
            protocol_version: Some(PROTOCOL_VERSION),
            supported_modes: Some(vec![SwitchMode::RouteOverlay, SwitchMode::AdapterReconfig]),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcOutcome {
    Ok {
        #[serde(rename = "result")]
        result: serde_json::Value,
    },
    Err {
        error: RpcError,
    },
}

/// RPC 错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// 服务端主动推送的事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
}

/// 一次 IPC 线上的单条消息（行分隔，服务端与客户端均使用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireMessage {
    Request(RpcRequest),
    Response(RpcResponse),
    Notification(RpcNotification),
}

// Common JSON-RPC error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
/// 业务错误（切换失败、配置不合法等）。
pub const APP_ERROR: i32 = 1;

/// 支持的 RPC 方法名。
pub mod method {
    pub const GET_STATUS: &str = "GetStatus";
    pub const GET_CONFIG: &str = "GetConfig";
    pub const UPDATE_CONFIG: &str = "UpdateConfig";
    pub const ENABLE_BYPASS: &str = "EnableBypass";
    pub const DISABLE_BYPASS: &str = "DisableBypass";
    pub const TEST_CONNECTIVITY: &str = "TestConnectivity";
    pub const LIST_ADAPTERS: &str = "ListAdapters";
    pub const SUBSCRIBE_EVENTS: &str = "SubscribeEvents";
    pub const EVENT_PUSH: &str = "EventPush";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SwitchMode::RouteOverlay).unwrap(),
            r#""routeoverlay""#
        );
        assert_eq!(
            serde_json::to_string(&SwitchMode::AdapterReconfig).unwrap(),
            r#""adapterreconfig""#
        );
        let m: SwitchMode = serde_json::from_str(r#""routeoverlay""#).unwrap();
        assert_eq!(m, SwitchMode::RouteOverlay);
    }

    #[test]
    fn app_config_round_trip() {
        let cfg = AppConfig::default();
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.health_check_interval_secs, 5);
        assert_eq!(back.failure_threshold, 3);
        assert!(!back.auto_reenable_after_recovery);
        assert!(back.notifications_enabled);
    }

    #[test]
    fn runtime_state_round_trip() {
        let rs = RuntimeState {
            is_enabled: true,
            current_mode: Some(SwitchMode::RouteOverlay),
            health: HealthStatus::Degraded {
                consecutive_failures: 2,
            },
            ..RuntimeState::default()
        };
        let text = serde_json::to_string(&rs).unwrap();
        let back: RuntimeState = serde_json::from_str(&text).unwrap();
        assert!(back.is_enabled);
        assert_eq!(back.current_mode, Some(SwitchMode::RouteOverlay));
        assert_eq!(
            back.health,
            HealthStatus::Degraded {
                consecutive_failures: 2
            }
        );
    }

    #[test]
    fn health_event_round_trip() {
        let evt = HealthEvent {
            event_type: HealthEventType::AutoFallbackTriggered {
                reason: "连续探测失败 3/3 次".into(),
            },
            reason: None,
            at: chrono::Utc::now(),
        };
        let text = serde_json::to_string(&evt).unwrap();
        let back: HealthEvent = serde_json::from_str(&text).unwrap();
        match back.event_type {
            HealthEventType::AutoFallbackTriggered { reason } => {
                assert!(reason.contains("3/3"));
            }
            other => panic!("unexpected event type: {other:?}"),
        }
    }

    #[test]
    fn rpc_response_carries_capabilities() {
        let resp = RpcResponse::default();
        let text = serde_json::to_string(&resp).unwrap();
        assert!(text.contains("protocol_version"));
        assert!(text.contains("supported_modes"));

        let back: RpcResponse = serde_json::from_str(&text).unwrap();
        assert_eq!(back.protocol_version, Some(PROTOCOL_VERSION));
        assert_eq!(
            back.supported_modes.as_deref(),
            Some(&[SwitchMode::RouteOverlay, SwitchMode::AdapterReconfig][..])
        );

        // 旧消息（无能力字段）仍可解析，字段落为 None。
        let legacy: RpcResponse =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":null}"#).unwrap();
        assert_eq!(legacy.id, 1);
        assert!(legacy.protocol_version.is_none());
        assert!(legacy.supported_modes.is_none());
    }

    #[test]
    fn rpc_response_ok_outcome_round_trip() {
        let resp = RpcResponse {
            jsonrpc: "2.0".into(),
            id: 7,
            result: RpcOutcome::Ok {
                result: serde_json::json!({ "is_enabled": false }),
            },
            ..Default::default()
        };
        let text = serde_json::to_string(&resp).unwrap();
        assert!(text.contains(r#""result""#));
        let back: RpcResponse = serde_json::from_str(&text).unwrap();
        assert_eq!(back.id, 7);
        match back.result {
            RpcOutcome::Ok { result } => assert_eq!(result["is_enabled"], false),
            RpcOutcome::Err { .. } => panic!("expected Ok outcome"),
        }
    }

    #[test]
    fn rpc_response_err_outcome_round_trip() {
        let resp = RpcResponse {
            jsonrpc: "2.0".into(),
            id: 3,
            result: RpcOutcome::Err {
                error: RpcError {
                    code: APP_ERROR,
                    message: "配置未完成".into(),
                    data: None,
                },
            },
            ..Default::default()
        };
        let text = serde_json::to_string(&resp).unwrap();
        let back: RpcResponse = serde_json::from_str(&text).unwrap();
        match back.result {
            RpcOutcome::Err { error } => {
                assert_eq!(error.code, APP_ERROR);
                assert_eq!(error.message, "配置未完成");
            }
            RpcOutcome::Ok { .. } => panic!("expected Err outcome"),
        }
    }

    #[test]
    fn wire_message_parses_request_and_notification() {
        let req_text = r#"{"jsonrpc":"2.0","id":1,"method":"GetStatus","params":null}"#;
        let msg: WireMessage = serde_json::from_str(req_text).unwrap();
        match msg {
            WireMessage::Request(r) => {
                assert_eq!(r.method, "GetStatus");
                assert_eq!(r.id, 1);
            }
            _ => panic!("expected request"),
        }

        let notif_text = r#"{"jsonrpc":"2.0","method":"EventPush","params":{}}"#;
        let msg: WireMessage = serde_json::from_str(notif_text).unwrap();
        match msg {
            WireMessage::Notification(n) => assert_eq!(n.method, "EventPush"),
            _ => panic!("expected notification"),
        }
    }

    #[test]
    fn error_code_constants_match_json_rpc_spec() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
        assert_eq!(APP_ERROR, 1);
    }
}
