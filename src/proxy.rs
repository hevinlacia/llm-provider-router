use crate::config::{KeyRef, ModelAlias, Settings};
use crate::router_state::{maybe_freeze_key, NoAvailableKeyError, RouterState};
use crate::search::{SearchPool, SearchProvidersFile, UnifiedSearchRequest};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    settings: Settings,
    state: Arc<Mutex<RouterState>>,
    client: reqwest::Client,
    search_pool: Arc<Mutex<SearchPool>>,
}

#[derive(Debug, Deserialize)]
struct UsageQuery {
    #[serde(default = "default_period")]
    period: String,
    start: Option<String>,
    end: Option<String>,
}

fn default_period() -> String {
    "all".to_string()
}

pub async fn serve(settings: Settings) -> anyhow::Result<()> {
    let timeout = Duration::from_secs_f64(settings.request_timeout_seconds);
    // opencode-go (zen/go) 上游用 Cloudflare 拦截非浏览器 UA（error 1010），
    // 全局使用浏览器 UA 以兼容该上游；OpenAI 兼容 API 不校验 UA，无副作用。
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
        .build()?;
    let state = Arc::new(Mutex::new(RouterState::new(settings.clone())?));
    let search_pool = Arc::new(Mutex::new(SearchPool::new(
        &settings.search_providers_path,
    )));
    let app_state = AppState {
        settings: settings.clone(),
        state,
        client,
        search_pool,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard))
        .route("/dashboard", get(dashboard))
        .route("/settings", get(dashboard))
        .route("/api/state", get(api_state))
        .route("/api/usage", get(api_usage))
        .route("/api/usage/reset", post(api_usage_reset))
        .route("/api/frozen/clear", post(api_frozen_clear))
        .route(
            "/api/config/weights",
            get(api_config_weights).put(api_config_weights_update),
        )
        .route(
            "/api/config/model-aliases",
            get(api_config_model_aliases).put(api_config_model_aliases_update),
        )
        .route("/api/config/v2", get(api_config_v2))
        .route(
            "/api/config/v2/providers",
            post(api_config_v2_providers_create).put(api_config_v2_providers_update),
        )
        .route(
            "/api/config/v2/providers/{name}/models",
            get(api_config_v2_provider_models),
        )
        .route(
            "/api/config/v2/virtual-models",
            post(api_config_v2_virtual_models_upsert)
                .delete(api_config_v2_virtual_models_delete),
        )
        .route(
            "/api/config/v2/logical-models",
            post(api_config_v2_logical_models_create)
                .put(api_config_v2_logical_models_update)
                .delete(api_config_v2_logical_models_delete),
        )
        .route(
            "/api/config/providers",
            get(api_config_providers).put(api_config_providers_update),
        )
        .route(
            "/api/config/token-prices",
            get(api_config_token_prices).put(api_config_token_prices_update),
        )
        .route(
            "/api/config/token-prices/apply-equivalents",
            post(api_config_token_prices_apply_equivalents),
        )
        .route(
            "/api/config/model-equivalences",
            get(api_config_model_equivalences).put(api_config_model_equivalences_update),
        )
        .route(
            "/api/config/keys",
            get(api_config_keys)
                .put(api_config_keys_update)
                .post(api_config_keys_add),
        )
        .route(
            "/api/config/search-providers",
            get(api_config_search_providers).put(api_config_search_providers_update),
        )
        .route("/v1/search", post(search_completions))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .nest_service("/assets", ServeDir::new("frontend/dist/assets"))
        .fallback(get(dashboard))
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.snapshot()?)))
}

async fn dashboard() -> Html<String> {
    let index = tokio::fs::read_to_string("frontend/dist/index.html").await.unwrap_or_else(|_| {
        r#"<!doctype html><html><head><title>LLM Provider Router</title></head><body><div id="root">Frontend not built. Run <code>npm install && npm run build</code>.</div></body></html>"#.to_string()
    });
    Html(index)
}

async fn api_state(State(app): State<AppState>, Query(query): Query<UsageQuery>) -> Response {
    with_state_json(&app, |state| {
        let snapshot = state.snapshot()?;
        let usage =
            state.usage_snapshot(&query.period, query.start.as_deref(), query.end.as_deref())?;
        let mut payload = merge_ok(snapshot);
        payload["usage"] = usage;
        Ok(payload)
    })
}

async fn api_usage(State(app): State<AppState>, Query(query): Query<UsageQuery>) -> Response {
    with_state_json(&app, |state| {
        state.usage_snapshot(&query.period, query.start.as_deref(), query.end.as_deref())
    })
}

async fn models(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    with_state_json(&app, |state| {
        let data = state
            .settings_aliases()
            .values()
            .map(|alias| {
                json!({
                    "id": alias.alias,
                    "object": "model",
                    "created": 0,
                    "owned_by": "llm-provider-router",
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "object": "list", "data": data }))
    })
}

async fn api_usage_reset(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        state.reset_usage()?;
        Ok(json!({ "ok": true, "usage": state.usage_snapshot("all", None, None)? }))
    })
}

async fn api_frozen_clear(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        state.clear_frozen()?;
        Ok(merge_ok(state.snapshot()?))
    })
}

async fn api_config_weights(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.key_config_snapshot()?)))
}

async fn api_config_weights_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(weights_obj) = payload.get("weights").and_then(Value::as_object) else {
        return bad_request("weights must be an object");
    };
    let weights = weights_obj
        .iter()
        .map(|(name, value)| (name.clone(), value.as_i64().unwrap_or(0)))
        .collect::<HashMap<_, _>>();
    let pool = payload
        .get("pool")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && *value != "__global__")
        .map(str::to_string);
    with_state_json(&app, |state| {
        if let Some(pool) = pool.as_deref() {
            state.set_pool_key_weights(pool, weights)?;
        } else {
            state.set_key_weights(weights)?;
        }
        Ok(merge_ok(state.key_config_snapshot()?))
    })
}

async fn api_config_model_aliases(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.model_alias_config_snapshot())))
}

