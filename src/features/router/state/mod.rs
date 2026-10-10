//! RouterState 定义与核心生命周期：new/cleanup/冻结/绑定/选 key/快照/用量。
//!
//! 子模块（同一 `state` 模块树，可访问 RouterState 私有字段）：
//! - `config`：权重/供应商/价格/等价组/自定义别名配置
//! - `v2`：v2 分层配置编辑与视图
//! - `routing`：route_aliases 模型展开
//! - `keys`：key 引用与物理引用推导

use crate::config::{expand_path, KeyRef, ModelAlias, Settings};
use crate::config_v2;
use crate::config_v2::V2Strategy;
use crate::error_rules::{self, ErrorRulesConfig, ResolvedTunables};
use crate::features::router::costing::apply_costs;
use crate::features::router::failure::{ErrorLogEntry, RecentErrors};
use crate::features::router::freeze::key_state_id;
use crate::features::router::selection::{key_exhausted, priority_key_pick, weighted_pick};
use crate::features::router::UnsupportedEntry;
use crate::json_config::{ApiKeysStore, ModelAliasConfig, TokenPriceConfig};
use crate::state_store::{now_seconds, StateStore};
use crate::usage_store::UsageStore;
use anyhow::Context;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
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
    /// key × 模型级“不支持”记录（阶梯退避 + 永久失效），键为 (provider, key_name, upstream_model)
    unsupported: HashMap<(String, String, String), UnsupportedEntry>,
    usage_store: UsageStore,
    token_price_config: TokenPriceConfig,
    model_alias_config: ModelAliasConfig,
    api_keys_store: ApiKeysStore,
    /// v2 分层配置（唯一配置路径；启动加载失败直接 fail-fast）。
    v2: config_v2::V2Config,
    /// 最近一次 v2 配置重载失败原因（运行期坏文件保留 last-good 时记录，
    /// 经 /api/config/v2 的 v2_error 字段透出供诊断）。
    v2_load_error: Option<String>,
    /// 报错分类规则（模板 + tunables + 供应商绑定），config/error-rules.json。
    error_rules: ErrorRulesConfig,
    /// 最近上游报错 ring buffer（内存，重启丢失），供 /api/errors/recent 与 dashboard 展示。
    recent_errors: RecentErrors,
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
        let unsupported = state_store.load_unsupported()?;
        let usage_store = UsageStore::new(&settings.usage_db_path)?;
        let model_alias_config = ModelAliasConfig::new(&settings.model_alias_config_path);
        // 默认价格传空：运行期 sync_token_price_defaults 会按 v2 物理模型补默认价。
        let token_price_config =
            TokenPriceConfig::new(&settings.token_price_config_path, HashMap::new());
        let api_keys_store = ApiKeysStore::new(&settings.api_keys_path);
        // v2 分层配置是唯一配置路径：启动加载失败直接 fail-fast（systemd 重启暴露问题），
        // 不静默回退——legacy 硬编码别名已随 v1 退役删除。
        let v2 = config_v2::load_v2_config()
            .context("load v2 config (providers-v2/models/logical-models) failed")?;
        // 报错分类规则：文件缺失/非法时用内置默认（default + ark 预设），不阻断启动。
        let (error_rules, error_rules_error) =
            match error_rules::load_error_rules(&settings.error_rules_path) {
                Ok(config) => (config, None),
                Err(err) => (error_rules::builtin_config(), Some(err)),
            };
        if let Some(err) = &error_rules_error {
            eprintln!(
                "llm-provider-router: error-rules load failed, using builtin defaults: {err}"
            );
        }
        // First run (file missing): seed from environment so existing keys are
        // captured into the sole source of truth. Otherwise: apply stored key
        // values to the process environment without overriding existing vars.
        if !api_keys_store.exists() {
            let mut seed: HashMap<String, String> = HashMap::new();
            for provider in v2.providers.values() {
                for key in provider.keys.values() {
                    // Env-only keys (e.g. deepseek-official) are never
                    // persisted to api-keys.json; they come from the
                    // environment only. Empty env_var (dashboard inline key)
                    // has nothing to seed from either.
                    if !key.persist || key.env_var.is_empty() {
                        continue;
                    }
                    if let Ok(value) = env::var(&key.env_var) {
                        if !value.is_empty() {
                            seed.insert(key.env_var.clone(), value);
                        }
                    }
                }
            }
            if !seed.is_empty() {
                let _ = api_keys_store.write(&seed);
            }
        } else {
            let env_only_vars: HashSet<String> = v2
                .providers
                .values()
                .flat_map(|provider| provider.keys.values())
                .filter(|key| !key.persist)
                .map(|key| key.env_var.clone())
                .collect();
            let known_env_vars: HashSet<String> = v2
                .providers
                .values()
                .flat_map(|provider| provider.keys.values())
                .filter(|key| !key.env_var.is_empty())
                .map(|key| key.env_var.clone())
                .collect();
            let stored = api_keys_store.load();
            let mut prune: Vec<String> = Vec::new();
            for (env_var, value) in &stored {
                // Vault-by-name entries (dashboard inline keys without an
                // env var) are read by upstream_key_value from the store;
                // never inject them as environment variables.
                if !known_env_vars.contains(env_var) {
                    continue;
                }
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
        let state = Self {
            settings,
            state_store,
            frozen,
            bindings,
            unsupported,
            usage_store,
            token_price_config,
            model_alias_config,
            api_keys_store,
            v2,
            v2_load_error: None,
            error_rules,
            recent_errors: VecDeque::new(),
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
        let _ = self.reload_v2();
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

    /// 解除会话粘性绑定（内存 + 持久化）。用于绑定 key 被上游明确拒绝
    /// （非重试错误/断流）时避免会话被钉死在坏 key 上；下次请求重新随机，
    /// 选中后重新 bind。
    pub fn unbind(&mut self, alias: &str, session_id: &str) {
        self.bindings
            .remove(&(alias.to_string(), session_id.to_string()));
        if let Err(err) = self
            .state_store
            .delete_bindings(&[(alias.to_string(), session_id.to_string())])
        {
            eprintln!(
                "llm-provider-router unbind failed alias={alias} session={session_id}: {err}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 报错分类规则（error_rules）与最近报错 ring buffer
    // -----------------------------------------------------------------------

    pub(crate) fn error_rules(&self) -> &ErrorRulesConfig {
        &self.error_rules
    }

    /// 运行时生效参数：JSON tunables 优先，invalid 类回落 env 设置。
    pub(crate) fn tunables(&self) -> ResolvedTunables {
        self.error_rules
            .tunables
            .resolve(self.settings.subscription_invalid_freeze_seconds)
    }

    /// 月度/5小时配额重置兜底时长（settings env）。
    pub(crate) fn quota_fallback_seconds(&self) -> (f64, f64) {
        (
            self.settings.monthly_quota_fallback_seconds,
            self.settings.five_hour_quota_fallback_seconds,
        )
    }

    /// 生效配置视图（dashboard 设置页编辑基底）。
    pub fn error_rules_effective(&self) -> Value {
        serde_json::to_value(&self.error_rules).unwrap_or(Value::Null)
    }

    /// 从磁盘重读 error-rules（热加载 watcher / API 写回后调用）。保留 last-good。
    pub fn reload_error_rules(&mut self) -> bool {
        match error_rules::load_error_rules(&self.settings.error_rules_path) {
            Ok(config) => {
                self.error_rules = config;
                true
            }
            Err(err) => {
                eprintln!(
                    "llm-provider-router: error-rules reload failed, keeping last good: {err}"
                );
                false
            }
        }
    }

    /// 校验 + 写盘 + 热生效。失败时保留旧配置并返回错误。
    pub fn update_error_rules(&mut self, value: &Value) -> anyhow::Result<()> {
        let config: ErrorRulesConfig =
            serde_json::from_value(value.clone()).context("invalid error-rules schema")?;
        let path = expand_path(&self.settings.error_rules_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
        self.error_rules = config;
        Ok(())
    }

    pub(crate) fn push_recent_error(&mut self, entry: ErrorLogEntry) {
        if self.recent_errors.len() >= super::failure::RECENT_ERRORS_CAP {
            self.recent_errors.pop_front();
        }
        self.recent_errors.push_back(entry);
    }

    pub(crate) fn recent_errors_iter(
        &self,
    ) -> std::collections::vec_deque::Iter<'_, ErrorLogEntry> {
        self.recent_errors.iter()
    }

    // -----------------------------------------------------------------------
    // key × 模型“不支持”学习（阶梯退避，类比 RocketMQ 延迟重试）
    //
    // 同一供应商不同 key 的订阅套餐支持的模型不同（如 ark coding plan 404）。
    // 上游明确拒绝时记录，按 [1m, 5m, 30m, 2h, 8h] 阶梯拉长 probe 间隔，
    // 下一次失败间隔达到 1 天即 permanent（不再自动尝试），仅可通过
    // dashboard 刷新按钮重置。成功跑通该模型即清除记录（套餐升级后自动恢复）。
    // -----------------------------------------------------------------------

    /// probe 间隔阶梯（秒）；超出末档后 permanent。
    const UNSUPPORTED_RETRY_LADDER: [f64; 5] = [60.0, 300.0, 1800.0, 7200.0, 28800.0];
    /// 退避间隔达到该值即永久失效。
    const UNSUPPORTED_PERMANENT_THRESHOLD: f64 = 86_400.0;

    /// 上游对该 key × 模型明确拒绝：记录并推进退避阶梯。
    pub fn mark_key_model_unsupported(
        &mut self,
        provider: &str,
        key_name: &str,
        model: &str,
        error: &str,
    ) {
        let key_id = (
            provider.to_string(),
            key_name.to_string(),
            model.to_string(),
        );
        let attempt = self
            .unsupported
            .get(&key_id)
            .map(|e| e.attempt)
            .unwrap_or(0)
            + 1;
        let idx = (attempt as usize).saturating_sub(1);
        let delay = if idx < Self::UNSUPPORTED_RETRY_LADDER.len() {
            Self::UNSUPPORTED_RETRY_LADDER[idx]
        } else {
            Self::UNSUPPORTED_PERMANENT_THRESHOLD
        };
        // 阶梯耗尽后固定 1d，且一旦计算出的间隔达到 1d 即永久失效。
        let permanent = delay >= Self::UNSUPPORTED_PERMANENT_THRESHOLD;
        let now = now_seconds();
        let entry = UnsupportedEntry {
            last_error: error.chars().take(300).collect(),
            attempt,
            last_error_at: now,
            retry_at: now + delay,
            permanent,
        };
        if let Err(err) = self
            .state_store
            .upsert_unsupported(provider, key_name, model, &entry)
        {
            eprintln!(
                "llm-provider-router mark_unsupported failed {provider}/{key_name} {model}: {err}"
            );
        }
        self.unsupported.insert(key_id, entry);
    }

    /// 该 key × 模型当前是否应被跳过（permanent，或仍在退避窗口内）。
    fn key_model_blocked(&self, provider: &str, key_name: &str, model: &str) -> bool {
        self.unsupported
            .get(&(
                provider.to_string(),
                key_name.to_string(),
                model.to_string(),
            ))
            .map(|e| e.permanent || now_seconds() < e.retry_at)
            .unwrap_or(false)
    }

    /// 该 key × 模型成功跑通：清除记录（套餐升级后自动恢复参与）。
    pub fn clear_key_model_unsupported(&mut self, provider: &str, key_name: &str, model: &str) {
        let key_id = (
            provider.to_string(),
            key_name.to_string(),
            model.to_string(),
        );
        if self.unsupported.remove(&key_id).is_some() {
            if let Err(err) =
                self.state_store
                    .delete_unsupported(Some(provider), Some(key_name), Some(model))
            {
                eprintln!(
                    "llm-provider-router clear_unsupported failed {provider}/{key_name} {model}: {err}"
                );
            }
        }
    }

    /// dashboard 刷新：按前缀过滤重置退避（删除记录）。返回删除条数。
    pub fn refresh_unsupported(
        &mut self,
        provider: Option<&str>,
        key_name: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<usize> {
        let removed = self
            .state_store
            .delete_unsupported(provider, key_name, model)?;
        self.unsupported.retain(|(p, k, m), _| {
            let provider_match = provider.is_none_or(|v| v == p);
            let key_match = key_name.is_none_or(|v| v == k);
            let model_match = model.is_none_or(|v| v == m);
            !(provider_match && key_match && model_match)
        });
        Ok(removed)
    }

    /// 诊断视图（dashboard Settings 页展示）。
    pub fn unsupported_view(&self) -> Vec<Value> {
        let now = now_seconds();
        let mut rows: Vec<Value> = self
            .unsupported
            .iter()
            .map(|((p, k, m), e)| {
                json!({
                    "provider": p,
                    "key": k,
                    "model": m,
                    "attempt": e.attempt,
                    "permanent": e.permanent,
                    "retry_in_seconds": if e.permanent { Value::Null } else {
                        json!((e.retry_at - now).max(0.0) as u64)
                    },
                    "last_error_at": e.last_error_at,
                    "last_error": e.last_error,
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            let ka = (
                a["provider"].as_str().unwrap_or_default(),
                a["key"].as_str().unwrap_or_default(),
                a["model"].as_str().unwrap_or_default(),
            );
            let kb = (
                b["provider"].as_str().unwrap_or_default(),
                b["key"].as_str().unwrap_or_default(),
                b["model"].as_str().unwrap_or_default(),
            );
            ka.cmp(&kb)
        });
        rows
    }

    pub fn select_key_excluding(
        &mut self,
        alias: &ModelAlias,
        session_id: Option<&str>,
        excluded: &HashSet<String>,
    ) -> Result<KeyRef, NoAvailableKeyError> {
        self.cleanup()
            .map_err(|_| NoAvailableKeyError { retry_after: 60 })?;
        // key × 模型“不支持”学习：跳过仍在退避窗口/永久失效的 (key, upstream_model)。
        let upstream_model = alias.upstream_model().to_string();
        if let Some(session_id) = session_id {
            let binding = self
                .bindings
                .get(&(alias.alias.clone(), session_id.to_string()))
                .cloned();
            if let Some(binding) = binding {
                if !excluded.contains(&binding.key_name)
                    && !self.is_frozen(&binding.key_name).unwrap_or(true)
                {
                    if let Some(key) = alias.keys.iter().find(|key| {
                        key_state_id(key) == binding.key_name
                            && key.weight > 0
                            // 重试循环的 tried 以裸 key.name 记账，而 binding.key_name 是
                            // provider/name；必须按裸名再排除一次，否则粘住的本请求刚失败过
                            // 的 key（未冻结场景，如 500/连接错误）会被原样返回，死循环重试。
                            && !excluded.contains(&key.name)
                            && !self.key_model_blocked(&key.provider, &key.name, &upstream_model)
                    }) {
                        // 日配额耗尽的 key 不再粘性复用：priority 语义下耗尽即降级，
                        // 解除绑定后由下方按策略重选（无配额字段时零查询开销）。
                        if key.daily_token_quota.is_some() {
                            let totals = self
                                .usage_store
                                .key_token_totals_today(std::slice::from_ref(&key.name))
                                .unwrap_or_default();
                            if key_exhausted(key, *totals.get(&key.name).unwrap_or(&0)) {
                                let _ = self.unbind(&alias.alias, session_id);
                                // 落到下方正常选择流程
                            } else {
                                let key = key.clone();
                                let _ = self.bind(&alias.alias, session_id, &key_state_id(&key));
                                return Ok(key);
                            }
                        } else {
                            let key = key.clone();
                            let _ = self.bind(&alias.alias, session_id, &key_state_id(&key));
                            return Ok(key);
                        }
                    }
                }
            }
        }
        let mut collect = |respect_unsupported: bool| -> Vec<KeyRef> {
            alias
                .keys
                .iter()
                .filter(|key| {
                    key.weight > 0
                        && !excluded.contains(&key.name)
                        && !self.is_frozen(&key_state_id(key)).unwrap_or(true)
                        && (!respect_unsupported
                            || !self.key_model_blocked(&key.provider, &key.name, &upstream_model))
                })
                .cloned()
                .collect()
        };
        // 优先跳过“不支持”记录；若全部都在退避窗口（该模型对所有 key 都疑似不支持），
        // 放开过滤继续 probe —— 由失败路径推进阶梯，避免模型永远无 key 可用。
        let mut candidates = collect(true);
        if candidates.is_empty() {
            candidates = collect(false);
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
        // key 层负载均衡：与所属模型池同一套策略（alias.strategy），两层独立计算。
        let key = match alias.strategy {
            // weighted：按 key.weight 纯概率采样（session 粘性），不做用量调整。
            V2Strategy::Weighted => weighted_pick(&candidates, session_id, &alias.alias)
                .unwrap_or_else(|| candidates[0].clone()),
            // priority：weight 降序为优先级序，先用未耗尽的最高优先级 key；
            // 全耗尽回退加权采样（quota 配置不准时不把请求打死，freeze 兜底）。
            V2Strategy::Priority => {
                let names: Vec<String> = candidates.iter().map(|k| k.name.clone()).collect();
                let totals = self
                    .usage_store
                    .key_token_totals_today(&names)
                    .unwrap_or_default();
                priority_key_pick(&candidates, &totals)
                    .or_else(|| weighted_pick(&candidates, session_id, &alias.alias))
                    .unwrap_or_else(|| candidates[0].clone())
            }
            // usage-aware：选当日 tokens/weight 比值最低的 key（平衡消耗，不看剩余额度）。
            V2Strategy::UsageAware => self
                .usage_adjusted_pick(alias, &candidates, session_id)
                .unwrap_or_else(|_| {
                    weighted_pick(&candidates, session_id, &alias.alias)
                        .unwrap_or_else(|| candidates[0].clone())
                }),
        };
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
    /// 与 usage_key_name 的 `provider/key` 记录格式保持一致。
    pub fn key_names_for_provider(&mut self, provider: &str) -> Vec<String> {
        self.all_key_refs()
            .into_iter()
            .filter(|k| k.provider.eq_ignore_ascii_case(provider))
            .map(|k| format!("{}/{}", k.provider, k.name))
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
