//! `stream/mod.rs` 流式 SSE 翻译状态机的单元测试。

use super::*;

/// 收集所有 SSE 事件的 type。
fn event_types(sse: &str) -> Vec<String> {
    sse.lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .map(|s| s.to_string())
        .collect()
}

fn last_event_json(sse: &str, wanted: &str) -> Value {
    let mut found = json!(null);
    let mut current_event = String::new();
    let mut current_data = String::new();
    for line in sse.lines() {
        if let Some(ev) = line.strip_prefix("event: ") {
            current_event = ev.to_string();
            current_data.clear();
        } else if let Some(data) = line.strip_prefix("data: ") {
            current_data.push_str(data);
            if current_event == wanted {
                if let Ok(v) = serde_json::from_str::<Value>(&current_data) {
                    found = v;
                }
            }
        }
    }
    found
}

#[test]
fn plain_text_stream_emits_expected_events() {
    let mut sse = ResponsesSse::with_echo("test-model", &json!({}));
    let mut collected = String::new();
    collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" }, "finish_reason": null }]
        })));
    collected.push_str(&sse.feed(&json!({
        "choices": [{ "index": 0, "delta": { "content": "Hel" }, "finish_reason": null }]
    })));
    collected.push_str(&sse.feed(&json!({
        "choices": [{ "index": 0, "delta": { "content": "lo" }, "finish_reason": null }]
    })));
    collected.push_str(&sse.feed(&json!({
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    })));
    let (finish_sse, (resp_id, history, _response)) = sse.finish();
    collected.push_str(&finish_sse);

    let types = event_types(&collected);
    assert!(types.contains(&"response.created".to_string()));
    assert!(types.contains(&"response.output_item.added".to_string()));
    assert!(types.contains(&"response.content_part.added".to_string()));
    assert_eq!(
        types
            .iter()
            .filter(|t| t.as_str() == "response.output_text.delta")
            .count(),
        2
    );
    assert!(types.contains(&"response.output_text.done".to_string()));
    assert!(types.contains(&"response.content_part.done".to_string()));
    assert!(types.contains(&"response.output_item.done".to_string()));
    assert!(types.contains(&"response.completed".to_string()));

    let completed = last_event_json(&collected, "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    let msg = &completed["response"]["output"][0];
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["content"][0]["text"], "Hello");
    assert_eq!(completed["response"]["output_text"], "Hello");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 0);

    assert!(!resp_id.is_empty());
    assert_eq!(history[0]["role"], "assistant");
    assert_eq!(history[0]["content"], "Hello");
}

#[test]
fn reasoning_and_tool_call_stream() {
    let mut sse = ResponsesSse::with_echo("test-model", &json!({}));
    let mut collected = String::new();
    // 推理内容 + 工具调用首帧（带 id/name）
    collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "role": "assistant", "reasoning_content": "think" }, "finish_reason": null }]
        })));
    collected.push_str(&sse.feed(&json!({
        "choices": [{ "index": 0, "delta": { "reasoning_content": "ing" }, "finish_reason": null }]
    })));
    collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": {
                "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "f", "arguments": "{\"a\":" } }]
            }, "finish_reason": null }]
        })));
    collected.push_str(&sse.feed(&json!({
        "choices": [{ "index": 0, "delta": {
            "tool_calls": [{ "index": 0, "function": { "arguments": "1}" } }]
        }, "finish_reason": null }]
    })));
    collected.push_str(&sse.feed(&json!({
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
    })));
    let (finish_sse, (_, history, _response)) = sse.finish();
    collected.push_str(&finish_sse);

    let types = event_types(&collected);
    assert!(types.contains(&"response.reasoning_summary_part.added".to_string()));
    assert!(types.contains(&"response.reasoning_summary_text.delta".to_string()));
    assert!(types.contains(&"response.function_call_arguments.delta".to_string()));
    assert!(types.contains(&"response.function_call_arguments.done".to_string()));
    assert!(types.contains(&"response.reasoning.done".to_string()));

    let completed = last_event_json(&collected, "response.completed");
    let output = completed["response"]["output"].as_array().unwrap();
    let types_out: Vec<&str> = output.iter().filter_map(|o| o["type"].as_str()).collect();
    assert_eq!(types_out, vec!["reasoning", "function_call"]);
    assert_eq!(output[0]["summary"][0]["text"], "thinking");
    assert_eq!(output[1]["call_id"], "call_1");
    assert_eq!(output[1]["arguments"], "{\"a\":1}");

    // previous_response_id 历史：assistant 消息带 tool_calls
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0]["tool_calls"][0]["function"]["arguments"],
        "{\"a\":1}"
    );
}