async fn api_config_v2(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(state.v2_status()))
}

/// 解析 v2 供应商对象：{ name, base_url, keys } -> (name, base_url, keys)。
/// 供新增/编辑供应商 handler 复用，错误返回可直接透传给 bad_request 的文案。
fn parse_v2_provider_body(
    provider: &serde_json::Map<String, serde_json::Value>,
) -> Result<(String, String, HashMap<String, crate::config_v2::V2Key>), String> {
    let Some(new_name) = provider.get("name").and_then(Value::as_str) else {
        return Err("provider.name (string) is required".to_string());
    };
    let Some(base_url) = provider.get("base_url").and_then(Value::as_str) else {
        return Err("provider.base_url (string) is required".to_string());
    };
    let mut keys = HashMap::new();
    if let Some(key_objs) = provider.get("keys").and_then(Value::as_object) {
        for (name, value) in key_objs {
            keys.insert(
                name.clone(),
                crate::config_v2::V2Key {
                    env_var: value
                        .get("env_var")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    weight: value.get("weight").and_then(Value::as_i64).unwrap_or(1),
                    billing_type: value
                        .get("billing_type")
                        .and_then(Value::as_str)
                        .unwrap_or("subscription")
                        .to_string(),
                    enabled: value.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    persist: value.get("persist").and_then(Value::as_bool).unwrap_or(true),
                },
            );
        }
    }
    Ok((new_name.to_string(), base_url.to_string(), keys))
}

async fn api_config_v2_providers_create(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(provider) = payload.get("provider").and_then(Value::as_object) else {
        return bad_request("provider (object) is required");
    };
    let (name, base_url, keys) = match parse_v2_provider_body(provider) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(&message),
    };
    match app.state.lock() {
        Ok(mut state) => match state.create_v2_provider(&name, &base_url, keys) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

async fn api_config_v2_providers_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(old_name) = payload.get("old_name").and_then(Value::as_str) else {
        return bad_request("old_name (string) is required");
    };
    let Some(provider) = payload.get("provider").and_then(Value::as_object) else {
        return bad_request("provider (object) is required");
    };
    let (new_name, base_url, keys) = match parse_v2_provider_body(provider) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(&message),
    };
    match app.state.lock() {
        Ok(mut state) => match state.update_v2_provider(old_name, &new_name, &base_url, keys) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

/// 解析逻辑模型 body：name / strategy / targets / params。
fn parse_logical_model_body(
    payload: &Value,
) -> Result<(String, crate::config_v2::V2Strategy, HashMap<String, serde_json::Value>, Vec<crate::config_v2::V2Target>), String> {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return Err("name (string) is required".to_string());
    };
    let Some(strategy) = payload
        .get("strategy")
        .and_then(Value::as_str)
        .and_then(|s| match s {
            "priority" => Some(crate::config_v2::V2Strategy::Priority),
            "weighted" => Some(crate::config_v2::V2Strategy::Weighted),
            "usage-aware" => Some(crate::config_v2::V2Strategy::UsageAware),
            _ => None,
        })
    else {
        return Err("strategy must be one of: priority, weighted, usage-aware".to_string());
    };
    let Some(targets_json) = payload.get("targets").and_then(Value::as_array) else {
        return Err("targets (array) is required".to_string());
    };
    let mut targets = Vec::new();
    for item in targets_json {
        let Some(model) = item.get("model").and_then(Value::as_str) else {
            return Err("each target needs a model (string)".to_string());
        };
        targets.push(crate::config_v2::V2Target {
            model: model.trim().to_string(),
            weight: item.get("weight").and_then(Value::as_i64),
        });
    }
    let params = payload
        .get("params")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    Ok((name.to_string(), strategy, params, targets))
}

async fn api_config_v2_logical_models_create(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let (name, strategy, params, targets) = match parse_logical_model_body(&payload) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(&message),
    };
    match app.state.lock() {
        Ok(mut state) => match state.create_v2_logical_model(&name, strategy, params, targets) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

async fn api_config_v2_logical_models_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let (name, strategy, params, targets) = match parse_logical_model_body(&payload) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(&message),
    };
    match app.state.lock() {
        Ok(mut state) => match state.update_v2_logical_model(&name, strategy, params, targets) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

async fn api_config_v2_logical_models_delete(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return bad_request("name (string) is required");
    };
    match app.state.lock() {
        Ok(mut state) => match state.delete_v2_logical_model(name) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

/// 供应商模型列表：
/// - 无 `?refresh=1`：返回本地缓存（若有），否则实时拉取并持久化。
/// - 带 `?refresh=1`：强制实时拉取供应商 `/models` 并持久化到 config/provider-models.json。
async fn api_config_v2_provider_models(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let refresh = params.get("refresh").map(|v| v == "1" || v == "true").unwrap_or(false);

    // 1. 未强制刷新时先查本地缓存
    if !refresh {
        let cached = crate::config_v2::load_provider_models_file(&app.settings.provider_models_path);
        if let Some(entry) = cached.providers.get(&name) {
            if entry.error.is_none() {
                return json_status(
                    StatusCode::OK,
                    json!({
                        "ok": true,
                        "provider": name,
                        "cached": true,
                        "models": entry.models,
                        "fetched_at": entry.fetched_at,
                    }),
                );
            }
        }
    }

    // 2. 取 provider 信息（base_url + enabled key env_var）
    let probe = match app.state.lock() {
        Ok(state) => state.v2_provider_probe(&name),
        Err(_) => return internal_error("router state lock poisoned"),
    };
    let Some((base_url, env_vars)) = probe else {
        return json_status(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "error": format!("unknown v2 provider: {name}"),
            }),
        );
    };
    if env_vars.is_empty() {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": format!("provider {name} has no keys configured") }),
        );
    }

    // 3. 逐个 key 尝试拉取 /models
    let mut last_error: Option<String> = None;
    let mut models: Option<Vec<String>> = None;
    for env_var in &env_vars {
        let key = std::env::var(env_var).ok().filter(|v| !v.is_empty());
        let Some(key) = key else {
            last_error = Some(format!("env var {env_var} is not set"));
            continue;
        };
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let result = app
            .client
            .get(&url)
            .bearer_auth(key)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(value) => {
                        let list = value
                            .get("data")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|m| {
                                        m.get("id")
                                            .and_then(Value::as_str)
                                            .map(str::to_string)
                                    })
                                    .collect::<Vec<String>>()
                            })
                            .unwrap_or_default();
                        if list.is_empty() {
                            last_error = Some(format!("provider {name} returned empty model list"));
                        } else {
                            models = Some(list);
                            break;
                        }
                    }
                    Err(err) => {
                        last_error = Some(format!("failed to parse /models response: {err}"));
                    }
                }
            }
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                let message = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| body.chars().take(200).collect());
                last_error = Some(format!("upstream {status}: {message}"));
            }
            Err(err) => {
                last_error = Some(format!("request failed: {err}"));
            }
        }
    }

    let fetched_at = crate::state_store::now_seconds();
    // 4. 持久化（成功写 models，失败写 error）
    let mut file = crate::config_v2::load_provider_models_file(&app.settings.provider_models_path);
    match models {
        Some(list) => {
            let mut list = list;
            list.sort();
            list.dedup();
            file.providers.insert(
                name.clone(),
                crate::config_v2::ProviderModelsEntry {
                    models: list.clone(),
                    fetched_at: Some(fetched_at),
                    error: None,
                },
            );
            let _ = crate::config_v2::write_provider_models_file(
                &app.settings.provider_models_path,
                &file,
            );
            json_status(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "provider": name,
                    "cached": false,
                    "models": list,
                    "fetched_at": fetched_at,
                }),
            )
        }
        None => {
            let error = last_error.unwrap_or_else(|| "unknown error".to_string());
            file.providers.insert(
                name.clone(),
                crate::config_v2::ProviderModelsEntry {
                    models: Vec::new(),
                    fetched_at: Some(fetched_at),
                    error: Some(error.clone()),
                },
            );
            let _ = crate::config_v2::write_provider_models_file(
                &app.settings.provider_models_path,
                &file,
            );
            json_status(
                StatusCode::BAD_GATEWAY,
                json!({
                    "ok": false,
                    "provider": name,
                    "error": error,
                }),
            )
        }
    }
}

