use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

pub const DEFAULT_ARK_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/coding/v3";
pub const DEFAULT_WEIGHT_CONFIG_PATH: &str = "config/key-weights.json";
pub const DEFAULT_PROVIDER_CONFIG_PATH: &str = "config/providers.json";
pub const DEFAULT_CUSTOM_KEY_CONFIG_PATH: &str = "config/custom-keys.json";
pub const DEFAULT_API_KEYS_PATH: &str = "config/api-keys.json";
pub const DEFAULT_TOKEN_PRICE_CONFIG_PATH: &str = "config/token-prices.json";
pub const DEFAULT_MODEL_ALIAS_CONFIG_PATH: &str = "config/custom-model-aliases.json";
pub const DEFAULT_SEARCH_PROVIDERS_PATH: &str = "config/search-providers.json";
pub const DEFAULT_USAGE_DB_PATH: &str = "~/.local/state/llm-provider-router/usage.sqlite3";
pub const DEFAULT_STATE_DB_PATH: &str = "~/.local/state/llm-provider-router/state.sqlite3";
pub const DEFAULT_DIAG_DIR: &str = "~/.local/state/llm-provider-router/logs";
pub const DEFAULT_DIAG_MAX_BYTES: &str = "10485760";
pub const DEFAULT_DIAG_MAX_FILES: &str = "50";
pub const DEFAULT_DIAG_SAMPLE_EVERY: &str = "1";

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct KeyRef {
    pub name: String,
    pub env_var: String,
    pub weight: i64,
    pub provider: String,
    pub billing_type: String,
    /// Whether the key value may be persisted to config/api-keys.json.
    /// Env-only keys (persist=false) are read strictly from the environment.
    #[serde(default = "default_persist")]
    pub persist: bool,
}

fn default_persist() -> bool {
    true
}

impl KeyRef {
    pub fn new(name: &str, env_var: &str, weight: i64) -> Self {
        Self::with_provider(name, env_var, weight, "ark", "subscription")
    }

