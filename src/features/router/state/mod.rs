//! RouterState 定义与核心生命周期：new/cleanup/冻结/绑定/选 key/快照/用量。
//!
//! 子模块（同一 `state` 模块树，可访问 RouterState 私有字段）：
//! - `config`：权重/供应商/价格/等价组/自定义别名配置
//! - `v2`：v2 分层配置编辑与视图
//! - `routing`：route_aliases 模型展开
//! - `keys`：key 引用与物理引用推导

use crate::config::{
    aliases, default_key_weights, default_provider_base_urls, expand_path, KeyRef, ModelAlias,
    Settings,
};
use crate::config_v2;
use crate::features::router::costing::{apply_costs, default_token_prices};
use crate::features::router::freeze::key_state_id;
use crate::features::router::selection::weighted_pick;
use crate::json_config::{
    ApiKeysStore, CustomKeyPoolConfig, KeyWeightConfig, ModelAliasConfig, ProviderConfig,
    TokenPriceConfig,
};
use crate::state_store::{now_seconds, StateStore};
use crate::usage_store::UsageStore;
use anyhow::Context;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;

pub(crate) mod config;
mod keys;
mod routing;
pub(crate) mod v2;

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
        let token_price_config =
            TokenPriceConfig::new(&settings.token_price_config_path, default_token_prices());
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

    /// 运行期重读 env 文件（如 systemd environment.d 生成的 agent-env.conf）并把变量
    /// 注入当前进程环境，使新增/更新的 provider key 无需重启即可被
    /// `upstream_key_value` 读到（key 值在路由时 live `env::var`）。
    /// 同时重读 v2 配置，让 providers-v2.json 里新增的 key/provider 即时生效。
    /// 未配置 `LLM_PROVIDER_ROUTER_ENV_FILE` 时返回空结果（不报错）。
    pub fn reload_env(&mut self) -> anyhow::Result<serde_json::Value> {
        let Some(path) = self.settings.env_file_path.clone() else {
            return Ok(serde_json::json!({
                "reloaded": 0,
                "path": "",
                "message": "LLM_PROVIDER_ROUTER_ENV_FILE not configured",
            }));
        };
        let expanded = expand_path(&path);
        let content = std::fs::read_to_string(&expanded)
            .map_err(|e| anyhow::anyhow!("read env file {}: {e}", expanded.display()))?;
        let mut imported = 0usize;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || !line.contains('=') {
                continue;
            }
            let (key, value) = match line.split_once('=') {
                Some(pair) => pair,
                None => continue,
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            std::env::set_var(key, value.trim());
            imported += 1;
        }
        // 重读 v2 配置：新增的 key/provider 即时生效。
        self.reload_v2();
        Ok(serde_json::json!({
            "reloaded": imported,
            "path": expanded.display().to_string(),
        }))
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
                        .find(|key| key_state_id(key) == binding.key_name && key.weight > 0)
                    {
                        let key = key.clone();
                        let _ = self.bind(&alias.alias, session_id, &key_state_id(&key));
                        return Ok(key);
                    }
                }
            }
        }
        let mut candidates = Vec::new();
        for key in &alias.keys {
            if key.weight > 0
                && !excluded.contains(&key.name)
                && !self.is_frozen(&key_state_id(key)).unwrap_or(true)
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
            let _ = self.bind(&alias.alias, session_id, &key_state_id(&key));
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
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.usage_store
            .record(model, key_name, status_code, usage, session_id)
    }

    /// 活跃 session 聚合（透传 usage_store）。
    pub fn active_sessions(&self) -> anyhow::Result<Value> {
        self.usage_store
            .active_sessions(3600, crate::state_store::now_seconds())
    }

    pub fn reset_usage(&mut self) -> anyhow::Result<()> {
        self.usage_store.reset()
    }

    /// 解析某供应商下所有 key 名（供 usage series 按供应商过滤）。
    /// v2 模式 key 名带 `provider/key` 前缀、以 KeyRef.provider 归属；非 v2 用原始 key 名。
    pub fn key_names_for_provider(&mut self, provider: &str) -> Vec<String> {
        let refs = if self.v2.is_some() {
            self.v2_key_refs()
        } else {
            self.all_key_refs()
        };
        refs.into_iter()
            .filter(|k| k.provider.eq_ignore_ascii_case(provider))
            .map(|k| k.name)
            .collect()
    }

    pub fn usage_snapshot(
        &mut self,
        period: &str,
        start: Option<&str>,
        end: Option<&str>,
    ) -> anyhow::Result<Value> {
        let mut snapshot = self.usage_store.snapshot(period, start, end, None)?;
        let prices = self.expanded_prices_for_cost();
        apply_costs(&mut snapshot, &prices);
        Ok(snapshot)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn usage_series(
        &mut self,
        period: &str,
        start: Option<&str>,
        end: Option<&str>,
        bucket: &str,
        group_by: &str,
        top: Option<usize>,
        key_names: Option<&[String]>,
    ) -> anyhow::Result<Value> {
        let mut payload = self
            .usage_store
            .series(period, start, end, bucket, group_by, top, key_names)?;
        // 附带总量（与时间/供应商过滤一致）以便前端同屏做份额、平均成本的小算术
        let prices = self.expanded_prices_for_cost();
        let mut snapshot = self.usage_store.snapshot(period, start, end, key_names)?;
        apply_costs(&mut snapshot, &prices);
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("total".to_string(), snapshot["total"].clone());
            obj.insert("total_cost".to_string(), snapshot["total_cost"].clone());
            obj.insert("range".to_string(), snapshot["range"].clone());
        }
        Ok(payload)
    }
}
