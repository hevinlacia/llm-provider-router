//! 上游调用主链路（流式）：SSE 转发 + finish_reason/[DONE] 补齐。

use crate::app::AppState;
use crate::config::ModelAlias;
use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashSet;

use super::payload::log_upstream_failure;
use super::payload::prepare_upstream_payload;
use super::select::{
    extract_usage, extract_usage_from_stream, freeze_maybe, record_usage, select_key_locked,
    stream_error_event, upstream_key_value_locked, usage_key_name,
};
use crate::routes::resp::internal_error;

pub(crate) async fn stream_upstream_route(
    app: AppState,
    aliases: Vec<ModelAlias>,
    session_id: Option<String>,
    payload: Value,
) -> Response {
    // 流式响应头需在 stream 被 move 前从首选候选取保守窗口提示
    let stream_headers: Option<(String, String, String, Option<u32>, Option<u32>)> =
        aliases.first().map(|first| {
            (
                first.alias.clone(),
                first.provider(),
                first.upstream_model(),
                first.context_window,
                first.max_output_tokens,
            )
        });
    let stream = async_stream::stream! {
        let mut last_error: Option<String> = None;
        let mut total_tried: usize = 0;
        let mut failed_alias: String = aliases.first().map(|a| a.alias.clone()).unwrap_or_else(|| "router".to_string());
        for base_alias in aliases {
            let alias = base_alias.clone();
            // 空地址防护：供应商未配置 chat completions base_url 时给出明确错误
            if alias.base_url.trim().is_empty() {
                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(stream_error_event(
                    &alias.alias,
                    0,
                    "provider has no chat completions base_url configured",
                )));
                return;
            }
            let upstream_payload = prepare_upstream_payload(&payload, &alias);
            let mut tried = HashSet::new();
            let retry_policy = alias.retry_policy.clone();

            loop {
                let selected_key = match select_key_locked(&app, &alias, session_id.as_deref(), &tried) {
                    Ok(result) => result,
                    Err(message) => {
                        yield Ok(Bytes::from(stream_error_event(&alias.alias, tried.len(), &message)));
                        return;
                    }
                };
                let key = match selected_key {
                    Ok(key) => key,
                    Err(_) => {
                        // key 全部不可用/冻结：立即退出当前 alias，外层 for 循环 fallback 到下一个 target。
                        break;
                    }
                };
                tried.insert(key.name.clone());
                total_tried += 1;
                failed_alias = alias.alias.clone();
                let key_value = match upstream_key_value_locked(&app, &key) {
                    Ok(value) => value,
                    Err(message) => {
                        yield Ok(Bytes::from(stream_error_event(&alias.alias, tried.len(), &message)));
                        return;
                    }
                };
                let Some(key_value) = key_value else {
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None, session_id.as_deref());
                    last_error = Some(format!("missing key value for {}", usage_key_name(&app, &key)));
                    continue;
                };
                let mut response: Option<reqwest::Response> = None;
                let mut last_exc: Option<String> = None;
                for attempt in 0..2 {
                    match app
                        .client
                        .post(format!("{}/chat/completions", alias.base_url.trim_end_matches('/')))
                        .bearer_auth(key_value.clone())
                        .header(CONTENT_TYPE, "application/json")
                        .json(&upstream_payload)
                        .send()
                        .await
                    {
                        Ok(resp) => {
                            response = Some(resp);
                            break;
                        }
                        Err(exc) => {
                            let retryable = exc.is_connect() || exc.is_timeout() || exc.is_request();
                            last_exc = Some(exc.to_string());
                            if attempt == 0 && retryable {
                                if crate::diag::diag_enabled(&app.settings) {
                                    crate::diag::append(
                                        &app.settings,
                                        "upstream.retry_pool_idle",
                                        serde_json::json!({
                                            "alias": alias.alias,
                                            "provider": alias.provider(),
                                            "key": usage_key_name(&app, &key),
                                            "attempt": attempt + 1,
                                            "error": last_exc,
                                        }),
                                    );
                                }
                                continue;
                            }
                            break;
                        }
                    }
                }
                let response = match response {
                    Some(r) => r,
                    None => {
                        record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None, session_id.as_deref());
                        last_error = Some(last_exc.unwrap_or_else(|| "upstream connect error".to_string()));
                        if crate::diag::diag_enabled(&app.settings) {
                            crate::diag::append(
                                &app.settings,
                                "upstream.connect_error",
                                serde_json::json!({
                                    "alias": alias.alias,
                                    "provider": alias.provider(),
                                    "key": usage_key_name(&app, &key),
                                    "error": last_error,
                                }),
                            );
                        }
                        continue;
                    }
                };
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                if retry_policy.as_ref().is_some_and(|policy| policy.retry_on_status.contains(&status)) {
                    let body_text = response.text().await.unwrap_or_default();
                    freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                    let usage = extract_usage_from_stream(&body_text).or_else(|| serde_json::from_str::<Value>(&body_text).ok().and_then(|value| extract_usage(&value).cloned()));
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref(), session_id.as_deref());
                    log_upstream_failure(&alias, status, &body_text);
                    // 与 responses 流一致：记录可重试失败，避免全 key 都因限流失败时
                    // 以 200 空流结束（客户端会当成传输截断反复重试）。
                    let message = serde_json::from_str::<Value>(&body_text)
                        .ok()
                        .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| "upstream error".to_string());
                    last_error = Some(format!("upstream {status}: {message}"));
                    continue;
                }

                if status >= 400 {
                    // 非重试上游错误（如 404 模型不在账号 coding plan 内）：与 responses 流
                    // 一致，以 SSE error 事件收尾并携带上游真实 message，而不是把错误 body
                    // 当 200 空流透传——空流会让客户端报 "Stream ended without finish_reason"
                    // 而无法定位真因。
                    let body_text = response.text().await.unwrap_or_default();
                    freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, None, session_id.as_deref());
                    log_upstream_failure(&alias, status, &body_text);
                    // 上游明确拒绝：解除会话粘性绑定，避免会话被钉死在坏 key 上。
                    if let Some(sid) = session_id.as_deref() {
                        if let Ok(mut state) = app.state.lock() {
                            state.unbind(&alias.alias, sid);
                        }
                    }
                    let message = serde_json::from_str::<Value>(&body_text)
                        .ok()
                        .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| "upstream error".to_string());
                    yield Ok(Bytes::from(stream_error_event(
                        &alias.alias,
                        tried.len(),
                        &format!("upstream {status}: {message}"),
                    )));
                    return;
                }

                let mut body_text = Vec::new();
                let mut bytes_stream = response.bytes_stream();
                // 兼容不标准上游（如 muse-spark 的 finish_reason 为 null 且无 [DONE]）：
                // 逐行跟踪流中是否出现过标准结束信号，缺失时在流尾补齐，
                // 避免客户端报 "Stream ended without finish_reason"。
                let mut saw_finish_reason = false;
                let mut saw_done = false;
                while let Some(item) = bytes_stream.next().await {
                    match item {
                        Ok(chunk) => {
                            body_text.extend_from_slice(&chunk);
                            let text = String::from_utf8_lossy(&chunk);
                            for line in text.lines() {
                                let trimmed_line = line.trim();
                                let Some(data) = trimmed_line.strip_prefix("data:") else {
                                    continue;
                                };
                                let body = data.trim();
                                if body == "[DONE]" {
                                    saw_done = true;
                                    continue;
                                }
                                if let Ok(value) = serde_json::from_str::<Value>(body) {
                                    if let Some(fr) = value.pointer("/choices/0/finish_reason") {
                                        if fr.as_str().is_some_and(|s| !s.is_empty()) {
                                            saw_finish_reason = true;
                                        }
                                    }
                                }
                            }
                            yield Ok(chunk);
                        }
                        Err(exc) => {
                            // 中流断开：解除粘性绑定，下次请求重新选 key（可能是网络抖动，
                            // 也可能是该 key 持续异常）。
                            if let Some(sid) = session_id.as_deref() {
                                if let Ok(mut state) = app.state.lock() {
                                    state.unbind(&alias.alias, sid);
                                }
                            }
                            yield Ok(Bytes::from(stream_error_event(&alias.alias, tried.len(), &exc.to_string())));
                            return;
                        }
                    }
                }
                // 上游缺失标准结束信号时补齐（仅影响不标准上游，标准上游无额外输出）
                if !saw_finish_reason {
                    yield Ok(Bytes::from(
                        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    ));
                }
                if !saw_done {
                    yield Ok(Bytes::from("data: [DONE]\n\n"));
                }
                let body_text = String::from_utf8_lossy(&body_text).to_string();
                // 流式结束埋点：上游是否缺失 finish_reason/[DONE]，用于量化 muse-spark 类不标准流的影响。
                if crate::diag::diag_enabled(&app.settings) && (!saw_finish_reason || !saw_done) {
                    crate::diag::append(&app.settings, "stream.incomplete_upstream", serde_json::json!({
                        "alias": alias.alias,
                        "provider": alias.provider(),
                        "model": alias.upstream_model(),
                        "status": status,
                        "saw_finish_reason": saw_finish_reason,
                        "saw_done": saw_done,
                        "bytes": body_text.len(),
                    }));
                }
                freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                let usage = extract_usage_from_stream(&body_text);
                record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref(), session_id.as_deref());
                log_upstream_failure(&alias, status, &body_text);
                return;
            }
        }
        if let Some(error) = last_error {
            // 之前写死 "router"/0 会让 `all 0 upstream keys failed for router`
            // 误导为“路由配置为空”；现用真实 alias + 累计 tried。
            let shown = if total_tried > 0 { total_tried } else { 1 };
            let alias = if total_tried > 0 { failed_alias } else { "router".to_string() };
            yield Ok(Bytes::from(stream_error_event(&alias, shown, &error)));
        }
    };
    // 流式响应头：取首选候选的窗口作为保守提示（精确命中窗口由非流式头提供；流式下在连接建立前无法确定最终命中）
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream");
    if let Some((alias, provider, upstream, context_window, max_output)) = stream_headers {
        if let Ok(hv) = HeaderValue::from_str(&alias) {
            builder = builder.header("x-llm-router-model", hv);
        }
        if let Ok(hv) = HeaderValue::from_str(&provider) {
            builder = builder.header("x-llm-router-provider", hv);
        }
        if let Ok(hv) = HeaderValue::from_str(&upstream) {
            builder = builder.header("x-llm-router-upstream-model", hv);
        }
        if let Some(v) = context_window {
            if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
                builder = builder.header("x-llm-router-context-window", hv);
            }
        }
        if let Some(v) = max_output {
            if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
                builder = builder.header("x-llm-router-max-output", hv);
            }
        }
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error("failed to create streaming response"))
}

