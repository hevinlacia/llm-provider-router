//! 最近上游报错（ring buffer）查询接口：dashboard 报错页数据源。

use crate::app::AppState;
use crate::features::router::failure::recent_errors_json;
use axum::extract::State;
use axum::response::Response;
use serde_json::json;

use super::resp::{merge_ok, with_state_json};

pub(crate) async fn api_errors_recent(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        Ok(merge_ok(json!({ "errors": recent_errors_json(state) })))
    })
}
