use crate::config::{
    aliases, default_key_weights, default_provider_base_urls, KeyRef, ModelAlias, Settings,
    DEFAULT_ARK_BASE_URL,
};
use crate::config_v2::{self, TargetCandidate, V2Strategy};
use crate::json_config::{
    ApiKeysStore, CustomKeyEntry, CustomKeyPoolConfig, KeyWeightConfig, KeyWeightsConfigData,
    ModelAliasConfig, ProviderConfig, TokenPrice, TokenPriceConfig,
};
use crate::state_store::{now_seconds, StateStore};
use crate::usage_store::UsageStore;
use anyhow::Context;
use http::HeaderMap;
use rand::Rng;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;

#[derive(Clone, Debug)]
pub struct FrozenKey {
    pub until: f64,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct SessionBinding {
    pub key_name: String,
    pub expires_at: f64,
}

#[derive(Debug)]
pub struct NoAvailableKeyError {
    pub retry_after: u64,
}

impl std::fmt::Display for NoAvailableKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "no available upstream key; retry after {}s",
            self.retry_after
        )
    }
}

impl std::error::Error for NoAvailableKeyError {}

pub struct RouterState {
    settings: Settings,
    state_store: StateStore,
    frozen: HashMap<String, FrozenKey>,
    bindings: HashMap<(String, String), SessionBinding>,
    usage_store: UsageStore,
    weight_config: KeyWeightConfig,
    provider_config: ProviderConfig,
    custom_key_config: CustomKeyPoolConfig,
    token_price_config: TokenPriceConfig,
    model_alias_config: ModelAliasConfig,
    api_keys_store: ApiKeysStore,
    /// v2 分层配置（加载失败为 None，回退旧逻辑）。
    v2: Option<config_v2::V2Config>,
}

impl RouterState {
    pub fn new(settings: Settings) -> anyhow::Result<Self> {
        let state_store = StateStore::new(&settings.state_db_path)?;
        let frozen = state_store
            .load_frozen()?
            .into_iter()
            .map(|(name, (until, reason))| (name, FrozenKey { until, reason }))
            .collect();
        let bindings = state_store
            .load_bindings()?
            .into_iter()
            .map(|(key, (key_name, expires_at))| {
                (
                    key,
                    SessionBinding {
                        key_name,
                        expires_at,
                    },
                )
            })
            .collect();
        let usage_store = UsageStore::new(&settings.usage_db_path)?;
        let weight_config =
            KeyWeightConfig::new(&settings.weight_config_path, default_key_weights());
        let provider_config =
            ProviderConfig::new(&settings.provider_config_path, default_provider_base_urls());
        let mut custom_key_config = CustomKeyPoolConfig::new(&settings.custom_key_config_path);
        let model_alias_config = ModelAliasConfig::new(&settings.model_alias_config_path);
        let token_price_config = TokenPriceConfig::new(
            &settings.token_price_config_path,
            default_token_prices(),
        );
        let api_keys_store = ApiKeysStore::new(&settings.api_keys_path);
        // First run (file missing): seed from environment so existing keys are
        // captured into the sole source of truth. Otherwise: apply stored key
        // values to the process environment without overriding existing vars.
        if !api_keys_store.exists() {
            let mut seed: HashMap<String, String> = HashMap::new();
            for alias in aliases().values() {
                for key in &alias.keys {
                    // Env-only keys (e.g. deepseek-official) are never
                    // persisted to api-keys.json; they come from the
                    // environment only.
                    if !key.persist {
                        continue;
                    }
                    if let Ok(value) = env::var(&key.env_var) {
                        if !value.is_empty() {
                            seed.insert(key.env_var.clone(), value);
                        }
                    }
                }
            }
            for (name, item) in custom_key_config.get().keys {
                let env_var = if item.env_var.is_empty() {
                    format!("AGENT_AI_ARK_{}_API_KEY", name.to_uppercase())
                } else {
                    item.env_var
                };
                if let Ok(value) = env::var(&env_var) {
                    if !value.is_empty() {
                        seed.insert(env_var, value);
                    }
                }
            }
            if !seed.is_empty() {
                let _ = api_keys_store.write(&seed);
            }
        } else {
            let env_only_vars: HashSet<String> = aliases()
                .values()
                .flat_map(|alias| alias.keys.iter())
                .filter(|key| !key.persist)
                .map(|key| key.env_var.clone())
                .collect();
            let stored = api_keys_store.load();
            let mut prune: Vec<String> = Vec::new();
            for (env_var, value) in &stored {
                // One-time cleanup: env-only keys must not linger in the
                // plaintext store; the environment is their only source.
                if env_only_vars.contains(env_var) {
                    prune.push(env_var.clone());
                    continue;
                }
                if env::var(env_var).ok().filter(|v| !v.is_empty()).is_none() {
                    env::set_var(env_var, value);
                }
            }
            if !prune.is_empty() {
                let mut remaining = stored;
                for var in &prune {
                    remaining.remove(var);
                }
                let _ = api_keys_store.write(&remaining);
            }
        }
        // v2 分层配置：默认启用（环境变量 LLM_PROVIDER_ROUTER_V2=0 可回退旧逻辑）。
        // 加载失败（文件缺失/解析错误）时静默回退，不阻塞启动。
        let v2 = if settings.v2_config_enabled {
            config_v2::load_v2_config().ok()
        } else {
            None
        };
        let state = Self {
            settings,
            state_store,
            frozen,
            bindings,
            usage_store,
            weight_config,
            provider_config,
            custom_key_config,
            token_price_config,
            model_alias_config,
            api_keys_store,
            v2,
        };
        Ok(state)
    }

    pub fn cleanup(&mut self) -> anyhow::Result<()> {
        let now = now_seconds();
        let expired_frozen = self
            .frozen
            .iter()
            .filter(|(_, item)| item.until <= now)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let expired_bindings = self
            .bindings
            .iter()
            .filter(|(_, item)| item.expires_at <= now)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for name in &expired_frozen {
            self.frozen.remove(name);
        }
        for key in &expired_bindings {
            self.bindings.remove(key);
        }
        self.state_store.delete_frozen(&expired_frozen)?;
        self.state_store.delete_bindings(&expired_bindings)?;
        Ok(())
    }

    pub fn is_frozen(&mut self, key_name: &str) -> anyhow::Result<bool> {
        let Some(item) = self.frozen.get(key_name) else {
            return Ok(false);
        };
        if item.until <= now_seconds() {
            self.frozen.remove(key_name);
            self.state_store.delete_frozen(&[key_name.to_string()])?;
            return Ok(false);
        }
        Ok(true)
    }

    pub fn freeze(&mut self, key_name: &str, until: f64, reason: &str) -> anyhow::Result<()> {
        let should_update = self
            .frozen
            .get(key_name)
            .map(|item| until > item.until)
            .unwrap_or(true);
        if should_update {
            self.frozen.insert(
                key_name.to_string(),
                FrozenKey {
                    until,
                    reason: reason.to_string(),
                },
            );
            self.state_store.upsert_frozen(key_name, until, reason)?;
        }
        Ok(())
    }

    pub fn clear_frozen(&mut self) -> anyhow::Result<()> {
        self.frozen.clear();
        self.state_store.clear_frozen()
    }

