//! Responses API 入口 handler：`POST /v1/responses`（透传 / 翻译混合代理）。
//!
//! 与 `/v1/chat/completions` 共享同一套模型别名解析 / 选 key / 重试 / 冻结 / 用量链路。
//! 对每个路由目标按供应商配置的三类地址决定模式：
//! - 配置了 `responses_base_url`（供应商原生支持 Responses API）-> **透传**：只改写 model 名，
//!   请求原样发到 `{responses_base_url}/responses`，响应原样返回；
//! - 未配置（仅 `base_url`，供应商只支持 Chat Completions）-> **翻译**：
//!   请求翻译成 chat completions 走 `{base_url}/chat/completions`，响应翻译回 Responses 格式。

use crate::app::AppState;
use crate::config::ModelAlias;
use crate::features::chat::payload::prepare_upstream_payload;
use crate::features::chat::select::{
    extract_usage, freeze_maybe, record_usage, select_key_locked, upstream_key_value_locked,
    usage_key_name,
};
use crate::features::chat::upstream::CallError;
use crate::features::responses::{store, translate};
use crate::features::router::NoAvailableKeyError;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

use super::chat::{extract_session_id, validate_auth};
use super::resp::{internal_error, json_status, status_code};

/// input_items 端点 query 参数。
#[derive(Debug, Deserialize)]
pub(crate) struct InputItemsQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    order: Option<String>,
}

