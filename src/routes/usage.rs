//! 健康检查 / dashboard / 用量快照 handler。

use crate::app::AppState;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::HeaderValue;
use axum::response::Response;
use serde_json::json;

use super::resp::{merge_ok, with_state_json};
use super::{UsageQuery, UsageSeriesQuery};

pub(crate) async fn health(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.snapshot()?)))
}

pub(crate) async fn dashboard() -> Response {
    let index = tokio::fs::read_to_string("frontend/dist/index.html").await.unwrap_or_else(|_| {
        r#"<!doctype html><html><head><title>LLM Provider Router</title></head><body><div id="root">Frontend not built. Run <code>npm install && npm run build</code>.</div></body></html>"#.to_string()
    });
    // index.html 引用带 hash 的静态资源：内容一变 hash 就变，
    // 用 no-cache（允许缓存但每次回源验证）保证拿到新引用。
    let mut response = Response::new(Body::from(index));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

pub(crate) async fn api_state(
    State(app): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Response {
    with_state_json(&app, |state| {
        let snapshot = state.snapshot()?;
        let usage =
            state.usage_snapshot(&query.period, query.start.as_deref(), query.end.as_deref())?;
        let mut payload = merge_ok(snapshot);
        payload["usage"] = usage;
        Ok(payload)
    })
}

pub(crate) async fn api_usage(
    State(app): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Response {
    with_state_json(&app, |state| {
        state.usage_snapshot(&query.period, query.start.as_deref(), query.end.as_deref())
    })
}

pub(crate) async fn api_usage_series(
    State(app): State<AppState>,
    Query(query): Query<UsageSeriesQuery>,
) -> Response {
    with_state_json(&app, |state| {
        // 供应商维度明细下钻：把 provider 解析成该供应商名下的 key 名集合再过滤
        let key_names = query
            .provider
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| state.key_names_for_provider(p));
        state.usage_series(
            &query.period,
            query.start.as_deref(),
            query.end.as_deref(),
            &query.bucket,
            &query.group_by,
            query.top,
            key_names.as_deref(),
        )
    })
}

pub(crate) async fn api_sessions_active(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| state.active_sessions())
}

pub(crate) async fn api_usage_reset(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        state.reset_usage()?;
        Ok(json!({ "ok": true, "usage": state.usage_snapshot("all", None, None)? }))
    })
}

pub(crate) async fn api_frozen_clear(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        state.clear_frozen()?;
        Ok(merge_ok(state.snapshot()?))
    })
}
