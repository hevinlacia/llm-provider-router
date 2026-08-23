//! 上游载荷归一化（纯函数）：按供应商最小改写请求，尽量保留客户端语义。

use crate::config::ModelAlias;
use serde_json::{json, Value};

pub(crate) fn is_muse_spark(alias: &ModelAlias) -> bool {
    alias.upstream_model().contains("muse-spark")
        || alias.provider() == "opencode-go"
        || alias.base_url.contains("opencode")
}

pub(crate) fn prepare_upstream_payload(payload: &Value, alias: &ModelAlias) -> Value {
    let mut next = payload.clone();
    next["model"] = Value::String(alias.upstream_model().to_string());
    // 思考强度翻译：DSH 只发 OpenAI 标准档位（xhigh），Router 按逻辑模型的 thinking_level_map
    // 翻译成上游方言（deepseek-official xhigh->max）。仅对走 Router 的请求生效，直连不经此路径。
    if let Some(map) = alias.thinking_level_map.as_ref() {
        translate_reasoning_effort(&mut next, map);
    }
    // 服务器侧默认/覆写参数（v2 逻辑模型默认 + 物理模型覆写 params 的合并结果）：
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
    let diag_settings =
        crate::config::load_settings().unwrap_or_else(|_| crate::config::Settings {
            host: "".into(),
            port: 0,
            session_ttl_seconds: 0.0,
            monthly_quota_fallback_seconds: 0.0,
            five_hour_quota_fallback_seconds: 0.0,
            request_timeout_seconds: 0.0,
            local_bearer_token: None,
            usage_db_path: "".into(),
            state_db_path: "".into(),
            weight_config_path: "".into(),
            provider_config_path: "".into(),
            custom_key_config_path: "".into(),
            api_keys_path: "".into(),
            token_price_config_path: "".into(),
            model_alias_config_path: "".into(),
            search_providers_path: "".into(),
            model_equivalences_path: "".into(),
            provider_models_path: "".into(),
            auth_invalid_freeze_seconds: 0.0,
            v2_config_enabled: false,
            diag_dir: crate::config::DEFAULT_DIAG_DIR.to_string(),
            diag_max_bytes: 10 * 1024 * 1024,
            diag_max_files: 50,
            diag_sample_every: 1,
        });
    // 收集期：记录原始 payload 的关键字段（不含消息正文/密钥），便于事后定位是协议兼容还是模型能力问题。
    if is_muse_spark(alias)
        && (payload.get("thinking").is_some()
            || payload.get("reasoning_effort").is_some()
            || payload.get("reasoning").is_some())
    {
        crate::diag::append(
            &diag_settings,
            "normalize.muse_spark.thinking_seen",
            json!({
                "alias": alias.alias, "provider": alias.provider(), "upstream_model": alias.upstream_model(),
                "summary_before": crate::diag::payload_summary(payload),
            }),
        );
    }
    if is_deepseek_official(alias) {
        let before_summary = crate::diag::payload_summary(&next);
        normalize_deepseek_official(&mut next);
        if before_summary != crate::diag::payload_summary(&next) {
            crate::diag::append(
                &diag_settings,
                "normalize.deepseek.applied",
                json!({
                    "alias": alias.alias, "provider": alias.provider(), "upstream_model": alias.upstream_model(),
                    "before": before_summary, "after": crate::diag::payload_summary(&next),
                }),
            );
        }
    }
    // muse-spark 收集期：仅观测，不改写（待收集期结束若证据充分再在此剥离 thinking/reasoning_effort）。
    next
}

pub(crate) fn is_deepseek_official(alias: &ModelAlias) -> bool {
    alias.provider() == "deepseek-official" || alias.base_url.contains("deepseek.com")
}

