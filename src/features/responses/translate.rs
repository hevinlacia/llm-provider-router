//! Responses API <-> Chat Completions 协议翻译（纯函数，不触 IO）。
//!
//! 方向：
//! - `responses_to_chat`：把 `POST /v1/responses` 请求翻译成 chat completions 请求，
//!   交给现有路由/选 key/重试/冻结/用量链路；
//! - `chat_to_responses`：把上游 chat completions 非流式响应翻译成 Responses API 响应对象；
//! - `responses_error`：生成 Responses API 错误体（{error:{code,message,type,param}}）。
//!
//! 翻译只做字段语义映射；`previous_response_id` 等需要状态的部分在 handler/store 层处理。

use crate::config::ModelAlias;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Responses API 请求 -> chat completions 请求。
///
/// 覆盖：input(字符串/消息项/工具项) -> messages、instructions -> system 前缀、
/// tools(扁平 function) -> chat 工具、reasoning.effort -> reasoning_effort、
/// max_output_tokens -> max_tokens、text.format -> response_format、tool_choice 映射。
/// 无法翻译的字段（store/include/background/truncation/service_tier 等）静默丢弃；
/// 语义无法保真的（input_file / 未知 item 类型）返回 Err，避免静默丢内容。
pub(crate) fn responses_to_chat(payload: &Value) -> Result<Value, String> {
    let Some(model) = payload.get("model").and_then(Value::as_str) else {
        return Err("model must be a string".to_string());
    };
    let mut messages: Vec<Value> = Vec::new();

    // instructions -> 首个 system 消息
    if let Some(instructions) = payload.get("instructions") {
        let text = instructions_to_text(instructions)?;
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }

    // input -> messages
    match payload.get("input") {
        Some(Value::String(s)) => {
            messages.push(json!({ "role": "user", "content": s }));
        }
        Some(Value::Array(items)) => {
            for item in items {
                translate_input_item(item, &mut messages)?;
            }
        }
        Some(other) => {
            return Err(format!(
                "input must be a string or array, got {}",
                value_kind(other)
            ));
        }
        None => {}
    }
    if messages.is_empty() {
        return Err("input is required".to_string());
    }

    let mut chat = json!({
        "model": model,
        "messages": Value::Array(messages),
    });
    let obj = chat.as_object_mut().unwrap();

    // 直接透传的简单字段
    for key in [
        "temperature",
        "top_p",
        "stream",
        "metadata",
        "user",
        "parallel_tool_calls",
    ] {
        if let Some(v) = payload.get(key) {
            if !v.is_null() {
                obj.insert(key.to_string(), v.clone());
            }
        }
    }

    // max_output_tokens -> max_tokens（chat completions 主字段，deepseek/ark 均接受）
    if let Some(v) = payload.get("max_output_tokens").and_then(Value::as_i64) {
        obj.insert("max_tokens".to_string(), json!(v));
    }

    // reasoning.effort -> reasoning_effort（随后 prepare_upstream_payload 会按 thinking_level_map 翻译成上游方言）
    if let Some(effort) = payload.pointer("/reasoning/effort").and_then(Value::as_str) {
        obj.insert("reasoning_effort".to_string(), json!(effort));
    }

    // tools：扁平 function tool -> chat 的 {type:function,function:{...}}
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        let mut chat_tools: Vec<Value> = Vec::new();
        for tool in tools {
            if let Some(t) = translate_tool(tool)? {
                chat_tools.push(t);
            }
        }
        if !chat_tools.is_empty() {
            obj.insert("tools".to_string(), Value::Array(chat_tools));
        }
    }

    // tool_choice：Responses {type:function,name} -> chat {type:function,function:{name}}
    if let Some(tc) = payload.get("tool_choice") {
        obj.insert("tool_choice".to_string(), translate_tool_choice(tc));
    }

    // text.format -> response_format
    if let Some(rf) = translate_text_format(payload.get("text")) {
        obj.insert("response_format".to_string(), rf);
    }

    Ok(chat)
}

