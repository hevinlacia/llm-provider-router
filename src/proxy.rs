use crate::config::{KeyRef, ModelAlias, Settings};
use crate::router_state::{maybe_freeze_key, parse_retry_after, NoAvailableKeyError, RouterState};
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    settings: Settings,
    state: Arc<Mutex<RouterState>>,
    client: reqwest::Client,
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
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let state = Arc::new(Mutex::new(RouterState::new(settings.clone())?));
    let app_state = AppState {
        settings: settings.clone(),
        state,
        client,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard))
        .route("/dashboard", get(dashboard))
        .route("/api/state", get(api_state))
        .route("/api/usage", get(api_usage))
        .route("/api/usage/reset", post(api_usage_reset))
        .route("/api/frozen/clear", post(api_frozen_clear))
        .route(
            "/api/config/weights",
            get(api_config_weights).put(api_config_weights_update),
        )
        .route(
            "/api/config/model-routes",
            get(api_config_model_routes).put(api_config_model_routes_update),
        )
        .route(
            "/api/config/model-aliases",
            get(api_config_model_aliases).put(api_config_model_aliases_update),
        )
        .route("/api/config/v2", get(api_config_v2))
        .route(
            "/api/config/v2/providers",
            put(api_config_v2_providers_update),
        )
        .route(
            "/api/config/v2/logical-models",
            put(api_config_v2_logical_models_update),
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
            "/api/config/keys",
            get(api_config_keys)
                .put(api_config_keys_update)
                .post(api_config_keys_add),
        )
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .nest_service("/assets", ServeDir::new("frontend/dist/assets"))
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

async fn api_config_model_routes(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.route_config_snapshot())))
}

async fn api_config_model_routes_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(routes_obj) = payload.get("routes").and_then(Value::as_object) else {
        return bad_request("routes must be an object");
    };
    let mut routes = HashMap::new();
    for (name, value) in routes_obj {
        let Some(target) = value.get("target").and_then(Value::as_str) else {
            continue;
        };
        let fallbacks = value
            .get("fallbacks")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        routes.insert(
            name.clone(),
            crate::config::ModelRoute {
                target: target.to_string(),
                fallbacks,
            },
        );
    }
    with_state_json(&app, |state| {
        state.set_model_routes(routes)?;
        Ok(merge_ok(state.route_config_snapshot()))
    })
}

async fn api_config_model_aliases(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.model_alias_config_snapshot())))
}

