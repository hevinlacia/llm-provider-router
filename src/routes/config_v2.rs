//! v2 分层配置管理 handler：供应商 / 物理模型 / 逻辑模型 / 虚拟模型映射。
//!
//! 从 `config.rs` 按 v2 能力边界拆出：`/api/config/v2/*` 供应商、逻辑模型、
//! 虚拟模型与 `/api/config/v2/physical-models` 批量更新挂载在本模块；
//! v1/通用配置（权重、别名、价格、keys、搜索供应商）仍在 `config.rs`。

use crate::app::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;

use super::resp::{bad_request, internal_error, json_status, with_state_json};

pub(crate) async fn api_config_v2(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| Ok(state.v2_status()))
}

/// GET /api/config/v2/unsupported-keys：key × 模型“不支持”学习记录列表。
pub(crate) async fn api_unsupported_keys(State(app): State<AppState>) -> Response {
    with_state_json(&app, |state| {
        Ok(json!({ "unsupported": state.unsupported_view() }))
    })
}

/// POST /api/config/v2/unsupported-keys/refresh：按前缀过滤重置退避
///（body 可选 { provider?, key?, model? }，缺省维度不限制，全空 = 全部重置）。
pub(crate) async fn api_unsupported_keys_refresh(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let provider = payload.get("provider").and_then(Value::as_str);
    let key = payload.get("key").and_then(Value::as_str);
    let model = payload.get("model").and_then(Value::as_str);
    let locked = app.state.lock();
    match locked {
        Ok(mut state) => match state.refresh_unsupported(provider, key, model) {
            Ok(removed) => json_status(StatusCode::OK, json!({ "ok": true, "removed": removed })),
            Err(err) => internal_error(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

/// 解析 v2 供应商对象：{ name, base_url, keys } -> (name, base_url, keys)。
/// 供新增/编辑供应商 handler 复用，错误返回可直接透传给 bad_request 的文案。
fn parse_v2_provider_body(
    provider: &serde_json::Map<String, serde_json::Value>,
) -> Result<
    (
        String,
        String,
        Option<String>,
        Option<String>,
        HashMap<String, crate::config_v2::V2Key>,
    ),
    String,
> {
    let Some(new_name) = provider.get("name").and_then(Value::as_str) else {
        return Err("provider.name (string) is required".to_string());
    };
    // 三类地址至少填一种：Chat Completions API（base_url） / Responses API（responses_base_url） / Anthropic API（anthropic_base_url）
    let base_url = provider
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let responses_base_url = provider
        .get("responses_base_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    let anthropic_base_url = provider
        .get("anthropic_base_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    if base_url.trim().is_empty() && responses_base_url.is_none() && anthropic_base_url.is_none() {
        return Err(
            "at least one of base_url / responses_base_url / anthropic_base_url must be set"
                .to_string(),
        );
    }
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
                    enabled: value
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    persist: value
                        .get("persist")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                },
            );
        }
    }
    Ok((
        new_name.to_string(),
        base_url.to_string(),
        responses_base_url,
        anthropic_base_url,
        keys,
    ))
}

pub(crate) async fn api_config_v2_providers_create(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(provider) = payload.get("provider").and_then(Value::as_object) else {
        return bad_request("provider (object) is required");
    };
    let (name, base_url, responses_base_url, anthropic_base_url, keys) =
        match parse_v2_provider_body(provider) {
            Ok(parsed) => parsed,
            Err(message) => return bad_request(&message),
        };
    match app.state.lock() {
        Ok(mut state) => {
            match state.create_v2_provider(
                &name,
                &base_url,
                responses_base_url,
                anthropic_base_url,
                keys,
            ) {
                Ok(value) => json_status(StatusCode::OK, value),
                Err(err) => bad_request(&err.to_string()),
            }
        }
        Err(_) => internal_error("router state lock poisoned"),
    }
}

pub(crate) async fn api_config_v2_providers_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(old_name) = payload.get("old_name").and_then(Value::as_str) else {
        return bad_request("old_name (string) is required");
    };
    let Some(provider) = payload.get("provider").and_then(Value::as_object) else {
        return bad_request("provider (object) is required");
    };
    let (new_name, base_url, responses_base_url, anthropic_base_url, keys) =
        match parse_v2_provider_body(provider) {
            Ok(parsed) => parsed,
            Err(message) => return bad_request(&message),
        };
    match app.state.lock() {
        Ok(mut state) => {
            match state.update_v2_provider(
                old_name,
                &new_name,
                &base_url,
                responses_base_url,
                anthropic_base_url,
                keys,
            ) {
                Ok(value) => json_status(StatusCode::OK, value),
                Err(err) => bad_request(&err.to_string()),
            }
        }
        Err(_) => internal_error("router state lock poisoned"),
    }
}

