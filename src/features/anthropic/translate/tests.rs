//! Anthropic 协议翻译单元测试。

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
