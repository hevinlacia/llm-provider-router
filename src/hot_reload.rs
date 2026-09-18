//! v2 配置文件热加载 watcher。
//!
//! 轮询 `config/` 下 v2 配置文件的 (mtime, len) 快照，检测到变化即重载 v2 配置，
//! 手工编辑 `providers-v2.json` / `models.json` / `logical-models.json` /
//! `virtual-models.json` 后无需调用 `/api/config/reload-env` 或重启即可生效。
//!
//! 设计取舍：
//! - 不引入 notify 等文件系统 watcher 依赖，2s 轮询对个人工具足够及时且实现最简。
//! - 编辑器非原子写入（半写状态）由「重载失败保留旧配置」兜底（见
//!   `RouterState::reload_v2`），文件写完整后的下一次变更检测会自动恢复。
use crate::app::AppState;
use crate::config_v2;
use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, SystemTime};

/// 监听的 v2 配置文件（与 `load_v2_config` 读取的路径一致）。
const WATCH_PATHS: &[&str] = &[
    config_v2::V2_PROVIDERS_PATH,
    config_v2::V2_MODELS_PATH,
    config_v2::V2_LOGICAL_MODELS_PATH,
    config_v2::V2_VIRTUAL_MODELS_PATH,
];

/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 单文件的 (mtime, len) 指纹；文件不存在为 None（如可选的 virtual-models.json）。
type FileStamp = Option<(SystemTime, u64)>;
type Snapshot = BTreeMap<String, FileStamp>;

fn file_stamp(path: &str) -> FileStamp {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn snapshot(paths: &[&str]) -> Snapshot {
    paths
        .iter()
        .map(|path| (path.to_string(), file_stamp(path)))
        .collect()
}

/// 启动 v2 配置热加载 watcher。在每个 backend 实例启动时调用一次。
pub(crate) fn spawn_watcher(app: AppState) {
    let mut last = snapshot(WATCH_PATHS);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let current = snapshot(WATCH_PATHS);
            if current == last {
                continue;
            }
            let changed: Vec<&str> = WATCH_PATHS
                .iter()
                .copied()
                .filter(|path| current.get(*path) != last.get(*path))
                .collect();
            last = current;
            // std Mutex 只在同步块内持有，重载为快速文件 IO + 解析，不跨 await。
            let loaded = {
                let mut state = match app.state.lock() {
                    Ok(state) => state,
                    Err(_) => {
                        eprintln!("llm-provider-router hot-reload: router state lock poisoned; skip reload");
                        continue;
                    }
                };
                state.hot_reload_v2()
            };
            eprintln!(
                "llm-provider-router hot-reload: v2 config change detected in {changed:?}; {}",
                if loaded {
                    "config reloaded"
                } else {
                    "reload failed, keeping last good config (v2 config invalid)"
                }
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_detects_create_modify_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("watched.json");
        let path_str = path.to_str().expect("utf8 path");

        let before = snapshot(&[path_str]);
        assert_eq!(before.get(path_str), Some(&None), "missing file -> None");

        fs::write(&path, "{\"a\":1}").expect("write");
        let created = snapshot(&[path_str]);
        assert!(created.get(path_str).is_some_and(Option::is_some));

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, "{\"a\":2}").expect("rewrite");
        let modified = snapshot(&[path_str]);
        assert_ne!(
            created.get(path_str),
            modified.get(path_str),
            "mtime/len change detected"
        );

        fs::remove_file(&path).expect("remove");
        let removed = snapshot(&[path_str]);
        assert_eq!(removed.get(path_str), Some(&None));
    }
}
