//! Anthropic Messages <-> Responses 纯函数翻译（便于单测）。

use serde_json::{json, Map, Value};

use super::{error_body, map_error_type};

// ---------------------------------------------------------------------------
// 请求：Anthropic Messages -> Responses
// ---------------------------------------------------------------------------

/// Anthropic `/v1/messages` 请求 -> Responses API 请求。
/// 翻译结果交给现有 `/v1/responses` 机制（透传或翻译成 chat completions）。
pub(crate) fn messages_to_responses(payload: &Value) -> Result<Value, String> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "model must be a string".to_string())?;

    let instructions = translate_system(payload.get("system"))?;
    let input = translate_messages(payload.get("messages"))?;

    let mut out = json!({
        "model": model,
        "input": input,
    });
    let obj = out.as_object_mut().expect("out is object");
    if let Some(text) = instructions {
        obj.insert("instructions".to_string(), json!(text));
    }
    if let Some(v) = payload.get("max_tokens") {
        if !v.is_null() {
            obj.insert("max_output_tokens".to_string(), v.clone());
        }
    }
    // 简单字段直传
    for key in ["stream", "temperature", "top_p", "metadata", "user"] {
        if let Some(v) = payload.get(key) {
            if !v.is_null() {
                obj.insert(key.to_string(), v.clone());
            }
        }
    }
    // tools: {name, description, input_schema} -> {type:function, name, description, parameters}
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Value::as_str)?;
                Some(json!({
                    "type": "function",
                    "name": name,
                    "description": tool.get("description").cloned().unwrap_or(json!("")),
                    "parameters": tool
                        .get("input_schema")
                        .cloned()
                        .unwrap_or(json!({ "type": "object" })),
                }))
            })
            .collect();
        if !mapped.is_empty() {
            obj.insert("tools".to_string(), Value::Array(mapped));
        }
    }
    // tool_choice: auto/any/tool -> auto/required/{type:function,name}
    if let Some(choice) = payload.get("tool_choice") {
        let mapped = match choice.get("type").and_then(Value::as_str) {
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            Some("tool") => json!({
                "type": "function",
                "name": choice.get("name").cloned().unwrap_or(Value::Null),
            }),
            _ => json!("auto"),
        };
        obj.insert("tool_choice".to_string(), mapped);
    }
    // thinking.budget_tokens -> reasoning.effort（粗粒度映射）
    if let Some(effort) = thinking_effort(payload.get("thinking")) {
        obj.insert("reasoning".to_string(), json!({ "effort": effort }));
    }
    Ok(out)
}

/// `system`：string 或 [{type:text,text}] -> instructions 文本。
fn translate_system(system: Option<&Value>) -> Result<Option<String>, String> {
    match system {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
                // 其它 system block 类型（cache_control 等装饰字段）忽略
            }
            Ok(Some(text))
        }
        Some(other) => Err(format!(
            "system must be a string or an array of text blocks, got {other}"
        )),
    }
}

/// `thinking`: {type: enabled, budget_tokens} -> reasoning effort 档位。
fn thinking_effort(thinking: Option<&Value>) -> Option<&'static str> {
    let thinking = thinking?;
    if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
        return None;
    }
    let budget = thinking
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(if budget >= 32_000 {
        "high"
    } else if budget >= 8_000 {
        "medium"
    } else {
        "low"
    })
}

