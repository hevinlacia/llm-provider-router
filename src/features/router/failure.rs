//! 上游失败统一处理：分类（error_rules）→ 动作（冻结/重试/切 key/终止）→ 日志。
//!
//! 所有调用链（messages 流式/非流式、responses 透传/翻译）共用本模块。
//! chat 客户端协议已于 2026-10 下线；对仅支持 chat completions 的上游，
//! 仍由 responses/messages 翻译层在内部调用 `/chat/completions` 端点。
//!
//! 动作语义（对应设置页四类报错）：
//! - invalid        -> 冻结整把 key（invalid_freeze_seconds）→ 换下一把
//! - rate_limited   -> 冻结到恢复时刻（retry-after / reset-at / 兜底）→ 换下一把
//! - model_unsupported -> 直接终止请求（配置错误，切 key 无意义）
//! - transient      -> 同 key 重试 N 次（attempts 配置）→ 换下一把；
//!   同请求内同行 key 成功时差分冻结失败的 key；
//!   全池耗尽且末次失败为 transient 时触发全池熔断。

use crate::app::AppState;
use crate::config::{KeyRef, ModelAlias};
use crate::error_rules::{classify, ErrorClass};
use crate::features::chat::payload::log_upstream_failure;
use crate::features::chat::select::{is_unsupported_signal, usage_key_name};
use serde_json::{json, Value};
use std::collections::VecDeque;

use super::freeze::{key_state_id, parse_quota_reset, parse_retry_after};
use super::state::RouterState;

/// 最近报错 ring buffer 容量。
pub(crate) const RECENT_ERRORS_CAP: usize = 200;
/// 连接层失败的内部状态码（无 HTTP 状态可用）。
pub(crate) const CONNECT_ERROR_STATUS: u16 = 599;
/// 流内错误的展示状态码：上游返回 HTTP 200 但 SSE 流/响应体里携带错误事件。
pub(crate) const EMBEDDED_ERROR_STATUS: u16 = 200;

/// 单条最近报错记录（内存 ring buffer，重启丢失）。
#[derive(Clone, Debug)]
pub struct ErrorLogEntry {
    pub ts: f64,
    pub provider: String,
    pub key: String,
    pub alias: String,
    pub model: String,
    pub status: u16,
    pub class: String,
    pub rule: String,
    pub message: String,
}

/// 失败处理后调用方应执行的动作。
#[derive(Clone, Debug)]
pub(crate) enum FailureAction {
    /// key 已冻结（失效/限流）：换下一把。
    NextKey(String),
    /// 其他类且该 key 重试配额耗尽：不冻结，换下一把；调用方记录差分候选 + 熔断线索。
    NextKeyTransient(String),
    /// 其他类同 key 重试（延迟毫秒）。
    RetrySame(u64, String),
    /// 模型不支持/配置错误：直接终止请求。
    Abort {
        status: u16,
        message: String,
        content: Value,
    },
}

fn extract_error_message(body_text: &str) -> String {
    serde_json::from_str::<Value>(body_text)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body_text.chars().take(300).collect())
}

/// 识别单个 SSE/JSON 事件是否为上游错误，返回 (code, message)。
/// 兼容四种形状（2026-10 zp 线上事故：5 小时限额以 HTTP 200 + 流内错误事件返回，
/// 此前被当成正常流转发，key 不冻结、会话不解绑，故障切换失效）：
/// - chat/自定义错误体：`{"error":{"code":..., "message":...}}`；
/// - Responses 流内错误事件：`{"type":"error","code":...,"message":...}`；
/// - Responses 失败事件：`{"type":"response.failed","response":{"error":{...}}}`；
/// - 路由自身错误事件：`{"code":...,"message":...}`（无 type 字段）。
pub(crate) fn extract_stream_error_event(value: &Value) -> Option<(String, String)> {
    let typ = value.get("type").and_then(Value::as_str).unwrap_or("");
    if typ == "response.failed" {
        let err = value.pointer("/response/error").filter(|v| v.is_object())?;
        let message = err.get("message").and_then(Value::as_str)?;
        let code = err
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| err.get("type").and_then(Value::as_str))
            .unwrap_or("upstream_error");
        return Some((code.to_string(), message.to_string()));
    }
    if let Some(err) = value.get("error").filter(|v| v.is_object()) {
        if let Some(message) = err.get("message").and_then(Value::as_str) {
            let code = err
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| err.get("type").and_then(Value::as_str))
                .unwrap_or("upstream_error");
            return Some((code.to_string(), message.to_string()));
        }
    }
    if typ == "error" || typ.is_empty() {
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            let code = value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("upstream_error");
            return Some((code.to_string(), message.to_string()));
        }
    }
    None
}

