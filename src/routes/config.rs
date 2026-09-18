//! 通用配置 handler：v1 权重、供应商 base_url、模型别名、token 价格、思考映射、keys、搜索供应商。
//! v2 分层配置管理见 `config_v2.rs`。

use crate::app::AppState;
use crate::search::SearchProvidersFile;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

use super::resp::{bad_request, internal_error, json_status, merge_ok, with_state_json};

pub(crate) async fn api_config_model_aliases(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        Ok(merge_ok(state.model_alias_config_snapshot()))
    })
}

pub(crate) async fn api_config_model_aliases_update(
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
                upstream_model: item.get("upstream_model")?.as_str()?.trim().to_string(),
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
    with_state_json(&app, |state| {
        Ok(merge_ok(state.set_model_aliases(custom_aliases)?))
    })
}

pub(crate) async fn api_config_token_prices(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.token_price_snapshot())))
}

pub(crate) async fn api_config_token_prices_update(
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

pub(crate) async fn api_config_thinking_maps(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.thinking_snapshot())))
}

pub(crate) async fn api_config_thinking_maps_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(maps) = payload.get("maps").and_then(Value::as_array) else {
        return bad_request("maps must be a list");
    };
    let mut parsed = Vec::new();
    for item in maps {
        let Some(model) = item.get("model").and_then(Value::as_str) else {
            return bad_request("each map needs model (string)");
        };
        let level_map = item.get("thinking_level_map").and_then(|v| {
            if v.is_null() {
                return None;
            }
            v.as_object().map(|obj| {
                obj.iter()
                    .map(|(k, v)| {
                        let wire = v.as_str().map(|s| s.to_string());
                        (k.clone(), wire)
                    })
                    .collect::<HashMap<String, Option<String>>>()
            })
        });
        let format = item
            .get("thinking_format")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        parsed.push((model.to_string(), level_map, format));
    }
    with_state_json(&app, |state| Ok(merge_ok(state.set_thinking_maps(parsed)?)))
}

pub(crate) async fn api_config_keys(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(merge_ok(state.key_secret_snapshot()?)))
}

pub(crate) async fn api_config_keys_update(
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

pub(crate) async fn api_config_reload_env(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| state.reload_env())
}

pub(crate) async fn api_config_search_providers(State(app): State<AppState>) -> Response {
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

pub(crate) async fn api_config_search_providers_update(
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