/// `messages` -> Responses `input` 项数组。
fn translate_messages(messages: Option<&Value>) -> Result<Vec<Value>, String> {
    let Some(items) = messages.and_then(Value::as_array) else {
        return Err("messages must be an array".to_string());
    };
    let mut input: Vec<Value> = Vec::new();
    for message in items {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_string();
        let text_part_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        match message.get("content") {
            Some(Value::String(s)) => {
                input.push(json!({
                    "role": role,
                    "content": [{ "type": text_part_type, "text": s }],
                }));
            }
            Some(Value::Array(blocks)) => {
                // 文本块累积成一条 message 项；tool_use / tool_result 拆成独立项
                let mut text_parts: Vec<Value> = Vec::new();
                for block in blocks {
                    let ty = block.get("type").and_then(Value::as_str).unwrap_or("text");
                    match ty {
                        "text" => {
                            if let Some(text) = block.get("text") {
                                text_parts.push(json!({ "type": text_part_type, "text": text }));
                            }
                        }
                        "image" => {
                            // 图片与文本同属一条消息的 content parts，不拆分消息项
                            let source = block
                                .get("source")
                                .ok_or_else(|| "image block missing source".to_string())?;
                            if source.get("type").and_then(Value::as_str) != Some("base64") {
                                return Err(
                                    "only base64 image sources are supported by the router"
                                        .to_string(),
                                );
                            }
                            let media_type = source
                                .get("media_type")
                                .and_then(Value::as_str)
                                .unwrap_or("image/png");
                            let data = source
                                .get("data")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "image source missing data".to_string())?;
                            text_parts.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media_type};base64,{data}"),
                            }));
                        }
                        "tool_use" => {
                            flush_message(&mut input, &role, &mut text_parts);
                            let call_id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let args = serde_json::to_string(
                                &block.get("input").cloned().unwrap_or(json!({})),
                            )
                            .unwrap_or_else(|_| "{}".to_string());
                            input.push(json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": args,
                            }));
                        }
                        "tool_result" => {
                            flush_message(&mut input, &role, &mut text_parts);
                            let call_id = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let output = match block.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                                None => String::new(),
                            };
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": output,
                            }));
                        }
                        // 思考块丢弃：上游推理由上游自行生成，签名也无法回传校验
                        "thinking" | "redacted_thinking" => {}
                        "document" => {
                            return Err(
                                "document blocks are not supported by the router (upstreams speak chat completions)"
                                    .to_string(),
                            );
                        }
                        _ => {}
                    }
                }
                flush_message(&mut input, &role, &mut text_parts);
            }
            None | Some(Value::Null) => {}
            Some(other) => {
                return Err(format!(
                    "message content must be a string or an array of blocks, got {other}"
                ))
            }
        }
    }
    if input.is_empty() {
        return Err("messages is required".to_string());
    }
    Ok(input)
}

/// 把累积的文本块作为一条 message 项写入 input（无文本则跳过）。
fn flush_message(input: &mut Vec<Value>, role: &str, text_parts: &mut Vec<Value>) {
    if text_parts.is_empty() {
        return;
    }
    let parts = std::mem::take(text_parts);
    input.push(json!({ "role": role, "content": parts }));
}

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

// ---------------------------------------------------------------------------
// 流式：Responses SSE 事件 -> Anthropic SSE 事件（逐事件映射的纯函数部分）
// ---------------------------------------------------------------------------

/// 单个 Responses SSE 事件 -> 0..n 个 Anthropic SSE 事件（`event:` + `data:` 文本）。
/// 状态（块 index 映射等）由 [`super::stream::SseTranslator`] 维护。
pub(crate) fn translate_stream_event(event: &Value, state: &mut StreamEventState) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let typ = event.get("type").and_then(Value::as_str).unwrap_or("");
    // responses stream 的错误事件：{"code":..., "message":..., "param":...}（无 type 字段）
    if typ.is_empty() {
        if let Some(message) = event.get("message").and_then(Value::as_str) {
            out.push(anthropic_error_event(message));
        }
        return out;
    }
    match typ {
        "response.created" => {
            let id = event
                .pointer("/response/id")
                .and_then(Value::as_str)
                .unwrap_or("msg_unknown")
                .to_string();
            let input_tokens = event
                .pointer("/response/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            state.message_id = id;
            state.started = true;
            out.push(sse_event(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": state.message_id,
                        "type": "message",
                        "role": "assistant",
                        "model": state.model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {
                            "input_tokens": input_tokens,
                            "output_tokens": 1,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                        }
                    }
                }),
            ));
        }
        "response.output_item.added" => {
            let item = event.get("item").cloned().unwrap_or(json!({}));
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let index = state.next_index;
            let block = match item_type {
                "message" => json!({ "type": "text", "text": "" }),
                "reasoning" => json!({ "type": "thinking", "thinking": "", "signature": "" }),
                "function_call" => {
                    state.has_tool_use = true;
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("id").and_then(Value::as_str))
                        .unwrap_or_default();
                    json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "input": {},
                    })
                }
                _ => return out,
            };
            state.next_index += 1;
            state.item_index.insert(item_id, index);
            state.open_blocks.insert(index);
            out.push(sse_event(
                "content_block_start",
                json!({ "type": "content_block_start", "index": index, "content_block": block }),
            ));
        }
        "response.output_text.delta" => {
            let Some((index, pending)) = state.block_index(event, "text") else {
                return out;
            };
            if let Some(start) = pending {
                out.push(start);
            }
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": delta },
                    }),
                ));
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let Some((index, pending)) = state.block_index(event, "thinking") else {
                return out;
            };
            if let Some(start) = pending {
                out.push(start);
            }
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "thinking_delta", "thinking": delta },
                    }),
                ));
            }
        }
        "response.function_call_arguments.delta" => {
            let Some((index, pending)) = state.block_index(event, "tool_use") else {
                return out;
            };
            if let Some(start) = pending {
                out.push(start);
            }
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": delta },
                    }),
                ));
            }
        }
        "response.output_item.done" => {
            let item_id = event
                .pointer("/item/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(index) = state.item_index.remove(&item_id) {
                state.open_blocks.remove(&index);
                out.push(sse_event(
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": index }),
                ));
            }
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            // 兜底关闭仍未 stop 的块（上游异常结束时可能缺 output_item.done）
            let mut open: Vec<usize> = state.open_blocks.iter().copied().collect();
            open.sort_unstable();
            for index in open {
                out.push(sse_event(
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": index }),
                ));
            }
            state.open_blocks.clear();
            if !state.message_stopped {
                state.message_stopped = true;
                let response = event.get("response").cloned().unwrap_or(json!({}));
                let stop_reason = if typ == "response.incomplete" {
                    "max_tokens"
                } else if typ == "response.failed" {
                    "end_turn"
                } else if state.has_tool_use {
                    "tool_use"
                } else {
                    "end_turn"
                };
                let output_tokens = response
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push(sse_event(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                        "usage": { "output_tokens": output_tokens },
                    }),
                ));
                out.push(sse_event("message_stop", json!({ "type": "message_stop" })));
            }
        }
        _ => {}
    }
    out
}