    pub fn bind(&mut self, alias: &str, session_id: &str, key_name: &str) -> anyhow::Result<()> {
        let expires_at = now_seconds() + self.settings.session_ttl_seconds;
        self.bindings.insert(
            (alias.to_string(), session_id.to_string()),
            SessionBinding {
                key_name: key_name.to_string(),
                expires_at,
            },
        );
        self.state_store
            .upsert_binding(alias, session_id, key_name, expires_at)
    }

    pub fn select_key_excluding(
        &mut self,
        alias: &ModelAlias,
        session_id: Option<&str>,
        excluded: &HashSet<String>,
    ) -> Result<KeyRef, NoAvailableKeyError> {
        self.cleanup()
            .map_err(|_| NoAvailableKeyError { retry_after: 60 })?;
        if let Some(session_id) = session_id {
            let binding = self
                .bindings
                .get(&(alias.alias.clone(), session_id.to_string()))
                .cloned();
            if let Some(binding) = binding {
                if !excluded.contains(&binding.key_name)
                    && !self.is_frozen(&binding.key_name).unwrap_or(true)
                {
                    if let Some(key) = alias
                        .keys
                        .iter()
                        .find(|key| key.name == binding.key_name && key.weight > 0)
                    {
                        let key = key.clone();
                        let _ = self.bind(&alias.alias, session_id, &key.name);
                        return Ok(key);
                    }
                }
            }
        }
        let mut candidates = Vec::new();
        for key in &alias.keys {
            if key.weight > 0
                && !excluded.contains(&key.name)
                && !self.is_frozen(&key.name).unwrap_or(true)
            {
                candidates.push(key.clone());
            }
        }
        if candidates.is_empty() {
            let retry_after = self
                .frozen
                .values()
                .map(|item| (item.until - now_seconds()).max(1.0) as u64)
                .min()
                .unwrap_or(60);
            return Err(NoAvailableKeyError { retry_after });
        }
        let key = self
            .usage_adjusted_pick(alias, &candidates, session_id)
            .unwrap_or_else(|_| {
                weighted_pick(&candidates, session_id, &alias.alias)
                    .unwrap_or_else(|| candidates[0].clone())
            });
        if let Some(session_id) = session_id {
            let _ = self.bind(&alias.alias, session_id, &key.name);
        }
        Ok(key)
    }

    fn usage_adjusted_pick(
        &mut self,
        alias: &ModelAlias,
        candidates: &[KeyRef],
        session_id: Option<&str>,
    ) -> anyhow::Result<KeyRef> {
        let names = candidates
            .iter()
            .map(|key| key.name.clone())
            .collect::<Vec<_>>();
        let totals = self
            .usage_store
            .key_token_totals_for_model(&alias.alias, &names)?;
        let positive = candidates
            .iter()
            .filter(|key| key.weight > 0)
            .cloned()
            .collect::<Vec<_>>();
        if positive.is_empty() {
            return weighted_pick(candidates, session_id, &alias.alias)
                .context("no key candidates");
        }
        let min_ratio = positive
            .iter()
            .map(|key| *totals.get(&key.name).unwrap_or(&0) as f64 / key.weight as f64)
            .fold(f64::INFINITY, f64::min);
        let lowest = positive
            .into_iter()
            .filter(|key| {
                let ratio = *totals.get(&key.name).unwrap_or(&0) as f64 / key.weight as f64;
                (ratio - min_ratio).abs() < f64::EPSILON
            })
            .collect::<Vec<_>>();
        weighted_pick(&lowest, session_id, &alias.alias).context("no key candidates")
    }

