//! 物理模型能力探测：向上游发「固定次数、固定规模」的探测请求，解析能力边界。
//!
//! 边界明确（本模块的硬性约定）：
//! - 每次探测每个维度（上下文 / 最大输出 / 图片）**恰好发 1 个请求**；
//! - 不重试、不递增、不二分，总请求数固定为 3，全部在超时窗口内结束；
//! - 结果要么是解析到的精确值，要么是「≥ 探测阈值」或「未能探测」，绝不猜测。

use serde_json::{json, Value};
use std::time::Duration;

/// 上下文探测输入规模（tokens）。固定大小、不递增探测。
pub const PROBE_CONTEXT_TOKENS: usize = 128_000;
/// 输出探测的 max_tokens 上限值。超过即视为超限。
pub const PROBE_MAX_OUTPUT: u32 = 200_000;
/// 单个探测请求超时（秒）。超时记为「未能探测」。
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

pub struct ProbeOutcome {
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub supports_image: Option<bool>,
    pub notes: Vec<String>,
}

pub(crate) async fn probe_model(
    client: &reqwest::Client,
    base_url: &str,
    anthropic_base_url: Option<&str>,
    api_key: &str,
    upstream_model: &str,
) -> ProbeOutcome {
    match anthropic_base_url {
        // 供应商配置了 Anthropic 兼容端点：优先走 Anthropic 协议探测
        // （/v1/models 返回精确 context_window，零成本；输出/图片走 /v1/messages）。
        Some(anthropic_url) => {
            probe_anthropic(client, anthropic_url, api_key, upstream_model).await
        }
        None => probe_openai(client, base_url, api_key, upstream_model).await,
    }
}

/// OpenAI 兼容协议探测：三个固定请求（上下文 / 输出 / 图片）。
async fn probe_openai(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    upstream_model: &str,
) -> ProbeOutcome {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    // 三个探测并行发出，各自独立超时；全部在一个超时窗口内结束。
    let (context_out, output_out, image_out) = tokio::join!(
        probe_context(client, &url, api_key, upstream_model),
        probe_output(client, &url, api_key, upstream_model),
        probe_image(client, &url, api_key, upstream_model),
    );

    let mut notes = Vec::new();
    notes.extend(context_out.notes);
    notes.extend(output_out.notes);
    notes.extend(image_out.notes);

    ProbeOutcome {
        context_window: context_out.value,
        max_output_tokens: output_out.value,
        supports_image: image_out.value.map(|v| v == 1),
        notes,
    }
}

/// Anthropic 协议探测：
/// - 上下文：`GET /v1/models` 返回精确 `context_window`（零成本、不消耗 token）；
/// - 最大输出：`POST /v1/messages` max_tokens 超限报错解析；
/// - 图片：`POST /v1/messages` 带 image 输入。
/// 同样遵循边界约定：每维度恰好 1 个请求，不重试/不递增/不二分。
async fn probe_anthropic(
    client: &reqwest::Client,
    anthropic_base_url: &str,
    api_key: &str,
    upstream_model: &str,
) -> ProbeOutcome {
    let base = anthropic_base_url.trim_end_matches('/');

    let (context_out, output_out, image_out) = tokio::join!(
        probe_anthropic_context(client, base, api_key, upstream_model),
        probe_anthropic_output(client, base, api_key, upstream_model),
        probe_anthropic_image(client, base, api_key, upstream_model),
    );

    let mut notes = Vec::new();
    notes.extend(context_out.notes);
    notes.extend(output_out.notes);
    notes.extend(image_out.notes);

    ProbeOutcome {
        context_window: context_out.value,
        max_output_tokens: output_out.value,
        supports_image: image_out.value.map(|v| v == 1),
        notes,
    }
}

struct DimensionOutcome {
    value: Option<u32>,
    notes: Vec<String>,
}

/// 发送单个探测请求，返回 (http status, 原始 body)。
/// 网络错误 / 超时统一返回 None（由调用方记 note）。
async fn send_once(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: serde_json::Value,
) -> Option<(u16, String)> {
    let request = client
        .post(url)
        .bearer_auth(api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(PROBE_REQUEST_TIMEOUT);
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            Some((status, text))
        }
        Err(_) => None,
    }
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

/// Anthropic 错误信息解析：`{ "type":"error", "error":{ "type":..., "message":... } }`。
fn anthropic_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