/// 流式翻译状态：Anthropic 块 index 分配与生命周期。
#[derive(Debug, Default)]
pub(crate) struct StreamEventState {
    pub(crate) message_id: String,
    pub(crate) model: String,
    pub(crate) started: bool,
    pub(crate) message_stopped: bool,
    pub(crate) next_index: usize,
    /// Responses item id -> Anthropic block index
    pub(crate) item_index: std::collections::HashMap<String, usize>,
    /// 已 start 未 stop 的块 index
    pub(crate) open_blocks: std::collections::HashSet<usize>,
    pub(crate) has_tool_use: bool,
}

impl StreamEventState {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            ..Default::default()
        }
    }

    /// 按 delta 事件定位块 index；块未 start 时惰性补建并返回待发的事件文本。
    fn block_index(&mut self, event: &Value, block_type: &str) -> Option<(usize, Option<String>)> {
        let item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(index) = self.item_index.get(&item_id) {
            return Some((*index, None));
        }
        // 惰性补建：item.added 事件缺失时按 delta 隐式开块，start 事件随 delta 一起补发
        let index = self.next_index;
        self.next_index += 1;
        self.item_index.insert(item_id.clone(), index);
        self.open_blocks.insert(index);
        let block = match block_type {
            "text" => json!({ "type": "text", "text": "" }),
            "thinking" => json!({ "type": "thinking", "thinking": "", "signature": "" }),
            _ => json!({ "type": "tool_use", "id": item_id, "name": "", "input": {} }),
        };
        let start = sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": block,
            }),
        );
        Some((index, Some(start)))
    }
}

fn sse_event(event: &str, data: Value) -> String {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string())
    )
}

