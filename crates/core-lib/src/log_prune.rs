//! 日志保留清理：删除超过保留期的滚动日志文件（§2 日志需求：保留 7 天）。

use std::path::Path;
use std::time::{Duration, SystemTime};

/// 日志保留期（7 天）。
pub const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// 删除 `dir` 下文件名以 `prefix` 开头、且最后修改时间早于 `retention` 的文件。
/// 返回删除的文件数；目录不存在等情况静默返回 0。
pub fn prune_old_logs(dir: &Path, prefix: &str, retention: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map(|age| age > retention)
            .unwrap_or(false);
        if expired && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 启动后台任务：立即清理一次，此后每 24 小时巡检（供 core 的长驻进程使用）。
pub fn spawn_daily_prune(dir: std::path::PathBuf, prefix: String) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(24 * 3600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let removed = prune_old_logs(&dir, &prefix, LOG_RETENTION);
            if removed > 0 {
                tracing::debug!("cleaned {removed} expired log file(s)");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("RouteToolLogPrune_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn retention_zero_deletes_all_prefixed_files() {
        let dir = temp_dir("zero");
        std::fs::write(dir.join("app.log.2020-01-01"), b"a").unwrap();
        std::fs::write(dir.join("app.log.2020-01-02"), b"b").unwrap();
        std::fs::write(dir.join("other.txt"), b"c").unwrap();

        let removed = prune_old_logs(&dir, "app.log", Duration::ZERO);
        assert_eq!(removed, 2);
        assert!(!dir.join("app.log.2020-01-01").exists());
        assert!(!dir.join("app.log.2020-01-02").exists());
        assert!(dir.join("other.txt").exists(), "非前缀文件不应被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_files_within_retention_are_kept() {
        let dir = temp_dir("keep");
        std::fs::write(dir.join("app.log.2026-09-08"), b"fresh").unwrap();

        let removed = prune_old_logs(&dir, "app.log", LOG_RETENTION);
        assert_eq!(removed, 0);
        assert!(dir.join("app.log.2026-09-08").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_silent() {
        assert_eq!(
            prune_old_logs(Path::new(r"\\?\nonexistent-dir"), "app.log", LOG_RETENTION),
            0
        );
    }
}
