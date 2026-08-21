//! RouterState 配置读写：key 权重、供应商 base_url、token 价格、模型等价组、自定义别名。

use super::RouterState;
use crate::config::default_provider_base_urls;
use crate::config::KeyRef;
use crate::config::ModelAlias;
use crate::features::router::selection::normalize_custom_key_name;
use crate::features::router::util::{sorted_join, validate_weight_names};
use crate::json_config::{CustomKeyEntry, TokenPrice};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;

impl RouterState {
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

    pub fn equivalences_snapshot(&mut self) -> Value {
        let file = self.model_equivalences.get();
        json!({ "groups": file.groups, "config_path": self.model_equivalences.path.to_string_lossy() })
    }

    pub fn set_equivalences(
        &mut self,
        groups: Vec<crate::json_config::EquivalenceGroup>,
    ) -> anyhow::Result<Value> {
        // 校验：id 唯一、非空；models 形如 provider/model
        let mut seen = HashSet::new();
        for g in &groups {
            if g.id.trim().is_empty() {
                anyhow::bail!("equivalence group id must not be empty");
            }
            if !seen.insert(g.id.clone()) {
                anyhow::bail!("duplicate equivalence group id: {}", g.id);
            }
            for m in &g.models {
                if !m.contains('/') {
                    anyhow::bail!("model must be provider/model: {}", m);
                }
            }
        }
        self.model_equivalences
            .set(crate::json_config::ModelEquivalencesFile { groups })?;
        Ok(self.equivalences_snapshot())
    }

    pub fn apply_price_to_equivalents(
        &mut self,
        model: &str,
        only_missing: bool,
    ) -> anyhow::Result<Value> {
        let prices = self.token_price_config.get();
        let source_price = prices
            .get(model)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown model: {}", model))?;
        let group_models: Vec<String> = {
            let file = self.model_equivalences.get();
            file.groups
                .into_iter()
                .find(|g| g.models.iter().any(|m| m == model))
                .map(|g| g.models)
                .unwrap_or_default()
        };
        if group_models.is_empty() {
            anyhow::bail!("model is not in any equivalence group: {}", model);
        }
        let mut next = prices.clone();
        let mut applied = Vec::new();
        for target in group_models {
            if target == model {
                continue;
            }
            if only_missing {
                let existing = next.get(&target);
                let is_zero = existing
                    .map(|p| {
                        p.input_uncached_per_million == 0.0
                            && p.input_cached_per_million == 0.0
                            && p.output_per_million == 0.0
                    })
                    .unwrap_or(true);
                if !is_zero {
                    continue;
                }
            }
            next.insert(target.clone(), source_price.clone());
            applied.push(target);
        }
        if applied.is_empty() {
            anyhow::bail!("no equivalent models to apply (all already priced or no peers)");
        }
        let known = self.referenced_physical_model_ids();
        // 仅写入 known（物理已引用）中的目标
        let filtered: HashMap<String, TokenPrice> = next
            .into_iter()
            .filter(|(k, _)| known.contains(k))
            .collect();
        self.token_price_config.set(filtered, &known)?;
        Ok(
            json!({ "applied_to": applied, "source": model, "price": source_price, "token_prices": self.token_price_snapshot() }),
        )
    }

    pub fn set_token_prices(
        &mut self,
        prices: HashMap<String, TokenPrice>,
    ) -> anyhow::Result<Value> {
        self.sync_token_price_defaults();
        let known = if self.v2.is_some() {
            self.referenced_physical_model_ids()
        } else {
            self.known_model_names()
        };
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
        let refs = if self.v2.is_some() {
            self.v2_key_refs()
        } else {
            self.all_key_refs()
        };
        let mut keys = Vec::new();
        for key in refs {
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
}

impl RouterState {
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
        let known_providers: HashSet<String> =
            default_provider_base_urls().keys().cloned().collect();
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
}
