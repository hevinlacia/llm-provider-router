//! Anthropic Messages API 入口：`POST /v1/messages`（透传 / 翻译混合代理）。
//!
//! 与 `/v1/responses` 共享同一套模型别名解析 / 选 key / 重试 / 冻结 / 用量链路。
//! 对每个路由目标按供应商配置决定模式：
//! - 配置了 `anthropic_base_url`（供应商原生支持 Anthropic 协议）-> **透传**：
//!   只改写 model 名，请求原样发到 `{anthropic_base_url}/v1/messages`，响应原样返回；
//! - 未配置 -> **翻译**：Anthropic 请求翻译成 Responses 请求，走现有
//!   `/v1/responses` 机制（内部再透传到供应商 Responses 端点或翻译成 chat completions），
//!   响应/SSE 再翻译回 Anthropic 格式。
//!
//! 鉴权：Anthropic 客户端用 `x-api-key` 头（也兼容 Authorization Bearer）。

use crate::app::AppState;
use crate::config::ModelAlias;
use crate::features::anthropic::stream::SseTranslator;
use crate::features::anthropic::translate;
use crate::features::chat::select::{
    alias_with_runtime_weights_locked, freeze_maybe, record_usage, select_key_locked,
    upstream_key_value_locked, usage_key_name,
};
use crate::features::chat::upstream::CallError;
use crate::features::router::NoAvailableKeyError;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashSet;

use super::chat::extract_session_id;
use super::resp::{internal_error, json_status, status_code};
use super::responses::{responses_alias_dispatch, translate_request};

/// 非流式响应体读取上限（64MB，与路由层 body limit 一致）。
const BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Anthropic 鉴权：`x-api-key` 或 `Authorization: Bearer` 任一匹配即可。
fn validate_auth_anthropic(
    settings: &crate::config::Settings,
    headers: &HeaderMap,
) -> Option<Response> {
    let expected_token = settings.local_bearer_token.as_ref()?;
    let bearer_ok = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {expected_token}").as_str());
    let api_key_ok = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        == Some(expected_token.as_str());
    if bearer_ok || api_key_ok {
        None
    } else {
        Some(json_status(
            StatusCode::UNAUTHORIZED,
            crate::features::anthropic::error_body("invalid x-api-key", "authentication_error"),
        ))
    }
}

fn anthropic_error_response(message: &str, error_type: &str, status: StatusCode) -> Response {
    json_status(
        status,
        crate::features::anthropic::error_body(message, error_type),
    )
}

fn anthropic_frozen_response(exc: NoAvailableKeyError) -> Response {
    let mut resp = json_status(
        StatusCode::TOO_MANY_REQUESTS,
        crate::features::anthropic::error_body(&exc.to_string(), "rate_limit_error"),
    );
    if let Ok(value) = HeaderValue::from_str(&exc.retry_after.to_string()) {
        resp.headers_mut().insert("retry-after", value);
    }
    resp
}

pub(crate) async fn messages(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if crate::diag::diag_enabled(&app.settings) {
        crate::diag::append(
            &app.settings,
            "request.messages",
            serde_json::json!({
                "model": payload.get("model").and_then(Value::as_str).unwrap_or(""),
                "summary": crate::diag::payload_summary(&payload),
            }),
        );
    }
    if let Some(response) = validate_auth_anthropic(&app.settings, &headers) {
        return response;
    }
    let Some(model_name) = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return anthropic_error_response(
            "model must be a string",
            "invalid_request_error",
            StatusCode::BAD_REQUEST,
        );
    };
    let session_id = extract_session_id(&payload, &headers);
    let route_aliases = match app.state.lock() {
        Ok(mut state) => state.route_aliases(&model_name, session_id.as_deref()),
        Err(_) => return internal_error("router state lock poisoned"),
    };
    if route_aliases.is_empty() {
        return anthropic_error_response(
            &format!("unsupported model alias: {model_name}"),
            "not_found_error",
            StatusCode::NOT_FOUND,
        );
    }
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        stream_messages_route(app, route_aliases, session_id, payload, &model_name).await
    } else {
        messages_non_stream(app, route_aliases, session_id, payload, &model_name).await
    }
}

