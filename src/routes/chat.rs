//! OpenAI 兼容入口 handler：`/v1/chat/completions` 与 `/v1/search`（薄层，转发给 features/chat）。

use crate::app::AppState;
use crate::config::Settings;
use crate::features::chat::payload::prepare_upstream_payload;
use crate::features::chat::select::all_keys_frozen_response;
use crate::features::chat::stream::stream_upstream_route;
use crate::features::chat::upstream::{call_upstream, CallError};
use crate::features::router::NoAvailableKeyError;
use crate::search::UnifiedSearchRequest;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use super::resp::{bad_request, internal_error, json_status};

pub(crate) async fn search_completions(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    let req: UnifiedSearchRequest = match serde_json::from_value(payload) {
        Ok(req) => req,
        Err(err) => return bad_request(&format!("invalid search request: {err}")),
    };
    let result = match app.search_pool.lock() {
        Ok(mut pool) => pool.resolve(&req),
        Err(_) => return internal_error("search pool lock poisoned"),
    };
    let resolved = match result {
        Ok(resolved) => resolved,
        Err(err) => {
            return json_status(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "detail": err.to_string() }),
            )
        }
    };
    match crate::search::SearchPool::execute(&resolved, &app.client, &req).await {
        Ok(payload) => json_status(StatusCode::OK, payload),
        Err(err) => json_status(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "detail": err.to_string() }),
        ),
    }
}

pub(crate) async fn chat_completions(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if crate::diag::diag_enabled(&app.settings) {
        // 请求入口埋点（不含消息正文/密钥）：收集期用于对照是 pi 下发参数问题还是上游兼容问题。
        crate::diag::append(
            &app.settings,
            "request.chat_completions",
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
        return bad_request("model must be a string");
    };
    let session_id = extract_session_id(&payload, &headers);
    let route_aliases = match app.state.lock() {
        Ok(mut state) => state.route_aliases(model_name, session_id.as_deref()),
        Err(_) => return internal_error("router state lock poisoned"),
    };
    if route_aliases.is_empty() {
        return json_status(
            StatusCode::NOT_FOUND,
            json!({ "detail": format!("unsupported model alias: {model_name}") }),
        );
    }
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        stream_upstream_route(app, route_aliases, session_id, payload).await
    } else {
        let mut last_frozen: Option<NoAvailableKeyError> = None;
        for base_alias in route_aliases {
            let alias = match app.state.lock() {
                Ok(mut state) => state.alias_with_runtime_weights(&base_alias),
                Err(_) => return internal_error("router state lock poisoned"),
            };
            let upstream_payload = prepare_upstream_payload(&payload, &alias);
            match call_upstream(&app, alias, session_id.clone(), upstream_payload).await {
                Ok(response) => return response,
                Err(CallError::NoAvailable(exc)) => last_frozen = Some(exc),
            }
        }
        if let Some(exc) = last_frozen {
            all_keys_frozen_response(exc)
        } else {
            json_status(
                StatusCode::NOT_FOUND,
                json!({ "detail": format!("unsupported model alias: {model_name}") }),
            )
        }
    }
}

pub(crate) fn validate_auth(settings: &Settings, headers: &HeaderMap) -> Option<Response> {
    let expected_token = settings.local_bearer_token.as_ref()?;
    let expected = format!("Bearer {expected_token}");
    let actual = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if actual == Some(expected.as_str()) {
        None
    } else {
        Some(json_status(
            StatusCode::UNAUTHORIZED,
            json!({ "detail": "invalid local bearer token" }),
        ))
    }
}

pub(crate) fn extract_session_id(payload: &Value, headers: &HeaderMap) -> Option<String> {
    header_str(headers, "x-litellm-session-id")
        .or_else(|| header_str(headers, "x-opencode-session-id"))
        .or_else(|| {
            payload
                .pointer("/metadata/session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/metadata/trace_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/litellm_metadata/session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/litellm_metadata/trace_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub(crate) fn header_str(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
