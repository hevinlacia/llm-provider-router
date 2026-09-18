//! Responses API 请求 -> Chat Completions 请求翻译。

use serde_json::{json, Value};

use super::{next_id, value_kind};

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