/// 非流式：逐 alias 分发（透传 / 经 Responses 机制翻译），首个成功响应直接返回。
async fn messages_non_stream(
    app: AppState,
    route_aliases: Vec<ModelAlias>,
    session_id: Option<String>,
    payload: Value,
    model: &str,
) -> Response {
    // Anthropic -> Responses 载荷懒计算：首个非透传目标出现时才翻译一次
    let mut responses_payload: Option<Value> = None;
    let mut last_frozen: Option<NoAvailableKeyError> = None;
    for base_alias in route_aliases {
        let alias = match app.state.lock() {
            Ok(mut state) => state.alias_with_runtime_weights(&base_alias),
            Err(_) => return internal_error("router state lock poisoned"),
        };
        let result = if alias.supports_anthropic() {
            call_anthropic_passthrough(&app, alias, session_id.clone(), payload.clone()).await
        } else {
            let translated = match &responses_payload {
                Some(payload) => payload.clone(),
                None => match translate::messages_to_responses(&payload) {
                    Ok(translated) => {
                        responses_payload = Some(translated.clone());
                        translated
                    }
                    Err(message) => {
                        return anthropic_error_response(
                            &message,
                            "invalid_request_error",
                            StatusCode::BAD_REQUEST,
                        );
                    }
                },
            };
            match responses_alias_dispatch(&app, alias, session_id.clone(), &translated, None).await
            {
                Ok(resp) => Ok(translate_non_stream_response(resp, model).await),
                Err(CallError::NoAvailable(exc)) => Err(CallError::NoAvailable(exc)),
            }
        };
        match result {
            Ok(response) => return response,
            Err(CallError::NoAvailable(exc)) => last_frozen = Some(exc),
        }
    }
    if let Some(exc) = last_frozen {
        anthropic_frozen_response(exc)
    } else {
        anthropic_error_response(
            &format!("unsupported model alias: {model}"),
            "not_found_error",
            StatusCode::NOT_FOUND,
        )
    }
}

/// 把 Responses 机制返回的 axum Response（JSON 或错误体）翻译成 Anthropic 格式。
/// 保留原响应头（含 `x-llm-router-*`）与状态码。
async fn translate_non_stream_response(resp: Response, model: &str) -> Response {
    let (mut parts, body) = resp.into_parts();
    let status = parts.status;
    let body_bytes = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return anthropic_error_response(
                &format!("failed to read upstream response: {err}"),
                "api_error",
                StatusCode::BAD_GATEWAY,
            );
        }
    };
    let content: Value = serde_json::from_str(&String::from_utf8_lossy(&body_bytes)).unwrap_or_else(
        |_| json!({ "error": { "message": String::from_utf8_lossy(&body_bytes), "type": "upstream_error" } }),
    );
    let translated = if status.is_success() {
        translate::responses_to_messages(&content, model)
    } else {
        translate::responses_error_to_anthropic(&content)
    };
    parts.status = status;
    let body_text = serde_json::to_string(&translated).unwrap_or_else(|_| "{}".to_string());
    Response::from_parts(parts, Body::from(body_text))
}

/// 流式：首个 alias 决定模式（与 Responses 流式回退语义对齐）：
/// - 首选 alias 支持 Anthropic 透传 -> 全部 Anthropic 原生 alias 走字节直通流；
/// - 否则 -> 非 Anthropic alias 走 Responses 流式机制，输出 SSE 包一层翻译器。
async fn stream_messages_route(
    app: AppState,
    route_aliases: Vec<ModelAlias>,
    session_id: Option<String>,
    payload: Value,
    model: &str,
) -> Response {
    let anthropic_first = route_aliases
        .first()
        .is_some_and(|a| a.supports_anthropic());
    if anthropic_first {
        let capable: Vec<ModelAlias> = route_aliases
            .into_iter()
            .filter(|a| a.supports_anthropic())
            .collect();
        stream_anthropic_passthrough(app, capable, session_id, payload).await
    } else {
        // Anthropic -> Responses 载荷；流式翻译模式还需要 Responses -> chat 载荷
        let responses_payload = match translate::messages_to_responses(&payload) {
            Ok(translated) => translated,
            Err(message) => {
                return anthropic_error_response(
                    &message,
                    "invalid_request_error",
                    StatusCode::BAD_REQUEST,
                );
            }
        };
        let chat_payload = match translate_request(&responses_payload) {
            Ok(chat) => Some(chat),
            Err(message) => {
                return anthropic_error_response(
                    &message,
                    "invalid_request_error",
                    StatusCode::BAD_REQUEST,
                );
            }
        };
        let resp = crate::features::responses::stream::stream_responses_route(
            app,
            route_aliases,
            session_id,
            responses_payload,
            chat_payload,
        )
        .await;
        wrap_sse_with_anthropic_translator(resp, model)
    }
}

/// 把 Responses SSE 响应体包一层 Anthropic SSE 翻译器（保留原响应头）。
fn wrap_sse_with_anthropic_translator(resp: Response, model: &str) -> Response {
    let (parts, body) = resp.into_parts();
    let mut translator = SseTranslator::new(model);
    let mut inner = body.into_data_stream();
    let translated = async_stream::stream! {
        while let Some(item) = inner.next().await {
            match item {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    yield Ok::<Bytes, axum::Error>(Bytes::from(translator.feed(&text)));
                }
                Err(err) => {
                    yield Err(err);
                    return;
                }
            }
        }
        // 流尾冲洗残余缓冲行
        yield Ok(Bytes::from(translator.finish()));
    };
    Response::from_parts(parts, Body::from_stream(translated))
}

