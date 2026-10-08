//! 会话指纹派生：客户端无任何显式 session 标识时的粘性兜底。
//!
//! 原理：主流客户端（pi / opencode 等）以无状态模式调用（每轮全量重发历史，
//! `store:false`），请求体里"system + 首条 user"在会话内严格不变、会话间大概率
//! 不同。对该稳定前缀做 sha256 得到派生键 `auto-<hash16>`，作为
//! [`extract_session_id`] 的最终 fallback，实现与客户端无关的会话粘性。
//!
//! 边界：
//! - 指纹碰撞（两会话开头相同）只是共享粘性桶，无害；
//! - `/compact`、resume、subagent 新会话 → 指纹变化 → 新粘性桶，符合软亲和语义；
//! - 只落 hash，不落原文；tools/温度等易变字段不参与指纹。

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 从请求体派生会话指纹。返回 `auto-<hash16>`；无法提取任何稳定内容时返回 None。
pub(crate) fn derive_session_fingerprint(payload: &Value) -> Option<String> {
    let mut stable: Vec<String> = Vec::new();
    collect_chat(payload, &mut stable).or_else(|| {
        collect_responses(payload, &mut stable);
        (!stable.is_empty()).then_some(())
    })?;
    if stable.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    for part in &stable {
        hasher.update(part.as_bytes());
        hasher.update([0x00]); // 分段分隔符，避免拼接歧义
    }
    let digest = hasher.finalize();
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("auto-{hex}"))
}

/// chat completions：`messages` 里开头的 system 消息 + 第一条 user 消息。
fn collect_chat(payload: &Value, out: &mut Vec<String>) -> Option<()> {
    let messages = payload.get("messages")?.as_array()?;
    if messages.is_empty() {
        return None;
    }
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
        if role == "system" || role == "developer" {
            out.push(format!("s:{}", normalize_content(msg.get("content"))));
        } else if role == "user" {
            out.push(format!("u:{}", normalize_content(msg.get("content"))));
            break;
        } else {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(())
    }
}

/// responses：`instructions`（system 等价物）+ `input` 里的第一条 user 文本。
/// input 为纯 string 时整体视为首条 user；为数组时找第一条 role=user 的 message。
fn collect_responses(payload: &Value, out: &mut Vec<String>) {
    match payload.get("instructions") {
        Some(Value::String(s)) if !s.is_empty() => out.push(format!("s:{s}")),
        Some(Value::Array(parts)) => {
            let text: Vec<String> = parts
                .iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect();
            if !text.is_empty() {
                out.push(format!("s:{}", text.join("\n")));
            }
        }
        _ => {}
    }
    match payload.get("input") {
        Some(Value::String(s)) if !s.is_empty() => out.push(format!("u:{s}")),
        Some(Value::Array(items)) => {
            for item in items {
                let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
                if role == "user" {
                    out.push(format!("u:{}", normalize_content(item.get("content"))));
                    break;
                }
            }
        }
        _ => {}
    }
}