/// 新增/更新虚拟模型映射：`{ name, provider, upstream_model }`。
async fn api_config_v2_virtual_models_upsert(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return bad_request("name (string) is required");
    };
    let Some(provider) = payload.get("provider").and_then(Value::as_str) else {
        return bad_request("provider (string) is required");
    };
    let Some(upstream_model) = payload.get("upstream_model").and_then(Value::as_str) else {
        return bad_request("upstream_model (string) is required");
    };
    match app.state.lock() {
        Ok(mut state) => {
            match state.upsert_v2_virtual_model(name, provider, upstream_model) {
                Ok(value) => json_status(StatusCode::OK, value),
                Err(err) => bad_request(&err.to_string()),
            }
        }
        Err(_) => internal_error("router state lock poisoned"),
    }
}

/// 删除虚拟模型映射：`{ name, provider }`。
async fn api_config_v2_virtual_models_delete(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return bad_request("name (string) is required");
    };
    let Some(provider) = payload.get("provider").and_then(Value::as_str) else {
        return bad_request("provider (string) is required");
    };
    match app.state.lock() {
        Ok(mut state) => {
            match state.delete_v2_virtual_model_mapping(name, provider) {
                Ok(value) => json_status(StatusCode::OK, value),
                Err(err) => bad_request(&err.to_string()),
            }
        }
        Err(_) => internal_error("router state lock poisoned"),
    }
}

async fn api_config_model_aliases_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(aliases) = payload.get("custom_aliases").and_then(Value::as_array) else {
        return bad_request("custom_aliases must be a list");
    };
    let custom_aliases = aliases
        .iter()
        .filter_map(|item| {
            Some(crate::json_config::CustomModelAlias {
                alias: item.get("alias")?.as_str()?.trim().to_string(),
                upstream_model: item
                    .get("upstream_model")?
                    .as_str()?
                    .trim()
                    .to_string(),
                provider: item.get("provider")?.as_str()?.trim().to_string(),
                max_retry_seconds: item
                    .get("max_retry_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(300),
                retry_delay_seconds: item
                    .get("retry_delay_seconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(5.0),
            })
        })
        .collect::<Vec<_>>();
    with_state_json(&app, |state| Ok(merge_ok(state.set_model_aliases(custom_aliases)?)))
}

async fn api_config_providers(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.provider_config_snapshot())))
}

async fn api_config_providers_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(providers_obj) = payload.get("providers").and_then(Value::as_object) else {
        return bad_request("providers must be an object");
    };
    let providers = providers_obj
        .iter()
        .filter_map(|(name, value)| value.as_str().map(|url| (name.clone(), url.to_string())))
        .collect::<HashMap<_, _>>();
    with_state_json(&app, |state| {
        Ok(merge_ok(state.set_provider_base_urls(providers)?))
    })
}

async fn api_config_token_prices(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.token_price_snapshot())))
}

async fn api_config_token_prices_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(models) = payload.get("models").and_then(Value::as_array) else {
        return bad_request("models must be a list");
    };
    let prices = models
        .iter()
        .filter_map(|item| {
            let model = item.get("model")?.as_str()?.to_string();
            Some((
                model,
                crate::json_config::TokenPrice {
                    input_uncached_per_million: item
                        .get("input_uncached_per_million")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    input_cached_per_million: item
                        .get("input_cached_per_million")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    output_per_million: item
                        .get("output_per_million")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                },
            ))
        })
        .collect::<HashMap<_, _>>();
    with_state_json(&app, |state| Ok(merge_ok(state.set_token_prices(prices)?)))
}

async fn api_config_token_prices_apply_equivalents(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(model) = payload.get("model").and_then(Value::as_str) else {
        return bad_request("model (string) is required");
    };
    let only_missing = payload.get("only_missing").and_then(Value::as_bool).unwrap_or(false);
    with_state_json(&app, |state| Ok(merge_ok(state.apply_price_to_equivalents(model, only_missing)?)))
}