/// 从已收集的完整响应/流文本扫描错误事件：先试整体 JSON（非流式 200 错误体），
/// 再逐行扫描 SSE `data:` 事件。返回最后一个可识别的 (code, message)。
fn scan_error_event_text(body_text: &str) -> Option<(String, String)> {
    let trimmed = body_text.trim();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        // SSE 流文本：逐行找 data: 事件
        for line in body_text.lines() {
            let trimmed = line.trim();
            let Some(data) = trimmed.strip_prefix("data:") else {
                continue;
            };
            let body = data.trim();
            if body.is_empty() || body == "[DONE]" {
                continue;
            }
            let parsed = serde_json::from_str::<Value>(body).ok();
            if let Some(found) = parsed.and_then(|value| extract_stream_error_event(&value)) {
                return Some(found);
            }
        }
        return None;
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|value| extract_stream_error_event(&value))
}

/// 对流内/嵌入失败执行分类动作：invalid/限流冻结 + 解绑会话 + 记录最近报错。
/// 返回是否识别并处理了错误。流已开始（HTTP 200 已提交），本请求无法换 key 重试；
/// 冻结与解绑保证下一次请求自动故障切换到健康 key/target。
pub(crate) fn apply_embedded_failure(
    app: &AppState,
    alias: &ModelAlias,
    key: &KeyRef,
    code: &str,
    message: &str,
    session_id: Option<&str>,
) -> bool {
    let Ok(mut state) = app.state.lock() else {
        return false;
    };
    let lowered = format!("{code} {message}").to_lowercase();
    let classified = classify(
        state.error_rules(),
        &alias.provider(),
        EMBEDDED_ERROR_STATUS,
        &lowered,
    );
    state.push_recent_error(ErrorLogEntry {
        ts: crate::state_store::now_seconds(),
        provider: alias.provider(),
        key: key.name.clone(),
        alias: alias.alias.clone(),
        model: alias.upstream_model(),
        status: EMBEDDED_ERROR_STATUS,
        class: classified.class.as_str().to_string(),
        rule: classified.rule.clone(),
        message: format!("[stream] {code}: {message}")
            .chars()
            .take(300)
            .collect(),
    });
    // 上游明确拒绝：解除会话粘性绑定，避免会话被钉死在坏 key 上。
    if let Some(sid) = session_id {
        state.unbind(&alias.alias, sid);
    }
    match classified.class {
        ErrorClass::Invalid => {
            let until = crate::state_store::now_seconds() + state.tunables().invalid_freeze_seconds;
            let _ = state.freeze(
                &key_state_id(key),
                until,
                &format!("invalid:{}", classified.rule),
            );
        }
        ErrorClass::RateLimited => {
            let (monthly_fallback, five_hour_fallback) = state.quota_fallback_seconds();
            if let Some((until, reason)) =
                parse_quota_reset(message, monthly_fallback, five_hour_fallback)
            {
                let _ = state.freeze(&key_state_id(key), until, reason);
            }
        }
        ErrorClass::ModelUnsupported | ErrorClass::Transient => {}
    }
    eprintln!(
        "llm-provider-router embedded_failure provider={} model={} key={} status=200 class={} rule={} error={}",
        alias.provider(),
        alias.upstream_model(),
        key.name,
        classified.class.as_str(),
        classified.rule,
        message.chars().take(300).collect::<String>()
    );
    true
}

/// 扫描已收集的流/响应体，发现嵌入错误则分类处理。返回是否发现错误。
/// 调用方在成功尾部调用：发现错误时跳过差分冻结与“不支持”清除（不是成功流）。
pub(crate) fn handle_embedded_failure(
    app: &AppState,
    alias: &ModelAlias,
    key: &KeyRef,
    body_text: &str,
    session_id: Option<&str>,
) -> bool {
    // 快速门禁：正文不含错误标记时直接返回（成功流零开销路径）。
    if !body_text.contains("\"error\"") && !body_text.contains("response.failed") {
        return false;
    }
    match scan_error_event_text(body_text) {
        Some((code, message)) => {
            apply_embedded_failure(app, alias, key, &code, &message, session_id)
        }
        None => false,
    }
}