#[test]
fn finish_without_any_chunk_still_emits_created() {
    let sse = ResponsesSse::with_echo("m", &json!({}));
    let (out, (_resp_id, history, _response)) = sse.finish();
    assert!(out.contains("response.created"));
    assert!(out.contains("response.incomplete"));
    assert!(!out.contains("response.completed"));
    assert!(history.is_empty());
}

/// 上游流中途结束（从未给出 finish_reason）时，收尾必须下发
/// response.incomplete（reason=interrupted）而不是 response.completed，
/// 让客户端感知输出不完整，而不是把半截输出当成正常 stop。
#[test]
fn stream_without_finish_reason_emits_incomplete_interrupted() {
    let mut sse = ResponsesSse::with_echo("m", &json!({}));
    let mut collected = String::new();
    // 只吐出部分推理内容，然后流直接 EOF（无 finish_reason）
    collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "role": "assistant", "reasoning_content": "half" }, "finish_reason": null }]
        })));
    collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "reasoning_content": "-thought" }, "finish_reason": null }]
        })));
    let (finish_sse, (_, _, response)) = sse.finish();
    collected.push_str(&finish_sse);

    assert!(!collected.contains("response.completed"));
    let incomplete = last_event_json(&collected, "response.incomplete");
    assert_eq!(incomplete["response"]["status"], "incomplete");
    assert_eq!(
        incomplete["response"]["incomplete_details"]["reason"],
        "interrupted"
    );
    // 半截推理内容仍应保留（客户端据此可重试或提示）
    let output = incomplete["response"]["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["summary"][0]["text"], "half-thought");
    assert_eq!(response["status"], "incomplete");
}