async fn api_config_model_equivalences(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.equivalences_snapshot())))
}

async fn api_config_model_equivalences_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(groups) = payload.get("groups").and_then(Value::as_array) else {
        return bad_request("groups must be a list");
    };
    let parsed: Vec<crate::json_config::EquivalenceGroup> = groups.iter().filter_map(|g| {
        Some(crate::json_config::EquivalenceGroup {
            id: g.get("id")?.as_str()?.to_string(),
            display_name: g.get("display_name")?.as_str()?.to_string(),
            models: g.get("models")?.as_array()?.iter().filter_map(|m| m.as_str().map(|s| s.to_string())).collect(),
        })
    }).collect();
    with_state_json(&app, |state| Ok(merge_ok(state.set_equivalences(parsed)?)))
}

async fn api_config_keys(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.key_secret_snapshot()?)))
}

async fn api_config_keys_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let values = payload
        .get("keys")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .map(|value| (name.clone(), value.to_string()))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let delete_names = payload
        .get("delete")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    with_state_json(&app, |state| {
        Ok(merge_ok(state.set_key_values(values, delete_names)?))
    })
}

async fn api_config_keys_add(State(app): State<AppState>, Json(payload): Json<Value>) -> Response {
    let Some(aliases) = payload.get("aliases").and_then(Value::as_array) else {
        return bad_request("aliases must be a list");
    };
    let aliases = aliases
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let value = payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let weight = payload.get("weight").and_then(Value::as_i64).unwrap_or(1);
    with_state_json(&app, |state| {
        Ok(merge_ok(
            state.add_key_to_pools(name, value, aliases, weight)?,
        ))
    })
}

async fn search_completions(
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

async fn api_config_search_providers(State(app): State<AppState>) -> Response {
    match app.search_pool.lock() {
        Ok(mut pool) => {
            let file = pool.get();
            let mut view = serde_json::Map::new();
            for (name, provider) in &file.providers {
                let mut keys = serde_json::Map::new();
                for (key_name, key) in &provider.keys {
                    keys.insert(
                        key_name.clone(),
                        json!({
                            "env_var": key.env_var,
                            "weight": key.weight,
                            "enabled": key.enabled,
                            "configured": pool.key_value(&key.env_var).is_some(),
                        }),
                    );
                }
                view.insert(
                    name.clone(),
                    json!({
                        "base_url": provider.base_url,
                        "keys": keys,
                    }),
                );
            }
            json_status(StatusCode::OK, json!({ "ok": true, "providers": view }))
        }
        Err(_) => internal_error("search pool lock poisoned"),
    }
}

async fn api_config_search_providers_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let file: SearchProvidersFile = match serde_json::from_value(payload) {
        Ok(file) => file,
        Err(err) => return bad_request(&format!("invalid search providers config: {err}")),
    };
    for name in file.providers.keys() {
        if crate::search::SearchProviderKind::from_name(name).is_none() {
            return bad_request(&format!(
                "provider '{name}' is not a known search provider (tavily/exa/brave)"
            ));
        }
    }
    match app.search_pool.lock() {
        Ok(mut pool) => match pool.set(file) {
            Ok(file) => {
                let mut view = serde_json::Map::new();
                for (name, provider) in &file.providers {
                    let mut keys = serde_json::Map::new();
                    for (key_name, key) in &provider.keys {
                        keys.insert(
                            key_name.clone(),
                            json!({
                                "env_var": key.env_var,
                                "weight": key.weight,
                                "enabled": key.enabled,
                                "configured": pool.key_value(&key.env_var).is_some(),
                            }),
                        );
                    }
                    view.insert(
                        name.clone(),
                        json!({ "base_url": provider.base_url, "keys": keys }),
                    );
                }
                json_status(StatusCode::OK, json!({ "ok": true, "providers": view }))
            }
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("search pool lock poisoned"),
    }
}

async fn chat_completions(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if crate::diag::diag_enabled(&app.settings) {
        // 请求入口埋点（不含消息正文/密钥）：收集期用于对照是 pi 下发参数问题还是上游兼容问题。
        crate::diag::append(&app.settings, "request.chat_completions", serde_json::json!({
            "model": payload.get("model").and_then(Value::as_str).unwrap_or(""),
            "summary": crate::diag::payload_summary(&payload),
        }));
    }
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    let Some(model_name) = payload.get("model").and_then(Value::as_str) else {
        return bad_request("model must be a string");
    };
    let session_id = extract_session_id(&payload, &headers);
    let route_aliases = match app.state.lock() {
        Ok(mut state) => state.route_aliases(model_name, session_id.as_deref()),
        Err(_) => return internal_error("router state lock poisoned"),
    };
    if route_aliases.is_empty() {
        return json_status(
            StatusCode::NOT_FOUND,
            json!({ "detail": format!("unsupported model alias: {model_name}") }),
        );
    }
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        stream_upstream_route(app, route_aliases, session_id, payload).await
    } else {
        let mut last_frozen: Option<NoAvailableKeyError> = None;
        for base_alias in route_aliases {
            let alias = match app.state.lock() {
                Ok(mut state) => state.alias_with_runtime_weights(&base_alias),
                Err(_) => return internal_error("router state lock poisoned"),
            };
            let upstream_payload = prepare_upstream_payload(&payload, &alias);
            match call_upstream(&app, alias, session_id.clone(), upstream_payload).await {
                Ok(response) => return response,
                Err(CallError::NoAvailable(exc)) => last_frozen = Some(exc),
            }
        }
        if let Some(exc) = last_frozen {
            all_keys_frozen_response(exc)
        } else {
            json_status(
                StatusCode::NOT_FOUND,
                json!({ "detail": format!("unsupported model alias: {model_name}") }),
            )
        }
    }
}

