//! Responses API <-> Chat Completions 协议翻译（纯函数，不触 IO）。
//!
//! 子模块布局：
//! - `request.rs`：`responses_to_chat` 把 `POST /v1/responses` 请求翻译成
//!   chat completions 请求，交给现有路由/选 key/重试/冻结/用量链路；
//! - `response.rs`：`chat_to_responses` 把上游 chat completions 非流式响应
//!   翻译成 Responses API 响应对象（含回显字段、usage 归一化）；
//! - 本文件：错误体生成 + 公共工具（id 生成、透传载荷、input items 提取、token 估算）。
//!
//! 翻译只做字段语义映射；`previous_response_id` 等需要状态的部分在 handler/store 层处理。

use crate::config::ModelAlias;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

mod request;
mod response;

pub(crate) use request::responses_to_chat;
pub(crate) use response::{chat_to_responses, response_echo_fields};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成一次调用的稳定 id（resp_ / msg_ / rs_ / fc_ / call_ 前缀 + 时间 + 单调计数）。
pub(crate) fn next_id(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{}_{n}", now_ts())
}

pub(crate) fn now_ts() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 透传载荷：只改写 model 名为上游物理模型，其余字段原样保留（供应商原生 Responses）。
/// 不应用 chat 专用的 params 默认值/思考翻译，避免把 chat 字段泄漏给 Responses 上游。
pub(crate) fn prepare_passthrough_payload(payload: &Value, alias: &ModelAlias) -> Value {
    let mut next = payload.clone();
    next["model"] = Value::String(alias.upstream_model().to_string());
    next
}

/// 空 usage（流式/缺省时填充，保证 SDK 总能拿到 usage 字段）。
pub(crate) fn zero_usage() -> Value {
    json!({
        "input_tokens": 0,
        "input_tokens_details": { "cached_tokens": 0 },
        "output_tokens": 0,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": 0
    })
}

/// 从 Responses 请求提取 input items（input_items 端点返回用）。
/// - input 为数组：原样保留（每条 item）；
/// - input 为字符串：归一成一条 user message item；
/// - 无 input：返回空（input_items 端点返回 not_found）。
pub(crate) fn extract_input_items(payload: &Value) -> Vec<Value> {
    match payload.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(s)) if !s.is_empty() => vec![json!({
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": s }
            ]
        })],
        _ => Vec::new(),
    }
}

/// 估算 input_tokens（POST /responses/input_tokens）。
///
/// 路由层面没有 tokenizer，这里用近似规则：先按翻译逻辑算出 chat messages，
/// 再按字符/字节估算 token 数（ASCII ~4 字符/token，非 ASCII 按 ~2 字符/token）。
/// 目的只是给客户端一个数量级参考，不是精确计价。
pub(crate) fn estimate_input_tokens(payload: &Value) -> i64 {
    // 复用翻译逻辑尽量贴近真实 token 消耗（含 instructions 与工具定义）
    let mut estimate: i64 = 0;
    if let Ok(chat) = responses_to_chat(payload) {
        if let Some(messages) = chat.get("messages").and_then(Value::as_array) {
            for msg in messages {
                estimate += estimate_value_tokens(msg);
            }
        }
        // 工具定义按 JSON 序列化长度估算
        if let Some(tools) = chat.get("tools") {
            estimate += estimate_value_tokens(tools);
        }
    } else {
        // 翻译失败（如 input_file 等）：退回对原始 payload 的整体估算
        estimate += estimate_value_tokens(payload);
    }
    estimate.max(0)
}

/// 粗略估算一个 JSON 值的 token 数：ASCII 字符按 4 字符/token，非 ASCII 按 2 字符/token。
fn estimate_value_tokens(value: &Value) -> i64 {
    let text = match value {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    };
    let mut ascii: i64 = 0;
    let mut non_ascii: i64 = 0;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii / 4 + non_ascii / 2
}

/// 把 chat completions 上游 4xx/5xx 错误体翻译成 Responses API 错误体。
pub(crate) fn upstream_error_to_responses(body: &str) -> Value {
    let (code, message) = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error").map(|e| {
                let code = e
                    .get("code")
                    .and_then(Value::as_str)
                    .or_else(|| e.get("type").and_then(Value::as_str))
                    .unwrap_or("upstream_error")
                    .to_string();
                let message = e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error")
                    .to_string();
                (code, message)
            })
        })
        .unwrap_or_else(|| {
            (
                "upstream_error".to_string(),
                body.chars().take(300).collect(),
            )
        });
    responses_error(&message, &code)
}

/// Responses API 错误体（SDK 解析用 {error:{...}} 包裹，HTTP 状态码由调用方决定）。
pub(crate) fn responses_error(message: &str, code: &str) -> Value {
    json!({
        "error": {
            "code": code,
            "message": message,
            "type": code,
            "param": Value::Null
        }
    })
}

/// 把上游 chat completions 非流式响应的 assistant 输出转成可回填多轮历史的 chat 消息
/// （供 previous_response_id 使用）。
pub(crate) fn assistant_chat_messages(content: &Value) -> Vec<Value> {
    let Some(message) = content.pointer("/choices/0/message") else {
        return Vec::new();
    };
    let mut msg = json!({ "role": "assistant" });
    if let Some(c) = message.get("content") {
        if !c.is_null() {
            msg["content"] = c.clone();
        }
    }
    if let Some(tcs) = message.get("tool_calls") {
        if let Some(arr) = tcs.as_array() {
            if !arr.is_empty() {
                msg["tool_calls"] = tcs.clone();
            }
        }
    }
    vec![msg]
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests;
