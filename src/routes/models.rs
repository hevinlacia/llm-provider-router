//! OpenAI 兼容模型列表 + 动态上下文协商视图 handler。

use crate::app::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::json;

use super::resp::with_state_json;
use super::validate_auth;

pub(crate) async fn models(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    with_state_json(&app, |state| {
        let caps = state.router_capabilities();
        let cap_map: std::collections::HashMap<
            String,
            (Option<u32>, Option<u32>, Option<serde_json::Value>, Option<bool>),
        > = caps
            .get("models")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id")?.as_str()?.to_string();
                        let cw = m
                            .get("effective")?
                            .get("contextWindow")?
                            .as_u64()
                            .map(|v| v as u32);
                        let mo = m
                            .get("effective")?
                            .get("maxTokens")?
                            .as_u64()
                            .map(|v| v as u32);
                        let input = m.get("input").cloned();
                        let reasoning = m.get("reasoning").and_then(|v| v.as_bool());
                        Some((id, (cw, mo, input, reasoning)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let data = state
            .settings_aliases()
            .values()
            .map(|alias| {
                let (cw, mo, input, reasoning) =
                    cap_map.get(&alias.alias).cloned().unwrap_or((None, None, None, None));
                let cw = cw.or(alias.context_window);
                let mo = mo.or(alias.max_output_tokens);
                // input/reasoning：优先 Router 下发的逻辑模型配置，次选 settings_aliases 的硬编码兜底。
                let caps_input = input.clone();
                let mut obj = serde_json::json!({
                    "id": alias.alias,
                    "object": "model",
                    "created": 0,
                    "owned_by": "llm-provider-router",
                });
                if let Some(v) = cw {
                    obj["context_window"] = json!(v);
                    obj["contextWindow"] = json!(v);
                }
                if let Some(v) = mo {
                    obj["max_output_tokens"] = json!(v);
                    obj["maxTokens"] = json!(v);
                }
                if let Some(input) = caps_input {
                    obj["input"] = input;
                }
                if let Some(reasoning) = reasoning {
                    obj["reasoning"] = json!(reasoning);
                }
                obj
            })
            .collect::<Vec<_>>();
        Ok(json!({ "object": "list", "data": data }))
    })
}

pub(crate) async fn router_capabilities(
    State(app): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = validate_auth(&app.settings, &headers) {
        return response;
    }
    with_state_json(&app, |state| Ok(state.router_capabilities()))
}