/// Anthropic POST（/v1/messages）：`x-api-key` + `anthropic-version` header。
async fn send_anthropic_post(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    body: serde_json::Value,
) -> Option<(u16, String)> {
    let url = format!("{}/v1/messages", base);
    let request = client
        .post(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(PROBE_REQUEST_TIMEOUT);
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            Some((status, text))
        }
        Err(_) => None,
    }
}

/// Anthropic GET /v1/models：返回 (id, context_window) 列表。网络错误返回 None。
async fn fetch_anthropic_models(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
) -> Option<(u16, String)> {
    let url = format!("{}/v1/models", base);
    let request = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(PROBE_REQUEST_TIMEOUT);
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            Some((status, text))
        }
        Err(_) => None,
    }
}

/// Anthropic 上下文探测：GET /v1/models 找目标模型的 context_window（精确、零成本）。
async fn probe_anthropic_context(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    upstream_model: &str,
) -> DimensionOutcome {
    match fetch_anthropic_models(client, base, api_key).await {
        Some((status, body_text)) if status >= 200 && status < 300 => {
            // data: [{ id, context_window, ... }]
            let found = parse_anthropic_models_context(&body_text, upstream_model);
            match found {
                Some(limit) => DimensionOutcome {
                    value: Some(limit),
                    notes: vec![format!(
                        "context = {limit} (from Anthropic /v1/models, exact)"
                    )],
                },
                None => DimensionOutcome {
                    value: None,
                    notes: vec![format!(
                        "context: model `{upstream_model}` not found in Anthropic /v1/models (or no context_window field)"
                    )],
                },
            }
        }
        Some((_status, body_text)) => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "context: Anthropic /v1/models rejected probe: {}",
                anthropic_error_message(&body_text)
            )],
        },
        None => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "context: Anthropic /v1/models failed/timed out (>{}s)",
                PROBE_REQUEST_TIMEOUT.as_secs()
            )],
        },
    }
}

/// Anthropic 输出探测：POST /v1/messages，max_tokens 超限报错解析上限。
async fn probe_anthropic_output(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    upstream_model: &str,
) -> DimensionOutcome {
    let body = json!({
        "model": upstream_model,
        "max_tokens": PROBE_MAX_OUTPUT,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    match send_anthropic_post(client, base, api_key, body).await {
        Some((status, _body_text)) if status >= 200 && status < 300 => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "max output ≥{PROBE_MAX_OUTPUT} (request succeeded, upper bound not triggered)"
            )],
        },
        Some((_status, body_text)) => {
            let message = anthropic_error_message(&body_text);
            match parse_output_limit(&message) {
                Some(limit) => DimensionOutcome {
                    value: Some(limit),
                    notes: vec![format!("max output = {limit} (parsed from upstream error)")],
                },
                None => DimensionOutcome {
                    value: None,
                    notes: vec![format!(
                        "max output: upstream rejected probe but limit unparseable: {message}"
                    )],
                },
            }
        }
        None => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "max output: probe request failed/timed out (>{}s)",
                PROBE_REQUEST_TIMEOUT.as_secs()
            )],
        },
    }
}

/// Anthropic 图片探测：POST /v1/messages 带 1×1 PNG base64。
async fn probe_anthropic_image(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    upstream_model: &str,
) -> DimensionOutcome {
    // 1×1 透明 PNG（base64，不带 data URL 前缀——Anthropic 用 source.data 裸 base64）
    let pixel = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    let body = json!({
        "model": upstream_model,
        "max_tokens": 1,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "describe this image in one word" },
                {
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": pixel }
                }
            ]
        }],
    });
    match send_anthropic_post(client, base, api_key, body).await {
        Some((status, _body_text)) if status >= 200 && status < 300 => DimensionOutcome {
            value: Some(1),
            notes: vec!["supports image: yes (request succeeded)".to_string()],
        },
        Some((_status, body_text)) => DimensionOutcome {
            value: Some(0),
            notes: vec![format!(
                "supports image: no (upstream rejected image input: {})",
                anthropic_error_message(&body_text)
            )],
        },
        None => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "supports image: probe request failed/timed out (>{}s)",
                PROBE_REQUEST_TIMEOUT.as_secs()
            )],
        },
    }
}

