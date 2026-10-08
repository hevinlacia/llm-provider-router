//! 限流恢复时刻解析与 key 状态 id（纯函数）。
//!
//! 历史说明：本模块原先还承担 key 级失效判定（is_subscription_invalid /
//! parse_auth_invalid / maybe_freeze_key，靠报错文案匹配）。2026-10 起
//! 失效/限流/模型不支持/其他四类判定迁移到 `error_rules`（模板化规则 +
//! 差分冻结），文案匹配不再是"是否切 key"的裁判；这里只保留
//! 限流恢复时刻的解析（retry-after 头 / body reset-at 时间戳）。
//! "subscription has expired" / 401-403 / ark codingplan 组合等特征
//! 已移入 error_rules 内置模板（default / ark），可在 dashboard 设置页编辑。

use crate::config::KeyRef;
use crate::state_store::now_seconds;
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

/// 解析月度/5小时配额重置时刻；未解析到时用调用方给的兜底时长。
pub fn parse_quota_reset(
    text: &str,
    monthly_fallback_seconds: f64,
    five_hour_fallback_seconds: f64,
) -> Option<(f64, &'static str)> {
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
        Some((now_seconds() + monthly_fallback_seconds, "monthly_quota"))
    } else {
        Some((
            now_seconds() + five_hour_fallback_seconds,
            "five_hour_quota",
        ))
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