/// Anthropic 透传流式：逐 alias 选 key 发 `{anthropic_base_url}/v1/messages`，
/// SSE 字节原样转发；结束后按流尾 usage 记账。
async fn stream_anthropic_passthrough(
    app: AppState,
    aliases: Vec<ModelAlias>,
    session_id: Option<String>,
    payload: Value,
) -> Response {
    let stream_headers: Option<(String, String, String)> = aliases.first().map(|first| {
        (
            first.alias.clone(),
            first.provider(),
            first.upstream_model(),
        )
    });

    let stream = async_stream::stream! {
        let mut last_error: Option<String> = None;
        let mut total_tried: usize = 0;
        'aliases: for base_alias in aliases {
            let alias = match alias_with_runtime_weights_locked(&app, &base_alias) {
                Ok(alias) => alias,
                Err(message) => {
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(anthropic_sse_error(&message)));
                    return;
                }
            };
            let Some(anthropic_base) = alias
                .anthropic_base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                last_error = Some("provider anthropic_base_url is empty".to_string());
                continue 'aliases;
            };
            let endpoint = format!("{}/v1/messages", anthropic_base.trim_end_matches('/'));
            let mut upstream_payload = payload.clone();
            if let Some(obj) = upstream_payload.as_object_mut() {
                obj.insert("model".to_string(), json!(alias.upstream_model()));
            }
            let mut tried = HashSet::new();
            let retry_policy = alias.retry_policy.clone();
            loop {
                let selected_key = match select_key_locked(&app, &alias, session_id.as_deref(), &tried) {
                    Ok(result) => result,
                    Err(message) => {
                        yield Ok(Bytes::from(anthropic_sse_error(&message)));
                        return;
                    }
                };
                let key = match selected_key {
                    Ok(key) => key,
                    Err(_) => break, // key 全冻结/不可用：fallback 下一 alias
                };
                tried.insert(key.name.clone());
                total_tried += 1;
                let key_value = match upstream_key_value_locked(&app, &key) {
                    Ok(value) => value,
                    Err(message) => {
                        yield Ok(Bytes::from(anthropic_sse_error(&message)));
                        return;
                    }
                };
                let Some(key_value) = key_value else {
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None, session_id.as_deref());
                    last_error = Some(format!("missing key value for {}", usage_key_name(&app, &key)));
                    continue;
                };

                let mut response: Option<reqwest::Response> = None;
                for attempt in 0..2 {
                    match app
                        .client
                        .post(&endpoint)
                        .header("x-api-key", key_value.clone())
                        .header("anthropic-version", "2023-06-01")
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
                            if attempt == 0 && retryable {
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
                        last_error = Some("upstream connect error".to_string());
                        continue;
                    }
                };
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                if retry_policy.as_ref().is_some_and(|p| p.retry_on_status.contains(&status)) {
                    let body_text = response.text().await.unwrap_or_default();
                    freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, None, session_id.as_deref());
                    crate::features::chat::payload::log_upstream_failure(&alias, status, &body_text);
                    last_error = Some(format!("upstream {status}"));
                    continue;
                }
                if status >= 400 {
                    let body_text = response.text().await.unwrap_or_default();
                    freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, None, session_id.as_deref());
                    crate::features::chat::payload::log_upstream_failure(&alias, status, &body_text);
                    // 200 SSE 流已提交，上游错误转成 Anthropic SSE error 事件下发
                    let message = serde_json::from_str::<Value>(&body_text)
                        .ok()
                        .and_then(|v| {
                            v.pointer("/error/message").and_then(Value::as_str).map(str::to_string)
                        })
                        .unwrap_or_else(|| body_text.chars().take(300).collect());
                    yield Ok(Bytes::from(anthropic_sse_error(&message)));
                    return;
                }

                // 正常流：SSE 字节原样转发，收集流尾 usage 记账
                let mut bytes_stream = response.bytes_stream();
                let mut body_text: Vec<u8> = Vec::new();
                while let Some(item) = bytes_stream.next().await {
                    match item {
                        Ok(chunk) => {
                            body_text.extend_from_slice(&chunk);
                            yield Ok(chunk);
                        }
                        Err(exc) => {
                            yield Ok(Bytes::from(anthropic_sse_error(&exc.to_string())));
                            return;
                        }
                    }
                }
                let body_str = String::from_utf8_lossy(&body_text).to_string();
                freeze_maybe(&app.state, &key, status, &headers, &body_str, &app.settings);
                let usage = translate::extract_anthropic_stream_usage(&body_str);
                record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref(), session_id.as_deref());
                return;
            }
        }
        if let Some(error) = last_error {
            let shown = if total_tried > 0 { total_tried } else { 1 };
            yield Ok(Bytes::from(anthropic_sse_error(&format!("all {shown} upstream keys failed: {error}"))));
        }
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream");
    if let Some((alias, provider, upstream)) = stream_headers {
        if let Ok(hv) = HeaderValue::from_str(&alias) {
            builder = builder.header("x-llm-router-model", hv);
        }
        if let Ok(hv) = HeaderValue::from_str(&provider) {
            builder = builder.header("x-llm-router-provider", hv);
        }
        if let Ok(hv) = HeaderValue::from_str(&upstream) {
            builder = builder.header("x-llm-router-upstream-model", hv);
        }
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error("failed to create streaming response"))
}

