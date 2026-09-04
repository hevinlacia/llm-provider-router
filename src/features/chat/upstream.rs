//! 上游调用主链路（非流式）：选 key / 重试 / 冻结 / 用量记录。

use crate::app::AppState;
use crate::config::ModelAlias;
use crate::features::router::NoAvailableKeyError;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashSet;

use super::payload::log_upstream_failure;
use super::select::{
    extract_usage, freeze_maybe, record_usage, select_key_locked, upstream_key_value_locked,
    usage_key_name,
};
use crate::routes::resp::{internal_error, json_status, status_code};

pub(crate) enum CallError {
    NoAvailable(NoAvailableKeyError),
}

pub(crate) async fn call_upstream(
    app: &AppState,
    alias: ModelAlias,
    session_id: Option<String>,
    payload: Value,
) -> Result<Response, CallError> {
    if alias.base_url.trim().is_empty() {
        return Ok(json_status(
            StatusCode::BAD_GATEWAY,
            json!({ "error": { "message": "provider has no chat completions base_url configured (base_url empty)", "type": "upstream_error" } }),
        ));
    }
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
            record_usage(
                &app.state,
                &alias.alias,
                &usage_key_name(&app, &key),
                599,
                None,
                session_id.as_deref(),
            );
            continue;
        };

        // 闲置后首包的连接池脏连接（hyper pool idle 90s vs 服务端已 FIN）会以
        // reqwest::Error(is_connect || is_timeout) 形式失败；对同一 key 立即重建
        // 连接重试一次，避免把整池 key 轮一遍全记 599。
        let mut response: Option<reqwest::Response> = None;
        let mut last_exc: Option<String> = None;
        for attempt in 0..2 {
            match app
                .client
                .post(format!(
                    "{}/chat/completions",
                    alias.base_url.trim_end_matches('/')
                ))
                .bearer_auth(key_value.clone())
                .header(CONTENT_TYPE, "application/json")
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => {
                    response = Some(resp);
                    break;
                }
                Err(exc) => {
                    let retryable = exc.is_connect() || exc.is_timeout() || exc.is_request();
                    last_exc = Some(exc.to_string());
                    if attempt == 0 && retryable {
                        if crate::diag::diag_enabled(&app.settings) {
                            crate::diag::append(
                                &app.settings,
                                "upstream.retry_pool_idle",
                                serde_json::json!({
                                    "alias": alias.alias,
                                    "provider": alias.provider(),
                                    "key": usage_key_name(&app, &key),
                                    "attempt": attempt + 1,
                                    "error": last_exc,
                                }),
                            );
                        }
                        continue;
                    }
                    break;
                }
            }
        }
        let response = match response {
            Some(r) => r,
            None => {
                record_usage(
                    &app.state,
                    &alias.alias,
                    &usage_key_name(&app, &key),
                    599,
                    None,
                    session_id.as_deref(),
                );
                if crate::diag::diag_enabled(&app.settings) {
                    crate::diag::append(
                        &app.settings,
                        "upstream.connect_error",
                        serde_json::json!({
                            "alias": alias.alias,
                            "provider": alias.provider(),
                            "key": usage_key_name(&app, &key),
                            "error": last_exc.unwrap_or_default(),
                        }),
                    );
                }
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
                session_id.as_deref(),
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
            session_id.as_deref(),
        );
        log_upstream_failure(&alias, status, &body_text);
        let mut resp = json_status(status_code(status), content);
        inject_router_headers(resp.headers_mut(), &alias);
        return Ok(resp);
    }
}

pub(crate) fn inject_router_headers(headers: &mut HeaderMap, alias: &ModelAlias) {
    headers.insert(
        "x-llm-router-model",
        HeaderValue::from_str(&alias.alias).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers.insert(
        "x-llm-router-upstream-model",
        HeaderValue::from_str(&alias.upstream_model())
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers.insert(
        "x-llm-router-provider",
        HeaderValue::from_str(&alias.provider())
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    if let Some(v) = alias.context_window {
        if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
            headers.insert("x-llm-router-context-window", hv);
        }
    }
    if let Some(v) = alias.max_output_tokens {
        if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
            headers.insert("x-llm-router-max-output", hv);
        }
    }
}