/// 透传流零输出异常终止（如 Ark 的 response.incomplete/network_error）：
/// 记录最近报错 + 解绑会话粘性 + 对该 key 做短差分冻结（下一请求优先同行健康 key）。
/// 调用时机：客户端尚未收到任何字节，随即换 key/target 静默重试。
pub(crate) fn note_zero_output_incomplete(
    app: &AppState,
    alias: &ModelAlias,
    key: &KeyRef,
    reason: &str,
    session_id: Option<&str>,
) {
    let freeze_seconds = if let Ok(mut state) = app.state.lock() {
        state.push_recent_error(ErrorLogEntry {
            ts: crate::state_store::now_seconds(),
            provider: alias.provider(),
            key: key.name.clone(),
            alias: alias.alias.clone(),
            model: alias.upstream_model(),
            status: EMBEDDED_ERROR_STATUS,
            class: ErrorClass::Transient.as_str().to_string(),
            rule: "zero_output_incomplete".to_string(),
            message: format!("[stream] zero-output incomplete: {reason}")
                .chars()
                .take(300)
                .collect(),
        });
        // 上游异常终止：解除会话粘性绑定，重试/下一请求重新选 key。
        if let Some(sid) = session_id {
            state.unbind(&alias.alias, sid);
        }
        let seconds = state.tunables().differential_freeze_seconds;
        let until = crate::state_store::now_seconds() + seconds;
        let _ = state.freeze(&key_state_id(key), until, "zero_output_incomplete");
        seconds
    } else {
        0.0
    };
    eprintln!(
        "llm-provider-router zero_output_incomplete provider={} model={} key={} reason={} differential_freeze_seconds={}",
        alias.provider(),
        alias.upstream_model(),
        key.name,
        reason,
        freeze_seconds
    );
    // 持久化到诊断文件（best-effort，失败不影响主链路）。
    if let Ok(settings) = crate::config::load_settings() {
        if crate::diag::diag_enabled(&settings) {
            crate::diag::append(
                &settings,
                "stream.zero_output_incomplete",
                serde_json::json!({
                    "provider": alias.provider(),
                    "model": alias.upstream_model(),
                    "alias": alias.alias,
                    "key": key.name,
                    "reason": reason,
                }),
            );
        }
    }
}

/// 响应级失败（status >= 400）统一处理。调用方保证只传失败状态。
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_response_failure(
    app: &AppState,
    alias: &ModelAlias,
    key: &KeyRef,
    status: u16,
    headers: &http::HeaderMap,
    body_text: &str,
    usage: Option<&Value>,
    session_id: Option<&str>,
    same_key_attempt: usize,
) -> FailureAction {
    let provider = alias.provider();
    let lowered = body_text.to_lowercase();
    let message = extract_error_message(body_text);
    let content = serde_json::from_str::<Value>(body_text)
        .unwrap_or_else(|_| json!({ "error": { "message": body_text, "type": "upstream_error" } }));

    let mut state = match app.state.lock() {
        Ok(state) => state,
        Err(_) => return FailureAction::NextKey(message.clone()),
    };
    let classified = classify(state.error_rules(), &provider, status, &lowered);
    let tunables = state.tunables();

    // 用量记账（失败请求 token 为 0，但请求数/错误数要记）。
    let _ = state.record_usage(
        &alias.alias,
        &usage_key_name(key),
        status,
        usage,
        session_id,
    );
    // journald 结构化日志（沿用现有格式，排障口径不变）。
    log_upstream_failure(alias, status, body_text);
    state.push_recent_error(ErrorLogEntry {
        ts: crate::state_store::now_seconds(),
        provider: provider.clone(),
        key: key.name.clone(),
        alias: alias.alias.clone(),
        model: alias.upstream_model(),
        status,
        class: classified.class.as_str().to_string(),
        rule: classified.rule.clone(),
        message: message.chars().take(300).collect(),
    });
    // 上游明确拒绝：解除会话粘性绑定，避免会话被钉死在坏 key 上。
    if let Some(sid) = session_id {
        state.unbind(&alias.alias, sid);
    }

    match classified.class {
        ErrorClass::Invalid => {
            let until = crate::state_store::now_seconds() + tunables.invalid_freeze_seconds;
            let _ = state.freeze(
                &key_state_id(key),
                until,
                &format!("invalid:{}", classified.rule),
            );
            FailureAction::NextKey(format!("upstream {status}: {message}"))
        }
        ErrorClass::RateLimited => {
            freeze_rate_limited(&mut state, key, headers, body_text);
            FailureAction::NextKey(format!("upstream {status}: {message}"))
        }
        ErrorClass::ModelUnsupported => {
            // key × 模型“不支持”信号仍记入阶梯退避（dashboard 诊断视图可见）。
            if is_unsupported_signal(status, body_text) {
                state.mark_key_model_unsupported(
                    &key.provider,
                    &key.name,
                    &alias.upstream_model(),
                    &message,
                );
            }
            FailureAction::Abort {
                status,
                message,
                content,
            }
        }
        ErrorClass::Transient => {
            if same_key_attempt as u32 + 1 < tunables.transient_attempts_per_key {
                FailureAction::RetrySame(
                    tunables.transient_retry_interval_ms,
                    format!("upstream {status}: {message}"),
                )
            } else {
                FailureAction::NextKeyTransient(format!("upstream {status}: {message}"))
            }
        }
    }
}

