//! `previous_response_id` 支持 + 翻译模式响应生命周期：进程内环形缓存最近若干次响应的
//! assistant 输出与完整 Response 对象，按响应 id 提供查询 / 取消 / 删除 / 输入项列表。
//!
//! 说明：chat completions 是无状态的，Responses API 的 `previous_response_id` 依赖服务端
//! 会话，所以路由器自己维护一个内存环。front-proxy 同一时刻只转发到单个活跃 slot，
//! 单进程内状态一致；进程重启后环清空（与会话绑定一样属于运行时状态，可接受）。
//! 透传模式的响应不登记（无法路由归属 alias），get/cancel/delete 只对翻译模式有效。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 100;
const TTL: Duration = Duration::from_secs(1800);

/// store 内单条响应的完整状态。
struct Entry {
    /// assistant chat 消息（previous_response_id 回填上下文用）。
    messages: Vec<Value>,
    /// 完整 Responses Response 对象（get/cancel 返回；cancel 时原地改 status）。
    response: Value,
    /// 原始 input items（input_items 端点返回；翻译后保留原样）。
    input_items: Vec<Value>,
    created: Instant,
}

fn store() -> &'static Mutex<HashMap<String, Entry>> {
    static STORE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Entry>> {
    store().lock().unwrap_or_else(|p| p.into_inner())
}

/// 登记一条完整响应（翻译模式）。messages 为空则不登记 previous 上下文。
pub(crate) fn put_full(
    response_id: &str,
    messages: Vec<Value>,
    response: Value,
    input_items: Vec<Value>,
) {
    if response_id.is_empty() {
        return;
    }
    let mut guard = lock();
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
            response,
            input_items,
            created: now,
        },
    );
}

/// 取 assistant chat 消息（previous_response_id 回填上下文用）。空则返回 None。
pub(crate) fn get(response_id: &str) -> Option<Vec<Value>> {
    let mut guard = lock();
    let now = Instant::now();
    guard.retain(|_, e| now.saturating_duration_since(e.created) < TTL);
    guard
        .get(response_id)
        .map(|e| e.messages.clone())
        .filter(|m| !m.is_empty())
}

/// 取完整 Response 对象（GET /responses/{id}）。
pub(crate) fn get_response(response_id: &str) -> Option<Value> {
    let mut guard = lock();
    let now = Instant::now();
    guard.retain(|_, e| now.saturating_duration_since(e.created) < TTL);
    guard.get(response_id).map(|e| e.response.clone())
}

/// 取原始 input items（GET /responses/{id}/input_items）。
pub(crate) fn get_input_items(response_id: &str) -> Option<Vec<Value>> {
    let mut guard = lock();
    let now = Instant::now();
    guard.retain(|_, e| now.saturating_duration_since(e.created) < TTL);
    guard
        .get(response_id)
        .map(|e| e.input_items.clone())
        .filter(|items| !items.is_empty())
}

/// 取消响应：把 store 里的 status 改为 cancelled，返回更新后的 Response 对象。
/// 找不到返回 None。幂等：已 cancelled 再 cancel 仍返回 cancelled 对象。
pub(crate) fn cancel(response_id: &str) -> Option<Value> {
    let mut guard = lock();
    let now = Instant::now();
    guard.retain(|_, e| now.saturating_duration_since(e.created) < TTL);
    let entry = guard.get_mut(response_id)?;
    if let Some(obj) = entry.response.as_object_mut() {
        obj.insert("status".to_string(), json!("cancelled"));
    }
    Some(entry.response.clone())
}

/// 删除响应：返回是否删除成功。
pub(crate) fn delete(response_id: &str) -> bool {
    let mut guard = lock();
    let now = Instant::now();
    guard.retain(|_, e| now.saturating_duration_since(e.created) < TTL);
    guard.remove(response_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_full_lifecycle() {
        let id = "resp_1";
        let resp = json!({
            "id": id,
            "object": "response",
            "status": "completed",
            "output": [json!({"type": "message"})],
        });
        put_full(
            id,
            vec![json!({ "role": "assistant", "content": "hi" })],
            resp.clone(),
            vec![json!({ "type": "message", "role": "user", "content": "hi" })],
        );
        assert_eq!(get(id).unwrap()[0]["content"], "hi");
        assert_eq!(get_response(id).unwrap()["status"], "completed");
        let items = get_input_items(id).unwrap();
        assert_eq!(items[0]["content"], "hi");

        // cancel 改状态
        let cancelled = cancel(id).unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(get_response(id).unwrap()["status"], "cancelled");

        // delete 后查不到
        assert!(delete(id));
        assert!(get(id).is_none());
        assert!(get_response(id).is_none());
        assert!(get_input_items(id).is_none());
    }

    #[test]
    fn cancel_unknown_returns_none() {
        assert!(cancel("resp_nope").is_none());
    }

    #[test]
    fn delete_unknown_returns_false() {
        assert!(!delete("resp_nope"));
    }

    #[test]
    fn get_input_items_empty_returns_none() {
        let id = "resp_2";
        put_full(
            id,
            vec![json!({ "role": "assistant", "content": "hi" })],
            json!({ "id": id }),
            Vec::new(),
        );
        assert!(get_input_items(id).is_none());
    }

    #[test]
    fn empty_input_items_lifecycle() {
        let id = "resp_3";
        put_full(id, Vec::new(), json!({ "id": id }), Vec::new());
        // 无 assistant 消息则 previous 上下文不可用，但 response 对象仍可查
        assert!(get(id).is_none());
        assert_eq!(get_response(id).unwrap()["id"], id);
    }
}
