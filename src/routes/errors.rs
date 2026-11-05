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

/// 上游连接失败统计：用于判断「是不是 OpenClash 透明代理链路不稳」。
/// 与 /api/errors/recent 互补——那边是逐条流水（含各类错误），这边专注连接层
/// 聚合计数（近 1h / 近 24h / 累计）+ 最近错误摘要。内存态，重启归零。
pub(crate) async fn api_errors_connect_stats(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        let (keys, summary) = state.connect_failures_snapshot();
        Ok(merge_ok(json!({
            "summary": summary,
            "keys": keys,
            "note": "仅统计重试后仍失败的上游建连错误（status 599）；内存态，重启归零；单 key 池不冻结但仍计数",
        })))
    })
}
