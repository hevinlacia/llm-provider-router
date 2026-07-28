use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_ARK_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/coding/v3";
pub const DEFAULT_WEIGHT_CONFIG_PATH: &str = "config/key-weights.json";
pub const DEFAULT_PROVIDER_CONFIG_PATH: &str = "config/providers.json";
pub const DEFAULT_CUSTOM_KEY_CONFIG_PATH: &str = "config/custom-keys.json";
pub const DEFAULT_MODEL_ROUTE_CONFIG_PATH: &str = "config/model-routes.json";
pub const DEFAULT_ROUTER_AUTH_CONFIG_PATH: &str = "config/router-auth.json";
pub const DEFAULT_USAGE_DB_PATH: &str = "~/.local/state/llm-provider-router/usage.sqlite3";
pub const DEFAULT_STATE_DB_PATH: &str = "~/.local/state/llm-provider-router/state.sqlite3";

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct KeyRef {
    pub name: String,
    pub env_var: String,
    pub weight: i64,
    pub provider: String,
    pub billing_type: String,
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
    pub max_retry_seconds: u64,
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
        }
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRoute {
    pub target: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
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
    pub model_route_config_path: String,
    pub auth_invalid_freeze_seconds: f64,
}

pub fn load_settings() -> anyhow::Result<Settings> {
    let router_auth_config_path = env_or(
        "LLM_PROVIDER_ROUTER_AUTH_CONFIG_PATH",
        DEFAULT_ROUTER_AUTH_CONFIG_PATH,
    );
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
            })
            .or_else(|| load_router_bearer_token(&router_auth_config_path)),
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
        model_route_config_path: env_or(
            "LLM_PROVIDER_ROUTER_MODEL_ROUTE_CONFIG_PATH",
            DEFAULT_MODEL_ROUTE_CONFIG_PATH,
        ),
        auth_invalid_freeze_seconds: env_or(
            "LLM_PROVIDER_ROUTER_AUTH_INVALID_FREEZE_SECONDS",
            "86400",
        )
        .parse()?,
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
    let deepseek_keys = vec![KeyRef::with_provider(
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
        ("deepseek-v4-flash-auto", "openai/deepseek-v4-flash"),
        ("minimax-latest-auto", "openai/minimax-m3"),
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

pub fn default_model_routes() -> HashMap<String, ModelRoute> {
    HashMap::from([
        (
            "high-model-auto".to_string(),
            ModelRoute {
                target: "openai-gpt-5.5-hevin".to_string(),
                fallbacks: vec!["glm-latest-auto".to_string()],
            },
        ),
        (
            "medium-model-auto".to_string(),
            ModelRoute {
                target: "glm-latest-auto".to_string(),
                fallbacks: vec!["deepseek-v4-pro-auto".to_string()],
            },
        ),
        (
            "low-model-auto".to_string(),
            ModelRoute {
                target: "deepseek-v4-flash-auto".to_string(),
                fallbacks: vec!["glm-latest-auto".to_string()],
            },
        ),
    ])
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

fn load_router_bearer_token(config_path: &str) -> Option<String> {
    let path = expand_path(config_path);
    if !Path::new(&path).is_file() {
        return None;
    }
    let data: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    data.get("bearer_token")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
