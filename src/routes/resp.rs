//! HTTP 响应工具函数（handler 共享）。

use crate::app::AppState;
use crate::features::router::RouterState;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

pub(crate) fn with_state_json(
    app: &AppState,
    f: impl FnOnce(&mut RouterState) -> anyhow::Result<Value>,
) -> Response {
    match app.state.lock() {
        Ok(mut state) => match f(&mut state) {
            Ok(value) => Json(value).into_response(),
            Err(exc) => bad_request(&exc.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

pub(crate) fn merge_ok(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("ok".to_string(), Value::Bool(true));
        value
    } else {
        json!({ "ok": true, "data": value })
    }
}

pub(crate) fn bad_request(message: &str) -> Response {
    json_status(StatusCode::BAD_REQUEST, json!({ "detail": message }))
}

pub(crate) fn internal_error(message: &str) -> Response {
    json_status(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "detail": message }),
    )
}

pub(crate) fn json_status(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

pub(crate) fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

// ---------------------------------------------------------------------------
// 鉴权与响应头（原 routes/chat.rs，chat 协议下线后迁至共享工具层）
// ---------------------------------------------------------------------------

/// 本地 Bearer 鉴权：未配置 local_bearer_token 时放行；配置后校验
/// `Authorization: Bearer <token>`，不匹配返回 401。
pub(crate) fn validate_auth(
    settings: &crate::config::Settings,
    headers: &HeaderMap,
) -> Option<Response> {
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

/// 非流式上游调用结果：全池不可用时交给上层 fallback 到下一个路由目标。
pub(crate) enum CallError {
    NoAvailable(crate::features::router::NoAvailableKeyError),
}

/// 非流式响应的 router 元信息头（模型/provider/上下文窗口）。
pub(crate) fn inject_router_headers(
    headers: &mut axum::http::HeaderMap,
    alias: &crate::config::ModelAlias,
) {
    use axum::http::HeaderValue;
    headers.insert(
        "x-llm-router-model",
        HeaderValue::from_str(&alias.alias).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers.insert(
        "x-llm-router-upstream-model",
        HeaderValue::from_str(&alias.upstream_model())
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers.insert(
        "x-llm-router-provider",
        HeaderValue::from_str(&alias.provider())
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    if let Some(v) = alias.context_window {
        if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
            headers.insert("x-llm-router-context-window", hv);
        }
    }
    if let Some(v) = alias.max_output_tokens {
        if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
            headers.insert("x-llm-router-max-output", hv);
        }
    }
}
