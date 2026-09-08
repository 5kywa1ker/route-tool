//! IPC 客户端（UI 侧）：行分隔 JSON-RPC over Named Pipe。

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;

use ipc_protocol::{
    method, AppConfig, RpcOutcome, RpcRequest, RpcResponse,
    PIPE_NAME,
};

/// UI 到 Core 的客户端。
pub struct IpcClient {
    reader: BufReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>>,
    writer: tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    next_id: u64,
}

impl IpcClient {
    /// 连接核心服务。
    pub async fn connect() -> std::io::Result<Self> {
        let pipe = ClientOptions::new().open(PIPE_NAME)?;
        let (read, write) = tokio::io::split(pipe);
        Ok(Self {
            reader: BufReader::new(read),
            writer: write,
            next_id: 1,
        })
    }

    async fn send_request(&mut self, m: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: self.next_id,
            method: m.to_string(),
            params,
        };
        self.next_id += 1;

        let text = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        self.writer
            .write_all(text.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|e| e.to_string())?;
        self.writer.flush().await.map_err(|e| e.to_string())?;

        // 读一行响应。
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        if line.is_empty() {
            return Err("连接已关闭".into());
        }

        let resp: RpcResponse = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
        match resp.result {
            RpcOutcome::Ok { result } => Ok(result),
            RpcOutcome::Err { error } => Err(format!("{} (code {})", error.message, error.code)),
        }
    }

    pub async fn get_status(&mut self) -> Result<ipc_protocol::RuntimeState, String> {
        let v = self.send_request(method::GET_STATUS, serde_json::json!(null)).await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    pub async fn get_config(&mut self) -> Result<AppConfig, String> {
        let v = self.send_request(method::GET_CONFIG, serde_json::json!(null)).await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    pub async fn update_config(&mut self, cfg: &AppConfig) -> Result<(), String> {
        let v = self
            .send_request(method::UPDATE_CONFIG, serde_json::to_value(cfg).unwrap())
            .await?;
        let _ = v;
        Ok(())
    }

    pub async fn enable(&mut self) -> Result<(), String> {
        self.send_request(method::ENABLE_BYPASS, serde_json::json!(null))
            .await
            .map(|_| ())
    }

    pub async fn disable(&mut self) -> Result<(), String> {
        self.send_request(method::DISABLE_BYPASS, serde_json::json!(null))
            .await
            .map(|_| ())
    }

    pub async fn test_connectivity(&mut self, ip: std::net::IpAddr) -> Result<bool, String> {
        let v = self
            .send_request(method::TEST_CONNECTIVITY, serde_json::json!(ip))
            .await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// 列出系统网卡（UI 暂未直接展示完整列表，保留接口）。
    #[allow(dead_code)]
    pub async fn list_adapters(&mut self) -> Result<Vec<ipc_protocol::AdapterInfo>, String> {
        let v = self
            .send_request(method::LIST_ADAPTERS, serde_json::json!(null))
            .await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}
