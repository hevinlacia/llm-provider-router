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
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashSet;

use super::chat::{extract_session_id, validate_auth};
use super::resp::{internal_error, json_status, status_code};

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
            let result = if alias.supports_responses() {
                // 透传：只改写 model 名，发到供应商 Responses 端点
                let upstream_payload = translate::prepare_passthrough_payload(&payload, &alias);
                call_responses_passthrough(&app, alias, session_id.clone(), upstream_payload).await
            } else {
                // 翻译：响应转 chat 请求，响应再翻译回 Responses
                let chat = match &chat_payload {
                    Some(chat) => chat.clone(),
                    None => match translate_request(&payload) {
                        Ok(chat) => chat,
                        Err(message) => {
                            return error_response(
                                &message,
                                "invalid_request_error",
                                StatusCode::BAD_REQUEST,
                            );
                        }
                    },
                };
                let upstream_payload = prepare_upstream_payload(&chat, &alias);
                call_upstream_responses(&app, alias, session_id.clone(), upstream_payload).await
            };
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

/// Responses 请求翻译成 chat completions 请求，并处理 previous_response_id 多轮历史。
fn translate_request(payload: &Value) -> Result<Value, String> {
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
async fn call_responses_passthrough(
    app: &AppState,
    alias: ModelAlias,
    session_id: Option<String>,
    payload: Value,
) -> Result<Response, CallError> {
    let retry_policy = alias.retry_policy.clone();
    let mut tried = HashSet::new();
    let responses_base = alias.responses_base_url.as_deref().unwrap_or("");
    let endpoint = format!("{}/responses", responses_base.trim_end_matches('/'));

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
) -> Result<Response, CallError> {
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

        // 成功：翻译成 Responses 响应对象 + 记录 previous_response_id 历史
        let response_id = translate::next_id("resp");
        let body = translate::chat_to_responses(&content, &response_id, &upstream_model);
        store::put(&response_id, translate::assistant_chat_messages(&content));
        let mut resp = json_status(status_code(status), body);
        crate::features::chat::upstream::inject_router_headers(resp.headers_mut(), &alias);
        return Ok(resp);
    }
}

fn error_response(message: &str, code: &str, status: StatusCode) -> Response {
    json_status(status, translate::responses_error(message, code))
}

fn frozen_response(exc: NoAvailableKeyError) -> Response {
    let mut resp = json_status(
        StatusCode::TOO_MANY_REQUESTS,
        translate::responses_error(&exc.to_string(), "all_keys_frozen"),
    );
    if let Ok(value) = HeaderValue::from_str(&exc.retry_after.to_string()) {
        resp.headers_mut().insert("retry-after", value);
    }
    resp
}
