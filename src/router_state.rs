use crate::config::{
    aliases, default_key_weights, default_model_routes, default_provider_base_urls, KeyRef,
    ModelAlias, ModelRoute, Settings,
};
use crate::json_config::{
    CustomKeyEntry, CustomKeyPoolConfig, KeyWeightConfig, ModelRouteConfig, ProviderConfig,
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
    model_route_config: ModelRouteConfig,
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
        let custom_key_config = CustomKeyPoolConfig::new(&settings.custom_key_config_path);
        let model_route_config =
            ModelRouteConfig::new(&settings.model_route_config_path, default_model_routes());
        let state = Self {
            settings,
            state_store,
            frozen,
            bindings,
            usage_store,
            weight_config,
            provider_config,
            custom_key_config,
            model_route_config,
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
                    if let Some(key) = alias.keys.iter().find(|key| key.name == binding.key_name) {
                        let key = key.clone();
                        let _ = self.bind(&alias.alias, session_id, &key.name);
                        return Ok(key);
                    }
                }
            }
        }
        let mut candidates = Vec::new();
        for key in &alias.keys {
            if !excluded.contains(&key.name) && !self.is_frozen(&key.name).unwrap_or(true) {
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
        &self,
        period: &str,
        start: Option<&str>,
        end: Option<&str>,
    ) -> anyhow::Result<Value> {
        self.usage_store.snapshot(period, start, end)
    }

    pub fn key_weight_overrides(&mut self) -> HashMap<String, i64> {
        self.sync_custom_key_weight_defaults();
        self.weight_config.get()
    }

    pub fn provider_base_urls(&mut self) -> HashMap<String, String> {
        self.provider_config.get()
    }

    pub fn key_config_snapshot(&mut self) -> anyhow::Result<Value> {
        let weights = self.key_weight_overrides();
        let provider_urls = self.provider_base_urls();
        let mut aliases_payload = serde_json::Map::new();
        for (alias_name, alias) in self.settings_aliases() {
            let effective_alias = alias
                .with_provider_base_urls(&provider_urls)
                .with_key_weights(&weights);
            let total_weight: i64 = effective_alias
                .keys
                .iter()
                .map(|key| key.weight.max(0))
                .sum();
            let keys = effective_alias
                .keys
                .iter()
                .zip(alias.keys.iter())
                .map(|(key, default_key)| {
                    json!({
                        "name": key.name,
                        "provider": key.provider,
                        "billing_type": key.billing_type,
                        "default_weight": default_key.weight,
                        "weight": key.weight,
                        "probability": if total_weight > 0 && key.weight > 0 {
                            ((key.weight as f64 / total_weight as f64) * 10_000.0).round() / 10_000.0
                        } else { 0.0 },
                    })
                })
                .collect::<Vec<_>>();
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
        Ok(json!({
            "aliases": aliases_payload,
            "model_routes": self.model_routes(),
            "weights": weights,
            "config_path": self.weight_config.path.to_string_lossy(),
            "model_route_config_path": self.model_route_config.path.to_string_lossy(),
        }))
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
        let effective = self.weight_config.set(weights)?;
        self.rebind_zero_weight_sessions(&effective)?;
        Ok(())
    }

    pub fn key_secret_snapshot(&mut self) -> anyhow::Result<Value> {
        let mut keys = Vec::new();
        for key in self.all_key_refs() {
            let env_configured = env::var(&key.env_var)
                .ok()
                .filter(|value| !value.is_empty())
                .is_some();
            let source = if env_configured { "environment" } else { "missing" };
            keys.push(json!({
                "name": key.name,
                "provider": key.provider,
                "billing_type": key.billing_type,
                "env_var": key.env_var,
                "configured": env_configured,
                "env_configured": env_configured,
                "source": source,
            }));
        }
        Ok(json!({
            "keys": keys,
            "auto_aliases": self.auto_alias_names(),
            "note": "key values are read from environment variables only; persist via vault (~/.config/opencode/agent-secrets.env) and restart",
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
        for (name, value) in &values {
            if let Some(env_var) = env_vars.get(name) {
                if value.is_empty() {
                    env::remove_var(env_var);
                } else {
                    env::set_var(env_var, value);
                }
            }
        }
        for name in &delete_names {
            if let Some(env_var) = env_vars.get(name) {
                env::remove_var(env_var);
            }
        }
        self.key_secret_snapshot()
    }

    pub fn upstream_key_value(&mut self, key: &KeyRef) -> anyhow::Result<Option<String>> {
        Ok(env::var(&key.env_var)
            .ok()
            .filter(|value| !value.is_empty()))
    }

    pub fn alias_with_runtime_weights(&mut self, alias: &ModelAlias) -> ModelAlias {
        alias
            .with_provider_base_urls(&self.provider_base_urls())
            .with_key_weights(&self.key_weight_overrides())
    }

    pub fn base_aliases(&mut self) -> HashMap<String, ModelAlias> {
        let mut aliases = aliases();
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
        let mut aliases = self.base_aliases();
        for (route_name, route) in self.model_routes() {
            if let Some(target) = aliases.get(&route.target).cloned() {
                aliases.insert(route_name.clone(), request_alias(&route_name, &target));
            }
        }
        aliases
    }

    pub fn model_routes(&mut self) -> HashMap<String, ModelRoute> {
        let known = self.base_aliases().keys().cloned().collect::<HashSet<_>>();
        self.model_route_config.get(&known)
    }

    pub fn set_model_routes(
        &mut self,
        routes: HashMap<String, ModelRoute>,
    ) -> anyhow::Result<HashMap<String, ModelRoute>> {
        let known = self.base_aliases().keys().cloned().collect::<HashSet<_>>();
        self.model_route_config.set(routes, &known)
    }

    pub fn route_config_snapshot(&mut self) -> Value {
        let aliases = self.base_aliases();
        let routes = self.model_routes();
        let mut base_aliases = aliases
            .iter()
            .map(|(name, alias)| {
                json!({
                    "name": name,
                    "upstream_model": alias.upstream_model(),
                    "provider": alias.provider(),
                    "base_url": alias.base_url,
                })
            })
            .collect::<Vec<_>>();
        base_aliases.sort_by_key(|item| {
            item.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        });
        json!({
            "routes": routes,
            "base_aliases": base_aliases,
            "config_path": self.model_route_config.path.to_string_lossy(),
        })
    }

    pub fn route_aliases(&mut self, model_name: &str) -> Vec<ModelAlias> {
        let aliases = self.base_aliases();
        if let Some(route) = self.model_routes().get(model_name).cloned() {
            let mut names = vec![route.target];
            names.extend(route.fallbacks);
            return names
                .into_iter()
                .filter_map(|name| aliases.get(&name).cloned())
                .map(|alias| request_alias(model_name, &alias))
                .collect();
        }
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

    fn sync_custom_key_weight_defaults(&mut self) {
        let defaults = self
            .custom_key_refs()
            .into_iter()
            .map(|key| (key.name, key.weight))
            .collect();
        self.weight_config.add_defaults(defaults);
    }

    fn rebind_zero_weight_sessions(
        &mut self,
        weights: &HashMap<String, i64>,
    ) -> anyhow::Result<()> {
        let zero = weights
            .iter()
            .filter(|(_, weight)| **weight <= 0)
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        if zero.is_empty() {
            return Ok(());
        }
        self.bindings
            .retain(|_, binding| !zero.contains(&binding.key_name));
        self.state_store.delete_bindings_for_keys(&zero)
    }
}

pub fn request_alias(alias_name: &str, target: &ModelAlias) -> ModelAlias {
    ModelAlias {
        alias: alias_name.to_string(),
        litellm_model: target.litellm_model.clone(),
        base_url: target.base_url.clone(),
        keys: target.keys.clone(),
        retry_policy: target.retry_policy.clone(),
    }
}

pub fn weighted_pick(keys: &[KeyRef], session_id: Option<&str>, alias: &str) -> Option<KeyRef> {
    let total: i64 = keys.iter().map(|key| key.weight.max(0)).sum();
    if total <= 0 {
        return keys.first().cloned();
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

fn sorted_join(mut values: Vec<String>) -> String {
    values.sort();
    values.dedup();
    values.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

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
            model_route_config_path: ":memory:".to_string(),
            auth_invalid_freeze_seconds: 86400.0,
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
}