pub(crate) async fn responses(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if crate::diag::diag_enabled(&app.settings) {
        crate::diag::append(
            &app.settings,
            "request.responses",
            serde_json::json!({
                "model": payload.get("model").and_then(Value::as_str).unwrap_or(""),
                "summary": crate::diag::payload_summary(&payload),
            }),
        );
    }
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    let Some(model_name) = payload.get("model").and_then(Value::as_str) else {
        return error_response(
            "model must be a string",
            "invalid_request_error",
            StatusCode::BAD_REQUEST,
        );
    };
    let session_id = extract_session_id(&payload, &headers);
    let route_aliases = match app.state.lock() {
        Ok(mut state) => state.route_aliases(model_name, session_id.as_deref()),
        Err(_) => return internal_error("router state lock poisoned"),
    };
    if route_aliases.is_empty() {
        return error_response(
            &format!("unsupported model alias: {model_name}"),
            "unsupported_model",
            StatusCode::NOT_FOUND,
        );
    }
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 翻译模式下才会用到 chat payload；存在任一非透传目标时先翻译（懒计算，避免透传路径无谓翻译）。
    let chat_payload = if stream {
        if route_aliases.iter().any(|a| !a.supports_responses()) {
            match translate_request(&payload) {
                Ok(chat) => Some(chat),
                Err(message) => {
                    return error_response(
                        &message,
                        "invalid_request_error",
                        StatusCode::BAD_REQUEST,
                    );
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    if stream {
        crate::features::responses::stream::stream_responses_route(
            app,
            route_aliases,
            session_id,
            payload,
            chat_payload,
        )
        .await
    } else {
        let mut last_frozen: Option<NoAvailableKeyError> = None;
        for base_alias in route_aliases {
            let alias = match app.state.lock() {
                Ok(mut state) => state.alias_with_runtime_weights(&base_alias),
                Err(_) => return internal_error("router state lock poisoned"),
            };
            let result = responses_alias_dispatch(
                &app,
                alias,
                session_id.clone(),
                &payload,
                chat_payload.as_ref(),
            )
            .await;
            match result {
                Ok(response) => return response,
                Err(CallError::NoAvailable(exc)) => last_frozen = Some(exc),
            }
        }
        if let Some(exc) = last_frozen {
            frozen_response(exc)
        } else {
            error_response(
                &format!("unsupported model alias: {model_name}"),
                "unsupported_model",
                StatusCode::NOT_FOUND,
            )
        }
    }
}

/// 单个 route alias 的 Responses 请求分发：透传（供应商原生 Responses）或翻译成 chat。
/// 返回 `Ok(Response)`（含错误响应体）或 `Err(NoAvailable)`（key 耗尽，由调用方 fallback 下一 alias）。
/// `/v1/responses` 与 Anthropic `/v1/messages`（翻译模式）共用。
pub(crate) async fn responses_alias_dispatch(
    app: &AppState,
    alias: ModelAlias,
    session_id: Option<String>,
    payload: &Value,
    chat_payload: Option<&Value>,
) -> Result<Response, CallError> {
    if alias.supports_responses() {
        // 透传：只改写 model 名，发到供应商 Responses 端点
        let upstream_payload = translate::prepare_passthrough_payload(payload, &alias);
        call_responses_passthrough(app, alias, session_id, upstream_payload, "/responses").await
    } else {
        // 翻译：请求转 chat，响应再翻译回 Responses
        let chat = match chat_payload {
            Some(chat) => chat.clone(),
            None => match translate_request(payload) {
                Ok(chat) => chat,
                Err(message) => {
                    return Ok(error_response(
                        &message,
                        "invalid_request_error",
                        StatusCode::BAD_REQUEST,
                    ))
                }
            },
        };
        let upstream_payload = prepare_upstream_payload(&chat, &alias);
        call_upstream_responses(app, alias, session_id, upstream_payload, payload).await
    }
}

/// Responses 请求翻译成 chat completions 请求，并处理 previous_response_id 多轮历史。
pub(crate) fn translate_request(payload: &Value) -> Result<Value, String> {
    let mut chat = translate::responses_to_chat(payload)?;
    if let Some(prev_id) = payload.get("previous_response_id").and_then(Value::as_str) {
        match store::get(prev_id) {
            Some(history) => {
                if let Some(messages) = chat.get_mut("messages").and_then(Value::as_array_mut) {
                    let mut merged = history;
                    merged.extend(messages.drain(..));
                    *messages = merged;
                }
            }
            None => return Err(format!("unknown previous_response_id: {prev_id}")),
        }
    }
    Ok(chat)
}

/// 非流式上游调用主链路（Responses 透传版）：选 key / 重试 / 冻结 / 用量，
/// 与翻译版 `call_upstream_responses` 行为一致，仅端点不同且响应原样透传。
/// `endpoint_path` 为端点路径（`/responses` 或 `/responses/compact`）。
async fn call_responses_passthrough(
    app: &AppState,
    alias: ModelAlias,
    session_id: Option<String>,
    payload: Value,
    endpoint_path: &str,
) -> Result<Response, CallError> {
    let responses_base = alias.responses_base_url.as_deref().unwrap_or("");
    if responses_base.trim().is_empty() {
        return Ok(json_status(
            StatusCode::BAD_GATEWAY,
            translate::responses_error(
                "provider responses_base_url is empty; cannot pass through /v1/responses",
                "upstream_error",
            ),
        ));
    }
    let retry_policy = alias.retry_policy.clone();
    let mut tried = HashSet::new();
    let endpoint = format!("{}{}", responses_base.trim_end_matches('/'), endpoint_path);

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
            );
            continue;
        };

        let mut response: Option<reqwest::Response> = None;
        for attempt in 0..2 {
            match app
                .client
                .post(&endpoint)
                .bearer_auth(key_value.clone())
                .header("content-type", "application/json")
                .json(&payload)
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
                );
                continue;
            }
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        let content = serde_json::from_str::<Value>(&body_text).unwrap_or_else(
            |_| json!({ "error": { "message": body_text, "type": "upstream_error" } }),
        );

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
                extract_usage(&content),
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
        record_usage(
            &app.state,
            &alias.alias,
            &usage_key_name(app, &key),
            status,
            extract_usage(&content),
        );
        crate::features::chat::payload::log_upstream_failure(&alias, status, &body_text);

        // 响应原样透传（上游已是 Responses 格式；4xx/5xx 也是 OpenAI/Responses 错误体）
        let mut resp = json_status(status_code(status), content);
        crate::features::chat::upstream::inject_router_headers(resp.headers_mut(), &alias);
        return Ok(resp);
    }
}

