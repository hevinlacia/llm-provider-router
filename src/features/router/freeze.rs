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
    // 中文文案（zp/智谱 bigmodel，2026-10）："已达到 5 小时使用上限，2026-10-14
    // 19:14:36 后可继续使用"。含 "5 小时" 归为 5 小时限额；其余含 "使用上限"
    // 的中文文案归为月度额度。
    let five_hour = lowered.contains("you have exceeded the 5-hour usage quota")
        || lowered.contains("5 小时使用上限")
        || lowered.contains("5小时使用上限");
    let monthly = lowered.contains("you have exceeded the monthly usage quota")
        || (lowered.contains("使用上限") && !five_hour);
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
    parse_reset_timestamp_offset(text).or_else(|| parse_reset_timestamp_cn(text))
}

/// 英文格式："reset at YYYY-MM-DD HH:MM:SS +ZZZZ"（自带时区）。
fn parse_reset_timestamp_offset(text: &str) -> Option<f64> {
    let regex =
        Regex::new(r"(?i)reset at (\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2}) ([+-]\d{4})").ok()?;
    let captures = regex.captures(text)?;
    let value = format!("{} {} {}", &captures[1], &captures[2], &captures[3]);
    chrono::DateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S %z")
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

/// 中文格式（zp/智谱 bigmodel）："2026-10-14 19:14:36 后可继续使用"，北京时间 UTC+8。
fn parse_reset_timestamp_cn(text: &str) -> Option<f64> {
    let regex = Regex::new(r"(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2}:\d{2})\s*后可继续使用").ok()?;
    let captures = regex.captures(text)?;
    let naive = chrono::NaiveDateTime::parse_from_str(
        &format!("{} {}", &captures[1], &captures[2]),
        "%Y-%m-%d %H:%M:%S",
    )
    .ok()?;
    let beijing = chrono::FixedOffset::east_opt(8 * 3600)?;
    use chrono::TimeZone;
    let dt = beijing.from_local_datetime(&naive).single()?;
    Some(dt.timestamp() as f64)
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

#[cfg(test)]
mod tests {
    use super::*;

    const MONTHLY_FALLBACK: f64 = 86_400.0;
    const FIVE_HOUR_FALLBACK: f64 = 5_400.0;

    #[test]
    fn english_reset_at_still_parsed() {
        let text = "You have exceeded the 5-hour usage quota. Reset at 2026-10-14 19:14:36 +0800.";
        let (until, reason) =
            parse_quota_reset(text, MONTHLY_FALLBACK, FIVE_HOUR_FALLBACK).unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-14T19:14:36+08:00").unwrap();
        assert_eq!(reason, "five_hour_quota");
        assert_eq!(until, expected.timestamp() as f64);
    }

    #[test]
    fn zp_chinese_five_hour_message_parses_beijing_reset() {
        let text = "Error: rate_limit_exceeded: 已达到 5 小时使用上限，2026-10-14 19:14:36 后可继续使用。如需超限额按量付费使用，可联系管理员开启超额按量付费。[202610141701269cebf285bfc34e9e]";
        let (until, reason) =
            parse_quota_reset(text, MONTHLY_FALLBACK, FIVE_HOUR_FALLBACK).unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-14T19:14:36+08:00").unwrap();
        assert_eq!(reason, "five_hour_quota");
        assert_eq!(until, expected.timestamp() as f64);
    }

    #[test]
    fn zp_chinese_without_space_variant_parses() {
        let text = "已达到5小时使用上限，2026-10-14 19:14:36后可继续使用。";
        let (until, reason) =
            parse_quota_reset(text, MONTHLY_FALLBACK, FIVE_HOUR_FALLBACK).unwrap();
        assert_eq!(reason, "five_hour_quota");
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-14T19:14:36+08:00").unwrap();
        assert_eq!(until, expected.timestamp() as f64);
    }

    #[test]
    fn zp_chinese_monthly_message_falls_back() {
        let text = "已达到包月使用上限，请联系管理员。";
        let before = now_seconds();
        let (until, reason) =
            parse_quota_reset(text, MONTHLY_FALLBACK, FIVE_HOUR_FALLBACK).unwrap();
        assert_eq!(reason, "monthly_quota");
        assert!((until - (before + MONTHLY_FALLBACK)).abs() < 5.0);
    }

    #[test]
    fn zp_chinese_five_hour_without_timestamp_uses_fallback() {
        let text = "已达到 5 小时使用上限。";
        let before = now_seconds();
        let (until, reason) =
            parse_quota_reset(text, MONTHLY_FALLBACK, FIVE_HOUR_FALLBACK).unwrap();
        assert_eq!(reason, "five_hour_quota");
        assert!((until - (before + FIVE_HOUR_FALLBACK)).abs() < 5.0);
    }

    #[test]
    fn non_quota_text_returns_none() {
        assert!(
            parse_quota_reset("invalid request", MONTHLY_FALLBACK, FIVE_HOUR_FALLBACK).is_none()
        );
        // 时间戳存在但无配额文案时不算恢复时刻
        let text = "request failed at 2026-10-14 19:14:36 后可继续使用";
        assert!(parse_quota_reset(text, MONTHLY_FALLBACK, FIVE_HOUR_FALLBACK).is_none());
    }
}