fn anthropic_error_event(message: &str) -> String {
    sse_event("error", error_body(message, "api_error"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_string_content_and_system() {
        let payload = json!({
            "model": "low-model-auto",
            "max_tokens": 1024,
            "system": "be terse",
            "messages": [{ "role": "user", "content": "hi" }],
        });
        let out = messages_to_responses(&payload).unwrap();
        assert_eq!(out["model"], "low-model-auto");
        assert_eq!(out["max_output_tokens"], 1024);
        assert_eq!(out["instructions"], "be terse");
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(out["input"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn request_tool_round_trip() {
        let payload = json!({
            "model": "m",
            "max_tokens": 100,
            "tools": [{ "name": "get_weather", "description": "w", "input_schema": { "type": "object" } }],
            "tool_choice": { "type": "any" },
            "messages": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "checking" },
                    { "type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": { "city": "sh" } },
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny" },
                ]},
            ],
        });
        let out = messages_to_responses(&payload).unwrap();
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["parameters"]["type"], "object");
        assert_eq!(out["tool_choice"], "required");
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "user");
        // assistant 文本 + tool_use 拆成两项
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "toolu_1");
        assert_eq!(input[2]["name"], "get_weather");
        let args: Value = serde_json::from_str(input[2]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "sh");
        // tool_result -> function_call_output
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "toolu_1");
        assert_eq!(input[3]["output"], "sunny");
    }

    #[test]
    fn request_image_and_thinking() {
        let payload = json!({
            "model": "m",
            "max_tokens": 100,
            "thinking": { "type": "enabled", "budget_tokens": 40_000 },
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "look" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
                { "type": "thinking", "thinking": "old thought", "signature": "x" },
            ]}],
        });
        let out = messages_to_responses(&payload).unwrap();
        assert_eq!(out["reasoning"]["effort"], "high");
        let content = out["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,AAAA");
        assert_eq!(content.len(), 2, "thinking blocks dropped");
    }

    #[test]
    fn response_full_mapping() {
        let resp = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "hmm" }] },
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "hello" }] },
                { "type": "function_call", "call_id": "call_9", "name": "f", "arguments": "{\"a\":1}" },
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "input_tokens_details": { "cached_tokens": 4 },
            }
        });
        let out = responses_to_messages(&resp, "my-model");
        assert_eq!(out["id"], "resp_1");
        assert_eq!(out["type"], "message");
        assert_eq!(out["model"], "my-model");
        assert_eq!(out["stop_reason"], "tool_use");
        let content = out["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "");
        assert_eq!(content[1], json!({ "type": "text", "text": "hello" }));
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(content[2]["id"], "call_9");
        assert_eq!(content[2]["input"], json!({ "a": 1 }));
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 20);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 4);
    }

    #[test]
    fn response_incomplete_maps_max_tokens() {
        let resp = json!({
            "id": "resp_2",
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "partial" }] }],
        });
        let out = responses_to_messages(&resp, "m");
        assert_eq!(out["stop_reason"], "max_tokens");
    }

    #[test]
    fn error_mapping() {
        let body = json!({ "error": { "message": "boom", "type": "rate_limit_error" } });
        let out = responses_error_to_anthropic(&body);
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "rate_limit_error");
        assert_eq!(out["error"]["message"], "boom");
    }

    #[test]
    fn usage_normalization() {
        let usage = json!({ "input_tokens": 7, "output_tokens": 3 });
        assert_eq!(
            super::super::normalize_usage(&usage),
            json!({ "prompt_tokens": 7, "completion_tokens": 3 })
        );
    }

    #[test]
    fn stream_events_translate() {
        let mut state = StreamEventState::new("m");
        let mut out = String::new();
        for event in [
            json!({ "type": "response.created", "response": { "id": "resp_x" } }),
            json!({ "type": "response.output_item.added", "item": { "type": "message", "id": "item_1" } }),
            json!({ "type": "response.output_text.delta", "item_id": "item_1", "delta": "he" }),
            json!({ "type": "response.output_text.delta", "item_id": "item_1", "delta": "y" }),
            json!({ "type": "response.output_item.done", "item": { "id": "item_1" } }),
            json!({ "type": "response.completed", "response": { "id": "resp_x", "status": "completed", "usage": { "output_tokens": 5 } } }),
        ] {
            out.push_str(&translate_stream_event(&event, &mut state).concat());
        }
        assert!(out.contains("event: message_start"));
        assert!(out.contains("\"id\":\"resp_x\""));
        assert!(out.contains("event: content_block_start"));
        assert!(out.contains("text_delta") && out.contains("\"text\":\"he\""));
        assert!(out.contains("\"text\":\"y\""));
        assert!(out.contains("event: content_block_stop"));
        assert!(out.contains("\"stop_reason\":\"end_turn\""));
        assert!(out.contains("\"output_tokens\":5"));
        assert!(out.contains("event: message_stop"));
    }

    #[test]
    fn stream_tool_call_and_error_events() {
        let mut state = StreamEventState::new("m");
        let mut out = String::new();
        out.push_str(
            &translate_stream_event(
                &json!({ "type": "response.created", "response": { "id": "r" } }),
                &mut state,
            )
            .concat(),
        );
        out.push_str(
            &translate_stream_event(
                &json!({ "type": "response.output_item.added", "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "f" } }),
                &mut state,
            )
            .concat(),
        );
        out.push_str(
            &translate_stream_event(
                &json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"a\":" }),
                &mut state,
            )
            .concat(),
        );
        out.push_str(
            &translate_stream_event(
                &json!({ "type": "response.completed", "response": { "status": "completed" } }),
                &mut state,
            )
            .concat(),
        );
        assert!(out.contains("\"type\":\"tool_use\""));
        assert!(out.contains("input_json_delta") && out.contains("partial_json"));
        assert!(out.contains("\"stop_reason\":\"tool_use\""));

        // 无 type 的错误事件 -> anthropic error 事件
        let mut state2 = StreamEventState::new("m");
        let err_out = translate_stream_event(
            &json!({ "code": "upstream_error", "message": "boom", "param": Value::Null }),
            &mut state2,
        );
        assert!(err_out[0].contains("event: error"));
        assert!(err_out[0].contains("boom"));
    }
}
