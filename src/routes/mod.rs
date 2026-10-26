//! HTTP 路由层：serve 组装 + handler 薄层 + 共享响应工具。
//!
//! 子模块：
//! - `usage.rs`：health / dashboard / 用量快照
//! - `models.rs`：模型列表 + 动态上下文协商
//! - `config.rs`：v1/通用配置 handler（权重、别名、价格、keys、搜索供应商）
//! - `config_v2.rs`：v2 分层配置管理 handler（供应商/逻辑模型/虚拟模型/物理模型）
//! - `messages.rs`：Anthropic Messages 入口（/v1/messages）
//! - `responses.rs`：Responses API 入口（/v1/responses）
//! - `search.rs`：统一搜索入口（/v1/search）
//! - `resp.rs`：共享响应/鉴权/调用错误工具
//! - `session_fingerprint.rs`：会话标识提取与指纹兜底
//!
//! 历史说明：`/v1/chat/completions`（OpenAI chat 协议）已于 2026-10 下线删除，
//! 客户端走 `/v1/messages`（Anthropic）或 `/v1/responses`；对仅支持 chat
//! completions 的上游仍由 responses/messages 翻译层内部调用。

pub(crate) mod config;
pub(crate) mod config_v2;
pub(crate) mod errors;
pub(crate) mod messages;
pub(crate) mod models;
pub(crate) mod resp;
pub(crate) mod responses;
pub(crate) mod search;
pub(crate) mod session_fingerprint;
pub(crate) mod usage;

use crate::app::AppState;
use crate::config::Settings;
use axum::extract::DefaultBodyLimit;
use axum::http::header::CACHE_CONTROL;
use axum::http::HeaderValue;
use axum::routing::{get, post, put};
use axum::Router;
use serde::Deserialize;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType};
use tower_http::compression::{CompressionLayer, Predicate};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

/// 请求体上限：图片以 base64 内嵌 JSON（约 1.37x 原始大小），2MB 默认限制会直接 413。
/// 64MB 可容纳数张大图；链路上 front-proxy 同样放开（见 front_proxy.rs）。
const BODY_LIMIT: usize = 64 * 1024 * 1024;

pub(crate) use resp::validate_auth;

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    #[serde(default = "default_period")]
    pub period: String,
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UsageSeriesQuery {
    #[serde(default = "default_period")]
    pub period: String,
    pub start: Option<String>,
    pub end: Option<String>,
    #[serde(default = "default_series_bucket")]
    pub bucket: String,
    #[serde(default = "default_series_group_by")]
    pub group_by: String,
    pub top: Option<usize>,
    pub provider: Option<String>,
}

fn default_period() -> String {
    "all".to_string()
}

fn default_series_bucket() -> String {
    "day".to_string()
}

fn default_series_group_by() -> String {
    "model".to_string()
}