/// 连接层失败（无响应/超时）：视作 transient，不冻结。
pub(crate) fn handle_connect_failure(
    app: &AppState,
    alias: &ModelAlias,
    key: &KeyRef,
    exc_text: &str,
    session_id: Option<&str>,
) -> FailureAction {
    let message = format!("upstream connect error: {exc_text}");
    if let Ok(mut state) = app.state.lock() {
        let _ = state.record_usage(
            &alias.alias,
            &usage_key_name(key),
            CONNECT_ERROR_STATUS,
            None,
            session_id,
        );
        if let Some(sid) = session_id {
            state.unbind(&alias.alias, sid);
        }
        state.push_recent_error(ErrorLogEntry {
            ts: crate::state_store::now_seconds(),
            provider: alias.provider(),
            key: key.name.clone(),
            alias: alias.alias.clone(),
            model: alias.upstream_model(),
            status: CONNECT_ERROR_STATUS,
            class: ErrorClass::Transient.as_str().to_string(),
            rule: "connect".to_string(),
            message: message.chars().take(300).collect(),
        });
    }
    FailureAction::NextKeyTransient(message)
}

/// 限流冻结：retry-after 头 > body "reset at" 时间戳 > 不冻结（key 留在池内）。
fn freeze_rate_limited(
    state: &mut RouterState,
    key: &KeyRef,
    headers: &http::HeaderMap,
    body_text: &str,
) {
    let (monthly_fallback, five_hour_fallback) = state.quota_fallback_seconds();
    if let Some(until) = parse_retry_after(headers.get("retry-after").and_then(|v| v.to_str().ok()))
    {
        let _ = state.freeze(&key_state_id(key), until, "retry_after");
        return;
    }
    if let Some((until, reason)) =
        parse_quota_reset(body_text, monthly_fallback, five_hour_fallback)
    {
        let _ = state.freeze(&key_state_id(key), until, reason);
    }
}

/// 请求内差分冻结：某 key 失败（transient，未冻结）后同行 key 成功 → 冻结失败 key。
/// 无需依赖报错文案，供应商改文案后依然自动把坏 key 排除出可用池。
pub(crate) fn note_success_differential(app: &AppState, ambiguous: &[KeyRef]) {
    if ambiguous.is_empty() {
        return;
    }
    if let Ok(mut state) = app.state.lock() {
        let seconds = state.tunables().differential_freeze_seconds;
        let now = crate::state_store::now_seconds();
        for key in ambiguous {
            let _ = state.freeze(
                &key_state_id(key),
                now + seconds,
                "differential_peer_success",
            );
        }
    }
}

/// 其他类全池耗尽 + 末次失败为 transient：全池短熔断（circuit breaker），
/// 避免上游整体故障时每个请求都白打一遍全部 key。限流/失效冻结不受影响
/// （freeze 只在更晚的 until 时才覆盖）。
pub(crate) fn circuit_breaker_on_exhaustion(app: &AppState, alias: &ModelAlias) {
    if let Ok(mut state) = app.state.lock() {
        let seconds = state.tunables().transient_exhausted_freeze_seconds;
        if seconds <= 0.0 {
            return;
        }
        let now = crate::state_store::now_seconds();
        for key in &alias.keys {
            if key.weight <= 0 {
                continue;
            }
            let frozen = state.is_frozen(&key_state_id(key)).unwrap_or(true);
            if !frozen {
                let _ = state.freeze(&key_state_id(key), now + seconds, "transient_exhausted");
            }
        }
    }
}

/// 最近报错视图（新→旧）。
pub(crate) fn recent_errors_json(state: &RouterState) -> Vec<Value> {
    state
        .recent_errors_iter()
        .rev()
        .map(|entry| {
            json!({
                "ts": entry.ts,
                "provider": entry.provider,
                "key": entry.key,
                "alias": entry.alias,
                "model": entry.model,
                "status": entry.status,
                "class": entry.class,
                "rule": entry.rule,
                "message": entry.message,
            })
        })
        .collect()
}

/// state 持有的 ring buffer 类型。
pub(crate) type RecentErrors = VecDeque<ErrorLogEntry>;