fn validate_auth(settings: &Settings, headers: &HeaderMap) -> Option<Response> {
    let expected_token = settings.local_bearer_token.as_ref()?;
    let expected = format!("Bearer {expected_token}");
    let actual = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if actual == Some(expected.as_str()) {
        None
    } else {
        Some(json_status(
            StatusCode::UNAUTHORIZED,
            json!({ "detail": "invalid local bearer token" }),
        ))
    }
}

fn extract_session_id(payload: &Value, headers: &HeaderMap) -> Option<String> {
    header_str(headers, "x-litellm-session-id")
        .or_else(|| header_str(headers, "x-opencode-session-id"))
        .or_else(|| {
            payload
                .pointer("/metadata/session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/metadata/trace_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/litellm_metadata/session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .pointer("/litellm_metadata/trace_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn header_str(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn is_muse_spark(alias: &ModelAlias) -> bool {
    alias.upstream_model().contains("muse-spark")
        || alias.provider() == "opencode-go"
        || alias.base_url.contains("opencode")
}

fn prepare_upstream_payload(payload: &Value, alias: &ModelAlias) -> Value {
    let mut next = payload.clone();
    next["model"] = Value::String(alias.upstream_model().to_string());
    // 服务器侧默认/覆写参数（v2 逻辑模型默认 + 物理模型覆写的合并结果）：
    // 只填充客户端未提供的字段，不覆盖客户端显式参数。
    for (key, value) in &alias.params {
        if next.get(key).is_none() {
            next[key] = value.clone();
        }
    }
    if let Some(messages) = next.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                message["role"] = Value::String("system".to_string());
            }
        }
    }
    // 上游兼容归一化：按供应商做最小改写（不覆盖客户端显式语义，必然 400 才改）。
    // DeepSeek 官方需 normalize_deepseek_official；muse-spark 为非推理模型，需记录 xhigh 下发的 thinking。
    let diag_settings = crate::config::load_settings().unwrap_or_else(|_| crate::config::Settings {
        host: "".into(), port: 0, session_ttl_seconds: 0.0,
        monthly_quota_fallback_seconds: 0.0, five_hour_quota_fallback_seconds: 0.0,
        request_timeout_seconds: 0.0, local_bearer_token: None,
        usage_db_path: "".into(), state_db_path: "".into(),
        weight_config_path: "".into(), provider_config_path: "".into(),
        custom_key_config_path: "".into(), api_keys_path: "".into(),
        token_price_config_path: "".into(), model_alias_config_path: "".into(),
        search_providers_path: "".into(), model_equivalences_path: "".into(),
        provider_models_path: "".into(), auth_invalid_freeze_seconds: 0.0,
        v2_config_enabled: false,
        diag_dir: crate::config::DEFAULT_DIAG_DIR.to_string(),
        diag_max_bytes: 10*1024*1024, diag_max_files: 50, diag_sample_every: 1,
    });
    // 收集期：记录原始 payload 的关键字段（不含消息正文/密钥），便于事后定位是协议兼容还是模型能力问题。
    if is_muse_spark(alias) && (payload.get("thinking").is_some() || payload.get("reasoning_effort").is_some() || payload.get("reasoning").is_some()) {
        crate::diag::append(&diag_settings, "normalize.muse_spark.thinking_seen", json!({
            "alias": alias.alias, "provider": alias.provider(), "upstream_model": alias.upstream_model(),
            "summary_before": crate::diag::payload_summary(payload),
        }));
    }
    if is_deepseek_official(alias) {
        let before_summary = crate::diag::payload_summary(&next);
        normalize_deepseek_official(&mut next);
        if before_summary != crate::diag::payload_summary(&next) {
            crate::diag::append(&diag_settings, "normalize.deepseek.applied", json!({
                "alias": alias.alias, "provider": alias.provider(), "upstream_model": alias.upstream_model(),
                "before": before_summary, "after": crate::diag::payload_summary(&next),
            }));
        }
    }
    // muse-spark 收集期：仅观测，不改写（待收集期结束若证据充分再在此剥离 thinking/reasoning_effort）。
    next
}

fn is_deepseek_official(alias: &ModelAlias) -> bool {
    alias.provider() == "deepseek-official" || alias.base_url.contains("deepseek.com")
}

/// DeepSeek 官方 API (api.deepseek.com) 比 ark 等供应商更严格的 OpenAI 兼容校验：
/// - response_format=json_object 要求 prompt 中出现 "json" 字样，否则 400；
/// - response_format=json_schema 不支持，直接 400；
/// - n>1 不支持，仅支持 n=1；
/// - thinking 模式下强制 tool_choice（required / function 对象）或 assistant 的
///   tool_calls 未回传 reasoning_content 都会 400。
/// 归一化原则：只在请求必然 400 时才改写，尽量保留客户端语义。
fn normalize_deepseek_official(next: &mut Value) {
    // 1) response_format: json_schema -> json_object（DeepSeek 不支持 json_schema）
    let wants_json = matches!(
        next.get("response_format")
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str),
        Some("json_object") | Some("json_schema")
    );
    if let Some(rf) = next.get_mut("response_format") {
        if rf.get("type").and_then(Value::as_str) == Some("json_schema") {
            rf["type"] = Value::String("json_object".to_string());
        }
    }
    // 2) json_object 模式要求 prompt 含 "json" 字样；缺省时补 system 提示词
    if wants_json && !prompt_mentions_json(next) {
        ensure_json_hint(next);
    }
    // 3) DeepSeek 仅支持 n=1
    if next.get("n").and_then(Value::as_i64).unwrap_or(1) > 1 {
        next["n"] = json!(1);
    }
    // 4) thinking 模式下强制 tool_choice / 未回传 reasoning_content 必 400：
    //    检测到这类请求时禁用 thinking（保留工具调用契约，绕开校验）。
    if thinking_enabled(next)
        && (forced_tool_choice(next) || assistant_tool_calls_missing_reasoning(next))
    {
        next["thinking"] = json!({ "type": "disabled" });
    }
}

fn prompt_mentions_json(next: &Value) -> bool {
    next.get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter().any(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .map(|s| s.to_ascii_lowercase().contains("json"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn ensure_json_hint(next: &mut Value) {
    const HINT: &str = "Respond in JSON format.";
    if let Some(messages) = next.get_mut("messages").and_then(Value::as_array_mut) {
        if let Some(first) = messages.first_mut() {
            if first.get("role").and_then(Value::as_str) == Some("system") {
                if let Some(content) = first.get_mut("content").and_then(|v| v.as_str()) {
                    first["content"] = Value::String(format!("{HINT}\n{content}"));
                    return;
                }
            }
        }
        messages.insert(0, json!({ "role": "system", "content": HINT }));
    }
}

/// DeepSeek 推理模型默认 thinking 开启；只有显式 disabled 才算关闭。
fn thinking_enabled(next: &Value) -> bool {
    next.get("thinking")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
        .map(|t| t != "disabled")
        .unwrap_or(true)
}

/// tool_choice 除 "auto" / "none" 外（required / function 对象）在 thinking 模式下都会被 DeepSeek 拒绝。
fn forced_tool_choice(next: &Value) -> bool {
    match next.get("tool_choice") {
        Some(Value::String(s)) => !matches!(s.as_str(), "auto" | "none"),
        Some(v) if v.is_object() => true,
        _ => false,
    }
}

/// assistant 消息带 tool_calls 但没回传 reasoning_content，thinking 模式下 DeepSeek 会 400。
fn assistant_tool_calls_missing_reasoning(next: &Value) -> bool {
    next.get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter().any(|m| {
                m.get("role").and_then(Value::as_str) == Some("assistant")
                    && m.get("tool_calls").is_some()
                    && m.get("reasoning_content").is_none()
            })
        })
        .unwrap_or(false)
}

/// 记录上游 4xx/5xx 失败：stderr（journal）+ 诊断文件（持久化，journal 损坏时仍可回看）。
/// 只记录 provider/model/status 和错误信息摘要，不输出请求正文/密钥。
fn log_upstream_failure(alias: &ModelAlias, status: u16, body_text: &str) {
    if status < 400 {
        return;
    }
    let message = serde_json::from_str::<Value>(body_text)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let cleaned = body_text
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>();
            cleaned.chars().take(300).collect()
        });
    eprintln!(
        "llm-provider-router upstream_failure provider={} model={} status={} error={}",
        alias.provider(),
        alias.upstream_model(),
        status,
        message
    );
    // 持久化到诊断文件（best-effort，失败不影响主链路）。
    if let Ok(settings) = crate::config::load_settings() {
        if crate::diag::diag_enabled(&settings) {
            crate::diag::append(&settings, "upstream.failure", serde_json::json!({
                "provider": alias.provider(),
                "model": alias.upstream_model(),
                "alias": alias.alias,
                "status": status,
                "error": message.chars().take(500).collect::<String>(),
            }));
        }
    }
}

enum CallError {
    NoAvailable(NoAvailableKeyError),
}

async fn call_upstream(
    app: &AppState,
    alias: ModelAlias,
    session_id: Option<String>,
    payload: Value,
) -> Result<Response, CallError> {
    let retry_policy = alias.retry_policy.clone();
    let mut tried = HashSet::new();

    loop {
        let selected_key = match select_key_locked(app, &alias, session_id.as_deref(), &tried) {
            Ok(result) => result,
            Err(message) => return Ok(internal_error(&message)),
        };
        let key = match selected_key {
            Ok(key) => key,
            Err(exc) => {
                // key 全部不可用/冻结：不空转重试（retry_policy 的退避只对上游可重试状态码生效），
                // 立即返回 NoAvailable，让上层 for 循环 fallback 到下一个 target。
                return Err(CallError::NoAvailable(exc));
            }
        };
        tried.insert(key.name.clone());

        let key_value = match upstream_key_value_locked(app, &key) {
            Ok(value) => value,
            Err(message) => return Ok(internal_error(&message)),
        };
        let Some(key_value) = key_value else {
            record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
            continue;
        };

        let response = app
            .client
            .post(format!(
                "{}/chat/completions",
                alias.base_url.trim_end_matches('/')
            ))
            .bearer_auth(key_value)
            .header(CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(_) => {
                record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
                continue;
            }
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        let content = serde_json::from_str::<Value>(&body_text).unwrap_or_else(
            |_| json!({ "error": { "message": body_text, "type": "upstream_error" } }),
        );

        if retry_policy
            .as_ref()
            .is_some_and(|policy| policy.retry_on_status.contains(&status))
        {
            freeze_maybe(
                &app.state,
                &key,
                status,
                &headers,
                &body_text,
                &app.settings,
            );
            record_usage(
                &app.state,
                &alias.alias,
                &usage_key_name(&app, &key),
                status,
                extract_usage(&content),
            );
            log_upstream_failure(&alias, status, &body_text);
            continue;
        }

        freeze_maybe(
            &app.state,
            &key,
            status,
            &headers,
            &body_text,
            &app.settings,
        );
        record_usage(
            &app.state,
            &alias.alias,
            &usage_key_name(&app, &key),
            status,
            extract_usage(&content),
        );
        log_upstream_failure(&alias, status, &body_text);
        return Ok(json_status(status_code(status), content));
    }
}

async fn stream_upstream_route(
    app: AppState,
    aliases: Vec<ModelAlias>,
    session_id: Option<String>,
    payload: Value,
) -> Response {
    let stream = async_stream::stream! {
        let mut last_error: Option<String> = None;
        for base_alias in aliases {
            let alias = match alias_with_runtime_weights_locked(&app, &base_alias) {
                Ok(alias) => alias,
                Err(message) => {
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(stream_error_event("router", 0, &message)));
                    return;
                }
            };
            let upstream_payload = prepare_upstream_payload(&payload, &alias);
            let mut tried = HashSet::new();
            let retry_policy = alias.retry_policy.clone();

            loop {
                let selected_key = match select_key_locked(&app, &alias, session_id.as_deref(), &tried) {
                    Ok(result) => result,
                    Err(message) => {
                        yield Ok(Bytes::from(stream_error_event(&alias.alias, tried.len(), &message)));
                        return;
                    }
                };
                let key = match selected_key {
                    Ok(key) => key,
                    Err(_) => {
                        // key 全部不可用/冻结：立即退出当前 alias，外层 for 循环 fallback 到下一个 target。
                        break;
                    }
                };
                tried.insert(key.name.clone());
                let key_value = match upstream_key_value_locked(&app, &key) {
                    Ok(value) => value,
                    Err(message) => {
                        yield Ok(Bytes::from(stream_error_event(&alias.alias, tried.len(), &message)));
                        return;
                    }
                };
                let Some(key_value) = key_value else {
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
                    continue;
                };
                let response = app
                    .client
                    .post(format!("{}/chat/completions", alias.base_url.trim_end_matches('/')))
                    .bearer_auth(key_value)
                    .header(CONTENT_TYPE, "application/json")
                    .json(&upstream_payload)
                    .send()
                    .await;
                let response = match response {
                    Ok(response) => response,
                    Err(exc) => {
                        record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
                        last_error = Some(exc.to_string());
                        continue;
                    }
                };
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                if retry_policy.as_ref().is_some_and(|policy| policy.retry_on_status.contains(&status)) {
                    let body_text = response.text().await.unwrap_or_default();
                    freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                    let usage = extract_usage_from_stream(&body_text).or_else(|| serde_json::from_str::<Value>(&body_text).ok().and_then(|value| extract_usage(&value).cloned()));
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref());
                    log_upstream_failure(&alias, status, &body_text);
                    continue;
                }

                let mut body_text = Vec::new();
                let mut bytes_stream = response.bytes_stream();
                // 兼容不标准上游（如 muse-spark 的 finish_reason 为 null 且无 [DONE]）：
                // 逐行跟踪流中是否出现过标准结束信号，缺失时在流尾补齐，
                // 避免客户端报 "Stream ended without finish_reason"。
                let mut saw_finish_reason = false;
                let mut saw_done = false;
                while let Some(item) = bytes_stream.next().await {
                    match item {
                        Ok(chunk) => {
                            body_text.extend_from_slice(&chunk);
                            let text = String::from_utf8_lossy(&chunk);
                            for line in text.lines() {
                                let trimmed_line = line.trim();
                                let Some(data) = trimmed_line.strip_prefix("data:") else {
                                    continue;
                                };
                                let body = data.trim();
                                if body == "[DONE]" {
                                    saw_done = true;
                                    continue;
                                }
                                if let Ok(value) = serde_json::from_str::<Value>(body) {
                                    if let Some(fr) = value.pointer("/choices/0/finish_reason") {
                                        if fr.as_str().is_some_and(|s| !s.is_empty()) {
                                            saw_finish_reason = true;
                                        }
                                    }
                                }
                            }
                            yield Ok(chunk);
                        }
                        Err(exc) => {
                            yield Ok(Bytes::from(stream_error_event(&alias.alias, tried.len(), &exc.to_string())));
                            return;
                        }
                    }
                }
                // 上游缺失标准结束信号时补齐（仅影响不标准上游，标准上游无额外输出）
                if !saw_finish_reason {
                    yield Ok(Bytes::from(
                        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    ));
                }
                if !saw_done {
                    yield Ok(Bytes::from("data: [DONE]\n\n"));
                }
                let body_text = String::from_utf8_lossy(&body_text).to_string();
                // 流式结束埋点：上游是否缺失 finish_reason/[DONE]，用于量化 muse-spark 类不标准流的影响。
                if crate::diag::diag_enabled(&app.settings) && (!saw_finish_reason || !saw_done) {
                    crate::diag::append(&app.settings, "stream.incomplete_upstream", serde_json::json!({
                        "alias": alias.alias,
                        "provider": alias.provider(),
                        "model": alias.upstream_model(),
                        "status": status,
                        "saw_finish_reason": saw_finish_reason,
                        "saw_done": saw_done,
                        "bytes": body_text.len(),
                    }));
                }
                freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                let usage = extract_usage_from_stream(&body_text);
                record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref());
                log_upstream_failure(&alias, status, &body_text);
                return;
            }
        }
        if let Some(error) = last_error {
            yield Ok(Bytes::from(stream_error_event("router", 0, &error)));
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error("failed to create streaming response"))
}