/// Anthropic 透传（非流式）：选 key 发 `{anthropic_base_url}/v1/messages`，
/// 响应字节原样返回（含错误体），usage 按 Anthropic 字段归一化记账。
async fn call_anthropic_passthrough(
    app: &AppState,
    alias: ModelAlias,
    session_id: Option<String>,
    payload: Value,
) -> Result<Response, CallError> {
    let Some(anthropic_base) = alias
        .anthropic_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Ok(json_status(
            StatusCode::BAD_GATEWAY,
            crate::features::anthropic::error_body(
                "provider anthropic_base_url is empty; cannot pass through /v1/messages",
                "api_error",
            ),
        ));
    };
    let retry_policy = alias.retry_policy.clone();
    let mut tried = HashSet::new();
    let endpoint = format!("{}/v1/messages", anthropic_base.trim_end_matches('/'));
    let mut upstream_payload = payload;
    if let Some(obj) = upstream_payload.as_object_mut() {
        obj.insert("model".to_string(), json!(alias.upstream_model()));
    }

    loop {
        let selected_key = match select_key_locked(app, &alias, session_id.as_deref(), &tried) {
            Ok(result) => result,
            Err(message) => return Ok(internal_error(&message)),
        };
        let key = match selected_key {
            Ok(key) => key,
            Err(exc) => return Err(CallError::NoAvailable(exc)),
        };
        tried.insert(key.name.clone());
        let key_value = match upstream_key_value_locked(app, &key) {
            Ok(value) => value,
            Err(message) => return Ok(internal_error(&message)),
        };
        let Some(key_value) = key_value else {
            record_usage(
                &app.state,
                &alias.alias,
                &usage_key_name(app, &key),
                599,
                None,
                session_id.as_deref(),
            );
            continue;
        };

        let mut response: Option<reqwest::Response> = None;
        for attempt in 0..2 {
            match app
                .client
                .post(&endpoint)
                .header("x-api-key", key_value.clone())
                .header("anthropic-version", "2023-06-01")
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
                    if attempt == 0 && retryable {
                        continue;
                    }
                    break;
                }
            }
        }
        let response = match response {
            Some(r) => r,
            None => {
                record_usage(
                    &app.state,
                    &alias.alias,
                    &usage_key_name(app, &key),
                    599,
                    None,
                    session_id.as_deref(),
                );
                continue;
            }
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        let content: Value = serde_json::from_str(&body_text)
            .unwrap_or_else(|_| json!({ "error": { "message": body_text, "type": "api_error" } }));

        if retry_policy
            .as_ref()
            .is_some_and(|policy| policy.retry_on_status.contains(&status))
        {
            freeze_maybe(
                &app.state,
                &key,
                status,
                &headers,
                &body_text,
                &app.settings,
            );
            record_usage(
                &app.state,
                &alias.alias,
                &usage_key_name(app, &key),
                status,
                None,
                session_id.as_deref(),
            );
            crate::features::chat::payload::log_upstream_failure(&alias, status, &body_text);
            continue;
        }

        freeze_maybe(
            &app.state,
            &key,
            status,
            &headers,
            &body_text,
            &app.settings,
        );
        let usage = if (200..300).contains(&status) {
            translate::extract_anthropic_usage(&content)
        } else {
            None
        };
        record_usage(
            &app.state,
            &alias.alias,
            &usage_key_name(app, &key),
            status,
            usage.as_ref(),
            session_id.as_deref(),
        );
        crate::features::chat::payload::log_upstream_failure(&alias, status, &body_text);

        // 响应原样透传（上游已是 Anthropic 格式）
        let mut resp = json_status(status_code(status), content);
        crate::features::chat::upstream::inject_router_headers(resp.headers_mut(), &alias);
        return Ok(resp);
    }
}

/// Anthropic SSE error 事件文本。
fn anthropic_sse_error(message: &str) -> String {
    format!(
        "event: error\ndata: {}\n\n",
        serde_json::to_string(&crate::features::anthropic::error_body(
            message,
            "api_error"
        ))
        .unwrap_or_else(|_| "{}".to_string())
    )
}