/// 非流式上游调用主链路（Responses 翻译版）：选 key / 重试 / 冻结 / 用量，
/// 与 chat 的 `call_upstream` 行为一致，仅在返回端把 body 翻译成 Responses 响应对象。
async fn call_upstream_responses(
    app: &AppState,
    alias: ModelAlias,
    session_id: Option<String>,
    payload: Value,
    original: &Value,
) -> Result<Response, CallError> {
    if alias.base_url.trim().is_empty() {
        return Ok(json_status(
            StatusCode::BAD_GATEWAY,
            translate::responses_error(
                "provider has no chat completions base_url configured (base_url empty); cannot translate /v1/responses",
                "upstream_error",
            ),
        ));
    }
    let retry_policy = alias.retry_policy.clone();
    let mut tried = HashSet::new();
    let upstream_model = alias.upstream_model();

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
            );
            continue;
        };

        let mut response: Option<reqwest::Response> = None;
        for attempt in 0..2 {
            match app
                .client
                .post(format!(
                    "{}/chat/completions",
                    alias.base_url.trim_end_matches('/')
                ))
                .bearer_auth(key_value.clone())
                .header("content-type", "application/json")
                .json(&payload)
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
                );
                continue;
            }
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        let content = serde_json::from_str::<Value>(&body_text).unwrap_or_else(
            |_| json!({ "error": { "message": body_text, "type": "upstream_error" } }),
        );

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
                extract_usage(&content),
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
        record_usage(
            &app.state,
            &alias.alias,
            &usage_key_name(app, &key),
            status,
            extract_usage(&content),
        );
        crate::features::chat::payload::log_upstream_failure(&alias, status, &body_text);

        if status >= 400 {
            // 上游错误体翻译成 Responses 错误
            return Ok(json_status(
                status_code(status),
                translate::upstream_error_to_responses(&body_text),
            ));
        }

        // 成功：翻译成 Responses 响应对象 + 记录 previous_response_id 历史与完整响应
        let response_id = translate::next_id("resp");
        let echo = translate::response_echo_fields(original);
        let body = translate::chat_to_responses(&content, &response_id, &upstream_model, &echo);
        let input_items = translate::extract_input_items(original);
        store::put_full(
            &response_id,
            translate::assistant_chat_messages(&content),
            body.clone(),
            input_items,
        );
        let mut resp = json_status(status_code(status), body);
        crate::features::chat::upstream::inject_router_headers(resp.headers_mut(), &alias);
        return Ok(resp);
    }
}

pub(crate) fn error_response(message: &str, code: &str, status: StatusCode) -> Response {
    json_status(status, translate::responses_error(message, code))
}

pub(crate) fn frozen_response(exc: NoAvailableKeyError) -> Response {
    let mut resp = json_status(
        StatusCode::TOO_MANY_REQUESTS,
        translate::responses_error(&exc.to_string(), "all_keys_frozen"),
    );
    if let Ok(value) = HeaderValue::from_str(&exc.retry_after.to_string()) {
        resp.headers_mut().insert("retry-after", value);
    }
    resp
}

// ---------------------------------------------------------------------------
// 响应生命周期端点：Get / Delete / Cancel / Compact / input_items / input_tokens
//
// 这些端点无 model 字段，无法走 alias 路由；一律基于本进程 store（翻译模式的响应）。
// 透传模式的响应不登记 store，get/delete/cancel 返回 not_found；compact 返回不支持。
// ---------------------------------------------------------------------------

/// `GET /v1/responses/{response_id}`：取完整 Response 对象（仅翻译模式登记过）。
pub(crate) async fn get_response(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    match store::get_response(&response_id) {
        Some(response) => json_status(StatusCode::OK, response),
        None => error_response(
            &format!("response not found: {response_id}"),
            "not_found",
            StatusCode::NOT_FOUND,
        ),
    }
}

/// `DELETE /v1/responses/{response_id}`：删除响应。
pub(crate) async fn delete_response(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    if store::delete(&response_id) {
        json_status(
            StatusCode::OK,
            json!({ "id": response_id, "object": "response", "deleted": true }),
        )
    } else {
        json_status(
            StatusCode::OK,
            json!({ "id": response_id, "object": "response", "deleted": false }),
        )
    }
}