    pub fn snapshot(&mut self) -> anyhow::Result<Value> {
        self.cleanup()?;
        let now = now_seconds();
        let frozen = self
            .frozen
            .iter()
            .map(|(name, item)| {
                (
                    name.clone(),
                    json!({
                        "seconds_remaining": (item.until - now).max(0.0) as i64,
                        "reason": item.reason,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        Ok(json!({ "frozen": frozen, "bindings": self.bindings.len() }))
    }

    pub fn record_usage(
        &mut self,
        model: &str,
        key_name: &str,
        status_code: u16,
        usage: Option<&Value>,
    ) -> anyhow::Result<()> {
        self.usage_store.record(model, key_name, status_code, usage)
    }

    pub fn reset_usage(&mut self) -> anyhow::Result<()> {
        self.usage_store.reset()
    }

    pub fn usage_snapshot(
        &mut self,
        period: &str,
        start: Option<&str>,
        end: Option<&str>,
    ) -> anyhow::Result<Value> {
        let mut snapshot = self.usage_store.snapshot(period, start, end)?;
        let prices = self.token_prices();
        apply_costs(&mut snapshot, &prices);
        Ok(snapshot)
    }

    pub fn effective_key_weights(&mut self, pool: &str) -> HashMap<String, i64> {
        self.sync_custom_key_weight_defaults();
        self.weight_config.effective_for_pool(pool)
    }

    pub fn provider_base_urls(&mut self) -> HashMap<String, String> {
        self.provider_config.get()
    }

    pub fn key_config_snapshot(&mut self) -> anyhow::Result<Value> {
        self.sync_custom_key_weight_defaults();
        let weights_config = self.weight_config.get_config();
        let provider_urls = self.provider_base_urls();
        let aliases = self.settings_aliases();
        let mut aliases_payload = serde_json::Map::new();
        let mut pool_names = Vec::new();
        for (alias_name, alias) in aliases {
            // Only virtual (name contains "auto") pools are weight-configurable:
            // the weight UI distributes traffic across keys *within a pool*.
            // Real upstream models (single provider/key, e.g.
            // openai-gpt-5.5-hevin, deepseek-v4-flash-official) must not appear
            // in the weight page, since a single key has no meaningful weight.
            if !alias_name.contains("auto") {
                continue;
            }
            let effective_weights = weights_config.effective_for_pool(&alias_name);
            let effective_alias = alias
                .with_provider_base_urls(&provider_urls)
                .with_key_weights(&effective_weights);
            let total_weight: i64 = effective_alias
                .keys
                .iter()
                .map(|key| key.weight.max(0))
                .sum();
            let explicit_pool_weights = weights_config.pools.get(&alias_name);
            let keys = effective_alias
                .keys
                .iter()
                .zip(alias.keys.iter())
                .map(|(key, default_key)| {
                    let global_weight = weights_config
                        .global
                        .get(&key.name)
                        .copied()
                        .unwrap_or(default_key.weight);
                    let pool_weight = explicit_pool_weights
                        .and_then(|weights| weights.get(&key.name))
                        .copied();
                    json!({
                        "name": key.name,
                        "provider": key.provider,
                        "billing_type": key.billing_type,
                        "default_weight": default_key.weight,
                        "global_weight": global_weight,
                        "pool_weight": pool_weight,
                        "weight": key.weight,
                        "enabled": key.weight > 0,
                        "probability": if total_weight > 0 && key.weight > 0 {
                            ((key.weight as f64 / total_weight as f64) * 10_000.0).round() / 10_000.0
                        } else { 0.0 },
                    })
                })
                .collect::<Vec<_>>();
            if !keys.is_empty() {
                pool_names.push(alias_name.clone());
            }
            aliases_payload.insert(
                alias_name,
                json!({
                    "model": alias.litellm_model,
                    "base_url": alias.base_url,
                    "effective_base_url": effective_alias.base_url,
                    "provider": alias.provider(),
                    "keys": keys,
                }),
            );
        }
        pool_names.sort();
        Ok(json!({
            "aliases": aliases_payload,
            "weights": weights_config.global.clone(),
            "global_weights": weights_config.global,
            "pool_weights": weights_config.pools,
            "pools": pool_names,
            "config_path": self.weight_config.path.to_string_lossy(),
        }))
    }

    pub fn token_prices(&mut self) -> HashMap<String, TokenPrice> {
        self.sync_token_price_defaults();
        self.token_price_config.get()
    }

    pub fn token_price_snapshot(&mut self) -> Value {
        let prices = self.token_prices();
        let mut models = prices
            .iter()
            .map(|(model, price)| {
                json!({
                    "model": model,
                    "input_uncached_per_million": price.input_uncached_per_million,
                    "input_cached_per_million": price.input_cached_per_million,
                    "output_per_million": price.output_per_million,
                })
            })
            .collect::<Vec<_>>();
        models.sort_by_key(|item| {
            item.get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        });
        json!({ "models": models, "config_path": self.token_price_config.path.to_string_lossy() })
    }

    pub fn set_token_prices(
        &mut self,
        prices: HashMap<String, TokenPrice>,
    ) -> anyhow::Result<Value> {
        self.sync_token_price_defaults();
        let known = self.known_model_names();
        let unknown = prices
            .keys()
            .filter(|model| !known.contains(*model))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            anyhow::bail!("unknown model(s): {}", sorted_join(unknown));
        }
        let invalid = prices
            .iter()
            .filter(|(_, price)| !price.is_valid())
            .map(|(model, _)| model.clone())
            .collect::<Vec<_>>();
        if !invalid.is_empty() {
            anyhow::bail!("invalid token price(s): {}", sorted_join(invalid));
        }
        self.token_price_config.set(prices, &known)?;
        Ok(self.token_price_snapshot())
    }

    pub fn provider_config_snapshot(&mut self) -> Value {
        let configured = self.provider_base_urls();
        let defaults = default_provider_base_urls();
        let mut providers = defaults
            .keys()
            .map(|name| {
                json!({
                    "name": name,
                    "base_url": configured.get(name).cloned().unwrap_or_default(),
                    "default_base_url": defaults.get(name).cloned().unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        providers.sort_by_key(|item| {
            item.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        });
        json!({ "providers": providers, "config_path": self.provider_config.path.to_string_lossy() })
    }

    pub fn set_provider_base_urls(
        &mut self,
        base_urls: HashMap<String, String>,
    ) -> anyhow::Result<Value> {
        let known = default_provider_base_urls()
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let unknown = base_urls
            .keys()
            .filter(|name| !known.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            anyhow::bail!("unknown provider(s): {}", sorted_join(unknown));
        }
        let invalid = base_urls
            .iter()
            .filter(|(_, url)| url.is_empty())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if !invalid.is_empty() {
            anyhow::bail!("empty base URL for provider(s): {}", sorted_join(invalid));
        }
        self.provider_config.set(base_urls)?;
        Ok(self.provider_config_snapshot())
    }

    pub fn set_key_weights(&mut self, weights: HashMap<String, i64>) -> anyhow::Result<()> {
        self.sync_custom_key_weight_defaults();
        let known = self.known_key_names();
        validate_weight_names(&weights, &known)?;
        let effective = self.weight_config.set_global(weights)?;
        self.rebind_disabled_sessions(&effective)?;
        Ok(())
    }

    pub fn set_pool_key_weights(
        &mut self,
        pool: &str,
        weights: HashMap<String, i64>,
    ) -> anyhow::Result<()> {
        self.sync_custom_key_weight_defaults();
        let Some(known) = self.pool_key_names(pool) else {
            anyhow::bail!("unknown key pool: {pool}");
        };
        validate_weight_names(&weights, &known)?;
        let effective = self.weight_config.set_pool(pool, weights)?;
        self.rebind_disabled_sessions(&effective)?;
        Ok(())
    }

    pub fn key_secret_snapshot(&mut self) -> anyhow::Result<Value> {
        let mut keys = Vec::new();
        for key in self.all_key_refs() {
            let env_configured = env::var(&key.env_var)
                .ok()
                .filter(|value| !value.is_empty())
                .is_some();
            let source = if env_configured {
                "environment"
            } else {
                "missing"
            };
            keys.push(json!({
                "name": key.name,
                "provider": key.provider,
                "billing_type": key.billing_type,
                "env_var": key.env_var,
                "configured": env_configured,
                "env_configured": env_configured,
                "source": source,
                "persist": key.persist,
            }));
        }
        Ok(json!({
            "keys": keys,
            "auto_aliases": self.auto_alias_names(),
            "v2_enabled": self.v2.is_some(),
            "note": "key values persist in config/api-keys.json (encrypted backup via ~/Developer/vault); deepseek-official keys are env-only (AGENT_AI_DEEPSEEK_API_KEY) and never stored in api-keys.json; env vars are applied on startup and shared via agent-env.conf",
            "custom_key_config_path": self.custom_key_config.path.to_string_lossy(),
        }))
    }

    pub fn add_key_to_pools(
        &mut self,
        name: &str,
        value: &str,
        aliases: Vec<String>,
        weight: i64,
    ) -> anyhow::Result<Value> {
        let name = normalize_custom_key_name(name);
        let key_name_pattern = Regex::new(r"^[a-z][a-z0-9-]*$").unwrap();
        if !key_name_pattern.is_match(&name) {
            anyhow::bail!("key name must use lowercase letters, numbers, and hyphens");
        }
        if self.known_key_names().contains(&name) {
            anyhow::bail!("key already exists: {name}");
        }
        if value.is_empty() {
            anyhow::bail!("key value is required");
        }
        let auto_aliases = self.auto_alias_names().into_iter().collect::<HashSet<_>>();
        let mut alias_names = aliases.into_iter().collect::<Vec<_>>();
        alias_names.sort();
        alias_names.dedup();
        let unknown = alias_names
            .iter()
            .filter(|alias| !auto_aliases.contains(*alias))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            anyhow::bail!("unknown auto alias(es): {}", sorted_join(unknown));
        }
        if alias_names.is_empty() {
            anyhow::bail!("select at least one auto key pool");
        }
        if weight < 0 {
            anyhow::bail!("weight must be zero or greater");
        }
        let env_var = format!(
            "AGENT_AI_ARK_{}_API_KEY",
            name.replace('-', "_").to_uppercase()
        );
        env::set_var(&env_var, value);
        self.api_keys_store.upsert(&env_var, value)?;
        self.custom_key_config.add_key(
            name.clone(),
            CustomKeyEntry {
                env_var,
                provider: "ark".to_string(),
                billing_type: "subscription".to_string(),
                weight,
                aliases: alias_names,
            },
        )?;
        self.key_secret_snapshot()
    }

    pub fn set_key_values(
        &mut self,
        values: HashMap<String, String>,
        delete_names: HashSet<String>,
    ) -> anyhow::Result<Value> {
        let known = self.known_key_names();
        let env_vars: HashMap<String, String> = self
            .all_key_refs()
            .into_iter()
            .map(|k| (k.name, k.env_var))
            .collect();
        let unknown: Vec<String> = values
            .keys()
            .chain(delete_names.iter())
            .filter(|name| !known.contains(*name))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            anyhow::bail!("unknown key name(s): {}", sorted_join(unknown));
        }
        let mut stored = self.api_keys_store.load();
        let persist_env_vars: HashSet<String> = self
            .all_key_refs()
            .into_iter()
            .filter(|key| key.persist)
            .map(|key| key.env_var)
            .collect();
        for (name, value) in &values {
            if let Some(env_var) = env_vars.get(name) {
                if value.is_empty() {
                    env::remove_var(env_var);
                    stored.remove(env_var.as_str());
                } else {
                    env::set_var(env_var, value);
                    // Env-only keys (persist=false) stay out of the store;
                    // persistent changes go through the env file / vault.
                    if persist_env_vars.contains(env_var) {
                        stored.insert(env_var.clone(), value.clone());
                    }
                }
            }
        }
        for name in &delete_names {
            if let Some(env_var) = env_vars.get(name) {
                env::remove_var(env_var);
                stored.remove(env_var.as_str());
            }
        }
        self.api_keys_store.write(&stored)?;
        self.key_secret_snapshot()
    }

    pub fn upstream_key_value(&mut self, key: &KeyRef) -> anyhow::Result<Option<String>> {
        Ok(env::var(&key.env_var)
            .ok()
            .filter(|value| !value.is_empty()))
    }

    pub fn alias_with_runtime_weights(&mut self, alias: &ModelAlias) -> ModelAlias {
        if self.v2.is_some() {
            // v2 模式：base_url / weight 以 v2 配置为权威，不套旧 providers.json / key-weights.json 覆盖层
            return alias.clone();
        }
        let provider_urls = self.provider_base_urls();
        let weights = self.effective_key_weights(&alias.alias);
        alias
            .with_provider_base_urls(&provider_urls)
            .with_key_weights(&weights)
    }

    /// v2 折叠视图：每个逻辑模型取主目标（route.targets[0]）折叠为 ModelAlias，
    /// 再接入 custom model aliases（运行时 API 手动新增的扁平逻辑模型）。
    fn v2_aliases(&mut self) -> HashMap<String, ModelAlias> {
        let mut aliases = self
            .v2
            .as_ref()
            .and_then(|cfg| config_v2::fold_to_aliases(cfg).ok())
            .unwrap_or_default();
        aliases.extend(self.custom_alias_models());
        aliases
    }

    /// v2 模式下 custom model aliases 接入：base_url / keys 取自其声明 provider（v2 供应商），
    /// retry 用 custom 自身配置，折叠为单物理模型 ModelAlias。
    fn custom_alias_models(&mut self) -> HashMap<String, ModelAlias> {
        let provider_urls = self.provider_base_urls();
        let mut out = HashMap::new();
        for custom in self.model_alias_config.get() {
            let base_url = provider_urls
                .get(&custom.provider)
                .cloned()
                .or_else(|| {
                    self.v2.as_ref().and_then(|cfg| {
                        cfg.providers.get(&custom.provider).map(|p| p.base_url.clone())
                    })
                })
                .unwrap_or_else(|| DEFAULT_ARK_BASE_URL.to_string());
            let keys = self
                .v2
                .as_ref()
                .and_then(|cfg| cfg.providers.get(&custom.provider))
                .map(|prov| {
                    prov.keys
                        .iter()
                        .filter(|(_, k)| k.enabled)
                        .map(|(name, key)| KeyRef {
                            name: name.clone(),
                            env_var: key.env_var.clone(),
                            weight: key.weight,
                            provider: custom.provider.clone(),
                            billing_type: key.billing_type.clone(),
                            persist: key.persist,
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.insert(
                custom.alias.clone(),
                ModelAlias::new(
                    &custom.alias,
                    &custom.upstream_model,
                    &base_url,
                    keys,
                    Some(crate::config::RetryPolicy::new(
                        custom.max_retry_seconds,
                        custom.retry_delay_seconds,
                        &[401, 402, 429, 500, 502, 503, 504],
                    )),
                ),
            );
        }
        out
    }

    pub fn base_aliases(&mut self) -> HashMap<String, ModelAlias> {
        if self.v2.is_some() {
            return self.v2_aliases();
        }
        let mut aliases = aliases();
        // Add custom model aliases
        let provider_urls = self.provider_base_urls();
        for custom_alias in self.model_alias_config.get() {
            // Use provider's base URL, falling back to Ark's default
            let base_url = provider_urls
                .get(&custom_alias.provider)
                .cloned()
                .unwrap_or_else(|| DEFAULT_ARK_BASE_URL.to_string());
            // Copy keys from an existing alias with the same provider (or use default keys)
            let keys = aliases
                .values()
                .find(|alias| alias.provider() == custom_alias.provider)
                .map(|alias| alias.keys.clone())
                .unwrap_or_default();
            aliases.insert(
                custom_alias.alias.clone(),
                ModelAlias::new(
                    &custom_alias.alias,
                    &custom_alias.upstream_model,
                    &base_url,
                    keys,
                    Some(crate::config::RetryPolicy::new(
                        custom_alias.max_retry_seconds,
                        custom_alias.retry_delay_seconds,
                        &[401, 402, 429, 500, 502, 503, 504],
                    )),
                ),
            );
        }
        // Merge custom keys into all aliases
        for key in self.custom_key_refs() {
            for alias_name in self.custom_key_aliases(&key.name) {
                if let Some(alias) = aliases.get_mut(&alias_name) {
                    alias.keys.push(key.clone());
                }
            }
        }
        aliases
    }

    pub fn settings_aliases(&mut self) -> HashMap<String, ModelAlias> {
        self.base_aliases()
    }

    /// v2 架构完整状态视图：供应商（含 key enabled/frozen/可用性聚合）、
    /// 物理模型、逻辑模型（策略 + 路由目标）。v2 未启用时返回最小对象。
    pub fn v2_status(&mut self) -> Value {        let Some(cfg) = self.v2.as_ref() else {
            return json!({ "v2_enabled": false });
        };
        let mut providers = serde_json::Map::new();
        for (name, prov) in &cfg.providers {
            let enabled: Vec<_> = prov.keys.iter().filter(|(_, k)| k.enabled).collect();
            let frozen_count = enabled
                .iter()
                .filter(|(kname, _)| self.frozen.contains_key(*kname))
                .count();
            let mut keys = serde_json::Map::new();
            for (kname, key) in &prov.keys {
                keys.insert(
                    kname.clone(),
                    json!({
                        "env_var": key.env_var,
                        "weight": key.weight,
                        "billing_type": key.billing_type,
                        "enabled": key.enabled,
                        "frozen": self.frozen.contains_key(kname),
                        "frozen_reason": self.frozen.get(kname).map(|f| f.reason.clone()),
                    }),
                );
            }
            providers.insert(
                name.clone(),
                json!({
                    "base_url": prov.base_url,
                    "key_total": prov.keys.len(),
                    "key_enabled": enabled.len(),
                    "key_frozen": frozen_count,
                    "available": enabled.len() - frozen_count > 0,
                    "keys": keys,
                }),
            );
        }
        let mut models = Vec::new();
        for (mid, pm) in &cfg.models {
            models.push(json!({
                "id": mid,
                "provider": pm.provider,
                "upstream_model": pm.upstream_model,
                "family": pm.family,
                "params": pm.params,
            }));
        }
        models.sort_by(|a, b| {
            a["id"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["id"].as_str().unwrap_or_default())
        });
        let mut logical = serde_json::Map::new();
        for (alias, lm) in &cfg.logical_models {
            let targets: Vec<Value> = lm
                .route
                .targets
                .iter()
                .map(|t| json!({ "model": t.model, "weight": t.weight }))
                .collect();
            let strategy = match lm.route.strategy {
                V2Strategy::Priority => "priority",
                V2Strategy::Weighted => "weighted",
                V2Strategy::UsageAware => "usage-aware",
            };
            logical.insert(
                alias.clone(),
                json!({
                    "params": lm.params,
                    "strategy": strategy,
                    "targets": targets,
                }),
            );
        }
        json!({
            "v2_enabled": true,
            "providers": providers,
            "models": models,
            "logical_models": logical,
        })
    }

    /// 编辑 v2 供应商：改名 / base_url / keys（新增、删除、启用停用）。
    /// 改名时同步 `models.json` 与 `logical-models.json` 的引用，完成后热加载并返回最新视图。
    pub fn update_v2_provider(
        &mut self,
        old_name: &str,
        new_name: &str,
        base_url: &str,
        keys: HashMap<String, config_v2::V2Key>,
    ) -> anyhow::Result<Value> {
        if new_name.trim().is_empty() {
            anyhow::bail!("provider name must not be empty");
        }
        if base_url.trim().is_empty() {
            anyhow::bail!("base_url must not be empty");
        }
        let mut providers = config_v2::load_providers_file(config_v2::V2_PROVIDERS_PATH)?;
        let renamed = old_name != new_name;
        if renamed && providers.providers.contains_key(new_name) {
            anyhow::bail!("provider {new_name} already exists");
        }
        let mut provider = providers
            .providers
            .remove(old_name)
            .ok_or_else(|| anyhow::anyhow!("provider {old_name} not found"))?;
        provider.base_url = base_url.trim().to_string();
        provider.keys = keys;
        providers.providers.insert(new_name.to_string(), provider);
        config_v2::write_providers_file(config_v2::V2_PROVIDERS_PATH, &providers)?;

        if renamed {
            config_v2::rename_provider_in_models(
                new_name,
                old_name,
                config_v2::V2_MODELS_PATH,
            )?;
            config_v2::rename_provider_in_logical(
                new_name,
                old_name,
                config_v2::V2_LOGICAL_MODELS_PATH,
            )?;
        }
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 编辑 v2 逻辑模型：路由策略 + 目标（物理模型或嵌套逻辑模型）。
    /// 写回 `logical-models.json` 后热加载并返回最新视图。
    pub fn update_v2_logical_model(
        &mut self,
        name: &str,
        strategy: config_v2::V2Strategy,
        params: HashMap<String, serde_json::Value>,
        targets: Vec<config_v2::V2Target>,
    ) -> anyhow::Result<Value> {
        if name.trim().is_empty() {
            anyhow::bail!("logical model name must not be empty");
        }
        if targets.is_empty() {
            anyhow::bail!("route must have at least one target");
        }
        // 用磁盘当前配置做引用校验（reload 前）
        let cfg = config_v2::load_v2_config()?;
        if !cfg.logical_models.contains_key(name) {
            anyhow::bail!("logical model {name} not found");
        }
        for target in &targets {
            let ok = cfg.models.contains_key(&target.model)
                || (cfg.logical_models.contains_key(&target.model) && target.model != name);
            if !ok {
                anyhow::bail!(
                    "target {}: unknown physical model or logical model (or self-reference)",
                    target.model
                );
            }
        }
        let mut logical =
            config_v2::load_logical_models_file(config_v2::V2_LOGICAL_MODELS_PATH)?;
        let lm = logical
            .logical_models
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("logical model {name} not found"))?;
        lm.route.strategy = strategy;
        lm.route.targets = targets;
        lm.params = params;
        config_v2::write_logical_models_file(
            config_v2::V2_LOGICAL_MODELS_PATH,
            &logical,
        )?;
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 重新加载 v2 配置（供应商编辑写回后热生效）。
    fn reload_v2(&mut self) {
        if self.settings.v2_config_enabled {
            self.v2 = config_v2::load_v2_config().ok();
        }
    }

    pub fn model_alias_config_snapshot(&mut self) -> Value {
        let custom_aliases = self.model_alias_config.get();
        let mut aliases = custom_aliases
            .into_iter()
            .map(|item| {
                json!({
                    "alias": item.alias,
                    "upstream_model": item.upstream_model,
                    "provider": item.provider,
                    "max_retry_seconds": item.max_retry_seconds,
                    "retry_delay_seconds": item.retry_delay_seconds,
                })
            })
            .collect::<Vec<_>>();
        aliases.sort_by(|a, b| {
            a.get("alias")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .cmp(b.get("alias").and_then(Value::as_str).unwrap_or_default())
        });
        json!({ "custom_aliases": aliases, "config_path": self.model_alias_config.path.to_string_lossy() })
    }

    pub fn set_model_aliases(
        &mut self,
        custom_aliases: Vec<crate::json_config::CustomModelAlias>,
    ) -> anyhow::Result<Value> {
        let known_providers: HashSet<String> = default_provider_base_urls().keys().cloned().collect();
        for alias in &custom_aliases {
            if !known_providers.contains(&alias.provider) {
                anyhow::bail!("unknown provider: {}", alias.provider);
            }
            if alias.alias.is_empty() {
                anyhow::bail!("alias name cannot be empty");
            }
            if alias.upstream_model.is_empty() {
                anyhow::bail!("upstream_model cannot be empty for alias: {}", alias.alias);
            }
        }
        let aliases: HashSet<String> = custom_aliases.iter().map(|a| a.alias.clone()).collect();
        if aliases.len() != custom_aliases.len() {
            anyhow::bail!("duplicate alias names found");
        }
        self.model_alias_config.set(custom_aliases)?;
        Ok(self.model_alias_config_snapshot())
    }

    pub fn route_aliases(&mut self, model_name: &str, session_id: Option<&str>) -> Vec<ModelAlias> {
        if self.v2.is_some() {
            // 请求名即逻辑模型名（或 custom alias），resolve_targets 会嵌套展开到物理候选。
            let expanded: Vec<(String, Vec<TargetCandidate>)> = {
                let cfg = self.v2.as_ref().expect("v2 enabled checked");
                config_v2::resolve_targets(cfg, model_name)
                    .map(|c| vec![(model_name.to_string(), c)])
                    .unwrap_or_default()
            };
            // custom model aliases（运行时 API 手动新增的扁平逻辑模型）
            let customs = self.custom_alias_models();
            let mut out = Vec::new();
            for (name, candidates) in expanded {
                let preferred = if matches!(
                    candidates.first().map(|c| &c.strategy),
                    Some(&V2Strategy::UsageAware)
                ) {
                    usage_preferred_index(&self.usage_store, &name, &candidates)
                } else {
                    None
                };
                out.extend(order_targets(candidates, session_id, preferred));
            }
            if let Some(model) = customs.get(model_name) {
                out.push(model.clone());
            }
            return out;
        }
        let aliases = self.base_aliases();
        aliases.get(model_name).cloned().into_iter().collect()
    }

    pub fn auto_alias_names(&mut self) -> Vec<String> {
        let mut names = self
            .settings_aliases()
            .keys()
            .filter(|name| name.contains("auto"))
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    pub fn custom_key_refs(&mut self) -> Vec<KeyRef> {
        let mut refs = Vec::new();
        for (name, item) in self.custom_key_config.get().keys {
            refs.push(KeyRef {
                env_var: if item.env_var.is_empty() {
                    format!("AGENT_AI_ARK_{}_API_KEY", name.to_uppercase())
                } else {
                    item.env_var
                },
                name,
                weight: item.weight,
                provider: item.provider,
                billing_type: item.billing_type,
                persist: true,
            });
        }
        refs.sort_by_key(|key| key.name.clone());
        refs
    }

    pub fn custom_key_aliases(&mut self, key_name: &str) -> Vec<String> {
        self.custom_key_config
            .get()
            .keys
            .get(key_name)
            .map(|item| item.aliases.clone())
            .unwrap_or_default()
    }

    pub fn all_key_refs(&mut self) -> Vec<KeyRef> {
        let mut refs = HashMap::new();
        for alias in self.settings_aliases().values() {
            for key in &alias.keys {
                refs.insert(key.name.clone(), key.clone());
            }
        }
        let mut values = refs.into_values().collect::<Vec<_>>();
        values.sort_by_key(|key| key.name.clone());
        values
    }

    pub fn known_key_names(&mut self) -> HashSet<String> {
        self.all_key_refs()
            .into_iter()
            .map(|key| key.name)
            .collect()
    }

    pub fn pool_key_names(&mut self, pool: &str) -> Option<HashSet<String>> {
        self.settings_aliases().get(pool).map(|alias| {
            alias
                .keys
                .iter()
                .map(|key| key.name.clone())
                .collect::<HashSet<_>>()
        })
    }

    fn sync_custom_key_weight_defaults(&mut self) {
        let defaults = self
            .all_key_refs()
            .into_iter()
            .map(|key| (key.name, key.weight))
            .collect();
        self.weight_config.add_defaults(defaults);
    }

    fn known_model_names(&mut self) -> HashSet<String> {
        self.settings_aliases().keys().cloned().collect()
    }

    fn sync_token_price_defaults(&mut self) {
        let defaults = default_token_prices();
        self.token_price_config.add_defaults(defaults);
    }

    fn rebind_disabled_sessions(&mut self, weights: &KeyWeightsConfigData) -> anyhow::Result<()> {
        let aliases = self.settings_aliases();
        let disabled_bindings = self
            .bindings
            .iter()
            .filter_map(|((alias_name, session_id), binding)| {
                let should_delete = aliases
                    .get(alias_name)
                    .map(|alias| {
                        let assigned = alias.keys.iter().any(|key| key.name == binding.key_name);
                        let effective_weight = weights
                            .effective_for_pool(alias_name)
                            .get(&binding.key_name)
                            .copied()
                            .unwrap_or(0);
                        !assigned || effective_weight <= 0
                    })
                    .unwrap_or(true);
                if should_delete {
                    Some((alias_name.clone(), session_id.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if disabled_bindings.is_empty() {
            return Ok(());
        }
        for key in &disabled_bindings {
            self.bindings.remove(key);
        }
        self.state_store.delete_bindings(&disabled_bindings)
    }
}

fn default_token_prices() -> HashMap<String, TokenPrice> {
    let mut prices = HashMap::new();
    for model in aliases().keys() {
        prices.insert(model.clone(), TokenPrice::default());
    }
    prices
}

fn apply_costs(snapshot: &mut Value, prices: &HashMap<String, TokenPrice>) {
    let by_model_costs = snapshot
        .get("by_model")
        .and_then(Value::as_object)
        .map(|models| {
            models
                .iter()
                .map(|(model, bucket)| (model.clone(), cost_for_bucket(bucket, prices.get(model))))
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    let total = sum_costs(by_model_costs.values());
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("by_model_cost".to_string(), Value::Object(by_model_costs));
        object.insert("total_cost".to_string(), total);
    }
}

fn cost_for_bucket(bucket: &Value, price: Option<&TokenPrice>) -> Value {
    let prompt = bucket
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cached = bucket
        .get("cached_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let uncached = bucket
        .get("prompt_uncached_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| (prompt - cached).max(0));
    let output = bucket
        .get("completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let price = price.cloned().unwrap_or_default();
    let (input_uncached_cost, input_cached_cost, output_cost) =
        price.cost_parts(uncached, cached, output);
    let total_cost = round_money(input_uncached_cost + input_cached_cost + output_cost);
    json!({
        "input_uncached_cost": round_money(input_uncached_cost),
        "input_cached_cost": round_money(input_cached_cost),
        "output_cost": round_money(output_cost),
        "total_cost": total_cost,
    })
}

fn sum_costs<'a>(items: impl Iterator<Item = &'a Value>) -> Value {
    let mut input_uncached_cost = 0.0;
    let mut input_cached_cost = 0.0;
    let mut output_cost = 0.0;
    let mut total_cost = 0.0;
    for item in items {
        input_uncached_cost += item
            .get("input_uncached_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        input_cached_cost += item
            .get("input_cached_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        output_cost += item
            .get("output_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        total_cost += item
            .get("total_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
    }
    json!({
        "input_uncached_cost": round_money(input_uncached_cost),
        "input_cached_cost": round_money(input_cached_cost),
        "output_cost": round_money(output_cost),
        "total_cost": round_money(total_cost),
    })
}

fn round_money(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}


/// 第 1 层路由排序：把物理模型候选排成"首选在前，其余作为回退"。
/// - priority：按 route.targets 原序；
/// - weighted / usage-aware：`preferred` 有值则用其作为首选（usage-aware 由调用方按用量算），
///   否则加权采样首选（session 粘性）；其余按 weight 降序作为回退。
pub fn order_targets(
    candidates: Vec<TargetCandidate>,
    session_id: Option<&str>,
    preferred: Option<usize>,
) -> Vec<ModelAlias> {
    let Some(first) = candidates.first() else {
        return Vec::new();
    };
    let strategy = first.strategy.clone();
    let alias = first.model.alias.clone();
    let mut items: Vec<(i64, ModelAlias)> = candidates
        .into_iter()
        .map(|c| (c.weight.unwrap_or(1).max(0), c.model))
        .collect();
    match strategy {
        V2Strategy::Priority => items.into_iter().map(|(_, m)| m).collect(),
        V2Strategy::Weighted | V2Strategy::UsageAware => {
            let total: i64 = items.iter().map(|(w, _)| w).sum();
            if total <= 0 {
                return items.into_iter().map(|(_, m)| m).collect();
            }
            let picked = match preferred {
                Some(idx) if idx < items.len() => idx,
                _ => weighted_first_index(&items, session_id, &alias),
            };
            let (_, first_model) = items.remove(picked);
            items.sort_by(|a, b| b.0.cmp(&a.0));
            let mut out = Vec::with_capacity(items.len() + 1);
            out.push(first_model);
            out.extend(items.into_iter().map(|(_, m)| m));
            out
        }
    }
}

/// 按 weight 加权采样首选的下标（session 粘性：同 session 结果稳定）。
fn weighted_first_index(
    items: &[(i64, ModelAlias)],
    session_id: Option<&str>,
    alias: &str,
) -> usize {
    let total: i64 = items.iter().map(|(w, _)| w).sum();
    if total <= 0 {
        return 0;
    }
    let mut target = if let Some(session) = session_id {
        let mut hasher = Sha256::new();
        hasher.update(format!("{alias}:{session}").as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) % total as u64) as i64
    } else {
        rand::thread_rng().gen_range(0..total)
    };
    for (i, (w, _)) in items.iter().enumerate() {
        target -= w;
        if target < 0 {
            return i;
        }
    }
    items.len() - 1
}

/// usage-aware 第 1 层：跨供应商按用量选首选。
/// 每个物理模型取其 key 池内最低 tokens/weight 比（池内最紧张的 key），
/// 选该比值最低的候选作为首选（即当前用量相对最宽裕的供应商）。
fn usage_preferred_index(
    usage: &UsageStore,
    alias: &str,
    candidates: &[TargetCandidate],
) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, candidate) in candidates.iter().enumerate() {
        let names: Vec<String> = candidate.model.keys.iter().map(|k| k.name.clone()).collect();
        let Ok(totals) = usage.key_token_totals_for_model(alias, &names) else {
            continue;
        };
        let mut min_ratio = f64::INFINITY;
        let mut any = false;
        for key in &candidate.model.keys {
            let weight = key.weight.max(0) as f64;
            if weight <= 0.0 {
                continue;
            }
            any = true;
            let tokens = totals.get(&key.name).copied().unwrap_or(0) as f64;
            min_ratio = min_ratio.min(tokens / weight);
        }
        if any && min_ratio < best.as_ref().map(|(_, r)| *r).unwrap_or(f64::INFINITY) {
            best = Some((i, min_ratio));
        }
    }
    best.map(|(i, _)| i)
}

pub fn weighted_pick(keys: &[KeyRef], session_id: Option<&str>, alias: &str) -> Option<KeyRef> {
    let total: i64 = keys.iter().map(|key| key.weight.max(0)).sum();
    if total <= 0 {
        return None;
    }
    let mut target = if let Some(session_id) = session_id {
        let mut hasher = Sha256::new();
        hasher.update(format!("{alias}:{session_id}").as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) % total as u64) as i64
    } else {
        rand::thread_rng().gen_range(0..total)
    };
    for key in keys {
        target -= key.weight.max(0);
        if target < 0 {
            return Some(key.clone());
        }
    }
    keys.last().cloned()
}

pub fn normalize_custom_key_name(value: &str) -> String {
    let mut name = value.trim().to_string();
    let upper = name.to_uppercase();
    for prefix in ["AGENT_AI_ARK_", "AI_ARK_"] {
        if upper.starts_with(prefix) {
            name = name[prefix.len()..].to_string();
            break;
        }
    }
    if name.to_uppercase().ends_with("_API_KEY") {
        let len = name.len() - "_API_KEY".len();
        name.truncate(len);
    }
    name.trim_matches(&['_', '-'][..])
        .to_lowercase()
        .replace('_', "-")
}

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
        state.freeze(&key.name, until, reason)?;
        return Ok(());
    }
    if matches!(status_code, 401 | 403) {
        if let Some((until, reason)) = parse_auth_invalid(body_text, settings) {
            state.freeze(&key.name, until, reason)?;
            return Ok(());
        }
    }
    if status_code == 429 {
        if let Some(until) = parse_retry_after(
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
        ) {
            state.freeze(&key.name, until, "retry_after")?;
        }
    }
    Ok(())
}

fn validate_weight_names(
    weights: &HashMap<String, i64>,
    known: &HashSet<String>,
) -> anyhow::Result<()> {
    let unknown = weights
        .keys()
        .filter(|name| !known.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        anyhow::bail!("unknown key name(s): {}", sorted_join(unknown));
    }
    let invalid = weights
        .iter()
        .filter(|(_, weight)| **weight < 0)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        anyhow::bail!("negative weight for key(s): {}", sorted_join(invalid));
    }
    Ok(())
}

fn sorted_join(mut values: Vec<String>) -> String {
    values.sort();
    values.dedup();
    values.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use std::fs;

    fn test_settings() -> Settings {
        Settings {
            host: "127.0.0.1".to_string(),
            port: 8789,
            session_ttl_seconds: 3600.0,
            monthly_quota_fallback_seconds: 86400.0,
            five_hour_quota_fallback_seconds: 5400.0,
            request_timeout_seconds: 30.0,
            local_bearer_token: None,
            usage_db_path: ":memory:".to_string(),
            state_db_path: ":memory:".to_string(),
            weight_config_path: ":memory:".to_string(),
            provider_config_path: ":memory:".to_string(),
            custom_key_config_path: ":memory:".to_string(),
            api_keys_path: ":memory:".to_string(),
            token_price_config_path: ":memory:".to_string(),
            model_alias_config_path: ":memory:".to_string(),
            auth_invalid_freeze_seconds: 86400.0,
            // router_state 测试覆盖旧逻辑；v2 行为由 config_v2 模块测试覆盖。
            v2_config_enabled: false,
        }
    }

    #[test]
    fn normalizes_custom_key_names() {
        assert_eq!(
            normalize_custom_key_name("AGENT_AI_ARK_SHELL_API_KEY"),
            "shell"
        );
        assert_eq!(
            normalize_custom_key_name("AI_ARK_FOO_BAR_API_KEY"),
            "foo-bar"
        );
    }

    #[test]
    fn weighted_pick_is_sticky_for_session() {
        let keys = vec![
            KeyRef::new("a", "A", 1),
            KeyRef::new("b", "B", 3),
            KeyRef::new("c", "C", 5),
        ];
        let first = weighted_pick(&keys, Some("session-1"), "alias").unwrap();
        let second = weighted_pick(&keys, Some("session-1"), "alias").unwrap();
        assert_eq!(first.name, second.name);
    }

    #[test]
    fn parses_quota_reset_fallback() {
        let settings = test_settings();
        let (until, reason) =
            parse_quota_reset("You have exceeded the monthly usage quota", &settings).unwrap();
        assert_eq!(reason, "monthly_quota");
        assert!(until > now_seconds() + 86000.0);
    }

    #[test]
    fn parses_auth_invalid_error() {
        let settings = test_settings();
        let (until, reason) =
            parse_auth_invalid("authentication_error: api key invalid", &settings).unwrap();
        assert_eq!(reason, "auth_invalid");
        assert!(until > now_seconds() + 86000.0);
    }

    #[test]
    fn env_only_keys_are_pruned_from_store_and_read_from_env() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("api-keys.json");
        fs::write(
            &store_path,
            json!({
                "AGENT_AI_ARK_TEST_PERSIST_API_KEY": "persist-value",
                "AGENT_AI_DEEPSEEK_API_KEY": "env-only-value",
            })
            .to_string(),
        )
        .unwrap();
        env::set_var("AGENT_AI_ARK_TEST_PERSIST_API_KEY", "persist-value");
        env::set_var("AGENT_AI_DEEPSEEK_API_KEY", "env-only-value");

        let settings = Settings {
            api_keys_path: store_path.to_str().unwrap().to_string(),
            ..test_settings()
        };
        let mut state = RouterState::new(settings).unwrap();

        // Env-only key must have been pruned from the plaintext store on startup.
        let stored: HashMap<String, String> =
            serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
        assert!(!stored.contains_key("AGENT_AI_DEEPSEEK_API_KEY"));
        assert!(stored.contains_key("AGENT_AI_ARK_TEST_PERSIST_API_KEY"));

        // Env-only key still resolves from the environment.
        let deepseek = state
            .all_key_refs()
            .into_iter()
            .find(|key| key.env_var == "AGENT_AI_DEEPSEEK_API_KEY")
            .unwrap();
        assert!(!deepseek.persist);
        assert_eq!(
            state.upstream_key_value(&deepseek).unwrap().as_deref(),
            Some("env-only-value")
        );
    }

    #[test]
    fn zero_weight_key_is_not_selected_or_reused_from_binding() {
        let settings = test_settings();
        let mut state = RouterState::new(settings).unwrap();
        let alias = ModelAlias::new(
            "test-pool",
            "openai/test",
            "https://example.test",
            vec![KeyRef::new("off", "OFF", 0), KeyRef::new("on", "ON", 1)],
            None,
        );
        state.bind("test-pool", "session-1", "off").unwrap();
        let selected = state
            .select_key_excluding(&alias, Some("session-1"), &HashSet::new())
            .unwrap();
        assert_eq!(selected.name, "on");
    }

    #[test]
    fn pool_specific_weight_overrides_global_weight() {
        let settings = test_settings();
        let mut state = RouterState::new(settings).unwrap();
        state
            .set_key_weights(HashMap::from([("hevin".to_string(), 0)]))
            .unwrap();
        state
            .set_pool_key_weights("glm-latest-auto", HashMap::from([("hevin".to_string(), 7)]))
            .unwrap();
        let weights = state.effective_key_weights("glm-latest-auto");
        assert_eq!(weights.get("hevin"), Some(&7));
        let global_weights = state.effective_key_weights("deepseek-v4-pro-auto");
        assert_eq!(global_weights.get("hevin"), Some(&0));
    }

    #[test]
    fn usage_snapshot_includes_cost_by_model() {
        let settings = test_settings();
        let mut state = RouterState::new(settings).unwrap();
        state
            .set_token_prices(HashMap::from([(
                "glm-latest-auto".to_string(),
                TokenPrice {
                    input_uncached_per_million: 10.0,
                    input_cached_per_million: 1.0,
                    output_per_million: 20.0,
                },
            )]))
            .unwrap();
        let usage = json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": { "cached_tokens": 40 },
            "completion_tokens": 25,
            "total_tokens": 125
        });
        state
            .record_usage("glm-latest-auto", "hevin", 200, Some(&usage))
            .unwrap();

        let snapshot = state.usage_snapshot("all", None, None).unwrap();

        assert_eq!(
            snapshot["by_model"]["glm-latest-auto"]["prompt_uncached_tokens"],
            60
        );
        assert_eq!(
            snapshot["by_model_cost"]["glm-latest-auto"]["total_cost"],
            0.00114
        );
        assert_eq!(snapshot["total_cost"]["total_cost"], 0.00114);
    }

    fn test_alias(name: &str, base_url: &str) -> ModelAlias {
        ModelAlias::new(name, &format!("openai/{name}"), base_url, vec![], None)
    }

    #[test]
    fn order_targets_priority_keeps_target_order() {
        let cands = vec![
            TargetCandidate {
                model: test_alias("m", "u-1"),
                weight: None,
                strategy: V2Strategy::Priority,
            },
            TargetCandidate {
                model: test_alias("m", "u-2"),
                weight: None,
                strategy: V2Strategy::Priority,
            },
        ];
        let ordered = order_targets(cands, None, None);
        let urls: Vec<&str> = ordered.iter().map(|a| a.base_url.as_str()).collect();
        assert_eq!(urls, vec!["u-1", "u-2"], "priority 应按 targets 原序");
    }

    #[test]
    fn order_targets_preferred_overrides_weighted_sampling() {
        let cands = vec![
            TargetCandidate {
                model: test_alias("m", "u-a"),
                weight: Some(1),
                strategy: V2Strategy::Weighted,
            },
            TargetCandidate {
                model: test_alias("m", "u-b"),
                weight: Some(9),
                strategy: V2Strategy::Weighted,
            },
            TargetCandidate {
                model: test_alias("m", "u-c"),
                weight: Some(5),
                strategy: V2Strategy::Weighted,
            },
        ];
        // preferred=2 强制首选 u-c，其余按 weight 降序
        let ordered = order_targets(cands, Some("sess"), Some(2));
        let urls: Vec<&str> = ordered.iter().map(|a| a.base_url.as_str()).collect();
        assert_eq!(urls[0], "u-c", "preferred 应作为首选");
        assert_eq!(urls[1], "u-b");
        assert_eq!(urls[2], "u-a");
    }

    #[test]
    fn order_targets_usage_aware_strategy_behaves_like_weighted_without_preferred() {
        let cands = vec![
            TargetCandidate {
                model: test_alias("m", "u-a"),
                weight: Some(3),
                strategy: V2Strategy::UsageAware,
            },
            TargetCandidate {
                model: test_alias("m", "u-b"),
                weight: Some(3),
                strategy: V2Strategy::UsageAware,
            },
        ];
        let o1 = order_targets(cands.clone(), Some("sess"), None);
        let o2 = order_targets(cands.clone(), Some("sess"), None);
        let u1: Vec<&str> = o1.iter().map(|a| a.base_url.as_str()).collect();
        let u2: Vec<&str> = o2.iter().map(|a| a.base_url.as_str()).collect();
        assert_eq!(u1, u2, "无 preferred 时按 session 粘性加权");
        assert_eq!(u1.len(), 2);
    }

    #[test]
    fn order_targets_weighted_session_sticky_and_fallback_sorted() {
        let cands = vec![
            TargetCandidate {
                model: test_alias("m", "u-a"),
                weight: Some(1),
                strategy: V2Strategy::Weighted,
            },
            TargetCandidate {
                model: test_alias("m", "u-b"),
                weight: Some(9),
                strategy: V2Strategy::Weighted,
            },
            TargetCandidate {
                model: test_alias("m", "u-c"),
                weight: Some(5),
                strategy: V2Strategy::Weighted,
            },
        ];
        let o1 = order_targets(cands.clone(), Some("sess"), None);
        let o2 = order_targets(cands.clone(), Some("sess"), None);
        assert_eq!(o1.len(), 3);
        // session 粘性：同 session 两次结果一致
        let u1: Vec<&str> = o1.iter().map(|a| a.base_url.as_str()).collect();
        let u2: Vec<&str> = o2.iter().map(|a| a.base_url.as_str()).collect();
        assert_eq!(u1, u2, "同一 session 首选应稳定");
        // 集合不变
        let mut all: Vec<&str> = u1.clone();
        all.sort();
        assert_eq!(all, vec!["u-a", "u-b", "u-c"]);
        // 首选之后的回退按 weight 降序
        let weight_of = |u: &str| match u {
            "u-a" => 1,
            "u-b" => 9,
            "u-c" => 5,
            _ => 0,
        };
        let rest: Vec<i64> = u1[1..].iter().map(|u| weight_of(u)).collect();
        let mut sorted = rest.clone();
        sorted.sort_by(|x, y| y.cmp(x));
        assert_eq!(rest, sorted, "回退应按 weight 降序");
    }
}