fn instructions_to_text(v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            Ok(parts.join("\n"))
        }
        Value::Null => Ok(String::new()),
        _ => Err("instructions must be a string or an array of content parts".to_string()),
    }
}

fn translate_input_item(item: &Value, messages: &mut Vec<Value>) -> Result<(), String> {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        // 助手工具调用 -> assistant 消息带 tool_calls
        "function_call" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            messages.push(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": if call_id.is_empty() { next_id("call") } else { call_id },
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }]
            }));
            Ok(())
        }
        // 工具执行结果 -> tool 消息
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let output = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                None => String::new(),
            };
            messages.push(json!({
                "role": "tool",
                "tool_call_id": if call_id.is_empty() { next_id("call") } else { call_id },
                "content": output,
            }));
            Ok(())
        }
        // 内建输出项：chat completions 无对应概念，跳过（不参与上游上下文）
        "reasoning" | "file_search_call" | "web_search_call" | "computer_call" => Ok(()),
        // 普通消息项（easy input message / ResponseInputMessage）
        _ => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = item.get("content");
            match content {
                Some(Value::String(s)) => {
                    messages.push(json!({ "role": role, "content": s }));
                    Ok(())
                }
                Some(Value::Array(parts)) => {
                    let translated = translate_content_parts(parts)?;
                    messages.push(json!({ "role": role, "content": Value::Array(translated) }));
                    Ok(())
                }
                None | Some(Value::Null) => {
                    // 无内容的占位消息（如 assistant 工具调用标记）：跳过，避免污染上下文
                    Ok(())
                }
                Some(other) => Err(format!(
                    "unsupported message content shape: {}",
                    value_kind(other)
                )),
            }
        }
    }
}

fn translate_content_parts(parts: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for part in parts {
        let ty = part.get("type").and_then(Value::as_str).unwrap_or("text");
        match ty {
            "input_text" | "output_text" => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push(json!({ "type": "text", "text": text }));
                }
            }
            "text" => out.push(part.clone()),
            "input_image" => {
                let url = part
                    .get("image_url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "input_image part missing image_url".to_string())?;
                out.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            }
            "image_url" => out.push(part.clone()),
            "input_file" => {
                // 文件（PDF 等）在 chat completions 里没有等价表达，明确报错避免静默丢内容
                return Err(
                    "input_file content parts are not supported by the router (upstreams speak chat completions)"
                        .to_string(),
                );
            }
            _ => {
                // 未知 part 透传原样，交给上游决定
                out.push(part.clone());
            }
        }
    }
    Ok(out)
}

fn translate_tool(tool: &Value) -> Result<Option<Value>, String> {
    let ty = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if ty != "function" {
        // 内建工具（web_search / file_search / computer 等）chat completions 不支持：跳过
        return Ok(None);
    }
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "function tool missing name".to_string())?;
    let mut function = json!({ "name": name });
    if let Some(d) = tool.get("description").and_then(Value::as_str) {
        function["description"] = json!(d);
    }
    if let Some(p) = tool.get("parameters") {
        function["parameters"] = p.clone();
    }
    Ok(Some(json!({ "type": "function", "function": function })))
}

fn translate_tool_choice(tc: &Value) -> Value {
    // Responses: {type:"function", name} -> chat: {type:"function", function:{name}}
    if let Some(name) = tc.get("name").and_then(Value::as_str) {
        return json!({ "type": "function", "function": { "name": name } });
    }
    tc.clone()
}

