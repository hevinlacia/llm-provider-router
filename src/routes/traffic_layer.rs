//! /v1 客户端流量统计接线：下行字节中间件 + session 归属扩展 + 上行推导。
//!
//! 分工（与 src/traffic.rs 配合）：
//! - **下行**（router→client）：本中间件包住 /v1/* 响应体，逐 chunk 记字节
//!   （位于压缩层之外，SSE 不压缩，非流式 JSON 计压缩后 wire 字节）；
//!   session 归属读响应 extensions 里 handler 塞的 [`TrafficSession`]。
//! - **上行**（client→router）：handler 调 [`request_up_bytes`] 推导后调
//!   `Traffic::record_up`（Content-Length 优先，delta 请求即真实增量字节）。
//! - 全局统计不依赖 session 归属：中间件对每个 /v1/* 响应都记全局下行，
//!   上行由 handler 记（/v1/responses、/v1/messages、/v1/search）。
//!
//! 性能：每 chunk 一次 Mutex 累加（~百 ns），无持久化、无额外分配拷贝。

use crate::app::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;

/// handler 塞进响应 extensions 的 session 归属标记（None = 只进全局）。
#[derive(Clone)]
pub(crate) struct TrafficSession(pub(crate) Option<String>);

/// /v1/* 响应体下行字节统计中间件（全局 + 按 extensions 里的 session 归属）。
/// 挂在整个 Router 最外层（压缩层之外），内部按路径只处理 /v1/*。
pub(crate) async fn traffic_down_middleware(
    State(app): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let is_v1 = request.uri().path().starts_with("/v1/");
    let response = next.run(request).await;
    if !is_v1 {
        return response;
    }
    let session = response
        .extensions()
        .get::<TrafficSession>()
        .and_then(|t| t.0.clone());
    let traffic = Arc::clone(&app.traffic);
    let (parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let counted = async_stream::stream! {
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    traffic.record_down_chunk(session.as_deref(), bytes.len() as u64);
                    yield Ok(bytes);
                }
                Err(err) => {
                    yield Err(err);
                    return;
                }
            }
        }
    };
    Response::from_parts(parts, Body::from_stream(counted))
}

/// 从请求头/载荷推导上行 wire 字节数：Content-Length 优先（delta 请求即真实
/// 增量大小）；chunked 无长度时用载荷序列化长度近似。
pub(crate) fn request_up_bytes(headers: &HeaderMap, payload: &Value) -> u64 {
    headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(|| serde_json::to_vec(payload).map(|v| v.len() as u64).unwrap_or(0))
}
