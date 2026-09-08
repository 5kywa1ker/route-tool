//! 状态与配置持久化：原子写入（临时文件 + fsync + rename）。
//!
//! 数据落在 <ProgramData>\BypassTool\ 下（服务以 SYSTEM 运行，不依赖用户目录）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{AdapterSnapshot, AppConfig, CoreError, Result, RuntimeState};

/// 数据根目录。
pub const BASE_DIR: &str = r"C:\ProgramData\BypassTool";
pub const CONFIG_FILE: &str = "config.json";
pub const RUNTIME_FILE: &str = "runtime_state.json";
pub const SNAPSHOT_FILE: &str = "adapter_snapshot.json";

/// 状态存储。负责目录初始化与两类文件（配置 / 运行状态 / 快照）的读写。
#[derive(Debug, Clone)]
pub struct StateStore {
    base_dir: PathBuf,
}

impl Default for StateStore {
    fn default() -> Self {
        Self {
            base_dir: PathBuf::from(BASE_DIR),
        }
    }
}

impl StateStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// 确保目录存在（含日志子目录）。
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.base_dir.join("logs"))
            .map_err(|e| CoreError::Persistence(format!("创建数据目录失败: {e}")))?;
        Ok(())
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// 原子写 JSON 文件：临时文件 -> fsync -> rename。
    pub fn atomic_write_json<T: Serialize>(&self, file: &str, value: &T) -> Result<()> {
        let text = serde_json::to_string_pretty(value)
            .map_err(|e| CoreError::Persistence(format!("序列化失败: {e}")))?;
        atomic_write_text(&self.base_dir, file, &text)
    }

    /// 读 JSON 文件；不存在或损坏时返回 None。
    pub fn read_json<T: DeserializeOwned>(&self, file: &str) -> Result<Option<T>> {
        let path = self.base_dir.join(file);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)
            .map_err(|e| CoreError::Persistence(format!("读取 {} 失败: {e}", path.display())))?;
        match serde_json::from_str(&text) {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                // 文件损坏（如异常中断）：记录并忽略，按默认处理。
                tracing::warn!("解析 {} 失败: {e}", path.display());
                Ok(None)
            }
        }
    }

    pub fn load_config(&self) -> Result<AppConfig> {
        Ok(self.read_json(CONFIG_FILE)?.unwrap_or_default())
    }

    pub fn save_config(&self, cfg: &AppConfig) -> Result<()> {
        self.atomic_write_json(CONFIG_FILE, cfg)
    }

    pub fn load_runtime(&self) -> Result<RuntimeState> {
        Ok(self.read_json(RUNTIME_FILE)?.unwrap_or_default())
    }

    pub fn save_runtime(&self, rs: &RuntimeState) -> Result<()> {
        self.atomic_write_json(RUNTIME_FILE, rs)
    }

    pub fn load_snapshot(&self) -> Result<Option<AdapterSnapshot>> {
        self.read_json(SNAPSHOT_FILE)
    }

    pub fn save_snapshot(&self, snap: &AdapterSnapshot) -> Result<()> {
        self.atomic_write_json(SNAPSHOT_FILE, snap)
    }

    pub fn clear_snapshot(&self) -> Result<()> {
        let path = self.base_dir.join(SNAPSHOT_FILE);
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| CoreError::Persistence(format!("删除快照失败: {e}")))?;
        }
        Ok(())
    }
}