/// DeepSeek 官方 API (api.deepseek.com) 比 ark 等供应商更严格的 OpenAI 兼容校验：
/// - response_format=json_object 要求 prompt 中出现 "json" 字样，否则 400；
/// - response_format=json_schema 不支持，直接 400；
/// - n>1 不支持，仅支持 n=1；
/// - thinking 模式下强制 tool_choice（required / function 对象）或 assistant 的
///   tool_calls 未回传 reasoning_content 都会 400。
/// 归一化原则：只在请求必然 400 时才改写，尽量保留客户端语义。
pub(crate) fn normalize_deepseek_official(next: &mut Value) {
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

pub(crate) fn prompt_mentions_json(next: &Value) -> bool {
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

pub(crate) fn ensure_json_hint(next: &mut Value) {
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

/// OpenAI 标准档位（Router 对外契约）。不在此集合的强度视为非标，兜底按 xhigh 处理。
const STANDARD_EFFORTS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh"];

fn is_standard_effort(s: &str) -> bool {
    STANDARD_EFFORTS.contains(&s.trim().to_ascii_lowercase().as_str())
}

/// 将客户端标准档位按 alias 的 thinking_level_map 翻译成上游 wire 值。
/// - map 含某档位且 Some(wire)：改写为 wire；
/// - map 含某档位且 None：该档位在上游不支持，删除该字段回退上游默认；
/// - map 不含该档位：保持原值（透传）；
/// - 非标档位（不在 OpenAI 规范内）：兜底按 xhigh 翻译（xhigh->max 等）。
/// 当前 DSH 对 OpenAI 协议走 `reasoning_effort`，部分历史/他端可能用 `reasoning` 或 `thinking` 字符串形态，这里全兼容。
fn translate_reasoning_effort(
    next: &mut Value,
    map: &std::collections::HashMap<String, Option<String>>,
) {
    // 1) reasoning_effort（主链路：openai-completions）
    if let Some(effort) = next
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        let key = effort.trim().to_ascii_lowercase();
        if let Some(entry) = map.get(&key) {
            match entry {
                Some(wire) => next["reasoning_effort"] = Value::String(wire.clone()),
                None => {
                    next.as_object_mut().unwrap().remove("reasoning_effort");
                }
            }
            return;
        }
        if !is_standard_effort(&key) {
            // 非标 -> 兜底 xhigh
            if let Some(entry) = map.get("xhigh") {
                match entry {
                    Some(wire) => next["reasoning_effort"] = Value::String(wire.clone()),
                    None => {
                        next.as_object_mut().unwrap().remove("reasoning_effort");
                    }
                }
            } else {
                next["reasoning_effort"] = Value::String("xhigh".to_string());
            }
            return;
        }
        // 标准但 map 未声明 -> 透传
        return;
    }
    // 2) reasoning（兼容别名）
    if let Some(effort) = next
        .get("reasoning")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        let key = effort.trim().to_ascii_lowercase();
        if let Some(entry) = map.get(&key) {
            match entry {
                Some(wire) => next["reasoning"] = Value::String(wire.clone()),
                None => {
                    next.as_object_mut().unwrap().remove("reasoning");
                }
            }
            return;
        }
        if !is_standard_effort(&key) {
            if let Some(entry) = map.get("xhigh") {
                match entry {
                    Some(wire) => next["reasoning"] = Value::String(wire.clone()),
                    None => {
                        next.as_object_mut().unwrap().remove("reasoning");
                    }
                }
            } else {
                next["reasoning"] = Value::String("xhigh".to_string());
            }
            return;
        }
        return;
    }
    // 3) thinking: 字符串形态（部分网关）
    if let Some(effort) = next
        .get("thinking")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        let key = effort.trim().to_ascii_lowercase();
        if let Some(entry) = map.get(&key) {
            match entry {
                Some(wire) => next["thinking"] = Value::String(wire.clone()),
                None => {
                    next.as_object_mut().unwrap().remove("thinking");
                }
            }
            return;
        }
        if !is_standard_effort(&key) {
            if let Some(entry) = map.get("xhigh") {
                match entry {
                    Some(wire) => next["thinking"] = Value::String(wire.clone()),
                    None => {
                        next.as_object_mut().unwrap().remove("thinking");
                    }
                }
            } else {
                next["thinking"] = Value::String("xhigh".to_string());
            }
        }
    }
}

/// DeepSeek 推理模型默认 thinking 开启；只有显式 disabled 才算关闭。
pub(crate) fn thinking_enabled(next: &Value) -> bool {
    next.get("thinking")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
        .map(|t| t != "disabled")
        .unwrap_or(true)
}

/// tool_choice 除 "auto" / "none" 外（required / function 对象）在 thinking 模式下都会被 DeepSeek 拒绝。
pub(crate) fn forced_tool_choice(next: &Value) -> bool {
    match next.get("tool_choice") {
        Some(Value::String(s)) => !matches!(s.as_str(), "auto" | "none"),
        Some(v) if v.is_object() => true,
        _ => false,
    }
}

/// assistant 消息带 tool_calls 但没回传 reasoning_content，thinking 模式下 DeepSeek 会 400。
pub(crate) fn assistant_tool_calls_missing_reasoning(next: &Value) -> bool {
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
pub(crate) fn log_upstream_failure(alias: &ModelAlias, status: u16, body_text: &str) {
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
            crate::diag::append(
                &settings,
                "upstream.failure",
                serde_json::json!({
                    "provider": alias.provider(),
                    "model": alias.upstream_model(),
                    "alias": alias.alias,
                    "status": status,
                    "error": message.chars().take(500).collect::<String>(),
                }),
            );
        }
    }
}