fn select_key_locked(
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

fn alias_with_runtime_weights_locked(
    app: &AppState,
    alias: &ModelAlias,
) -> Result<ModelAlias, String> {
    app.state
        .lock()
        .map_err(|_| "router state lock poisoned".to_string())
        .map(|mut state| state.alias_with_runtime_weights(alias))
}

fn upstream_key_value_locked(app: &AppState, key: &KeyRef) -> Result<Option<String>, String> {
    app.state
        .lock()
        .map_err(|_| "router state lock poisoned".to_string())
        .map(|mut state| state.upstream_key_value(key).unwrap_or(None))
}

fn freeze_maybe(
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
fn usage_key_name(app: &AppState, key: &KeyRef) -> String {
    if app.settings.v2_config_enabled {
        format!("{}/{}", key.provider, key.name)
    } else {
        key.name.clone()
    }
}

fn record_usage(
    state: &Arc<Mutex<RouterState>>,
    model: &str,
    key_name: &str,
    status_code: u16,
    usage: Option<&Value>,
) {
    if let Ok(mut state) = state.lock() {
        let _ = state.record_usage(model, key_name, status_code, usage);
    }
}

fn extract_usage(content: &Value) -> Option<&Value> {
    content.get("usage").filter(|value| value.is_object())
}

fn extract_usage_from_stream(body_text: &str) -> Option<Value> {
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

fn all_keys_frozen_response(exc: NoAvailableKeyError) -> Response {
    let mut response = json_status(
        StatusCode::TOO_MANY_REQUESTS,
        json!({ "error": { "message": exc.to_string(), "type": "all_keys_frozen" } }),
    );
    if let Ok(value) = HeaderValue::from_str(&exc.retry_after.to_string()) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

fn stream_error_event(alias: &str, tried: usize, exc: &str) -> String {
    let error = json!({
        "error": {
            "message": format!("all {tried} upstream keys failed for {alias}"),
            "type": "upstream_connect_error",
            "last_error": exc,
        }
    });
    format!("data: {}\n\ndata: [DONE]\n\n", error)
}

fn with_state_json(
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

fn merge_ok(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("ok".to_string(), Value::Bool(true));
        value
    } else {
        json!({ "ok": true, "data": value })
    }
}

fn bad_request(message: &str) -> Response {
    json_status(StatusCode::BAD_REQUEST, json!({ "detail": message }))
}

fn internal_error(message: &str) -> Response {
    json_status(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "detail": message }),
    )
}

fn json_status(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deepseek_alias() -> ModelAlias {
        ModelAlias::new(
            "deepseek-v4-flash-auto",
            "openai/deepseek-v4-flash",
            "https://api.deepseek.com",
            vec![KeyRef::env_only(
                "deepseek-official",
                "AGENT_AI_DEEPSEEK_API_KEY",
                1,
                "deepseek-official",
                "payg",
            )],
            None,
        )
    }

    fn ark_alias() -> ModelAlias {
        ModelAlias::new(
            "deepseek-v4-flash-auto",
            "openai/deepseek-v4-flash-260801",
            "https://ark.cn-beijing.volces.com/api/coding/v3",
            vec![KeyRef::new("hevin", "AGENT_AI_ARK_HEVIN_API_KEY", 6)],
            None,
        )
    }

    fn base_payload() -> Value {
        json!({
            "model": "deepseek-v4-flash-auto",
            "messages": [
                { "role": "system", "content": "You are a terse assistant." },
                { "role": "user", "content": "List 2 items." }
            ],
            "max_tokens": 256,
            "stream": true
        })
    }

    #[test]
    fn json_object_without_json_word_gets_hint() {
        let mut payload = base_payload();
        payload["response_format"] = json!({ "type": "json_object" });
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        let sys = out["messages"][0].as_object().unwrap();
        assert_eq!(sys["role"], "system");
        let content = sys["content"].as_str().unwrap();
        assert!(content.to_ascii_lowercase().contains("json"), "system 应包含 json 字样: {content}");
        assert_eq!(out["response_format"]["type"], "json_object");
    }

    #[test]
    fn json_object_with_json_word_unchanged() {
        let mut payload = base_payload();
        payload["messages"][1]["content"] = json!("List 2 items as json.");
        payload["response_format"] = json!({ "type": "json_object" });
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        let first = out["messages"][0]["content"].as_str().unwrap();
        assert!(!first.contains("Respond in JSON format"), "不应插入提示词: {first}");
    }

    #[test]
    fn json_schema_converted_to_json_object_with_hint() {
        let mut payload = base_payload();
        payload["response_format"] = json!({
            "type": "json_schema",
            "json_schema": { "name": "items", "schema": {} }
        });
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert_eq!(out["response_format"]["type"], "json_object");
        let sys = out["messages"][0]["content"].as_str().unwrap();
        assert!(sys.to_ascii_lowercase().contains("json"), "json_schema 转换后应补 json 提示词: {sys}");
    }

    #[test]
    fn n_greater_than_one_clamped_to_one() {
        let mut payload = base_payload();
        payload["n"] = json!(3);
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert_eq!(out["n"], 1);
    }

    #[test]
    fn forced_tool_choice_disables_thinking() {
        let mut payload = base_payload();
        payload["tools"] = json!([{ "type": "function", "function": { "name": "f", "parameters": { "type": "object", "properties": {} } } }]);
        payload["tool_choice"] = json!({ "type": "function", "function": { "name": "f" } });
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn tool_choice_required_disables_thinking() {
        let mut payload = base_payload();
        payload["tools"] = json!([{ "type": "function", "function": { "name": "f", "parameters": { "type": "object", "properties": {} } } }]);
        payload["tool_choice"] = json!("required");
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn tool_choice_auto_keeps_thinking() {
        let mut payload = base_payload();
        payload["tools"] = json!([{ "type": "function", "function": { "name": "f", "parameters": { "type": "object", "properties": {} } } }]);
        payload["tool_choice"] = json!("auto");
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert!(out.get("thinking").is_none());
    }

    #[test]
    fn assistant_tool_calls_missing_reasoning_disables_thinking() {
        let mut payload = base_payload();
        payload["messages"] = json!([
            { "role": "system", "content": "t" },
            { "role": "user", "content": "call f" },
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "f", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "c1", "content": "done" }
        ]);
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn assistant_tool_calls_with_reasoning_keeps_thinking() {
        let mut payload = base_payload();
        payload["messages"] = json!([
            { "role": "system", "content": "t" },
            { "role": "user", "content": "call f" },
            { "role": "assistant", "content": null, "reasoning_content": "thinking...", "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "f", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "c1", "content": "done" }
        ]);
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert!(out.get("thinking").is_none());
    }

    #[test]
    fn explicit_thinking_disabled_not_overridden() {
        let mut payload = base_payload();
        payload["thinking"] = json!({ "type": "disabled" });
        payload["tool_choice"] = json!("required");
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn ark_alias_not_normalized() {
        let mut payload = base_payload();
        payload["response_format"] = json!({ "type": "json_object" });
        payload["n"] = json!(3);
        payload["tool_choice"] = json!("required");
        let out = prepare_upstream_payload(&payload, &ark_alias());
        assert_eq!(out["response_format"]["type"], "json_object");
        assert_eq!(out["n"], 3);
        assert_eq!(out["tool_choice"], "required");
        assert!(out.get("thinking").is_none());
    }

    #[test]
    fn developer_role_converted_and_deepseek_normalized() {
        let mut payload = base_payload();
        payload["messages"][0]["role"] = json!("developer");
        payload["response_format"] = json!({ "type": "json_object" });
        let out = prepare_upstream_payload(&payload, &deepseek_alias());
        assert_eq!(out["messages"][0]["role"], "system");
        assert!(out["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("json"));
    }
}