    pub fn with_provider(
        name: &str,
        env_var: &str,
        weight: i64,
        provider: &str,
        billing_type: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            env_var: env_var.to_string(),
            weight,
            provider: provider.to_string(),
            billing_type: billing_type.to_string(),
            persist: true,
        }
    }

    /// Key that must come from the environment only; its value is never
    /// persisted to config/api-keys.json (vault restores it into the env file).
    pub fn env_only(
        name: &str,
        env_var: &str,
        weight: i64,
        provider: &str,
        billing_type: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            env_var: env_var.to_string(),
            weight,
            provider: provider.to_string(),
            billing_type: billing_type.to_string(),
            persist: false,
        }
    }

    pub fn with_weight(&self, weight: i64) -> Self {
        Self {
            weight,
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    // max_retry_seconds / retry_delay_seconds 保留用于配置解析与展示；
    // 运行时退避重试已改为：key 全冻结时直接 fallback，不做长时间空转。
    #[allow(dead_code)]
    pub max_retry_seconds: u64,
    #[allow(dead_code)]
    pub retry_delay_seconds: f64,
    pub retry_on_status: Vec<u16>,
}

impl RetryPolicy {
    pub fn new(max_retry_seconds: u64, retry_delay_seconds: f64, retry_on_status: &[u16]) -> Self {
        Self {
            max_retry_seconds,
            retry_delay_seconds,
            retry_on_status: retry_on_status.to_vec(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelAlias {
    pub alias: String,
    pub litellm_model: String,
    pub base_url: String,
    pub keys: Vec<KeyRef>,
    pub retry_policy: Option<RetryPolicy>,
    /// 服务器侧默认/覆写参数（v2：逻辑模型默认 params 与物理模型覆写 params 的合并结果）。
    /// 应用时只填充客户端未提供的字段，不覆盖客户端显式参数。
    pub params: HashMap<String, serde_json::Value>,
    /// 协商用：物理模型的真实上下文窗口（tokens）。None = 未声明，取保守默认。
    pub context_window: Option<u32>,
    /// 协商用：物理模型单次最大输出 tokens。
    pub max_output_tokens: Option<u32>,
    /// 思考强度映射：客户端标准档位 -> 上游实际 wire 值（None=不支持）。
    pub thinking_level_map: Option<HashMap<String, Option<String>>>,
    /// 上游思考字段协议（reasoning_effort / deepseek 等），供调试与后续 format 翻译使用。
    pub thinking_format: Option<String>,
    /// 供应商配置的 Responses API 基础地址（可选）。配置后 = 原生支持 Responses，
    /// 对 `/v1/responses` 请求透传到 `{responses_base_url}/responses`；
    /// None = 由 Router 翻译成 chat completions 走 `base_url`。
    pub responses_base_url: Option<String>,
}

impl ModelAlias {
    pub fn new(
        alias: &str,
        litellm_model: &str,
        base_url: &str,
        keys: Vec<KeyRef>,
        retry_policy: Option<RetryPolicy>,
    ) -> Self {
        Self {
            alias: alias.to_string(),
            litellm_model: litellm_model.to_string(),
            base_url: base_url.to_string(),
            keys,
            retry_policy,
            params: HashMap::new(),
            context_window: None,
            max_output_tokens: None,
            thinking_level_map: None,
            thinking_format: None,
            responses_base_url: None,
        }
    }

    /// 追加服务器侧参数（v2 路由展开时用于携带逻辑模型默认 + 物理模型覆写）。
    pub fn with_params(mut self, params: HashMap<String, serde_json::Value>) -> Self {
        self.params = params;
        self
    }

    pub fn with_thinking(
        mut self,
        level_map: Option<HashMap<String, Option<String>>>,
        format: Option<String>,
    ) -> Self {
        self.thinking_level_map = level_map;
        self.thinking_format = format;
        self
    }

    pub fn with_windows(
        mut self,
        context_window: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> Self {
        self.context_window = context_window;
        self.max_output_tokens = max_output_tokens;
        self
    }

    /// 标记上游原生支持 Responses API（透传模式）：设置其 Responses API 基础地址。
    pub fn with_responses_base_url(mut self, url: Option<String>) -> Self {
        self.responses_base_url = url;
        self
    }

    /// 是否原生支持 Responses API（配置了 responses_base_url 即支持，走透传）。
    pub fn supports_responses(&self) -> bool {
        self.responses_base_url.is_some()
    }

    pub fn upstream_model(&self) -> String {
        self.litellm_model
            .strip_prefix("openai/")
            .unwrap_or(&self.litellm_model)
            .to_string()
    }

    pub fn provider(&self) -> String {
        self.keys
            .first()
            .map(|key| key.provider.clone())
            .unwrap_or_else(|| self.alias.clone())
    }

    pub fn with_key_weights(&self, weights: &HashMap<String, i64>) -> Self {
        Self {
            keys: self
                .keys
                .iter()
                .map(|key| key.with_weight(*weights.get(&key.name).unwrap_or(&key.weight)))
                .collect(),
            ..self.clone()
        }
    }

    pub fn with_provider_base_urls(&self, base_urls: &HashMap<String, String>) -> Self {
        Self {
            base_url: base_urls
                .get(&self.provider())
                .cloned()
                .unwrap_or_else(|| self.base_url.clone()),
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub session_ttl_seconds: f64,
    pub monthly_quota_fallback_seconds: f64,
    pub five_hour_quota_fallback_seconds: f64,
    pub request_timeout_seconds: f64,
    pub local_bearer_token: Option<String>,
    pub usage_db_path: String,
    pub state_db_path: String,
    pub weight_config_path: String,
    pub provider_config_path: String,
    pub custom_key_config_path: String,
    pub api_keys_path: String,
    pub token_price_config_path: String,
    pub model_alias_config_path: String,
    pub search_providers_path: String,
    /// 运行期刷新 key 用的 env 文件（如 systemd environment.d 生成的 agent-env.conf）；
    /// 设置后 `/api/config/reload-env` 可重读该文件并把变量 set_var 进当前进程环境。
    pub env_file_path: Option<String>,
    /// 供应商模型列表持久化路径（设置界面“查看供应商详情”缓存）。
    pub provider_models_path: String,
    pub auth_invalid_freeze_seconds: f64,
    /// v2 分层配置开关（默认启用；设 0 回退旧硬编码 aliases 逻辑）。
    pub v2_config_enabled: bool,
    /// 诊断日志落盘目录（默认 ~/.local/state/llm-provider-router/logs，journal 不可信时的持久化证据）。
    pub diag_dir: String,
    /// 单个诊断文件最大体积（字节），超限轮转。
    pub diag_max_bytes: u64,
    /// 诊断文件最多保留个数，超限删除最旧。
    pub diag_max_files: usize,
    /// 诊断采样率：每 N 个请求记录 1 个请求级事件（1=全量，默认 1）。
    pub diag_sample_every: u64,
}

pub fn load_settings() -> anyhow::Result<Settings> {
    Ok(Settings {
        host: env_or("LLM_PROVIDER_ROUTER_HOST", "127.0.0.1"),
        port: env_or("LLM_PROVIDER_ROUTER_PORT", "8789")
            .parse()
            .context("LLM_PROVIDER_ROUTER_PORT must be a valid port")?,
        session_ttl_seconds: env_or("LLM_PROVIDER_ROUTER_SESSION_TTL_SECONDS", "3600").parse()?,
        monthly_quota_fallback_seconds: env_or(
            "LLM_PROVIDER_ROUTER_MONTHLY_QUOTA_FALLBACK_SECONDS",
            "86400",
        )
        .parse()?,
        five_hour_quota_fallback_seconds: env_or(
            "LLM_PROVIDER_ROUTER_5H_QUOTA_FALLBACK_SECONDS",
            "5400",
        )
        .parse()?,
        request_timeout_seconds: env_or("LLM_PROVIDER_ROUTER_REQUEST_TIMEOUT_SECONDS", "600")
            .parse()?,
        local_bearer_token: env::var("LLM_PROVIDER_ROUTER_BEARER_TOKEN")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                env::var("LLM_PROVIDER_ROUTER_API_KEY")
                    .ok()
                    .filter(|value| !value.is_empty())
            }),
        usage_db_path: env_or("LLM_PROVIDER_ROUTER_USAGE_DB_PATH", DEFAULT_USAGE_DB_PATH),
        state_db_path: env_or("LLM_PROVIDER_ROUTER_STATE_DB_PATH", DEFAULT_STATE_DB_PATH),
        weight_config_path: env_or(
            "LLM_PROVIDER_ROUTER_WEIGHT_CONFIG_PATH",
            DEFAULT_WEIGHT_CONFIG_PATH,
        ),
        provider_config_path: env_or(
            "LLM_PROVIDER_ROUTER_PROVIDER_CONFIG_PATH",
            DEFAULT_PROVIDER_CONFIG_PATH,
        ),
        custom_key_config_path: env_or(
            "LLM_PROVIDER_ROUTER_CUSTOM_KEY_CONFIG_PATH",
            DEFAULT_CUSTOM_KEY_CONFIG_PATH,
        ),
        api_keys_path: env_or("LLM_PROVIDER_ROUTER_API_KEYS_PATH", DEFAULT_API_KEYS_PATH),
        token_price_config_path: env_or(
            "LLM_PROVIDER_ROUTER_TOKEN_PRICE_CONFIG_PATH",
            DEFAULT_TOKEN_PRICE_CONFIG_PATH,
        ),
        model_alias_config_path: env_or(
            "LLM_PROVIDER_ROUTER_MODEL_ALIAS_CONFIG_PATH",
            DEFAULT_MODEL_ALIAS_CONFIG_PATH,
        ),
        search_providers_path: env_or(
            "LLM_PROVIDER_ROUTER_SEARCH_PROVIDERS_PATH",
            DEFAULT_SEARCH_PROVIDERS_PATH,
        ),
        env_file_path: env::var("LLM_PROVIDER_ROUTER_ENV_FILE")
            .ok()
            .filter(|value| !value.is_empty()),
        provider_models_path: env_or(
            "LLM_PROVIDER_ROUTER_PROVIDER_MODELS_PATH",
            "config/provider-models.json",
        ),
        auth_invalid_freeze_seconds: env_or(
            "LLM_PROVIDER_ROUTER_AUTH_INVALID_FREEZE_SECONDS",
            "86400",
        )
        .parse()?,
        v2_config_enabled: env_or("LLM_PROVIDER_ROUTER_V2", "1") != "0",
        diag_dir: env_or("LLM_PROVIDER_ROUTER_DIAG_DIR", DEFAULT_DIAG_DIR),
        diag_max_bytes: env_or("LLM_PROVIDER_ROUTER_DIAG_MAX_BYTES", DEFAULT_DIAG_MAX_BYTES)
            .parse()
            .unwrap_or(10 * 1024 * 1024),
        diag_max_files: env_or("LLM_PROVIDER_ROUTER_DIAG_MAX_FILES", DEFAULT_DIAG_MAX_FILES)
            .parse()
            .unwrap_or(50),
        diag_sample_every: env_or(
            "LLM_PROVIDER_ROUTER_DIAG_SAMPLE_EVERY",
            DEFAULT_DIAG_SAMPLE_EVERY,
        )
        .parse()
        .unwrap_or(1),
    })
}

pub fn aliases() -> HashMap<String, ModelAlias> {
    let ark_keys = vec![
        KeyRef::new("garvin", "AGENT_AI_ARK_GARVIN_API_KEY", 6),
        KeyRef::new("wilford", "AGENT_AI_ARK_WILFORD_API_KEY", 3),
        KeyRef::new("hevin", "AGENT_AI_ARK_HEVIN_API_KEY", 5),
        KeyRef::new("khaine", "AGENT_AI_ARK_KHAINE_API_KEY", 6),
        KeyRef::new("cyril", "AGENT_AI_ARK_CYRIL_API_KEY", 4),
        KeyRef::new("moss", "AGENT_AI_ARK_MOSS_API_KEY", 4),
        KeyRef::new("ronnie", "AGENT_AI_ARK_RONNIE_API_KEY", 4),
    ];
    let oai_hevin_keys = vec![KeyRef::with_provider(
        "oai-hevin",
        "AGENT_AI_OPENAI_HEVIN_API_KEY",
        1,
        "openai-relay",
        "subscription",
    )];
    let deepseek_keys = vec![KeyRef::env_only(
        "deepseek-official",
        "AGENT_AI_DEEPSEEK_API_KEY",
        1,
        "deepseek-official",
        "payg",
    )];
    let ark_retry = RetryPolicy::new(300, 5.0, &[401, 402, 429, 500, 502, 503, 504]);
    let oai_retry = RetryPolicy::new(1800, 15.0, &[429, 500, 502, 503, 504]);

    let mut map = HashMap::new();
    for (name, model) in [
        ("low-model-auto", "openai/deepseek-v4-flash"),
        ("medium-model-auto", "openai/glm-5.2"),
        ("picture-model-auto", "openai/minimax-m3"),
        ("glm-latest-auto", "openai/glm-5.2"),
        ("deepseek-v4-pro-auto", "openai/deepseek-v4-pro"),
        ("deepseek-v4-flash-auto", "openai/deepseek-v4-flash-260801"),
        (
            "deepseek-v4-flash-260801",
            "openai/deepseek-v4-flash-260801",
        ),
        ("minimax-latest-auto", "openai/minimax-m3"),
        ("ark-code-latest-auto", "openai/ark-code-latest"),
    ] {
        map.insert(
            name.to_string(),
            ModelAlias::new(
                name,
                model,
                DEFAULT_ARK_BASE_URL,
                ark_keys.clone(),
                Some(ark_retry.clone()),
            ),
        );
    }
    map.insert(
        "high-model-auto".to_string(),
        ModelAlias::new(
            "high-model-auto",
            "openai/gpt-5.5",
            "https://api.aixhan.com/v1",
            oai_hevin_keys.clone(),
            Some(oai_retry.clone()),
        ),
    );
    map.insert(
        "openai-gpt-5.5-hevin".to_string(),
        ModelAlias::new(
            "openai-gpt-5.5-hevin",
            "openai/gpt-5.5",
            "https://api.aixhan.com/v1",
            oai_hevin_keys.clone(),
            Some(oai_retry.clone()),
        ),
    );
    map.insert(
        "openai-gpt-5.6-sol-hevin".to_string(),
        ModelAlias::new(
            "openai-gpt-5.6-sol-hevin",
            "openai/gpt-5.6-sol",
            "https://api.aixhan.com/v1",
            oai_hevin_keys,
            Some(oai_retry),
        ),
    );
    map.insert(
        "deepseek-v4-flash-official".to_string(),
        ModelAlias::new(
            "deepseek-v4-flash-official",
            "openai/deepseek-v4-flash",
            "https://api.deepseek.com",
            deepseek_keys.clone(),
            None,
        ),
    );
    map.insert(
        "deepseek-v4-pro-official".to_string(),
        ModelAlias::new(
            "deepseek-v4-pro-official",
            "openai/deepseek-v4-pro",
            "https://api.deepseek.com",
            deepseek_keys,
            None,
        ),
    );
    map
}

pub fn default_key_weights() -> HashMap<String, i64> {
    let mut weights = HashMap::new();
    for alias in aliases().values() {
        for key in &alias.keys {
            weights.insert(key.name.clone(), key.weight);
        }
    }
    weights
}

pub fn default_provider_base_urls() -> HashMap<String, String> {
    let mut base_urls = HashMap::new();
    for alias in aliases().values() {
        base_urls
            .entry(alias.provider())
            .or_insert(alias.base_url.clone());
    }
    base_urls
}

pub fn expand_path(value: &str) -> PathBuf {
    let expanded = if let Some(rest) = value.strip_prefix("~/") {
        env::var("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|_| PathBuf::from(value))
    } else {
        PathBuf::from(value)
    };
    if expanded.is_absolute() || value == ":memory:" {
        expanded
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    }
}

pub fn env_or(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub mod v2;