#[cfg(test)]
mod tests {
    //! 回归：非重试上游错误状态（如 404 模型不在账号 coding plan）必须以 SSE error
    //! 事件收尾并携带上游真实 message，而不是把错误 body 当 200 空流透传。

    use super::*;
    use crate::config::{KeyRef, ModelAlias, RetryPolicy, Settings};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 上游返回 404（不在 retry_on_status）时，流尾必须有带真实文案的 error 事件。
    #[tokio::test]
    async fn nonretryable_upstream_404_ends_with_error_event_not_empty_stream() {
        // 本地 mock 上游：无论收到什么请求都回 404 + coding plan 文案。
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_server = hits.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let hits = hits_for_server.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        Json(json!({
                            "error": {
                                "message": "The requested model does not support the coding plan feature.",
                                "code": "model_not_supported",
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
        std::env::set_var("CHAT_TEST_ROUTER_404_KEY", "test-key-value");

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

        // 单 key、retry 策略不含 404（模拟 ark coding 端点现状）。
        let alias = ModelAlias::new(
            "mock/test-model",
            "openai/mock-test-model",
            &format!("http://{addr}"),
            vec![KeyRef {
                name: "test-key".into(),
                env_var: "CHAT_TEST_ROUTER_404_KEY".into(),
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

        let response = stream_upstream_route(
            app_state.clone(),
            vec![alias],
            Some("sess-unbind-1".to_string()),
            json!({
                "model": "mock/test-model",
                "messages": [{ "role": "user", "content": "hi" }],
                "stream": true,
            }),
        )
        .await;

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        server.abort();

        // 核心断言：不是空流，且 error 事件携带上游真实状态与文案。
        // 修复前：404 body 被当 200 SSE 透传，客户端只能看到
        // "Stream ended without finish_reason"，无法定位真因。
        assert!(
            !text.is_empty(),
            "stream must not end empty on non-retryable 404"
        );
        assert!(
            text.contains("upstream 404"),
            "expected upstream status in error event, got: {text}"
        );
        assert!(
            text.contains("coding plan"),
            "expected real upstream message in error event, got: {text}"
        );
        assert!(
            text.contains("data: [DONE]"),
            "error event must be followed by [DONE], got: {text}"
        );
        // 非重试状态不应空转重试同一 key。
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // 选中成功会建立粘性绑定，上游明确拒绝后必须解绑，
        // 否则该会话会被钉死在坏 key 上（404 不触发 freeze）。
        assert!(
            app_state
                .state
                .lock()
                .unwrap()
                .binding_for("mock/test-model", "sess-unbind-1")
                .is_none(),
            "session binding must be cleared after non-retryable upstream rejection"
        );
    }
}
