//! 报错分类规则（模板化）：把上游失败按 失效/限流/模型不支持/其他 分档，
//! 决定切 key、冻结、直接报错等动作。
//!
//! 设计要点：
//! - 分类优先级固定：invalid > rate_limited > model_unsupported > transient（兜底）。
//!   未命中任何规则的状态码一律按 transient 处理 —— 新供应商/新文案不再造成
//!   "该切 key 不切" 的硬失败（2026-10 ark CodingPlanEnterprise 文案事故）。
//! - 内置 default / ark 预设模板（代码内），`config/error-rules.json` 可选覆盖
//!   （模板整体替换同名模板 + tunables 覆盖 + 绑定覆盖）。dashboard 设置页可编辑。
//! - 关键词一律小写包含匹配；`keywords_all` 为 AND 组合（如 codingplan + subscription），
//!   `keywords_any` 为 OR。
//! - 限流类的解冻时刻：retry-after 头 → body "reset at" 时间戳 → 兜底时长（settings）。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const ERROR_RULES_PATH: &str = "config/error-rules.json";

/// 失败分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// key 失效（鉴权/欠费/订阅过期）：冻结整把 key，切下一把。
    Invalid,
    /// 限流：冻结到恢复时刻（retry-after / reset-at / 兜底），切下一把，到点自动恢复。
    RateLimited,
    /// 模型不支持：直接报错，不切 key（用户配置错误，非 key 问题）。
    ModelUnsupported,
    /// 其他/未知：按 key 重试 N 次 → 切下一把；差分冻结 + 全池熔断兜底。
    Transient,
}

impl ErrorClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorClass::Invalid => "invalid",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::ModelUnsupported => "model_unsupported",
            ErrorClass::Transient => "transient",
        }
    }
}

/// 单条分类规则：状态码集合（空 = 任意状态）+ 关键词组合。
/// 匹配条件 = (status 为空或包含) AND (keywords_all 全部命中) AND (keywords_any 为空或任一命中)。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClassRule {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords_all: Vec<String>,
}

impl ClassRule {
    pub fn matches(&self, status: u16, lowered_body: &str) -> bool {
        if !self.status.is_empty() && !self.status.contains(&status) {
            return false;
        }
        if !self.keywords_all.iter().all(|k| lowered_body.contains(k)) {
            return false;
        }
        if self.keywords_any.is_empty() {
            return true;
        }
        self.keywords_any.iter().any(|k| lowered_body.contains(k))
    }
}

/// 一个供应商模板：四类规则按优先级顺序求值。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClassTemplate {
    #[serde(default)]
    pub invalid: Vec<ClassRule>,
    #[serde(default)]
    pub rate_limited: Vec<ClassRule>,
    #[serde(default)]
    pub model_unsupported: Vec<ClassRule>,
    /// 仅作文档/展示用途：未命中以上三类时兜底就是 transient。
    #[serde(default)]
    pub transient: Vec<ClassRule>,
}

/// 可调参数（设置页编辑；None 项回落到 env Settings 默认值）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tunables {
    /// 失效类冻结时长（秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_freeze_seconds: Option<f64>,
    /// 差分冻结时长（秒）：本请求内该 key 失败而同行 key 成功时冻结。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub differential_freeze_seconds: Option<f64>,
    /// 其他类全池耗尽后的熔断时长（秒）；0 = 不熔断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient_exhausted_freeze_seconds: Option<f64>,
    /// 其他类每把 key 的总尝试次数（1 = 不重试直接切）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient_attempts_per_key: Option<u32>,
    /// 其他类同 key 重试间隔（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient_retry_interval_ms: Option<u64>,
}

/// 运行时生效的可调参数（JSON tunables 覆盖代码/环境默认值后的结果）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedTunables {
    pub invalid_freeze_seconds: f64,
    pub differential_freeze_seconds: f64,
    pub transient_exhausted_freeze_seconds: f64,
    pub transient_attempts_per_key: u32,
    pub transient_retry_interval_ms: u64,
}

impl Default for ResolvedTunables {
    fn default() -> Self {
        Self {
            invalid_freeze_seconds: 86_400.0,
            differential_freeze_seconds: 600.0,
            transient_exhausted_freeze_seconds: 300.0,
            transient_attempts_per_key: 1,
            transient_retry_interval_ms: 1_000,
        }
    }
}

