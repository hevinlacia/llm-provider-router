//! 统一搜索入口：`POST /v1/search`（search pool 编排，与 LLM 代理协议无关）。

use crate::app::AppState;
use crate::search::UnifiedSearchRequest;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use super::resp::{bad_request, internal_error, json_status, validate_auth};

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
