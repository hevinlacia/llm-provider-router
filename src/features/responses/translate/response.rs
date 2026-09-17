//! Chat Completions 响应 -> Responses API 响应对象翻译。

use serde_json::{json, Value};

use super::{next_id, now_ts};

/// 从 Responses 请求 payload 提取回显到响应对象的字段（供非流式/流式共用）。
/// 透传时这些字段原样保留在上游响应里；翻译模式下响应对象需要回显请求参数，
/// 保证 SDK 严格校验 / 客户端读取请求配置时不缺字段。
pub(crate) fn response_echo_fields(payload: &Value) -> Value {
    let mut echo = serde_json::Map::new();
    // 简单字段：存在且非 null 才回显
    for key in [
        "instructions",
        "metadata",
        "temperature",
        "top_p",
        "max_output_tokens",
        "parallel_tool_calls",
        "service_tier",
        "truncation",
        "reasoning",
        "text",
        "tool_choice",
        "tools",
        "user",
        "store",
        "background",
        "conversation",
        "previous_response_id",
        "max_tool_calls",
        "top_logprobs",
    ] {
        if let Some(v) = payload.get(key) {
            if !v.is_null() {
                echo.insert(key.to_string(), v.clone());
            }
        }
    }
    Value::Object(echo)
}

/// 上游 chat completions 非流式响应 -> Responses API 响应对象。
/// `echo` 为从请求回显的字段（response_echo_fields 产物），可传 json!(null) 表示不回显。
pub(crate) fn chat_to_responses(
    content: &Value,
    response_id: &str,
    model: &str,
    echo: &Value,
) -> Value {
    let created_at = now_ts();
    let mut output: Vec<Value> = Vec::new();
    let mut output_text = String::new();

    let message = content
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // deepseek 推理内容 -> reasoning 项（放在 message 之前，与输出顺序一致）
    if let Some(rc) = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        output.push(json!({
            "id": next_id("rs"),
            "type": "reasoning",
            "status": "completed",
            "summary": [{ "type": "summary_text", "text": rc }]
        }));
    }

    // 正文 -> message 项
    let mut content_parts: Vec<Value> = Vec::new();
    match message.get("content") {
        Some(Value::String(s)) => {
            output_text.push_str(s);
            content_parts.push(json!({ "type": "output_text", "text": s, "annotations": [] }));
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if part.get("type").and_then(Value::as_str).unwrap_or("text") == "text" {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        output_text.push_str(t);
                        content_parts
                            .push(json!({ "type": "output_text", "text": t, "annotations": [] }));
                    }
                }
            }
        }
        _ => {}
    }
    if !content_parts.is_empty() {
        output.push(json!({
            "id": next_id("msg"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": Value::Array(content_parts)
        }));
    }

    // 工具调用 -> function_call 项
    if let Some(tcs) = message.get("tool_calls").and_then(Value::as_array) {
        for tc in tcs {
            let call_id = tc.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = tc
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = tc
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            output.push(json!({
                "id": next_id("fc"),
                "type": "function_call",
                "status": "completed",
                "call_id": if call_id.is_empty() { next_id("call") } else { call_id.to_string() },
                "name": name,
                "arguments": arguments
            }));
        }
    }

    let usage = translate_usage(content);
    let echo = if echo.is_null() { &json!({}) } else { echo };

    // 从回显字段取值（缺失/无回显时用标准默认值），保证 35 个顶层字段齐全
    let instructions = echo.get("instructions").cloned().unwrap_or(Value::Null);
    let metadata = echo.get("metadata").cloned().unwrap_or(Value::Null);
    let temperature = echo.get("temperature").cloned().unwrap_or(Value::Null);
    let top_p = echo.get("top_p").cloned().unwrap_or(Value::Null);
    let max_output_tokens = echo
        .get("max_output_tokens")
        .cloned()
        .unwrap_or(Value::Null);
    let parallel_tool_calls = echo
        .get("parallel_tool_calls")
        .cloned()
        .unwrap_or(json!(true));
    let tool_choice = echo.get("tool_choice").cloned().unwrap_or(json!("auto"));
    let tools = echo.get("tools").cloned().unwrap_or(json!([]));
    let service_tier = echo.get("service_tier").cloned().unwrap_or(Value::Null);
    let truncation = echo.get("truncation").cloned().unwrap_or(Value::Null);
    let reasoning = echo.get("reasoning").cloned().unwrap_or(Value::Null);
    let text = echo.get("text").cloned().unwrap_or(Value::Null);
    let background = echo.get("background").cloned().unwrap_or(Value::Null);
    let conversation = echo.get("conversation").cloned().unwrap_or(Value::Null);
    let max_tool_calls = echo.get("max_tool_calls").cloned().unwrap_or(Value::Null);
    let top_logprobs = echo.get("top_logprobs").cloned().unwrap_or(Value::Null);
    let user = echo.get("user").cloned().unwrap_or(Value::Null);
    let store = echo.get("store").cloned().unwrap_or(json!(true));
    let previous_response_id = echo
        .get("previous_response_id")
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "completed_at": created_at,
        "status": "completed",
        "model": model,
        "output": Value::Array(output),
        "output_text": output_text,
        "usage": usage,
        "error": Value::Null,
        "incomplete_details": Value::Null,
        "instructions": instructions,
        "metadata": metadata,
        "parallel_tool_calls": parallel_tool_calls,
        "temperature": temperature,
        "tool_choice": tool_choice,
        "tools": tools,
        "top_p": top_p,
        "max_output_tokens": max_output_tokens,
        "max_tool_calls": max_tool_calls,
        "background": background,
        "conversation": conversation,
        "previous_response_id": previous_response_id,
        "store": store,
        "service_tier": service_tier,
        "truncation": truncation,
        "reasoning": reasoning,
        "text": text,
        "user": user,
        "top_logprobs": top_logprobs,
        "prompt": Value::Null,
        "prompt_cache_key": Value::Null,
        "prompt_cache_options": Value::Null,
        "prompt_cache_retention": Value::Null,
        "moderation": Value::Null,
        "safety_identifier": Value::Null
    })
}

/// 把上游 chat usage 映射成 Responses usage 形状（含 cached/reasoning 细分，尽力而为）。
pub(crate) fn translate_usage(content: &Value) -> Value {
    let input = content
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = content
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total = content
        .pointer("/usage/total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(input + output);
    let cached = content
        .pointer("/usage/cached_tokens")
        .and_then(Value::as_i64)
        .or_else(|| {
            content
                .pointer("/usage/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    let reasoning = content
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({
        "input_tokens": input,
        "input_tokens_details": { "cached_tokens": cached },
        "output_tokens": output,
        "output_tokens_details": { "reasoning_tokens": reasoning },
        "total_tokens": total
    })
}