/// 上游显式以 finish_reason=length 截断时，标记为 incomplete/max_output_tokens
/// （对应真实 Responses API 的 max_output_tokens 截断语义）。
#[test]
fn stream_with_length_finish_emits_incomplete_max_output_tokens() {
    let mut sse = ResponsesSse::with_echo("m", &json!({}));
    let mut collected = String::new();
    collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "partial" }, "finish_reason": null }]
        })));
    collected.push_str(&sse.feed(&json!({
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "length" }]
    })));
    let (finish_sse, (_, _, _response)) = sse.finish();
    collected.push_str(&finish_sse);

    assert!(!collected.contains("response.completed"));
    let incomplete = last_event_json(&collected, "response.incomplete");
    assert_eq!(incomplete["response"]["status"], "incomplete");
    assert_eq!(
        incomplete["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert_eq!(incomplete["response"]["output_text"], "partial");
}

/// 回归：alias 内所有 key 都因可重试状态（429）失败时，流尾必须 yield 一个
/// SSE error 事件，而不是以 HTTP 200 空流静默结束——空流会让客户端把限流
/// 误判成传输截断（TRANSPORT）并反复重试。
#[tokio::test]
async fn all_keys_429_ends_with_error_event_not_empty_stream() {
    use crate::app::AppState;
    use crate::config::{KeyRef, ModelAlias, RetryPolicy, Settings};
    use axum::body::to_bytes;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // 本地 mock 上游：无论收到什么请求都回 429（无 retry-after 头，key 不冻结）。
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_server = hits.clone();
    let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let hits = hits_for_server.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({
                            "error": {
                                "message": "Requests are too frequent. Please reduce your request frequency.",
                                "code": "rate_limit_exceeded",
                            }
                        })),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // key 值从环境变量读取（upstream_key_value 只读 env）。
    std::env::set_var("DSH_TEST_ROUTER_429_KEY", "test-key-value");

    let settings = Settings {
        host: "127.0.0.1".to_string(),
        port: 0,
        session_ttl_seconds: 3600.0,
        monthly_quota_fallback_seconds: 86400.0,
        five_hour_quota_fallback_seconds: 5400.0,
        request_timeout_seconds: 30.0,
        local_bearer_token: None,
        usage_db_path: ":memory:".to_string(),
        state_db_path: ":memory:".to_string(),
        api_keys_path: ":memory:".to_string(),
        token_price_config_path: ":memory:".to_string(),
        model_alias_config_path: ":memory:".to_string(),
        search_providers_path: ":memory:".to_string(),
        provider_models_path: ":memory:".to_string(),
        auth_invalid_freeze_seconds: 86400.0,
        diag_dir: ":memory:".to_string(),
        diag_max_bytes: 10 * 1024 * 1024,
        diag_max_files: 0,
        diag_sample_every: 1,
        env_file_path: None,
    };
    let app_state = AppState::new(settings).unwrap();

    // 单 key、translate 模式（responses_base_url=None）、retry 策略含 429。
    let alias = ModelAlias::new(
        "mock/test-model",
        "openai/mock-test-model",
        &format!("http://{addr}"),
        vec![KeyRef {
            name: "test-key".into(),
            env_var: "DSH_TEST_ROUTER_429_KEY".into(),
            weight: 1,
            provider: "mock-provider".into(),
            billing_type: "subscription".into(),
            persist: true,
        }],
        Some(RetryPolicy::new(
            300,
            5.0,
            &[401, 402, 429, 500, 502, 503, 504],
        )),
    );

    let response = stream_responses_route(
        app_state,
        vec![alias],
        None,
        json!({ "model": "mock/test-model", "input": "hi", "stream": true }),
        Some(json!({
            "model": "mock/test-model",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true,
        })),
    )
    .await;

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    server.abort();

    // 核心断言：不是空流，且带 SSE error 事件与上游 429 文案（dsh 才能按
    // RATE_LIMIT 分类；修复前这里 body 是空的）。
    assert!(!text.is_empty(), "stream must not end empty on all-key 429");
    assert!(
        text.contains("event: error"),
        "expected SSE error event, got: {text}"
    );
    assert!(
        text.contains("429") || text.contains("too frequent"),
        "expected rate-limit wording in error, got: {text}"
    );
    // 只发生了一次真实上游请求（failover 未空转重试同一 key）。
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// 上游返回 200 但流中途断开（只发部分 reasoning delta，无 finish_reason）时，
/// 翻译层收尾必须下发 response.incomplete（reason=interrupted）而非
/// response.completed——否则客户端（pi-ai mapStopReason: "completed" -> "stop"）
/// 会把半截输出当成正常结束，用户只看到思考停住且无任何报错。
#[tokio::test]
async fn upstream_truncated_stream_emits_response_incomplete() {
    use crate::app::AppState;
    use crate::config::{KeyRef, ModelAlias, RetryPolicy, Settings};
    use axum::body::to_bytes;
    use axum::routing::post;
    use axum::Router;
    use futures_util::stream::once;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // 本地 mock 上游：返回 200 + 一段只含部分 reasoning 的 SSE 流，然后正常 EOF
    //（模拟 ark 中途断流：没有 finish_reason、没有 [DONE]）。
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_server = hits.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits = hits_for_server.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let sse = concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
                    "\"reasoning_content\":\"think\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"ing\"},",
                    "\"finish_reason\":null}]}\n\n",
                );
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(once(async move {
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(sse))
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    std::env::set_var("DSH_TEST_ROUTER_TRUNC_KEY", "test-key-value");

    let settings = Settings {
        host: "127.0.0.1".to_string(),
        port: 0,
        session_ttl_seconds: 3600.0,
        monthly_quota_fallback_seconds: 86400.0,
        five_hour_quota_fallback_seconds: 5400.0,
        request_timeout_seconds: 30.0,
        local_bearer_token: None,
        usage_db_path: ":memory:".to_string(),
        state_db_path: ":memory:".to_string(),
        api_keys_path: ":memory:".to_string(),
        token_price_config_path: ":memory:".to_string(),
        model_alias_config_path: ":memory:".to_string(),
        search_providers_path: ":memory:".to_string(),
        provider_models_path: ":memory:".to_string(),
        auth_invalid_freeze_seconds: 86400.0,
        diag_dir: ":memory:".to_string(),
        diag_max_bytes: 10 * 1024 * 1024,
        diag_max_files: 0,
        diag_sample_every: 1,
        env_file_path: None,
    };
    let app_state = AppState::new(settings).unwrap();

    // 单 key、translate 模式（responses_base_url=None）。
    let alias = ModelAlias::new(
        "mock/test-model",
        "openai/mock-test-model",
        &format!("http://{addr}"),
        vec![KeyRef {
            name: "test-key".into(),
            env_var: "DSH_TEST_ROUTER_TRUNC_KEY".into(),
            weight: 1,
            provider: "mock-provider".into(),
            billing_type: "subscription".into(),
            persist: true,
        }],
        Some(RetryPolicy::new(
            300,
            5.0,
            &[401, 402, 429, 500, 502, 503, 504],
        )),
    );

    let response = stream_responses_route(
        app_state,
        vec![alias],
        None,
        json!({ "model": "mock/test-model", "input": "hi", "stream": true }),
        Some(json!({
            "model": "mock/test-model",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true,
        })),
    )
    .await;

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    server.abort();

    // 核心断言：翻译层把上游断流收尾成 response.incomplete（而非 completed），
    // 且带 reason=interrupted；半截 reasoning 内容保留。
    assert!(
        text.contains("response.incomplete"),
        "expected response.incomplete for truncated upstream, got: {text}"
    );
    assert!(
        !text.contains("response.completed"),
        "must not emit response.completed for truncated upstream, got: {text}"
    );
    assert!(
        text.contains("\"reason\":\"interrupted\"") || text.contains("\"reason\": \"interrupted\""),
        "expected interrupted reason, got: {text}"
    );
    assert!(
        text.contains("thinking"),
        "partial reasoning content should be preserved, got: {text}"
    );
    // 只发生了一次真实上游请求。
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
