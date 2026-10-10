//! 统一搜索入口：`POST /v1/search`（search pool 编排，与 LLM 代理协议无关）。
//!
//! 编排顺序：`resolve()` 产出 SearchPlan —— 缺省/auto 优先 chrome 渲染搜索
//! （google→bing，无 key），失败/空结果降级 API key 池；显式点名
//! chrome/bing/google 时失败如实报错；显式 tavily/exa/brave 直接走 key 池。

use crate::app::AppState;
use crate::search::{SearchPlan, UnifiedSearchRequest};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use super::resp::{bad_request, internal_error, json_status, validate_auth};

fn has_results(payload: &Value) -> bool {
    payload
        .get("results")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

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
    let plan = match app.search_pool.lock() {
        Ok(mut pool) => pool.resolve(&req),
        Err(_) => return internal_error("search pool lock poisoned"),
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(err) => {
            return json_status(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "detail": err.to_string() }),
            )
        }
    };
    match plan {
        SearchPlan::Api(resolved) => {
            match crate::search::SearchPool::execute(&resolved, &app.client, &req).await {
                Ok(payload) => json_status(StatusCode::OK, payload),
                Err(err) => json_status(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "detail": err.to_string() }),
                ),
            }
        }
        SearchPlan::Chrome {
            cfg,
            engines,
            explicit,
        } => {
            let outcome = crate::search_chrome::search(&cfg, &engines, &app.client, &req).await;
            if let Ok(payload) = &outcome {
                if has_results(payload) {
                    return json_status(StatusCode::OK, payload.clone());
                }
            }
            let chrome_detail = match &outcome {
                Ok(_) => "chrome search returned no results".to_string(),
                Err(err) => err.to_string(),
            };
            if explicit {
                // 请求点名 chrome/bing/google 时失败如实报错，不静默降级。
                return json_status(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "detail": chrome_detail }),
                );
            }
            // auto：chrome 失败/空结果时降级到 API key 池。
            let fallback = match app.search_pool.lock() {
                Ok(mut pool) => pool.resolve_api(&req).map_err(|e| e.to_string()),
                Err(_) => Err("search pool lock poisoned".to_string()),
            };
            match fallback {
                Ok(resolved) => {
                    match crate::search::SearchPool::execute(&resolved, &app.client, &req).await {
                        Ok(payload) if has_results(&payload) => {
                            json_status(StatusCode::OK, payload)
                        }
                        Ok(_) => json_status(
                            StatusCode::SERVICE_UNAVAILABLE,
                            json!({ "detail": format!("chrome search: {chrome_detail}; api fallback returned no results") }),
                        ),
                        Err(err) => json_status(
                            StatusCode::SERVICE_UNAVAILABLE,
                            json!({ "detail": format!("chrome search: {chrome_detail}; api fallback: {err}") }),
                        ),
                    }
                }
                Err(err) => json_status(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({ "detail": format!("chrome search: {chrome_detail}; api fallback unavailable: {err}") }),
                ),
            }
        }
    }
}
