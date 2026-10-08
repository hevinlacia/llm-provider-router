//! 部署互斥锁（flock）：防止两个会话/进程同时切流覆盖彼此的部署。
//!
//! 锁文件由 `bin/deploy_lock.py` 写入（flock + token）。互斥本体靠 `flock(LOCK_EX)`：
//! 持有者进程死亡（含 kill -9）内核自动释放；锁文件内容残留不代表仍持锁，
//! 判定一律以"能否抢到 flock"为准。
//!
//! `set_active` 切流前的守卫逻辑：
//! - 锁空闲（抢得到 flock）→ 放行；
//! - 锁被持有且请求头 `X-Deploy-Lock` 与锁文件 token 一致 → 放行（部署流程自身的切流）；
//! - 锁被持有且 token 缺失/不匹配 → 409，返回持有者信息（应急：kill holder pid
//!   或 `bin/deploy_lock.py release --force`）。

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const DEFAULT_LOCK_FILE: &str = "~/.local/state/llm-provider-router/deploy.lock";
/// 持锁者切流时必须携带的请求头（值为 deploy_lock.py acquire 打印的 token）。
pub const DEPLOY_LOCK_HEADER: &str = "x-deploy-lock";

#[derive(Debug, Default, Deserialize)]
pub struct LockHolder {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub holder: String,
    #[serde(default)]
    pub holder_pid: serde_json::Value,
    #[serde(default)]
    pub acquired_at: String,
}

pub enum LockState {
    Free,
    Held(LockHolder),
}

fn lock_file_path() -> PathBuf {
    let raw = std::env::var("LLM_PROVIDER_ROUTER_DEPLOY_LOCK_FILE")
        .unwrap_or_else(|_| DEFAULT_LOCK_FILE.to_string());
    expand_home(&raw)
}

fn expand_home(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(raw)
}

/// 对独立 fd 抢非阻塞 flock：抢得到 = 空闲（文件内容视为上一任残留）。
fn probe_lock_at_path(path: &Path) -> LockState {
    let file: File = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // 绝不能 truncate：锁被持有时探测方不能破坏持有者写入的内容
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => return LockState::Free,
    };
    let fd = file.as_raw_fd();
    // SAFETY: fd 来自刚打开的普通文件；flock 为内核级锁，与本进程其他 fd 互斥。
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        LockState::Free
    } else {
        let holder = fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        LockState::Held(holder)
    }
}

/// 生产入口：按环境变量/默认路径探测。
pub fn probe_lock() -> LockState {
    probe_lock_at_path(&lock_file_path())
}

fn check_switch_allowed_at(path: &Path, headers: &HeaderMap) -> Option<Response> {
    let provided = headers
        .get(DEPLOY_LOCK_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    match probe_lock_at_path(path) {
        LockState::Free => None,
        LockState::Held(holder) => {
            if !holder.token.is_empty() && provided == holder.token {
                return None;
            }
            Some((
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "error": "deploy lock held; switching rejected to prevent concurrent deploys",
                    "lock": {
                        "holder": holder.holder,
                        "holder_pid": holder.holder_pid,
                        "acquired_at": holder.acquired_at,
                        "hint": "acquire the deploy lock via bin/deploy_lock.py (token goes in the X-Deploy-Lock header); emergency override: kill the holder pid or `bin/deploy_lock.py release --force`",
                    },
                })),
            )
                .into_response())
        }
    }
}

/// set_active 守卫入口：锁被持有时仅放行携带匹配 token 的切流请求。
/// `None` = 放行；`Some(resp)` = 拒绝并直接返回该响应。
pub fn check_switch_allowed(headers: &HeaderMap) -> Option<Response> {
    check_switch_allowed_at(&lock_file_path(), headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn temp_lock_path(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "llm-router-deploy-lock-test-{}-{}.lock",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn free_when_no_file_and_when_stale_content() {
        let path = temp_lock_path("free");
        // 文件不存在 → Free（OpenOptions create 会顺带建文件，清理掉）
        assert!(matches!(probe_lock_at_path(&path), LockState::Free));
        // 残留内容但无人持锁 → Free
        fs::write(&path, r#"{"token":"stale","holder":"dead"}"#).unwrap();
        assert!(matches!(probe_lock_at_path(&path), LockState::Free));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn held_blocks_wrong_token_and_allows_matching_token() {
        let path = temp_lock_path("held");
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&writer_path)
                .unwrap();
            let fd = file.as_raw_fd();
            // SAFETY: 测试内独立 fd，阻塞抢锁模拟外部持有者；线程结束关 fd 自动释放
            unsafe { libc::flock(fd, libc::LOCK_EX) };
            file.write_all(br#"{"token":"tok123","holder":"tester","holder_pid":1}"#)
                .unwrap();
            file.flush().unwrap();
            std::thread::sleep(Duration::from_millis(400));
        });

        std::thread::sleep(Duration::from_millis(80));

        match probe_lock_at_path(&path) {
            LockState::Held(holder) => assert_eq!(holder.token, "tok123"),
            LockState::Free => panic!("expected held"),
        }

        // 错误 token / 缺 token → 拒绝
        let mut wrong = HeaderMap::new();
        wrong.insert("x-deploy-lock", "nope".parse().unwrap());
        assert!(check_switch_allowed_at(&path, &wrong).is_some());
        assert!(check_switch_allowed_at(&path, &HeaderMap::new()).is_some());

        // 匹配 token → 放行
        let mut right = HeaderMap::new();
        right.insert("x-deploy-lock", "tok123".parse().unwrap());
        assert!(check_switch_allowed_at(&path, &right).is_none());

        writer.join().unwrap();
        // 持有者退出 → Free
        assert!(matches!(probe_lock_at_path(&path), LockState::Free));
        let _ = fs::remove_file(&path);
    }
}