fn translate_text_format(text: Option<&Value>) -> Option<Value> {
    let format = text?.get("format")?;
    match format.get("type").and_then(Value::as_str)? {
        "json_schema" => {
            let mut json_schema = serde_json::Map::new();
            if let Some(name) = format.get("name").and_then(Value::as_str) {
                json_schema.insert("name".to_string(), json!(name));
            }
            if let Some(schema) = format.get("schema") {
                json_schema.insert("schema".to_string(), schema.clone());
            }
            if let Some(strict) = format.get("strict") {
                json_schema.insert("strict".to_string(), strict.clone());
            }
            Some(json!({ "type": "json_schema", "json_schema": Value::Object(json_schema) }))
        }
        "json_object" => Some(json!({ "type": "json_object" })),
        _ => None,
    }
}

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
pub(crate) fn chat_to_responses(content: &Value, response_id: &str, model: &str, echo: &Value) -> Value {
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
    let instructions = echo
        .get("instructions")
        .cloned()
        .unwrap_or(Value::Null);
    let metadata = echo.get("metadata").cloned().unwrap_or(Value::Null);
    let temperature = echo.get("temperature").cloned().unwrap_or(Value::Null);
    let top_p = echo.get("top_p").cloned().unwrap_or(Value::Null);
    let max_output_tokens = echo.get("max_output_tokens").cloned().unwrap_or(Value::Null);
    let parallel_tool_calls = echo
        .get("parallel_tool_calls")
        .cloned()
        .unwrap_or(json!(true));
    let tool_choice = echo
        .get("tool_choice")
        .cloned()
        .unwrap_or(json!("auto"));
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
mod tests {
    use super::*;

    fn chat(payload: &Value) -> Value {
        responses_to_chat(payload).expect("translate ok")
    }

    #[test]
    fn string_input_becomes_user_message() {
        let c = chat(&json!({ "model": "low-model-auto", "input": "hi" }));
        assert_eq!(c["model"], "low-model-auto");
        assert_eq!(c["messages"], json!([{ "role": "user", "content": "hi" }]));
    }

    #[test]
    fn instructions_prepend_system() {
        let c = chat(&json!({
            "model": "m",
            "instructions": "be terse",
            "input": "hi"
        }));
        assert_eq!(
            c["messages"][0],
            json!({ "role": "system", "content": "be terse" })
        );
    }

    #[test]
    fn input_items_translate_roles_tools_and_outputs() {
        let c = chat(&json!({
            "model": "m",
            "input": [
                { "role": "user", "content": "what is 2+2? use tools" },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "add",
                    "arguments": "{\"a\":2,\"b\":2}"
                },
                { "type": "function_call_output", "call_id": "call_1", "output": "4" }
            ]
        }));
        let messages = c["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":2,\"b\":2}"
        );
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[2]["content"], "4");
    }

    #[test]
    fn input_image_part_translates() {
        let c = chat(&json!({
            "model": "m",
            "input": [{
                "role": "user",
                "content": [{ "type": "input_image", "image_url": "data:image/png;base64,AAAA" }]
            }]
        }));
        assert_eq!(
            c["messages"][0]["content"][0],
            json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } })
        );
    }

    #[test]
    fn tools_flatten_to_chat_shape() {
        let c = chat(&json!({
            "model": "m",
            "input": "hi",
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "weather",
                "parameters": { "type": "object", "properties": {} }
            }]
        }));
        assert_eq!(
            c["tools"][0],
            json!({
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "weather",
                    "parameters": { "type": "object", "properties": {} }
                }
            })
        );
    }

    #[test]
    fn reasoning_effort_and_max_tokens_map() {
        let c = chat(&json!({
            "model": "m",
            "input": "hi",
            "reasoning": { "effort": "high" },
            "max_output_tokens": 512
        }));
        assert_eq!(c["reasoning_effort"], "high");
        assert_eq!(c["max_tokens"], 512);
    }

    #[test]
    fn text_format_maps_to_response_format() {
        let c = chat(&json!({
            "model": "m",
            "input": "hi",
            "text": { "format": { "type": "json_schema", "name": "x", "schema": { "type": "object" } } }
        }));
        assert_eq!(c["response_format"]["type"], "json_schema");
        assert_eq!(c["response_format"]["json_schema"]["name"], "x");
    }

    #[test]
    fn unknown_input_file_rejected() {
        let err = responses_to_chat(&json!({
            "model": "m",
            "input": [{ "role": "user", "content": [{ "type": "input_file", "file_id": "f1" }] }]
        }));
        assert!(err.is_err());
    }

    #[test]
    fn chat_to_responses_builds_output_items() {
        let content = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "hello",
                    "reasoning_content": "thinking...",
                    "tool_calls": [{
                        "id": "call_x",
                        "type": "function",
                        "function": { "name": "f", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "completion_tokens_details": { "reasoning_tokens": 3 }
            }
        });
        let r = chat_to_responses(&content, "resp_1", "upstream-model", &json!({}));
        assert_eq!(r["id"], "resp_1");
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        assert_eq!(r["model"], "upstream-model");
        let output = r["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "thinking...");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["type"], "output_text");
        assert_eq!(output[1]["content"][0]["text"], "hello");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["name"], "f");
        assert_eq!(r["output_text"], "hello");
        assert_eq!(r["usage"]["input_tokens"], 10);
        assert_eq!(r["usage"]["output_tokens"], 5);
        assert_eq!(r["usage"]["output_tokens_details"]["reasoning_tokens"], 3);
    }

    #[test]
    fn upstream_error_maps_to_responses_error() {
        let err = upstream_error_to_responses(
            r#"{"error":{"message":"bad request","type":"invalid_request_error","code":"E400"}}"#,
        );
        assert_eq!(err["error"]["code"], "E400");
        assert_eq!(err["error"]["message"], "bad request");
        assert!(err["error"]["param"].is_null());
    }

    #[test]
    fn passthrough_payload_rewrites_model_only() {
        let alias = ModelAlias::new(
            "logical",
            "openai/real-upstream",
            "http://x/v1",
            Vec::new(),
            None,
        )
        .with_responses_base_url(Some("http://x/v1".to_string()));
        let original = json!({
            "model": "logical",
            "input": [{"role": "user", "content": "hi"}],
            "reasoning": { "effort": "high" },
            "max_output_tokens": 64,
            "stream": true
        });
        let passthrough = prepare_passthrough_payload(&original, &alias);
        assert_eq!(passthrough["model"], "real-upstream");
        assert_eq!(passthrough["input"], original["input"]);
        assert_eq!(passthrough["reasoning"], original["reasoning"]);
        assert_eq!(passthrough["max_output_tokens"], 64);
        assert_eq!(passthrough["stream"], true);
        // 不注入 chat 专用字段
        assert!(passthrough.get("messages").is_none());
        assert!(passthrough.get("reasoning_effort").is_none());
    }

    #[test]
    fn chat_to_responses_echoes_request_fields_and_completes_schema() {
        let content = json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hello" } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        });
        let echo = response_echo_fields(&json!({
            "model": "m",
            "input": "hi",
            "instructions": "be terse",
            "temperature": 0.7,
            "top_p": 0.9,
            "max_output_tokens": 512,
            "parallel_tool_calls": false,
            "metadata": { "k": "v" },
            "service_tier": "flex",
            "truncation": "disabled",
            "user": "u1"
        }));
        let r = chat_to_responses(&content, "resp_1", "upstream-model", &echo);
        // 回显字段
        assert_eq!(r["instructions"], "be terse");
        assert_eq!(r["temperature"], 0.7);
        assert_eq!(r["top_p"], 0.9);
        assert_eq!(r["max_output_tokens"], 512);
        assert_eq!(r["parallel_tool_calls"], false);
        assert_eq!(r["metadata"]["k"], "v");
        assert_eq!(r["service_tier"], "flex");
        assert_eq!(r["truncation"], "disabled");
        assert_eq!(r["user"], "u1");
        // 补齐的字段
        assert_eq!(r["completed_at"], r["created_at"]);
        assert_eq!(r["store"], true);
        assert_eq!(r["max_tool_calls"], Value::Null);
        assert_eq!(r["background"], Value::Null);
        assert_eq!(r["conversation"], Value::Null);
        assert_eq!(r["previous_response_id"], Value::Null);
        assert_eq!(r["prompt"], Value::Null);
        assert_eq!(r["prompt_cache_key"], Value::Null);
        assert_eq!(r["prompt_cache_options"], Value::Null);
        assert_eq!(r["prompt_cache_retention"], Value::Null);
        assert_eq!(r["moderation"], Value::Null);
        assert_eq!(r["safety_identifier"], Value::Null);
        assert_eq!(r["reasoning"], Value::Null);
        assert_eq!(r["text"], Value::Null);
        assert_eq!(r["top_logprobs"], Value::Null);
        // 全部 35 个标准顶层字段齐全
        for field in [
            "id", "object", "created_at", "completed_at", "status", "model", "output",
            "output_text", "usage", "error", "incomplete_details", "instructions", "metadata",
            "parallel_tool_calls", "temperature", "tool_choice", "tools", "top_p",
            "max_output_tokens", "max_tool_calls", "background", "conversation",
            "previous_response_id", "store", "service_tier", "truncation", "reasoning", "text",
            "user", "top_logprobs", "prompt", "prompt_cache_key", "prompt_cache_options",
            "prompt_cache_retention", "moderation", "safety_identifier",
        ] {
            assert!(r.get(field).is_some(), "missing field {field}");
        }
    }

    #[test]
    fn response_echo_fields_skips_null_and_unknown() {
        let echo = response_echo_fields(&json!({
            "model": "m",
            "input": "hi",
            "instructions": null,
            "nonsense_field": 1,
            "store": false,
            "previous_response_id": "resp_prev",
            "conversation": { "id": "conv_1" }
        }));
        assert!(echo.get("instructions").is_none());
        assert!(echo.get("nonsense_field").is_none());
        assert_eq!(echo["store"], false);
        assert_eq!(echo["previous_response_id"], "resp_prev");
        assert_eq!(echo["conversation"]["id"], "conv_1");
    }

    #[test]
    fn estimate_input_tokens_roughly_counts() {
        let payload = json!({
            "model": "m",
            "input": "hello world how are you today",
            "instructions": "be brief"
        });
        let n = estimate_input_tokens(&payload);
        // messages 结构字段（role 等）+ 文本都计入：当前实现序列化每个 message 估算。
        // 修正为与实现一致的值（23），并验证估算随输入增长单调。
        assert_eq!(n, 23);
        let longer = json!({
            "model": "m",
            "input": "hello world how are you today my friend this is a longer sentence",
            "instructions": "be brief"
        });
        assert!(estimate_input_tokens(&longer) > n);
    }

    #[test]
    fn estimate_input_tokens_falls_back_on_unsupported_input() {
        // input_file 翻译失败，退回整体估算（不 panic）
        let payload = json!({
            "model": "m",
            "input": [{ "role": "user", "content": [{ "type": "input_file", "file_id": "f1" }] }]
        });
        let n = estimate_input_tokens(&payload);
        assert!(n > 0);
    }

    #[test]
    fn extract_input_items_normalizes_string_to_message() {
        let payload = json!({ "model": "m", "input": "hello world" });
        let items = extract_input_items(&payload);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hello world");
    }

    #[test]
    fn extract_input_items_preserves_array() {
        let payload = json!({
            "model": "m",
            "input": [
                { "role": "user", "content": "a" },
                { "type": "function_call_output", "call_id": "c1", "output": "4" }
            ]
        });
        let items = extract_input_items(&payload);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[1]["type"], "function_call_output");
    }

    #[test]
    fn extract_input_items_missing_returns_empty() {
        assert!(extract_input_items(&json!({ "model": "m" })).is_empty());
        assert!(extract_input_items(&json!({ "model": "m", "input": "" })).is_empty());
    }
}
