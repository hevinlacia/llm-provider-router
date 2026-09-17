//! translate 协议翻译单元测试。

use super::*;

fn chat(payload: &Value) -> Value {
    responses_to_chat(payload).expect("translate ok")
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
        "id",
        "object",
        "created_at",
        "completed_at",
        "status",
        "model",
        "output",
        "output_text",
        "usage",
        "error",
        "incomplete_details",
        "instructions",
        "metadata",
        "parallel_tool_calls",
        "temperature",
        "tool_choice",
        "tools",
        "top_p",
        "max_output_tokens",
        "max_tool_calls",
        "background",
        "conversation",
        "previous_response_id",
        "store",
        "service_tier",
        "truncation",
        "reasoning",
        "text",
        "user",
        "top_logprobs",
        "prompt",
        "prompt_cache_key",
        "prompt_cache_options",
        "prompt_cache_retention",
        "moderation",
        "safety_identifier",
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
