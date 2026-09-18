//! Anthropic Messages <-> Responses 纯函数翻译（便于单测）。
//!
//! 子模块布局：
//! - `request.rs`：请求方向（Anthropic Messages -> Responses）；
//! - `response.rs`：响应方向（Responses -> Anthropic Messages）；
//! - `sse.rs`：流式逐事件翻译（`translate_stream_event` + `StreamEventState`）；
//! - 本文件：错误体/usage 提取（`responses_error_to_anthropic`、`extract_anthropic_*`）。

use serde_json::{json, Map, Value};

use super::{error_body, map_error_type};

mod request;
mod response;
mod sse;

pub(crate) use request::messages_to_responses;
pub(crate) use response::responses_to_messages;
pub(crate) use sse::{translate_stream_event, StreamEventState};

/// OpenAI 风格错误体（`{error:{message,type}}`）-> Anthropic 错误体。
pub(crate) fn responses_error_to_anthropic(body: &Value) -> Value {
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("upstream error")
        .to_string();
    let error_type = body
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    error_body(&message, map_error_type(error_type))
}

/// 从 Anthropic 非流式响应体提取 usage（归一化为内部统计字段）；无 usage 返回 None。
pub(crate) fn extract_anthropic_usage(content: &Value) -> Option<Value> {
    let usage = content.get("usage").filter(|u| u.is_object())?;
    let normalized = super::normalize_usage(usage);
    let is_zero = normalized.as_object().map(Map::is_empty).unwrap_or(true);
    if is_zero {
        None
    } else {
        Some(normalized)
    }
}

/// 从 Anthropic SSE 流文本提取 usage：`message_start` 带 `message.usage.input_tokens`，
/// `message_delta` 带 `usage.output_tokens`；合并后归一化。无 usage 返回 None。
pub(crate) fn extract_anthropic_stream_usage(body_text: &str) -> Option<Value> {
    let mut input_tokens: Option<u64> = None;
    let mut output_tokens: Option<u64> = None;
    for line in body_text.lines().map(str::trim) {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                input_tokens = event
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64);
            }
            Some("message_delta") => {
                output_tokens = event
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .or(output_tokens);
            }
            _ => {}
        }
    }
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(super::normalize_usage(&json!({
        "input_tokens": input_tokens.unwrap_or(0),
        "output_tokens": output_tokens.unwrap_or(0),
    })))
}

#[cfg(test)]
mod tests;
