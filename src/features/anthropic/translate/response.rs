//! Responses API 响应 -> Anthropic Messages 响应翻译。

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 响应：Responses -> Anthropic Messages
// ---------------------------------------------------------------------------

/// Responses 响应对象 -> Anthropic Messages 响应。
pub(crate) fn responses_to_messages(resp: &Value, requested_model: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut has_tool_use = false;
    for item in resp
        .get("output")
        .and_then(Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[])
    {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "reasoning" => {
                let mut text = String::new();
                if let Some(parts) = item.get("summary").and_then(Value::as_array) {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                        }
                    }
                }
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("reasoning_text") {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                    }
                }
                if !text.is_empty() {
                    // signature 置空：翻译链路无真实 Anthropic 签名；回传时输入翻译会丢弃思考块
                    content.push(json!({ "type": "thinking", "thinking": text, "signature": "" }));
                }
            }
            "message" => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            content.push(json!({ "type": "text", "text": text }));
                        }
                    }
                }
            }
            "function_call" => {
                has_tool_use = true;
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let args_text = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let input_value: Value =
                    serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": input_value,
                }));
            }
            _ => {}
        }
    }
    let status = resp
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let stop_reason = if status == "incomplete" {
        "max_tokens"
    } else if has_tool_use {
        "tool_use"
    } else {
        "end_turn"
    };
    let usage = resp.get("usage").cloned().unwrap_or(json!({}));
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_unknown")
        .to_string();
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }
    })
}
