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

use super::freeze::{key_state_id, model_scoped_state_id, parse_quota_reset, parse_retry_after};
use super::state::RouterState;

/// 最近报错 ring buffer 容量。
pub(crate) const RECENT_ERRORS_CAP: usize = 200;
/// 连接层失败的内部状态码（无 HTTP 状态可用）。
pub(crate) const CONNECT_ERROR_STATUS: u16 = 599;
/// 流内错误的展示状态码：上游返回 HTTP 200 但 SSE 流/响应体里携带错误事件。
pub(crate) const EMBEDDED_ERROR_STATUS: u16 = 200;
/// 上游流中断（HTTP 200 已建立、body 传输中途断开）的合成记账状态码。
/// 与 599（连接失败，请求未建立）区分；dashboard/errors 页按此识别断流。
pub(crate) const STREAM_BREAK_STATUS: u16 = 598;

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

/// 取错误文案：`message` 优先，兼容智谱/BigModel 的 `msg` 别名
/// （2026-11 zp 鉴权失败以 HTTP 200 + `{"code":1000,"msg":"身份验证失败。"}`
/// 返回，此前该形状识别不到 → 当成功透传，key 不冻结、会话不解绑）。
fn error_message<'a>(value: &'a Value) -> Option<&'a str> {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("msg").and_then(Value::as_str))
}

/// 取错误码：字符串或数字都接受（智谱是数字 code 1000，OpenAI 系是字符串码）。
fn error_code(value: &Value) -> String {
    value
        .get("code")
        .and_then(|code| {
            code.as_str()
                .map(str::to_string)
                .or_else(|| code.as_i64().map(|n| n.to_string()))
        })
        .or_else(|| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "upstream_error".to_string())
}

