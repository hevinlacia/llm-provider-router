//! 会话指纹派生：客户端无任何显式 session 标识时的粘性兜底。
//!
//! 原理：主流客户端（pi / opencode 等）以无状态模式调用（每轮全量重发历史，
//! `store:false`），请求体里"system + 首条 user"在会话内严格不变、会话间大概率
//! 不同。对该稳定前缀做 sha256 得到派生键 `auto-<hash16>`，作为
//! [`crate::routes::chat::extract_session_id`] 的最终 fallback，实现与客户端
//! 无关的会话粘性。
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
}