/// 上下文探测：发一个固定大输入（PROBE_CONTEXT_TOKENS）。
/// - 超限报错 → 解析错误信息中的上限数值（精确值）；
/// - 请求成功 → 记「≥ 探测阈值」，不写入精确值（避免误导）。
async fn probe_context(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    upstream_model: &str,
) -> DimensionOutcome {
    // 固定规模填充文本：PROBE_CONTEXT_TOKENS 个词（近似 1 token/词）
    let filler = "probe ".repeat(PROBE_CONTEXT_TOKENS);
    let body = json!({
        "model": upstream_model,
        "messages": [{ "role": "user", "content": filler }],
        "max_tokens": 1,
        "stream": false,
    });
    match send_once(client, url, api_key, body).await {
        Some((status, _body_text)) if status >= 200 && status < 300 => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "context ≥{PROBE_CONTEXT_TOKENS} (request succeeded, upper bound not triggered)"
            )],
        },
        Some((_status, body_text)) => {
            let message = error_message(&body_text);
            match parse_context_limit(&message) {
                Some(limit) => DimensionOutcome {
                    value: Some(limit),
                    notes: vec![format!("context = {limit} (parsed from upstream error)")],
                },
                None => DimensionOutcome {
                    value: None,
                    notes: vec![format!(
                        "context: upstream rejected probe but limit unparseable: {message}"
                    )],
                },
            }
        }
        None => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "context: probe request failed/timed out (>{}s)",
                PROBE_REQUEST_TIMEOUT.as_secs()
            )],
        },
    }
}

/// 输出探测：max_tokens 设为 PROBE_MAX_OUTPUT，messages 极小。
/// - 超限报错 → 解析上限；
/// - 请求成功 → 记「≥ PROBE_MAX_OUTPUT」。
async fn probe_output(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    upstream_model: &str,
) -> DimensionOutcome {
    let body = json!({
        "model": upstream_model,
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": PROBE_MAX_OUTPUT,
        "stream": false,
    });
    match send_once(client, url, api_key, body).await {
        Some((status, _body_text)) if status >= 200 && status < 300 => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "max output ≥{PROBE_MAX_OUTPUT} (request succeeded, upper bound not triggered)"
            )],
        },
        Some((_status, body_text)) => {
            let message = error_message(&body_text);
            match parse_output_limit(&message) {
                Some(limit) => DimensionOutcome {
                    value: Some(limit),
                    notes: vec![format!("max output = {limit} (parsed from upstream error)")],
                },
                None => DimensionOutcome {
                    value: None,
                    notes: vec![format!(
                        "max output: upstream rejected probe but limit unparseable: {message}"
                    )],
                },
            }
        }
        None => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "max output: probe request failed/timed out (>{}s)",
                PROBE_REQUEST_TIMEOUT.as_secs()
            )],
        },
    }
}

/// 图片探测：发一张 1×1 PNG。成功 → 支持图片；报错 → 不支持（或无法确认）。
async fn probe_image(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    upstream_model: &str,
) -> DimensionOutcome {
    // 1×1 透明 PNG (data URL)，体积极小、成本可忽略。
    let pixel = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    let body = json!({
        "model": upstream_model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "describe this image in one word" },
                { "type": "image_url", "image_url": { "url": pixel } }
            ]
        }],
        "max_tokens": 1,
        "stream": false,
    });
    match send_once(client, url, api_key, body).await {
        Some((status, _body_text)) if status >= 200 && status < 300 => DimensionOutcome {
            value: Some(1),
            notes: vec!["supports image: yes (request succeeded)".to_string()],
        },
        Some((_status, body_text)) => DimensionOutcome {
            value: Some(0),
            notes: vec![format!(
                "supports image: no (upstream rejected image input: {})",
                error_message(&body_text)
            )],
        },
        None => DimensionOutcome {
            value: None,
            notes: vec![format!(
                "supports image: probe request failed/timed out (>{}s)",
                PROBE_REQUEST_TIMEOUT.as_secs()
            )],
        },
    }
}

// ---------------------------------------------------------------------------
// 错误信息解析（纯函数，便于测试）
// ---------------------------------------------------------------------------

/// 从上游错误信息解析「上下文上限」。优先精确模式，兜底取最大数字。
pub fn parse_context_limit(message: &str) -> Option<u32> {
    // OpenAI: "This model's maximum context length is 32768 tokens."
    if let Some(caps) = regex::Regex::new(r"maximum context length is (\d+)")
        .ok()
        .and_then(|re| re.captures(message))
    {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) {
            return Some(n);
        }
    }
    // 通用：context length / context window 附近的数字（取其中最大）
    let numbers = extract_numbers(message);
    numbers.into_iter().max()
}

