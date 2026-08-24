//! HTTP 路由层：serve 组装 + handler 薄层 + 共享响应工具。
//!
//! 子模块：
//! - `usage.rs`：health / dashboard / 用量快照
//! - `models.rs`：模型列表 + 动态上下文协商
//! - `config.rs`：配置类 handler
//! - `chat.rs`：OpenAI 兼容入口（转发 features/chat）
//! - `resp.rs`：共享响应工具

pub(crate) mod chat;
pub(crate) mod config;
pub(crate) mod models;
pub(crate) mod resp;
pub(crate) mod usage;

use crate::app::AppState;
use crate::config::Settings;
use axum::routing::{get, post, put};
use axum::Router;
use serde::Deserialize;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

pub(crate) use chat::validate_auth;

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
    let app = Router::new()
        .route("/analytics", get(usage::dashboard))
        .route("/health", get(usage::health))
        .route("/", get(usage::dashboard))
        .route("/dashboard", get(usage::dashboard))
        .route("/settings", get(usage::dashboard))
        .route("/api/state", get(usage::api_state))
        .route("/api/usage", get(usage::api_usage))
        .route("/api/usage/series", get(usage::api_usage_series))
        .route("/api/usage/reset", post(usage::api_usage_reset))
        .route("/api/frozen/clear", post(usage::api_frozen_clear))
        .route(
            "/api/config/weights",
            get(config::api_config_weights).put(config::api_config_weights_update),
        )
        .route(
            "/api/config/model-aliases",
            get(config::api_config_model_aliases).put(config::api_config_model_aliases_update),
        )
        .route("/api/config/v2", get(config::api_config_v2))
        .route(
            "/api/config/v2/providers",
            post(config::api_config_v2_providers_create)
                .put(config::api_config_v2_providers_update),
        )
        .route(
            "/api/config/v2/providers/{name}/models",
            get(config::api_config_v2_provider_models),
        )
        .route(
            "/api/config/v2/virtual-models",
            post(config::api_config_v2_virtual_models_upsert)
                .delete(config::api_config_v2_virtual_models_delete),
        )
        .route(
            "/api/config/v2/logical-models",
            post(config::api_config_v2_logical_models_create)
                .put(config::api_config_v2_logical_models_update)
                .delete(config::api_config_v2_logical_models_delete),
        )
        .route(
            "/api/config/providers",
            get(config::api_config_providers).put(config::api_config_providers_update),
        )
        .route(
            "/api/config/token-prices",
            get(config::api_config_token_prices).put(config::api_config_token_prices_update),
        )
        .route(
            "/api/config/thinking-maps",
            get(config::api_config_thinking_maps).put(config::api_config_thinking_maps_update),
        )
        .route(
            "/api/config/v2/physical-models",
            put(config::api_config_physical_models_update),
        )
        .route(
            "/api/config/keys",
            get(config::api_config_keys)
                .put(config::api_config_keys_update)
                .post(config::api_config_keys_add),
        )
        .route(
            "/api/config/search-providers",
            get(config::api_config_search_providers)
                .put(config::api_config_search_providers_update),
        )
        .route("/v1/search", post(chat::search_completions))
        .route("/v1/models", get(models::models))
        .route("/api/router/capabilities", get(models::router_capabilities))
        .route("/v1/chat/completions", post(chat::chat_completions))
        .nest_service("/assets", ServeDir::new("frontend/dist/assets"))
        .fallback(get(usage::dashboard))
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