pub async fn serve(settings: Settings) -> anyhow::Result<()> {
    let app_state = AppState::new(settings.clone())?;
    // v2 配置热加载：文件变更自动重载，手工编辑 config/*.json 无需 reload-env/重启。
    crate::hot_reload::spawn_watcher(app_state.clone());
    let app = Router::new()
        .route("/analytics", get(usage::dashboard))
        .route("/health", get(usage::health))
        .route("/", get(usage::dashboard))
        .route("/dashboard", get(usage::dashboard))
        .route("/settings", get(usage::dashboard))
        .route("/api/state", get(usage::api_state))
        .route("/api/errors/recent", get(errors::api_errors_recent))
        .route("/api/usage", get(usage::api_usage))
        .route("/api/usage/series", get(usage::api_usage_series))
        .route("/api/sessions/active", get(usage::api_sessions_active))
        .route("/api/usage/reset", post(usage::api_usage_reset))
        .route("/api/frozen/clear", post(usage::api_frozen_clear))
        .route(
            "/api/config/model-aliases",
            get(config::api_config_model_aliases).put(config::api_config_model_aliases_update),
        )
        .route("/api/config/v2", get(config_v2::api_config_v2))
        .route(
            "/api/config/v2/unsupported-keys",
            get(config_v2::api_unsupported_keys).post(config_v2::api_unsupported_keys_refresh),
        )
        .route(
            "/api/config/v2/providers",
            post(config_v2::api_config_v2_providers_create)
                .put(config_v2::api_config_v2_providers_update),
        )
        .route(
            "/api/config/v2/providers/{name}/models",
            get(config_v2::api_config_v2_provider_models),
        )
        .route(
            "/api/config/v2/virtual-models",
            post(config_v2::api_config_v2_virtual_models_upsert)
                .delete(config_v2::api_config_v2_virtual_models_delete),
        )
        .route(
            "/api/config/v2/logical-models",
            post(config_v2::api_config_v2_logical_models_create)
                .put(config_v2::api_config_v2_logical_models_update)
                .delete(config_v2::api_config_v2_logical_models_delete),
        )
        .route(
            "/api/config/token-prices",
            get(config::api_config_token_prices).put(config::api_config_token_prices_update),
        )
        .route(
            "/api/config/dns-overrides",
            get(config::api_config_dns_overrides).put(config::api_config_dns_overrides_update),
        )
        .route(
            "/api/config/thinking-maps",
            get(config::api_config_thinking_maps).put(config::api_config_thinking_maps_update),
        )
        .route(
            "/api/config/v2/physical-models",
            put(config_v2::api_config_physical_models_update),
        )
        .route(
            "/api/config/keys",
            get(config::api_config_keys).put(config::api_config_keys_update),
        )
        .route(
            "/api/config/reload-env",
            post(config::api_config_reload_env),
        )
        .route(
            "/api/config/search-providers",
            get(config::api_config_search_providers)
                .put(config::api_config_search_providers_update),
        )
        .route(
            "/api/config/error-rules",
            get(config::api_config_error_rules).put(config::api_config_error_rules_update),
        )
        .route("/v1/search", post(search::search_completions))
        .route("/v1/models", get(models::models))
        .route("/api/router/capabilities", get(models::router_capabilities))
        .route("/v1/messages", post(messages::messages))
        .route("/v1/responses", post(responses::responses))
        .route(
            "/v1/responses/{response_id}",
            get(responses::get_response).delete(responses::delete_response),
        )
        .route(
            "/v1/responses/{response_id}/cancel",
            post(responses::cancel_response),
        )
        .route("/v1/responses/compact", post(responses::compact_response))
        .route(
            "/v1/responses/{response_id}/input_items",
            get(responses::response_input_items),
        )
        .route(
            "/v1/responses/input_tokens",
            post(responses::response_input_tokens),
        )
        .nest_service(
            "/assets",
            // 静态资源文件名带内容 hash：可永久缓存。部署新版本后 hash 变化，
            // 自然失效；index.html 用 no-cache 每次回源验证，保证拿到新引用。
            ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::overriding(
                    CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=31536000, immutable"),
                ))
                .service(ServeDir::new("frontend/dist/assets")),
        )
        // /llm-router 前缀的同源资源：443 反代按子路径部署后 index.html 引用带前缀资源，
        // 直连 :8789 / 公网 root 入口同样能拿到文件，UI 不因入口不同而白屏。
        .nest_service(
            "/llm-router/assets",
            ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::overriding(
                    CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=31536000, immutable"),
                ))
                .service(ServeDir::new("frontend/dist/assets")),
        )
        .fallback(get(usage::dashboard))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(
            // API/静态资源 gzip：dashboard 轮询（/api/state ~22KB → ~4KB）与
            // JS bundle（~322KB → ~90KB）走 peer-relay 隧道，与 LLM 流共享带宽。
            // 显式排除 text/event-stream（0.6 的 DefaultPredicate 已排除，
            // 写出防上游默认变更）：/v1/messages、/v1/responses 的 SSE 流
            // 被压缩层缓冲会直接破坏流式语义。
            CompressionLayer::new()
                .compress_when(DefaultPredicate::new().and(NotForContentType::SSE)),
        )
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::shutdown::wait_for_shutdown_signal())
        .await?;
    Ok(())
}