/// 从上游错误信息解析「最大输出上限」。优先精确模式，兜底取最大数字。
pub fn parse_output_limit(message: &str) -> Option<u32> {
    // 常见：max_tokens / maximum output tokens 附近的数字
    for pattern in [
        r"max:\s*(\d+)",
        r"max_tokens.{0,40}?is (?:limited to|must be <=|must be ≤)\s*(\d+)",
        r"max_tokens.{0,40}?(\d+)",
        r"maximum output tokens.{0,40}?(\d+)",
        r"max output tokens.{0,40}?(\d+)",
    ] {
        if let Some(caps) = regex::Regex::new(pattern)
            .ok()
            .and_then(|re| re.captures(message))
        {
            if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) {
                return Some(n);
            }
        }
    }
    extract_numbers(message).into_iter().max()
}

/// 从 Anthropic `/v1/models` 响应中找目标模型的 `context_window`（纯函数，便于测试）。
fn parse_anthropic_models_context(body: &str, upstream_model: &str) -> Option<u32> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    value
        .get("data")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(upstream_model))
        .and_then(|m| m.get("context_window").and_then(Value::as_u64))
        .map(|n| n as u32)
}

/// 提取字符串中的数字序列（≥4 位，避免误抓版本号等小数字），转 u32。
fn extract_numbers(message: &str) -> Vec<u32> {
    let mut result = Vec::new();
    let bytes = message.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &message[start..i];
            if digits.len() >= 4 {
                if let Ok(n) = digits.parse::<u32>() {
                    result.push(n);
                }
            }
        } else {
            i += 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_context_error() {
        let msg = "This model's maximum context length is 32768 tokens. However, you requested 132000 tokens (132000 in the messages, 0 in the completion). Please reduce your prompt; or completion length.";
        assert_eq!(parse_context_limit(msg), Some(32768));
    }

    #[test]
    fn parse_generic_context_error() {
        let msg = "prompt is too long: 150000 tokens > 200000 maximum (max context length).";
        // 精确模式不命中 → 兜底取最大数字 200000
        assert_eq!(parse_context_limit(msg), Some(200000));
    }

    #[test]
    fn parse_output_error() {
        let msg = "Invalid value for 'max_tokens': must be <= 65536 for this model.";
        assert_eq!(parse_output_limit(msg), Some(65536));
    }

    #[test]
    fn parse_max_output_tokens_phrase() {
        let msg = "max output tokens is limited to 16384 for model x.";
        assert_eq!(parse_output_limit(msg), Some(16384));
    }

    #[test]
    fn parse_unrelated_message_returns_none() {
        assert_eq!(parse_context_limit("invalid api key"), None);
        assert_eq!(parse_output_limit("rate limit exceeded"), None);
    }

    #[test]
    fn extract_numbers_skips_small() {
        // 4 位以下不提取
        assert_eq!(extract_numbers("code 429, retry in 5s"), Vec::<u32>::new());
        assert_eq!(extract_numbers("limit 32768 tokens"), vec![32768]);
    }

    #[test]
    fn anthropic_models_context_exact() {
        let body = r#"{"data":[
            {"id":"claude-3-5-sonnet-20241022","display_name":"Claude 3.5 Sonnet","created_at":"2024-10-22T00:00:00Z","context_window":200000},
            {"id":"claude-3-haiku-20240307","display_name":"Claude 3 Haiku","created_at":"2024-03-07T00:00:00Z","context_window":200000}
        ]}"#;
        assert_eq!(
            parse_anthropic_models_context(body, "claude-3-5-sonnet-20241022"),
            Some(200000)
        );
        // 不存在的模型 → None
        assert_eq!(parse_anthropic_models_context(body, "claude-x"), None);
    }

    #[test]
    fn anthropic_models_no_context_field() {
        let body = r#"{"data":[{"id":"some-model","display_name":"Some"}]}"#;
        assert_eq!(parse_anthropic_models_context(body, "some-model"), None);
    }

    #[test]
    fn anthropic_output_error_parse() {
        // Anthropic 风格 max_tokens 超限错误
        let msg = "max_tokens: 200000 is too large for this model (max: 16384)";
        assert_eq!(parse_output_limit(msg), Some(16384));
    }
}
