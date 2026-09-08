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

    // SECURITY_ATTRIBUTES 在进程内只构建一次并泄漏给进程生命周期：
    // 每次 CreateNamedPipeW 都会把 SD 拷进内核对象，但 SA 本身只需在调用期间有效，
    // 泄漏后可保证所有循环迭代的 create() 都能读到稳定指针。
    let pipe_sa = match build_pipe_security_attributes() {
        Ok(p) => p,
        Err(e) => {
            error!("构建管道 SECURITY_ATTRIBUTES 失败，IPC 不可用: {e}");
            // 不无限空转：把错误直接报给上层（服务应记录并停止）。
            return;
        }
    };

    loop {
        // 创建新的 server pipe 实例等待连接。必须通过 create_with_security_attributes_raw
        // 带上我们预构建的 DACL（SYSTEM/Administrators/AuthUsers）—— 走 create() 默认 DACL
        // 会被 SERVICE 限制为仅创建者/管理员，UI 用户（Authenticated Users）会 0xC0000022/5
        // 拒访，进而导致 UI 连不上核心、配置与网卡列表永远拉不到。
        let server: NamedPipeServer = unsafe {
            match ServerOptions::new()
                .first_pipe_instance(false)
                .create_with_security_attributes_raw(PIPE_NAME, pipe_sa)
            {
                Ok(s) => s,
                Err(e) => {
                    error!("创建 pipe 实例失败: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
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

/// 构建管道使用的 SECURITY_ATTRIBUTES（含宽松 DACL：SYSTEM / Administrators /
/// Authenticated Users 均可访问）。
///
/// 旧实现通过 `SetKernelObjectSecurity` 事后改 DACL 在服务进程里 0x80070005 失败：
/// 命名管道的 server 句柄默认不带 WRITE_DAC，且同进程内拿不到带 WRITE_DAC 的句柄。
/// 正确做法是 `CreateNamedPipeW` 时通过 `lpSecurityAttributes` 直接传预构建的 SA + SD。
///
/// 内存管理：
/// - SD 由 `ConvertStringSecurityDescriptorToSecurityDescriptorW` 分配（LocalAlloc），
///   进程内泄漏（不 LocalFree），覆盖所有后续 CreateNamedPipeW 调用即可。
/// - SA 放在泄漏的 `Box` 里（`'static`），指针稳定可重复使用。
fn build_pipe_security_attributes() -> windows::core::Result<*mut core::ffi::c_void> {
    use std::sync::OnceLock;
    use windows::core::PCWSTR;
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::SECURITY_ATTRIBUTES;

    const SDDL_REVISION_1: u32 = 1;
    // D: DACL, P: 无继承, A: 允许; GA: GENERIC_ALL
    // SY=SYSTEM, BA=Administrators, AU=Authenticated Users
    const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AU)";

    static SA_PTR: OnceLock<usize> = OnceLock::new();

    if let Some(&raw) = SA_PTR.get() {
        return Ok(raw as *mut core::ffi::c_void);
    }

    unsafe {
        let sddl_w: Vec<u16> = SDDL.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = windows::Win32::Security::PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        let mut sd_len = 0u32;
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            Some(&mut sd_len),
        )?;

        // SA 泄漏：Box 永驻进程，其内部 lpSecurityDescriptor 指向 SD 也跟着常驻。
        let sa_box: &'static mut SECURITY_ATTRIBUTES = Box::leak(Box::new(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: windows::core::BOOL(0),
        }));
        let raw = sa_box as *mut SECURITY_ATTRIBUTES as usize;
        let _ = sd_len;
        let _ = SA_PTR.set(raw);
        Ok(raw as *mut core::ffi::c_void)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::PWSTR;
    use windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    /// 回归测试：build_pipe_security_attributes 构建的 SA 必须包含可让 Authenticated
    /// Users 访问的 ACE。之前 SetKernelObjectSecurity 路径会 0x80070005 失败，管道
    /// 退回默认 DACL，UI 用户连不上核心，导致配置和网卡列表永远拿不到（下拉框空）。
    /// 修复后通过 CreateNamedPipeW 时的 lpSecurityAttributes 直接挂上正确 DACL。
    /// 这里把 SA 里的 SD 反向转回 SDDL 字符串，断言必须包含 "AU"。
    #[test]
    fn pipe_security_attributes_grants_authenticated_users() {
        let sa_raw = build_pipe_security_attributes().expect("SA build ok");
        // SAFETY: 这是我们刚刚泄漏的 SA，进程内单例，可读。
        let sa = unsafe { &*(sa_raw as *const windows::Win32::Security::SECURITY_ATTRIBUTES) };
        assert!(!sa.lpSecurityDescriptor.is_null(), "SD must not be null");

        // 把 SD 转回 SDDL 字符串验证 DACL。
        let mut sddl_out = PWSTR(std::ptr::null_mut());
        let mut sddl_len: u32 = 0;
        // SAFETY: 传入有效 SD 指针 + 输出 buffer。
        let conv_ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                PSECURITY_DESCRIPTOR(sa.lpSecurityDescriptor),
                1, // SDDL_REVISION_1
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut sddl_out,
                Some(&mut sddl_len),
            )
        }
        .is_ok();
        assert!(
            conv_ok,
            "ConvertSecurityDescriptorToStringSecurityDescriptorW failed"
        );
        // SAFETY: 成功返回的 PWSTR 由 LocalAlloc 分配，转成字符串读。
        let sddl = unsafe {
            let slice = std::slice::from_raw_parts(sddl_out.0, sddl_len as usize);
            String::from_utf16_lossy(slice)
        };
        // 释放（LocalFree）。
        // SAFETY: 函数返回的 PWSTR 需用 LocalFree，按 *mut c_void 传入。
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(sddl_out.0 as *mut std::ffi::c_void),
            ));
        }
        assert!(
            sddl.contains("AU"),
            "DACL must contain Authenticated Users (AU), got: {sddl}"
        );
        // 顺手也确认 SYSTEM / Administrators 都在。
        assert!(sddl.contains("SY"), "DACL must contain SYSTEM, got: {sddl}");
        assert!(
            sddl.contains("BA"),
            "DACL must contain Administrators, got: {sddl}"
        );
    }
}
