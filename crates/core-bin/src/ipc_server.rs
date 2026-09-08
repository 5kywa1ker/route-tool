//! Named Pipe IPC server：行分隔 JSON-RPC 2.0。
//!
//! 多客户端：为每个连接创建 pipe 实例，循环 accept。

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use ipc_protocol::{
    method, AppConfig, HealthEvent, RpcError, RpcNotification, RpcOutcome, RpcRequest, RpcResponse,
    APP_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, PIPE_NAME,
};

use crate::state::ServerState;

/// 启动 IPC server（阻塞当前任务直到服务退出）。
pub async fn serve(state: Arc<ServerState>, mut shutdown: broadcast::Receiver<()>) {
    info!("IPC server listening on {PIPE_NAME}");

    loop {
        // 创建新的 server pipe 实例等待连接。
        let server: NamedPipeServer = match ServerOptions::new()
            .first_pipe_instance(false)
            .create(PIPE_NAME)
        {
            Ok(s) => {
                harden_pipe_acl(&s);
                s
            }
            Err(e) => {
                error!("创建 pipe 实例失败: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        // select: 等待连接或关闭信号。
        tokio::select! {
            _ = shutdown.recv() => {
                info!("IPC server shutting down");
                return;
            }
            r = server.connect() => {
                if let Err(e) = r {
                    error!("pipe connect 失败: {e}");
                    continue;
                }
            }
        }

        let state = state.clone();
        tokio::spawn(async move {
            let (read, write) = tokio::io::split(server);
            handle_connection(state, read, write).await;
        });
    }
}

/// 处理单个客户端连接：循环读请求、处理、写响应；订阅者额外接收事件推送。
async fn handle_connection<S, W>(state: Arc<ServerState>, read: S, mut write: W)
where
    S: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let mut event_rx: Option<broadcast::Receiver<HealthEvent>> = None;

    loop {
        line.clear();
        tokio::select! {
            // 事件推送（仅订阅后）。
            evt = async {
                match event_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Ok(evt) = evt {
                    let n = RpcNotification {
                        jsonrpc: "2.0".into(),
                        method: method::EVENT_PUSH.to_string(),
                        params: serde_json::to_value(evt).unwrap_or_default(),
                    };
                    if let Ok(text) = serde_json::to_string(&n) {
                        if write.write_all(text.as_bytes()).await.is_err()
                            || write.write_all(b"\n").await.is_err()
                        {
                            break;
                        }
                        let _ = write.flush().await;
                    }
                }
            }
            n = reader.read_line(&mut line) => {
                match n {
                    Ok(0) | Err(_) => break, // EOF 或错误
                    Ok(_) => {}
                }
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: Result<RpcRequest, _> = serde_json::from_str(trimmed);
        let req = match msg {
            Ok(r) => r,
            Err(e) => {
                warn!("解析请求失败: {e}");
                continue;
            }
        };

        debug!("rpc <- {} id={}", req.method, req.id);

        // 订阅事件：挂上 broadcast。
        if req.method == method::SUBSCRIBE_EVENTS {
            if event_rx.is_none() {
                event_rx = Some(state.controller.event_tx.subscribe());
            }
            let resp = RpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id,
                result: RpcOutcome::Ok {
                    result: serde_json::json!({ "subscribed": true }),
                },
                ..Default::default()
            };
            write_line(&mut write, &resp).await;
            continue;
        }

        let outcome = dispatch(&state, &req).await;
        let resp = RpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id,
            result: outcome,
            ..Default::default()
        };
        write_line(&mut write, &resp).await;
    }
}

async fn write_line<W: AsyncWrite + Unpin, T: serde::Serialize>(w: &mut W, v: &T) {
    match serde_json::to_string(v) {
        Ok(text) => {
            if w.write_all(text.as_bytes()).await.is_err() || w.write_all(b"\n").await.is_err() {
                debug!("客户端断开");
            }
            let _ = w.flush().await;
        }
        Err(e) => error!("序列化响应失败: {e}"),
    }
}

/// 方法分发。
async fn dispatch(state: &ServerState, req: &RpcRequest) -> RpcOutcome {
    let c = &state.controller;
    match req.method.as_str() {
        method::GET_STATUS => {
            let rs = c.runtime.read().await.clone();
            ok(&rs)
        }
        method::GET_CONFIG => {
            let cfg = c.config.read().await.clone();
            ok(&cfg)
        }
        method::UPDATE_CONFIG => {
            let cfg: AppConfig = match serde_json::from_value(req.params.clone()) {
                Ok(v) => v,
                Err(e) => return err(INVALID_PARAMS, &format!("参数不合法: {e}")),
            };
            match c.update_config(cfg).await {
                Ok(()) => ok(&serde_json::json!(null)),
                Err(e) => err(e.app_error_code(), &e.to_string()),
            }
        }
        method::ENABLE_BYPASS => match c.enable().await {
            Ok(()) => ok(&serde_json::json!(null)),
            Err(e) => err(e.app_error_code(), &e.to_string()),
        },
        method::DISABLE_BYPASS => match c.disable().await {
            Ok(()) => ok(&serde_json::json!(null)),
            Err(e) => err(e.app_error_code(), &e.to_string()),
        },
        method::TEST_CONNECTIVITY => {
            let ip: std::net::IpAddr = match serde_json::from_value(req.params.clone()) {
                Ok(v) => v,
                Err(_) => {
                    // 也允许 { "ip": ... } 形式。
                    match serde_json::from_value::<serde_json::Value>(req.params.clone())
                        .ok()
                        .and_then(|v| v.get("ip").cloned())
                        .and_then(|v| serde_json::from_value(v).ok())
                    {
                        Some(v) => v,
                        None => return err(INVALID_PARAMS, "需要 IP 地址参数"),
                    }
                }
            };
            let reachable = c.inspector().ping(ip, 3000).await.unwrap_or(false);
            ok(&reachable)
        }
        method::LIST_ADAPTERS => match c.inspector().list_adapters().await {
            Ok(list) => ok(&list),
            Err(e) => err(e.app_error_code(), &e.to_string()),
        },
        other => err(METHOD_NOT_FOUND, &format!("未知方法: {other}")),
    }
}

fn ok<T: serde::Serialize>(v: &T) -> RpcOutcome {
    match serde_json::to_value(v) {
        Ok(x) => RpcOutcome::Ok { result: x },
        Err(e) => err(APP_ERROR, &format!("序列化失败: {e}")),
    }
}

fn err(code: i32, message: &str) -> RpcOutcome {
    RpcOutcome::Err {
        error: RpcError {
            code,
            message: message.to_string(),
            data: None,
        },
    }
}

/// 客户端连接（UI 侧测试用）。
#[allow(dead_code)]
pub async fn connect_client() -> std::io::Result<NamedPipeClient> {
    ClientOptions::new().open(PIPE_NAME)
}

/// 收紧管道 DACL：仅 SYSTEM / Administrators / Authenticated Users 可访问。
///
/// tokio 的 ServerOptions 不支持直接设置安全属性，这里在创建后用
/// SetKernelObjectSecurity 改写 DACL（SDDL 转换）。失败仅告警不拒绝服务
/// （默认 DACL 下管道本就只授予创建者与本地访问）。
fn harden_pipe_acl(server: &NamedPipeServer) {
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    const SDDL_REVISION_1: u32 = 1;
    // D: DACL, P: 无继承, A: 允许; GA: GENERIC_ALL
    // SY=SYSTEM, BA=Administrators, AU=Authenticated Users
    const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AU)";

    unsafe {
        // UTF-16 编码 SDDL 字符串（PCWSTR 要求宽字符）。
        let sddl_w: Vec<u16> = SDDL.encode_utf16().chain(std::iter::once(0)).collect();

        let mut sd_len = 0u32;
        let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        if let Err(e) = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            Some(&mut sd_len),
        ) {
            warn!("转换管道 SDDL 失败，跳过 ACL 加固: {e}");
            return;
        }

        // 从 SD 中取 DACL（确认 SDDL 转换产物确实包含 DACL）。
        let mut has_dacl = false.into();
        let mut dacl_ptr: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
        if let Err(e) =
            GetSecurityDescriptorDacl(sd, &mut has_dacl, &mut dacl_ptr, &mut false.into())
        {
            warn!("读取 SD 的 DACL 失败，跳过 ACL 加固: {e}");
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            return;
        }
        if !has_dacl.as_bool() || dacl_ptr.is_null() {
            warn!("SDDL 转换产物无 DACL，跳过 ACL 加固");
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            return;
        }

        // 把新 DACL 应用到管道内核对象：传入整个 SD，仅施加 DACL_SECURITY_INFORMATION。
        let hd = HANDLE(server.as_raw_handle());
        let set =
            windows::Win32::Security::SetKernelObjectSecurity(hd, DACL_SECURITY_INFORMATION, sd);
        if let Err(e) = set {
            warn!("设置管道 DACL 失败: {e}");
        } else {
            debug!("pipe DACL hardened (SYSTEM/Administrators/AuthUsers)");
        }
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
}
