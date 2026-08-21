//! HTTP 响应工具函数（handler 共享）。

use crate::app::AppState;
use crate::features::router::RouterState;
use axum::http::StatusCode;
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
