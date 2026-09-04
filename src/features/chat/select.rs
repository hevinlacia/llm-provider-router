//! key 选择/冻结/用量记录辅助（对 AppState 加锁）。

use crate::app::AppState;
use crate::config::{KeyRef, ModelAlias, Settings};
use crate::features::router::{maybe_freeze_key, NoAvailableKeyError, RouterState};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::routes::resp::json_status;

pub(crate) fn select_key_locked(
    app: &AppState,
    alias: &ModelAlias,
    session_id: Option<&str>,
    tried: &HashSet<String>,
) -> Result<Result<KeyRef, NoAvailableKeyError>, String> {
    app.state
        .lock()
        .map_err(|_| "router state lock poisoned".to_string())
        .map(|mut state| state.select_key_excluding(alias, session_id, tried))
}

pub(crate) fn alias_with_runtime_weights_locked(
    app: &AppState,
    alias: &ModelAlias,
) -> Result<ModelAlias, String> {
    app.state
        .lock()
        .map_err(|_| "router state lock poisoned".to_string())
        .map(|mut state| state.alias_with_runtime_weights(alias))
}

pub(crate) fn upstream_key_value_locked(
    app: &AppState,
    key: &KeyRef,
) -> Result<Option<String>, String> {
    app.state
        .lock()
        .map_err(|_| "router state lock poisoned".to_string())
        .map(|mut state| state.upstream_key_value(key).unwrap_or(None))
}

pub(crate) fn freeze_maybe(
    state: &Arc<Mutex<RouterState>>,
    key: &crate::config::KeyRef,
    status_code: u16,
    headers: &HeaderMap,
    body_text: &str,
    settings: &Settings,
) {
    if let Ok(mut state) = state.lock() {
        let _ = maybe_freeze_key(&mut state, key, status_code, headers, body_text, settings);
    }
}

/// v2 模式下 usage 记录的 key 名带 provider 前缀，避免不同供应商同名 key 合并统计；
/// 非 v2（旧逻辑）保持原名，避免破坏历史数据兼容。
pub(crate) fn usage_key_name(app: &AppState, key: &KeyRef) -> String {
    if app.settings.v2_config_enabled {
        format!("{}/{}", key.provider, key.name)
    } else {
        key.name.clone()
    }
}

pub(crate) fn record_usage(
    state: &Arc<Mutex<RouterState>>,
    model: &str,
    key_name: &str,
    status_code: u16,
    usage: Option<&Value>,
    session_id: Option<&str>,
) {
    if let Ok(mut state) = state.lock() {
        let _ = state.record_usage(model, key_name, status_code, usage, session_id);
    }
}

pub(crate) fn extract_usage(content: &Value) -> Option<&Value> {
    content.get("usage").filter(|value| value.is_object())
}

pub(crate) fn extract_usage_from_stream(body_text: &str) -> Option<Value> {
    let mut usage = None;
    for line in body_text.lines().map(str::trim) {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            if let Some(chunk_usage) = value.get("usage").filter(|item| item.is_object()) {
                usage = Some(chunk_usage.clone());
            }
        }
    }
    usage
}

pub(crate) fn all_keys_frozen_response(exc: NoAvailableKeyError) -> Response {
    let mut response = json_status(
        StatusCode::TOO_MANY_REQUESTS,
        json!({ "error": { "message": exc.to_string(), "type": "all_keys_frozen" } }),
    );
    if let Ok(value) = HeaderValue::from_str(&exc.retry_after.to_string()) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

pub(crate) fn stream_error_event(alias: &str, tried: usize, exc: &str) -> String {
    let error = json!({
        "error": {
            "message": format!("all {tried} upstream keys failed for {alias}"),
            "type": "upstream_connect_error",
            "last_error": exc,
        }
    });
    format!("data: {}\n\ndata: [DONE]\n\n", error)
}
