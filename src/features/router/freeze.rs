//! 配额/鉴权失败/retry-after 解析与 key 冻结判定（纯函数）。

use super::state::RouterState;
use crate::config::{KeyRef, Settings};
use crate::state_store::now_seconds;
use http::HeaderMap;
use regex::Regex;

pub fn parse_retry_after(value: Option<&str>) -> Option<f64> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(now_seconds() + seconds.max(1) as f64);
    }
    httpdate::parse_http_date(value).ok().and_then(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs_f64())
    })
}

pub fn parse_quota_reset(text: &str, settings: &Settings) -> Option<(f64, &'static str)> {
    let lowered = text.to_lowercase();
    let monthly = lowered.contains("you have exceeded the monthly usage quota");
    let five_hour = lowered.contains("you have exceeded the 5-hour usage quota");
    if !monthly && !five_hour {
        return None;
    }
    if let Some(reset_at) = parse_reset_timestamp(text) {
        return Some((
            reset_at,
            if monthly {
                "monthly_quota"
            } else {
                "five_hour_quota"
            },
        ));
    }
    if monthly {
        Some((
            now_seconds() + settings.monthly_quota_fallback_seconds,
            "monthly_quota",
        ))
    } else {
        Some((
            now_seconds() + settings.five_hour_quota_fallback_seconds,
            "five_hour_quota",
        ))
    }
}

pub fn parse_auth_invalid(text: &str, settings: &Settings) -> Option<(f64, &'static str)> {
    let lowered = text.to_lowercase();
    if lowered.contains("authentication_error")
        || lowered.contains("authentication fails")
        || (lowered.contains("api key") && lowered.contains("invalid"))
    {
        Some((
            now_seconds() + settings.auth_invalid_freeze_seconds,
            "auth_invalid",
        ))
    } else {
        None
    }
}

fn parse_reset_timestamp(text: &str) -> Option<f64> {
    let regex =
        Regex::new(r"(?i)reset at (\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2}) ([+-]\d{4})").ok()?;
    let captures = regex.captures(text)?;
    let value = format!("{} {} {}", &captures[1], &captures[2], &captures[3]);
    chrono::DateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S %z")
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

/// key 状态唯一标识：跨供应商同名 key 用 `provider/name` 区分，
/// 避免不同供应商同名 key 在 frozen / binding 中互相影响。
pub(crate) fn key_state_id(key: &KeyRef) -> String {
    if key.provider.is_empty() {
        key.name.clone()
    } else {
        format!("{}/{}", key.provider, key.name)
    }
}

pub fn maybe_freeze_key(
    state: &mut RouterState,
    key: &KeyRef,
    status_code: u16,
    headers: &HeaderMap,
    body_text: &str,
    settings: &Settings,
) -> anyhow::Result<()> {
    if status_code < 400 {
        return Ok(());
    }
    if let Some((until, reason)) = parse_quota_reset(body_text, settings) {
        state.freeze(&key_state_id(key), until, reason)?;
        return Ok(());
    }
    if matches!(status_code, 401 | 403) {
        if let Some((until, reason)) = parse_auth_invalid(body_text, settings) {
            state.freeze(&key_state_id(key), until, reason)?;
            return Ok(());
        }
    }
    if status_code == 429 {
        if let Some(until) = parse_retry_after(
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
        ) {
            state.freeze(&key_state_id(key), until, "retry_after")?;
        }
    }
    Ok(())
}