/// `POST /v1/responses/{response_id}/cancel`：取消响应（翻译模式 store 里改 status）。
pub(crate) async fn cancel_response(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    match store::cancel(&response_id) {
        Some(response) => json_status(StatusCode::OK, response),
        None => error_response(
            &format!("response not found: {response_id}"),
            "not_found",
            StatusCode::NOT_FOUND,
        ),
    }
}

/// `POST /v1/responses/compact`：上下文压缩。
///
/// Router 自身无压缩能力；透传模式（alias 配置了 `responses_base_url`，原生支持
/// Responses API）把请求原样转发给上游 `{responses_base_url}/responses/compact`，
/// 是否支持由上游决定（上游不实现则原样返回其 404/501 错误）。
/// 纯翻译模式（仅 chat completions base_url）无压缩能力，返回 501。
pub(crate) async fn compact_response(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    let Some(model_name) = payload.get("model").and_then(Value::as_str) else {
        return error_response(
            "compact requires a model parameter",
            "invalid_request_error",
            StatusCode::BAD_REQUEST,
        );
    };
    let session_id = extract_session_id(&payload, &headers);
    let route_aliases = match app.state.lock() {
        Ok(mut state) => state.route_aliases(model_name, session_id.as_deref()),
        Err(_) => return internal_error("router state lock poisoned"),
    };
    if route_aliases.is_empty() {
        return error_response(
            &format!("unsupported model alias: {model_name}"),
            "unsupported_model",
            StatusCode::NOT_FOUND,
        );
    }
    let mut last_frozen: Option<NoAvailableKeyError> = None;
    for base_alias in route_aliases {
        if !base_alias.supports_responses() {
            continue;
        }
        let alias = match app.state.lock() {
            Ok(mut state) => state.alias_with_runtime_weights(&base_alias),
            Err(_) => return internal_error("router state lock poisoned"),
        };
        let upstream_payload = translate::prepare_passthrough_payload(&payload, &alias);
        match call_responses_passthrough(
            &app,
            alias,
            session_id.clone(),
            upstream_payload,
            "/responses/compact",
        )
        .await
        {
            Ok(response) => return response,
            Err(CallError::NoAvailable(exc)) => last_frozen = Some(exc),
        }
    }
    if let Some(exc) = last_frozen {
        return frozen_response(exc);
    }
    json_status(
        StatusCode::NOT_IMPLEMENTED,
        translate::responses_error(
            "compact is not supported by the router (no responses-native upstream available; add a provider with responses_base_url)",
            "unsupported_feature",
        ),
    )
}

/// `GET /v1/responses/{response_id}/input_items`：列出该响应的输入 items。
/// 支持 limit / order query 参数（OpenAI 标准：limit 1-100 默认 20，order asc/desc 默认 desc）。
pub(crate) async fn response_input_items(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
    Query(query): Query<InputItemsQuery>,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    let items = match store::get_input_items(&response_id) {
        Some(items) => items,
        None => {
            return error_response(
                &format!("response not found: {response_id}"),
                "not_found",
                StatusCode::NOT_FOUND,
            );
        }
    };
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let mut items = items;
    if query.order.as_deref().unwrap_or("desc") == "desc" {
        items.reverse();
    }
    items.truncate(limit);
    let (first_id, last_id) = match (items.first(), items.last()) {
        (Some(f), Some(l)) => (
            f.get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            l.get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        _ => (String::new(), String::new()),
    };
    json_status(
        StatusCode::OK,
        json!({
            "object": "list",
            "data": Value::Array(items),
            "first_id": first_id,
            "last_id": last_id,
            "has_more": false,
        }),
    )
}

/// `POST /v1/responses/input_tokens`：估算输入 token 数（近似值，非精确计价）。
pub(crate) async fn response_input_tokens(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    let tokens = translate::estimate_input_tokens(&payload);
    json_status(
        StatusCode::OK,
        json!({ "object": "response.input_tokens", "input_tokens": tokens }),
    )
}