impl Tunables {
    /// 叠加到默认值上：None 项用 default（invalid 可额外指定 env 默认）。
    pub fn resolve(&self, invalid_fallback: f64) -> ResolvedTunables {
        let base = ResolvedTunables::default();
        ResolvedTunables {
            invalid_freeze_seconds: self.invalid_freeze_seconds.unwrap_or(invalid_fallback),
            differential_freeze_seconds: self
                .differential_freeze_seconds
                .unwrap_or(base.differential_freeze_seconds),
            transient_exhausted_freeze_seconds: self
                .transient_exhausted_freeze_seconds
                .unwrap_or(base.transient_exhausted_freeze_seconds),
            transient_attempts_per_key: self
                .transient_attempts_per_key
                .unwrap_or(base.transient_attempts_per_key),
            transient_retry_interval_ms: self
                .transient_retry_interval_ms
                .unwrap_or(base.transient_retry_interval_ms),
        }
    }
}

/// 报错规则完整配置。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ErrorRulesConfig {
    #[serde(default)]
    pub tunables: Tunables,
    #[serde(default)]
    pub templates: BTreeMap<String, ClassTemplate>,
    /// 供应商 id -> 模板名；未登记的供应商先用同名模板（若有），再回落 default。
    #[serde(default)]
    pub provider_bindings: BTreeMap<String, String>,
}

/// 内置预设：default（供应商无关的状态码判据）。
pub fn default_template() -> ClassTemplate {
    ClassTemplate {
        invalid: vec![
            ClassRule {
                status: vec![401, 402, 403],
                ..Default::default()
            },
            ClassRule {
                status: Vec::new(),
                keywords_any: vec!["subscription has expired".into()],
                keywords_all: Vec::new(),
            },
        ],
        rate_limited: vec![ClassRule {
            status: vec![429],
            ..Default::default()
        }],
        model_unsupported: vec![ClassRule {
            status: vec![404],
            ..Default::default()
        }],
        transient: vec![ClassRule {
            status: vec![500, 502, 503, 504],
            ..Default::default()
        }],
    }
}

/// 内置预设：ark（两代订阅失效文案 + 企业版 CodingPlanEnterprise）。
pub fn ark_template() -> ClassTemplate {
    ClassTemplate {
        invalid: vec![
            ClassRule {
                status: vec![401, 402, 403],
                ..Default::default()
            },
            // 旧：does not have a valid CodingPlan subscription, or your subscription has expired
            // 新：lacks a valid CodingPlanEnterprise subscription, the plan has expired, or no seat is allocated
            ClassRule {
                status: vec![400],
                keywords_any: Vec::new(),
                keywords_all: vec!["codingplan".into(), "subscription".into()],
            },
            ClassRule {
                status: Vec::new(),
                keywords_any: vec!["subscription has expired".into()],
                keywords_all: Vec::new(),
            },
        ],
        rate_limited: vec![ClassRule {
            status: vec![429],
            ..Default::default()
        }],
        model_unsupported: vec![ClassRule {
            status: vec![404],
            ..Default::default()
        }],
        transient: vec![ClassRule {
            status: vec![500, 502, 503, 504],
            ..Default::default()
        }],
    }
}

/// 代码内置完整配置（error-rules.json 缺失/非法时的兜底，也是合并基底）。
pub fn builtin_config() -> ErrorRulesConfig {
    let mut templates = BTreeMap::new();
    templates.insert("default".to_string(), default_template());
    templates.insert("ark".to_string(), ark_template());
    ErrorRulesConfig {
        tunables: Tunables::default(),
        templates,
        provider_bindings: BTreeMap::new(),
    }
}

/// 解析供应商生效模板：显式绑定 > 同名模板 > default。
pub fn template_for<'a>(
    config: &'a ErrorRulesConfig,
    provider: &'a str,
) -> (&'a ClassTemplate, &'a str) {
    if let Some(name) = config.provider_bindings.get(provider) {
        if let Some(tpl) = config.templates.get(name) {
            return (tpl, name);
        }
    }
    if let Some(tpl) = config.templates.get(provider) {
        return (tpl, provider);
    }
    config
        .templates
        .get("default")
        .map(|tpl| (tpl, "default"))
        .unwrap_or_else(|| {
            // default 模板理论上是代码内置的；空配置（用户删空）时给空规则集。
            static EMPTY: std::sync::OnceLock<ClassTemplate> = std::sync::OnceLock::new();
            (EMPTY.get_or_init(ClassTemplate::default), "empty")
        })
}

/// 分类结果：类别 + 命中规则标签（用于日志/设置页对照）。
#[derive(Clone, Debug)]
pub struct ClassifiedFailure {
    pub class: ErrorClass,
    pub rule: String,
}

/// 对一次上游失败分类。`lowered_body` 为小写化的响应体（连接错误传空串）。
pub fn classify(
    config: &ErrorRulesConfig,
    provider: &str,
    status: u16,
    lowered_body: &str,
) -> ClassifiedFailure {
    let (template, template_name) = template_for(config, provider);
    let checks: [(&str, ErrorClass, &Vec<ClassRule>); 3] = [
        ("invalid", ErrorClass::Invalid, &template.invalid),
        (
            "rate_limited",
            ErrorClass::RateLimited,
            &template.rate_limited,
        ),
        (
            "model_unsupported",
            ErrorClass::ModelUnsupported,
            &template.model_unsupported,
        ),
    ];
    for (label, class, rules) in checks {
        for (idx, rule) in rules.iter().enumerate() {
            if rule.matches(status, lowered_body) {
                return ClassifiedFailure {
                    class,
                    rule: format!("{template_name}/{label}#{idx}"),
                };
            }
        }
    }
    ClassifiedFailure {
        class: ErrorClass::Transient,
        rule: format!("{template_name}/transient#fallback"),
    }
}