/// 通用原子写文本：临时文件（同目录，隐藏名）+ fsync + rename。
fn atomic_write_text(base_dir: &Path, file: &str, text: &str) -> Result<()> {
    let dir = base_dir;
    fs::create_dir_all(dir)
        .map_err(|e| CoreError::Persistence(format!("创建 {} 失败: {e}", dir.display())))?;
    let final_path = dir.join(file);
    let tmp_path = dir.join(format!(".{file}.tmp"));

    let mut f = fs::File::create(&tmp_path)
        .map_err(|e| CoreError::Persistence(format!("创建临时文件失败: {e}")))?;
    f.write_all(text.as_bytes())
        .map_err(|e| CoreError::Persistence(format!("写入临时文件失败: {e}")))?;
    f.sync_all()
        .map_err(|e| CoreError::Persistence(format!("fsync 失败: {e}")))?;
    drop(f);

    fs::rename(&tmp_path, &final_path).map_err(|e| {
        CoreError::Persistence(format!("rename 到 {} 失败: {e}", final_path.display()))
    })?;

    // 对目录再 fsync 以保证 rename 持久（Windows 上目录 fsync 支持有限，尽力而为）。
    let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HealthStatus, SwitchMode};

    fn temp_store(tag: &str) -> StateStore {
        let dir =
            std::env::temp_dir().join(format!("BypassToolTest_{tag}_{:?}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        StateStore::new(dir)
    }

    fn cleanup(store: &StateStore) {
        let _ = fs::remove_dir_all(store.base_dir());
    }

    #[test]
    fn config_round_trip_with_defaults() {
        let store = temp_store("cfg");
        store.ensure_dirs().unwrap();

        // 无文件 -> 默认配置。
        let cfg = store.load_config().unwrap();
        assert_eq!(cfg.health_check_interval_secs, 5);

        // 保存后再读一致。
        let mut cfg = cfg;
        cfg.bypass_ip = "192.168.2.1".parse().unwrap();
        cfg.adapter_id = "abc-123".into();
        store.save_config(&cfg).unwrap();
        let back = store.load_config().unwrap();
        assert_eq!(back.adapter_id, "abc-123");
        assert_eq!(back.bypass_ip.to_string(), "192.168.2.1");
        cleanup(&store);
    }

    #[test]
    fn runtime_round_trip() {
        let store = temp_store("rt");
        store.ensure_dirs().unwrap();

        let rs = RuntimeState {
            is_enabled: true,
            current_mode: Some(SwitchMode::AdapterReconfig),
            health: HealthStatus::Healthy,
            ..RuntimeState::default()
        };
        store.save_runtime(&rs).unwrap();

        let back = store.load_runtime().unwrap();
        assert!(back.is_enabled);
        assert_eq!(back.current_mode, Some(SwitchMode::AdapterReconfig));
        cleanup(&store);
    }

    #[test]
    fn corrupted_file_treated_as_missing() {
        let store = temp_store("bad");
        store.ensure_dirs().unwrap();
        fs::write(store.base_dir().join(CONFIG_FILE), "{not valid json").unwrap();

        let cfg = store.load_config().unwrap();
        assert_eq!(cfg.adapter_id, ""); // 落回默认值
        cleanup(&store);
    }

    #[test]
    fn snapshot_clear_is_idempotent() {
        let store = temp_store("snap");
        store.ensure_dirs().unwrap();

        assert!(store.load_snapshot().unwrap().is_none());
        let snap = AdapterSnapshot {
            adapter_id: "guid-1".into(),
            is_dhcp_enabled: true,
            static_ipv4: vec![],
            static_ipv4_mask: vec![],
            gateway: vec![],
            dns: vec![],
        };
        store.save_snapshot(&snap).unwrap();
        assert!(store.load_snapshot().unwrap().is_some());
        store.clear_snapshot().unwrap();
        assert!(store.load_snapshot().unwrap().is_none());
        // 再次 clear 不报错。
        store.clear_snapshot().unwrap();
        cleanup(&store);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let store = temp_store("tmp");
        store.ensure_dirs().unwrap();
        let rs = RuntimeState::default();
        store.save_runtime(&rs).unwrap();
        let entries: Vec<_> = fs::read_dir(store.base_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(entries.contains(&RUNTIME_FILE.to_string()));
        assert!(
            !entries.iter().any(|n| n.contains(".tmp")),
            "leftover tmp: {entries:?}"
        );
        cleanup(&store);
    }
}
