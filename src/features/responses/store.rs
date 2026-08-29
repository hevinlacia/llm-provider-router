//! `previous_response_id` 支持：进程内环形缓存最近若干次响应的 assistant 输出，
//! 按响应 id 查回 chat 消息拼到下一次请求上下文。
//!
//! 说明：chat completions 是无状态的，Responses API 的 `previous_response_id` 依赖服务端
//! 会话，所以路由器自己维护一个内存环。front-proxy 同一时刻只转发到单个活跃 slot，
//! 单进程内状态一致；进程重启后环清空（与会话绑定一样属于运行时状态，可接受）。

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 100;
const TTL: Duration = Duration::from_secs(1800);

struct Entry {
    messages: Vec<Value>,
    created: Instant,
}

fn store() -> &'static Mutex<HashMap<String, Entry>> {
    static STORE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn put(response_id: &str, messages: Vec<Value>) {
    if response_id.is_empty() || messages.is_empty() {
        return;
    }
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    guard.retain(|_, e| now.saturating_duration_since(e.created) < TTL);
    while guard.len() >= MAX_ENTRIES {
        let oldest = guard
            .iter()
            .min_by_key(|(_, e)| e.created)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(key) => {
                guard.remove(&key);
            }
            None => break,
        }
    }
    guard.insert(
        response_id.to_string(),
        Entry {
            messages,
            created: now,
        },
    );
}

pub(crate) fn get(response_id: &str) -> Option<Vec<Value>> {
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    guard.retain(|_, e| now.saturating_duration_since(e.created) < TTL);
    guard.get(response_id).map(|e| e.messages.clone())
}