/// 内容规范化：string 直接用；数组（多模态 parts）逐个提取文本部分后拼接，
/// 保证同一内容的不同 JSON 表达（如多余空白字段）尽可能得到相同指纹输入。
fn normalize_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                p.as_str()
                    .map(str::to_string)
                    .or_else(|| p.get("text").and_then(Value::as_str).map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 会话标识提取（原 routes/chat.rs，chat 协议下线后迁入本模块）：
// 显式 header > body 字段 > Claude Code metadata.user_id > prompt_cache_key > 指纹兜底。
// ---------------------------------------------------------------------------

use axum::http::HeaderMap;

/// 从请求头 / 请求体里推断会话标识：显式 header 优先，其次 body 内的
/// session/trace 字段，最后兜底解析 Claude Code 的 `metadata.user_id`
/// （形如 `user_<hash>_account_<uuid>_session_<uuid>`）。
pub(crate) fn extract_session_id(payload: &Value, headers: &HeaderMap) -> Option<String> {
    header_str(headers, "x-litellm-session-id")
        .or_else(|| header_str(headers, "x-opencode-session-id"))
        .or_else(|| header_str(headers, "x-session-id"))
        .or_else(|| header_str(headers, "x-session-affinity"))
        .or_else(|| header_str(headers, "session_id"))
        .or_else(|| {
            payload
                .pointer("/metadata/session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/metadata/trace_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/litellm_metadata/session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/litellm_metadata/trace_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/metadata/user_id")
                .and_then(Value::as_str)
                .and_then(parse_session_from_user_id)
        })
        // OpenAI Responses API 官方会话亲和字段：pi 等客户端每请求携带
        // prompt_cache_key=<session id>（store:false 无状态模式），无需额外配置即可粘性。
        .or_else(|| {
            payload
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        // 兜底：客户端完全无标识时，用请求体稳定前缀（system + 首条 user）派生会话指纹。
        // 与客户端无关的粘性：无状态客户端每轮全量重发历史，该前缀会话内不变。
        .or_else(|| derive_session_fingerprint(payload))
}

/// Claude Code 把会话 UUID 拼在 `metadata.user_id` 尾部：取最后一个 `_session_` 之后的段。
fn parse_session_from_user_id(user_id: &str) -> Option<String> {
    let marker = "_session_";
    let idx = user_id.rfind(marker)?;
    let session = &user_id[idx + marker.len()..];
    if session.is_empty() {
        None
    } else {
        Some(session.to_string())
    }
}

pub(crate) fn header_str(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_fingerprint_stable_across_history_growth() {
        // 会话第 1 轮：system + 一条 user
        let round1 = json!({
            "messages": [
                { "role": "system", "content": "You are a coding agent." },
                { "role": "user", "content": "help me fix the bug" }
            ]
        });
        // 会话第 N 轮：历史单调追加，system 与首条 user 不变
        let round_n = json!({
            "messages": [
                { "role": "system", "content": "You are a coding agent." },
                { "role": "user", "content": "help me fix the bug" },
                { "role": "assistant", "content": "sure" },
                { "role": "user", "content": "also add tests" },
                { "role": "assistant", "content": "done" },
                { "role": "user", "content": "run them" }
            ]
        });
        let fp1 = derive_session_fingerprint(&round1).unwrap();
        let fpn = derive_session_fingerprint(&round_n).unwrap();
        assert_eq!(fp1, fpn, "历史追加不应改变指纹");
        assert!(fp1.starts_with("auto-"));
    }

    #[test]
    fn chat_fingerprint_differs_across_sessions() {
        let a = json!({
            "messages": [
                { "role": "system", "content": "You are a coding agent." },
                { "role": "user", "content": "fix bug A" }
            ]
        });
        let b = json!({
            "messages": [
                { "role": "system", "content": "You are a coding agent." },
                { "role": "user", "content": "fix bug B" }
            ]
        });
        assert_ne!(
            derive_session_fingerprint(&a),
            derive_session_fingerprint(&b)
        );
    }

    #[test]
    fn responses_fingerprint_stable_and_distinct() {
        let round1 = json!({
            "instructions": "You are a coding agent.",
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "hello there" }] }
            ],
            "store": false
        });
        let round_n = json!({
            "instructions": "You are a coding agent.",
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "hello there" }] },
                { "role": "assistant", "content": [{ "type": "output_text", "text": "hi" }] },
                { "role": "user", "content": [{ "type": "input_text", "text": "continue" }] }
            ],
            "store": false
        });
        assert_eq!(
            derive_session_fingerprint(&round1),
            derive_session_fingerprint(&round_n)
        );

        let other = json!({
            "instructions": "You are a coding agent.",
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "different opener" }] }
            ],
            "store": false
        });
        assert_ne!(
            derive_session_fingerprint(&round1),
            derive_session_fingerprint(&other)
        );
    }

    #[test]
    fn responses_input_string_is_supported() {
        let a = json!({ "input": "hello world", "instructions": "sys" });
        let b = json!({ "input": "hello world", "instructions": "sys" });
        let c = json!({ "input": "hello world!", "instructions": "sys" });
        assert_eq!(
            derive_session_fingerprint(&a),
            derive_session_fingerprint(&b)
        );
        assert_ne!(
            derive_session_fingerprint(&a),
            derive_session_fingerprint(&c)
        );
    }

    #[test]
    fn empty_payload_returns_none() {
        assert!(derive_session_fingerprint(&json!({})).is_none());
        assert!(derive_session_fingerprint(&json!({ "messages": [] })).is_none());
        assert!(derive_session_fingerprint(&json!({ "input": "" })).is_none());
    }

    // —— 会话标识提取（原 chat.rs 测试，随代码迁入）——

    #[test]
    fn extract_prefers_explicit_session_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-affinity", "affinity-123".parse().unwrap());
        headers.insert("session_id", "plain-456".parse().unwrap());
        assert_eq!(
            extract_session_id(&serde_json::json!({}), &headers).as_deref(),
            Some("affinity-123")
        );
    }

    #[test]
    fn extract_parses_claude_code_user_id() {
        let payload = serde_json::json!({
            "metadata": { "user_id": "user_ab12cd34_account_1111-2222_session_9f8e7d6c" }
        });
        assert_eq!(
            extract_session_id(&payload, &HeaderMap::new()).as_deref(),
            Some("9f8e7d6c")
        );
    }

    #[test]
    fn explicit_metadata_session_beats_user_id() {
        let payload = serde_json::json!({
            "metadata": { "session_id": "explicit", "user_id": "user_a_account_b_session_c" }
        });
        assert_eq!(
            extract_session_id(&payload, &HeaderMap::new()).as_deref(),
            Some("explicit")
        );
    }

    #[test]
    fn extract_returns_none_without_signals() {
        assert_eq!(
            extract_session_id(&serde_json::json!({}), &HeaderMap::new()),
            None
        );
    }

    /// pi 的 responses 请求每轮携带 prompt_cache_key=<sessionId>，
    /// router 应直接识别为会话标识（responses 协议自动粘性的关键）。
    #[test]
    fn extract_reads_prompt_cache_key() {
        let payload = serde_json::json!({
            "model": "deepseek-v4-flash-auto",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }],
            "prompt_cache_key": "pi-session-abc123",
            "store": false
        });
        assert_eq!(
            extract_session_id(&payload, &HeaderMap::new()).as_deref(),
            Some("pi-session-abc123")
        );
    }

    /// 客户端完全无标识时，回退到请求体稳定前缀派生的会话指纹。
    #[test]
    fn extract_falls_back_to_session_fingerprint() {
        let payload = serde_json::json!({
            "messages": [
                { "role": "system", "content": "You are a coding agent." },
                { "role": "user", "content": "fix the login bug" }
            ]
        });
        let sid = extract_session_id(&payload, &HeaderMap::new()).unwrap();
        assert!(sid.starts_with("auto-"), "指纹应带 auto- 前缀，got {sid}");
        // 同一会话历史追加 → 指纹不变（粘性稳定）
        let mut grown = payload.clone();
        grown["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "role": "assistant", "content": "ok"
            }));
        grown["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "role": "user", "content": "thanks, next step"
            }));
        assert_eq!(extract_session_id(&grown, &HeaderMap::new()), Some(sid));
    }

    /// 显式标识永远优先于指纹兜底（对已发标识的客户端零行为变化）。
    #[test]
    fn explicit_session_beats_fingerprint() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "explicit-sess".parse().unwrap());
        let payload = serde_json::json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "hello" }
            ]
        });
        assert_eq!(
            extract_session_id(&payload, &headers).as_deref(),
            Some("explicit-sess")
        );
    }
}