/// 解析逻辑模型 body：name / strategy / targets / params。
fn parse_logical_model_body(
    payload: &Value,
) -> Result<
    (
        String,
        crate::config_v2::V2Strategy,
        HashMap<String, serde_json::Value>,
        Vec<crate::config_v2::V2Target>,
    ),
    String,
> {
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
        let keys = item.get("keys").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<String>>()
        });
        targets.push(crate::config_v2::V2Target {
            model: model.trim().to_string(),
            weight: item.get("weight").and_then(Value::as_i64),
            keys,
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

pub(crate) async fn api_config_v2_logical_models_create(
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

pub(crate) async fn api_config_v2_logical_models_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let (name, strategy, params, targets) = match parse_logical_model_body(&payload) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(&message),
    };
    // 可选 newName：提供且与 name 不同时执行改名（级联更新其他池引用）
    let new_name = payload.get("newName").and_then(Value::as_str);
    match app.state.lock() {
        Ok(mut state) => {
            match state.update_v2_logical_model(&name, new_name, strategy, params, targets) {
                Ok(value) => json_status(StatusCode::OK, value),
                Err(err) => bad_request(&err.to_string()),
            }
        }
        Err(_) => internal_error("router state lock poisoned"),
    }
}

pub(crate) async fn api_config_v2_logical_models_delete(
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
pub(crate) async fn api_config_v2_provider_models(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let refresh = params
        .get("refresh")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    // 1. 未强制刷新时先查本地缓存
    if !refresh {
        let cached =
            crate::config_v2::load_provider_models_file(&app.settings.provider_models_path);
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

    // 2. 取 provider 信息（Chat Completions API 地址 + enabled key env_var）
    let probe = match app.state.lock() {
        Ok(state) => state.v2_provider_probe(&name),
        Err(_) => return internal_error("router state lock poisoned"),
    };
    let Some((base_url, responses_base_url, env_vars)) = probe else {
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
    // 统一走 Chat Completions API（base_url）拉取模型名列表；base_url 未配置时回退 Responses API 端点。
    let models_base = if base_url.trim().is_empty() {
        responses_base_url.as_deref().unwrap_or("").to_string()
    } else {
        base_url.clone()
    };
    if models_base.trim().is_empty() {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": format!("provider {name} has no Chat Completions API base URL") }),
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
        let url = format!("{}/models", models_base.trim_end_matches('/'));
        let result = app.client.get(&url).bearer_auth(key).send().await;
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
                                        m.get("id").and_then(Value::as_str).map(str::to_string)
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
pub(crate) async fn api_config_v2_virtual_models_upsert(
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
        Ok(mut state) => match state.upsert_v2_virtual_model(name, provider, upstream_model) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

/// 删除虚拟模型映射：`{ name, provider }`。
pub(crate) async fn api_config_v2_virtual_models_delete(
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
        Ok(mut state) => match state.delete_v2_virtual_model_mapping(name, provider) {
            Ok(value) => json_status(StatusCode::OK, value),
            Err(err) => bad_request(&err.to_string()),
        },
        Err(_) => internal_error("router state lock poisoned"),
    }
}

/// 批量保存物理模型完整配置（供应商模型配置面板）。
/// body: `{ models: [{ model, context_window?, max_output_tokens?, supports_image?, thinking_level_map?, thinking_format? }] }`
pub(crate) async fn api_config_physical_models_update(
    State(app): State<AppState>,
    Json(payload): Json<Value>,
) -> Response {
    let Some(models) = payload.get("models").and_then(Value::as_array) else {
        return bad_request("models must be a list");
    };
    let mut parsed = Vec::new();
    for item in models {
        let Some(model) = item.get("model").and_then(Value::as_str) else {
            return bad_request("each model needs model (string)");
        };
        let context_window = item
            .get("context_window")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        let max_output_tokens = item
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        let supports_image = item.get("supports_image").and_then(Value::as_bool);
        // null/缺省 = 不修改；object = 设置
        let thinking_level_map = match item.get("thinking_level_map") {
            Some(Value::Object(obj)) => {
                let map = obj
                    .iter()
                    .map(|(k, v)| {
                        let wire = v.as_str().map(|s| s.to_string());
                        (k.clone(), wire)
                    })
                    .collect::<HashMap<String, Option<String>>>();
                Some(Some(map))
            }
            _ => None,
        };
        let thinking_format = match item.get("thinking_format") {
            Some(Value::String(s)) => Some(Some(s.clone())),
            _ => None,
        };
        parsed.push(crate::features::router::PhysicalModelPatch {
            model: model.to_string(),
            context_window,
            max_output_tokens,
            supports_image,
            thinking_level_map,
            thinking_format,
        });
    }
    with_state_json(&app, |state| Ok(state.set_physical_models(parsed)?))
}
