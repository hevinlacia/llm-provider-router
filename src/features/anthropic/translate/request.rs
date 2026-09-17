//! Anthropic Messages 请求 -> Responses API 请求翻译。

use serde_json::{json, Value};

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
