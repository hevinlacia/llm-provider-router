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
    alias_with_runtime_weights_locked, extract_usage, extract_usage_from_stream, freeze_maybe,
    record_usage, select_key_locked, stream_error_event, upstream_key_value_locked, usage_key_name,
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
            let alias = match alias_with_runtime_weights_locked(&app, &base_alias) {
                Ok(alias) => alias,
                Err(message) => {
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(stream_error_event("router", 0, &message)));
                    return;
                }
            };
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
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
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
                        record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
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
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref());
                    log_upstream_failure(&alias, status, &body_text);
                    continue;
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
                record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref());
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