/// 识别单个 SSE/JSON 事件是否为上游错误，返回 (code, message)。
/// 兼容五种形状（2026-10 zp 线上事故：5 小时限额以 HTTP 200 + 流内错误事件返回，
/// 此前被当成正常流转发，key 不冻结、会话不解绑，故障切换失效）：
/// - chat/自定义错误体：`{"error":{"code":..., "message":...}}`；
/// - Responses 流内错误事件：`{"type":"error","code":...,"message":...}`；
/// - Responses 失败事件：`{"type":"response.failed","response":{"error":{...}}}`；
/// - 路由自身错误事件：`{"code":...,"message":...}`（无 type 字段）；
/// - 智谱/BigModel 形状：`{"code":1000,"msg":"身份验证失败。"}`（`msg` 别名 + 数字码）。
pub(crate) fn extract_stream_error_event(value: &Value) -> Option<(String, String)> {
    let typ = value.get("type").and_then(Value::as_str).unwrap_or("");
    if typ == "response.failed" {
        let err = value.pointer("/response/error").filter(|v| v.is_object())?;
        let message = error_message(err)?;
        return Some((error_code(err), message.to_string()));
    }
    if let Some(err) = value.get("error").filter(|v| v.is_object()) {
        if let Some(message) = error_message(err) {
            return Some((error_code(err), message.to_string()));
        }
    }
    if typ == "error" || typ.is_empty() {
        if let Some(message) = error_message(value) {
            return Some((error_code(value), message.to_string()));
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
            let state_id = freeze_state_id(
                state.provider_freeze_scope(&alias.provider()),
                key,
                &alias.upstream_model(),
            );
            let _ = state.freeze(&state_id, until, &format!("invalid:{}", classified.rule));
        }
        ErrorClass::RateLimited => {
            let (monthly_fallback, five_hour_fallback) = state.quota_fallback_seconds();
            let state_id = freeze_state_id(
                state.provider_freeze_scope(&alias.provider()),
                key,
                &alias.upstream_model(),
            );
            if let Some((until, reason)) =
                parse_quota_reset(message, monthly_fallback, five_hour_fallback)
            {
                let _ = state.freeze(&state_id, until, reason);
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
    // `"msg"` / `"code"` 覆盖智谱形状（HTTP 200 + {code,msg}），误判只多一次扫描。
    if !body_text.contains("\"error\"")
        && !body_text.contains("response.failed")
        && !body_text.contains("\"msg\"")
        && !body_text.contains("\"code\"")
    {
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
            let state_id = freeze_state_id(
                state.provider_freeze_scope(&provider),
                key,
                &alias.upstream_model(),
            );
            let _ = state.freeze(&state_id, until, &format!("invalid:{}", classified.rule));
            FailureAction::NextKey(format!("upstream {status}: {message}"))
        }
        ErrorClass::RateLimited => {
            let state_id = freeze_state_id(
                state.provider_freeze_scope(&provider),
                key,
                &alias.upstream_model(),
            );
            freeze_rate_limited(&mut state, &state_id, headers, body_text);
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

/// 连接层失败（无响应/超时）：除原有 transient 语义外，按 tunables 给 key
/// 短 TTL 冻结（connect_error_freeze_seconds，连败指数退避）。冻结后
/// select_key_excluding 直接跳过该 key：粘性哈希到死 key 的 session 在
/// 选取阶段就落桶到备用候选，不再对死主机反复建连（2026-10 风暴治理）。
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
        // 短熔断：freeze 只延长不缩短，不会覆盖更长的配额/差分冻结。
        //
        // 单 key 池豁免（2026-11-05）：冻结的本意是「把坏 key 隔离掉、failover 到
        // 另一个 key」。池里只有 1 个 key 时没有 failover 目标，冻结唯一 key 只会把
        // 「偶发一次连接失败」升级成「全池直接不可用」（且指数退避可长到 960s，
        // OpenClash 透明代理链路抖动下极易发生）。因此单 key 池只记录不冻结：
        // 本次请求失败返回，下一次请求重新建连尝试（已带一次内部重试）。
        let single_key_pool = alias.keys.len() <= 1;
        let key_id = key_state_id(key);
        // 可观测性：无论是否冻结都计数（单 key 池不冻结，更依赖这个计数发现
        // 「代理链路不稳」），重试后仍失败的真实次数。
        state.note_connect_failure(&key_id, &message);
        let base = state.tunables().connect_error_freeze_seconds;
        if base > 0.0 && !single_key_pool {
            let ttl = state.note_connect_failure_streak(&key_id, base);
            if ttl > 0.0 {
                let _ = state.freeze(
                    &key_id,
                    crate::state_store::now_seconds() + ttl,
                    "connect_error",
                );
            }
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

/// 按供应商的 freeze_scope 把“失败隔离”落到正确的粒度上：
///
/// - `key`（缺省）：`provider/name`，冻结整把 key（历史行为）；
/// - `model`：`provider/name#upstream_model`，只隔离这个模型（opencode-go 这类
///   按模型给地区限制/配额的供应商，一个模型坏了不能连带冻结其它模型）。
fn freeze_state_id(
    scope: crate::config_v2::V2FreezeScope,
    key: &KeyRef,
    upstream_model: &str,
) -> String {
    match scope {
        crate::config_v2::V2FreezeScope::Key => key_state_id(key),
        crate::config_v2::V2FreezeScope::Model => model_scoped_state_id(key, upstream_model),
    }
}

/// 限流冻结：retry-after 头 > body "reset at" 时间戳 > 不冻结（key 留在池内）。
/// `state_id` 由调用方按供应商 freeze_scope 算好（key 级或 key × 模型级）。
fn freeze_rate_limited(
    state: &mut RouterState,
    state_id: &str,
    headers: &http::HeaderMap,
    body_text: &str,
) {
    let (monthly_fallback, five_hour_fallback) = state.quota_fallback_seconds();
    if let Some(until) = parse_retry_after(headers.get("retry-after").and_then(|v| v.to_str().ok()))
    {
        let _ = state.freeze(state_id, until, "retry_after");
        return;
    }
    if let Some((until, reason)) =
        parse_quota_reset(body_text, monthly_fallback, five_hour_fallback)
    {
        let _ = state.freeze(state_id, until, reason);
    }
}

/// 请求内差分冻结：某 key 失败（transient，未冻结）后同行 key 成功 → 冻结失败 key。
/// 无需依赖报错文案，供应商改文案后依然自动把坏 key 排除出可用池。
/// 上游流中断观测记录：HTTP 200 已提交、SSE body 传输中途断开（reqwest
/// `bytes_stream` 返回 Err，Display 统一为 "error decoding response body"）。
/// 按错误源分类：is_timeout()=true 为读空闲超时/死线类（含历史 total 超时——
/// 2026-11-02 事故根因，600s 总超时把长流殄断）；false 为连接重置/TLS 截断等
/// 真传输层故障。流已提交无法换 key 重试，但此前完全不记 usage/错误日志——
/// dashboard 与 /api/errors/recent 均无痕迹，间歇性断流排障无证据。此处补齐：
/// 用量记账（598 状态计数）、ring buffer（errors 页，按 provider/key 定位坏上游）、
/// journal+diag（upstream.failure 事件持久化）。
pub(crate) fn note_stream_break(
    app: &AppState,
    alias: &ModelAlias,
    key: &KeyRef,
    exc_text: &str,
    is_timeout: bool,
    bytes_received: usize,
    session_id: Option<&str>,
) {
    // 分类标签可 greppable：timeout = 读空闲超时/死线类（多为 router 自身超时配置），
    // transport = 连接重置/TLS 截断等真上游故障。
    let kind = if is_timeout { "timeout" } else { "transport" };
    let message = format!("[stream-break:{kind}] {bytes_received} bytes received: {exc_text}");
    let mut state = match app.state.lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    // 用量记账：中断流 token 记 0，但请求数/598 状态计数在 dashboard 可见。
    let _ = state.record_usage(
        &alias.alias,
        &usage_key_name(key),
        STREAM_BREAK_STATUS,
        None,
        session_id,
    );
    // 中流断开：解除粘性绑定，下次请求重新选 key。
    if let Some(sid) = session_id {
        state.unbind(&alias.alias, sid);
    }
    state.push_recent_error(ErrorLogEntry {
        ts: crate::state_store::now_seconds(),
        provider: alias.provider(),
        key: key.name.clone(),
        alias: alias.alias.clone(),
        model: alias.upstream_model(),
        status: STREAM_BREAK_STATUS,
        class: ErrorClass::Transient.as_str().to_string(),
        rule: String::new(),
        message: message.chars().take(300).collect(),
    });
    drop(state);
    // journal（stderr）+ diag 文件持久化：status=598 可通过 log_upstream_failure
    // 的 >=400 门。
    log_upstream_failure(alias, STREAM_BREAK_STATUS, &message);
}

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
        // 熔断也遵守供应商 freeze_scope：freeze_scope=model 的供应商（opencode-go）
        // 只因某模型瞬态耗尽就冻结整把 key 会连带打挂同 key 的其它模型
        // （2026-11-12 实测：beam-free 全池瞬态耗尽 → key 级冻结 300s → deepseek 也 429）。
        let scope = state.provider_freeze_scope(&alias.provider());
        let upstream_model = alias.upstream_model();
        for key in &alias.keys {
            if key.weight <= 0 {
                continue;
            }
            let state_id = freeze_state_id(scope, key, &upstream_model);
            let frozen = state.is_frozen(&state_id).unwrap_or(true);
            if !frozen {
                let _ = state.freeze(&state_id, now + seconds, "transient_exhausted");
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