async fn api_config_v2(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(state.v2_status()))
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
    let Some(new_name) = provider.get("name").and_then(Value::as_str) else {
        return bad_request("provider.name (string) is required");
    };
    let Some(base_url) = provider.get("base_url").and_then(Value::as_str) else {
        return bad_request("provider.base_url (string) is required");
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
    match app.state.lock() {
        Ok(mut state) => match state.update_v2_provider(old_name, new_name, base_url, keys) {
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
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return bad_request("name (string) is required");
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
        return bad_request("strategy must be one of: priority, weighted, usage-aware");
    };
    let Some(targets_json) = payload.get("targets").and_then(Value::as_array) else {
        return bad_request("targets (array) is required");
    };
    let mut targets = Vec::new();
    for item in targets_json {
        let Some(model) = item.get("model").and_then(Value::as_str) else {
            return bad_request("each target needs a model (string)");
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
    match app.state.lock() {
        Ok(mut state) => match state.update_v2_logical_model(name, strategy, params, targets) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
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

async fn chat_completions(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
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
    next
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
    let deadline = retry_policy
        .as_ref()
        .map(|policy| Instant::now() + Duration::from_secs(policy.max_retry_seconds));
    let mut tried = HashSet::new();
    let mut last_error: Option<String> = None;
    let mut last_retriable_status: Option<u16> = None;
    let mut last_retriable_content: Option<Value> = None;
    let mut last_retry_after: Option<f64> = None;

    loop {
        let selected_key = match select_key_locked(app, &alias, session_id.as_deref(), &tried) {
            Ok(result) => result,
            Err(message) => return Ok(internal_error(&message)),
        };
        let key = match selected_key {
            Ok(key) => key,
            Err(exc) => {
                if let Some(policy) = retry_policy.as_ref() {
                    if deadline
                        .map(|deadline| Instant::now() < deadline)
                        .unwrap_or(false)
                    {
                        let delay = if last_error.is_some() {
                            2.0
                        } else {
                            compute_retry_delay(
                                policy.retry_delay_seconds,
                                deadline.unwrap(),
                                last_retry_after,
                            )
                        };
                        if delay > 0.0 {
                            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                        }
                        tried.clear();
                        continue;
                    }
                }
                if let (Some(status), Some(content)) =
                    (last_retriable_status, last_retriable_content.clone())
                {
                    return Ok(json_status(status_code(status), content));
                }
                if last_error.is_some() && !tried.is_empty() {
                    return Ok(upstream_unavailable_response(
                        &alias,
                        &tried,
                        last_error.as_deref().unwrap_or("upstream_error"),
                    ));
                }
                return Err(CallError::NoAvailable(exc));
            }
        };
        tried.insert(key.name.clone());

        let key_value = match upstream_key_value_locked(app, &key) {
            Ok(value) => value,
            Err(message) => return Ok(internal_error(&message)),
        };
        let Some(key_value) = key_value else {
            record_usage(&app.state, &alias.alias, &key.name, 599, None);
            last_error = Some("missing_upstream_key".to_string());
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
            Err(exc) => {
                record_usage(&app.state, &alias.alias, &key.name, 599, None);
                last_error = Some(exc.to_string());
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
                &key.name,
                status,
                extract_usage(&content),
            );
            last_retriable_status = Some(status);
            last_retriable_content = Some(content);
            last_retry_after = parse_retry_after(
                headers
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok()),
            );
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
            &key.name,
            status,
            extract_usage(&content),
        );
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
            let deadline = retry_policy.as_ref().map(|policy| Instant::now() + Duration::from_secs(policy.max_retry_seconds));
            let mut last_retry_after: Option<f64> = None;

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
                        if let Some(policy) = retry_policy.as_ref() {
                            if deadline.map(|deadline| Instant::now() < deadline).unwrap_or(false) {
                                let delay = if last_error.is_some() {
                                    2.0
                                } else {
                                    compute_retry_delay(policy.retry_delay_seconds, deadline.unwrap(), last_retry_after)
                                };
                                if delay > 0.0 {
                                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                                }
                                tried.clear();
                                continue;
                            }
                        }
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
                    record_usage(&app.state, &alias.alias, &key.name, 599, None);
                    last_error = Some("missing_upstream_key".to_string());
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
                        record_usage(&app.state, &alias.alias, &key.name, 599, None);
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
                    record_usage(&app.state, &alias.alias, &key.name, status, usage.as_ref());
                    last_retry_after = parse_retry_after(headers.get("retry-after").and_then(|value| value.to_str().ok()));
                    continue;
                }

                let mut body_text = Vec::new();
                let mut bytes_stream = response.bytes_stream();
                while let Some(item) = bytes_stream.next().await {
                    match item {
                        Ok(chunk) => {
                            body_text.extend_from_slice(&chunk);
                            yield Ok(chunk);
                        }
                        Err(exc) => {
                            yield Ok(Bytes::from(stream_error_event(&alias.alias, tried.len(), &exc.to_string())));
                            return;
                        }
                    }
                }
                let body_text = String::from_utf8_lossy(&body_text).to_string();
                freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                let usage = extract_usage_from_stream(&body_text);
                record_usage(&app.state, &alias.alias, &key.name, status, usage.as_ref());
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

fn compute_retry_delay(base_delay: f64, deadline: Instant, last_retry_after: Option<f64>) -> f64 {
    let mut delay = base_delay;
    if let Some(retry_after) = last_retry_after {
        let remaining = retry_after - crate::state_store::now_seconds();
        if remaining > delay {
            delay = remaining;
        }
    }
    let remaining_deadline = deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64();
    delay.min(remaining_deadline)
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

fn upstream_unavailable_response(
    alias: &ModelAlias,
    tried: &HashSet<String>,
    exc: &str,
) -> Response {
    json_status(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
            "error": {
                "message": format!("all {} upstream keys failed for {}", tried.len(), alias.alias),
                "type": "upstream_connect_error",
                "last_error": exc,
            }
        }),
    )
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