/// 从磁盘加载 error-rules.json；文件缺失返回内置默认，非法 JSON 返回错误信息
/// （调用方保留 last-good 配置）。
pub fn load_error_rules(path: &str) -> Result<ErrorRulesConfig, String> {
    let expanded = crate::config::expand_path(path);
    let content = match std::fs::read_to_string(&expanded) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(builtin_config());
        }
        Err(err) => return Err(format!("read {}: {err}", expanded.display())),
    };
    let value: Value = serde_json::from_str(&content)
        .map_err(|err| format!("parse {}: {err}", expanded.display()))?;
    serde_json::from_value(value).map_err(|err| format!("invalid error-rules schema: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_classifies_by_status() {
        let config = builtin_config();
        assert_eq!(
            classify(&config, "aixhan", 401, "").class,
            ErrorClass::Invalid
        );
        assert_eq!(
            classify(&config, "aixhan", 429, "").class,
            ErrorClass::RateLimited
        );
        assert_eq!(
            classify(&config, "aixhan", 404, "model not found").class,
            ErrorClass::ModelUnsupported
        );
        assert_eq!(
            classify(
                &config,
                "aixhan",
                400,
                "invalid parameter: messages is empty"
            )
            .class,
            ErrorClass::Transient
        );
        assert_eq!(
            classify(&config, "aixhan", 503, "").class,
            ErrorClass::Transient
        );
    }

    #[test]
    fn ark_template_matches_both_subscription_message_generations() {
        let config = builtin_config();
        let old_body = r#"{"error":{"message":"Your account (2102661813) does not have a valid CodingPlan subscription, or your subscription has expired."}}"#.to_lowercase();
        let new_body = r#"{"error":{"message":"Your account (2102405658, 77769405) lacks a valid CodingPlanEnterprise subscription, the plan has expired, or no seat is allocated."}}"#.to_lowercase();
        assert_eq!(
            classify(&config, "ark", 400, &old_body).class,
            ErrorClass::Invalid
        );
        assert_eq!(
            classify(&config, "ark", 400, &new_body).class,
            ErrorClass::Invalid
        );
        // 非 ark 供应商同文案也按 default 的订阅过期兜底命中
        assert_eq!(
            classify(&config, "aixhan", 400, &old_body).class,
            ErrorClass::Invalid
        );
        // 普通 400 不误伤
        assert_eq!(
            classify(&config, "ark", 400, "invalid parameter: messages is empty").class,
            ErrorClass::Transient
        );
    }

    #[test]
    fn provider_binding_resolution_order() {
        let mut config = builtin_config();
        config
            .provider_bindings
            .insert("aixhan".to_string(), "ark".to_string());
        let body = r#"{"error":{"message":"lacks a valid CodingPlanEnterprise subscription"}}"#
            .to_lowercase();
        assert_eq!(
            classify(&config, "aixhan", 400, &body).class,
            ErrorClass::Invalid
        );
        assert_eq!(
            classify(&config, "aixhan", 400, &body).rule,
            "ark/invalid#1"
        );
        // 未登记供应商用同名模板（若有）
        assert_eq!(classify(&config, "ark", 400, &body).rule, "ark/invalid#1");
        // 未登记且无同名模板 -> default
        assert_eq!(
            classify(&config, "opencode-go", 401, "").rule,
            "default/invalid#0"
        );
    }

    #[test]
    fn load_missing_file_falls_back_to_builtin() {
        let config = load_error_rules("/nonexistent/error-rules.json").unwrap();
        assert_eq!(config, builtin_config());
    }

    #[test]
    fn rule_matching_combinations() {
        let rule = ClassRule {
            status: vec![400],
            keywords_any: Vec::new(),
            keywords_all: vec!["codingplan".into(), "subscription".into()],
        };
        assert!(rule.matches(400, "lacks a valid codingplanenterprise subscription"));
        assert!(!rule.matches(401, "codingplan subscription"));
        assert!(!rule.matches(400, "codingplan only no sub"));
        let any = ClassRule {
            status: Vec::new(),
            keywords_any: vec!["alpha".into(), "beta".into()],
            keywords_all: Vec::new(),
        };
        assert!(any.matches(500, "carries beta signal"));
        assert!(!any.matches(500, "carries gamma signal"));
    }
}
