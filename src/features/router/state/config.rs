//! RouterState 配置读写：key 权重、供应商 base_url、token 价格、模型等价组、自定义别名。

use super::RouterState;
use crate::config::{KeyRef, KeySource};
use crate::features::router::util::sorted_join;
use crate::json_config::TokenPrice;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;

/// 物理模型配置补丁（供应商模型配置面板保存）：None 字段保持原值不变。
#[derive(Clone, Debug)]
pub struct PhysicalModelPatch {
    pub model: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub supports_image: Option<bool>,
    pub thinking_level_map: Option<Option<HashMap<String, Option<String>>>>,
    pub thinking_format: Option<Option<String>>,
}

impl RouterState {
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

    // ---- Thinking maps (physical: provider/model) ----

    pub fn thinking_snapshot(&mut self) -> Value {
        let maps: Vec<Value> = {
            let cfg = &self.v2;
            {
                let mut out = Vec::new();
                for (id, pm) in &cfg.models {
                    out.push(json!({
                        "model": id,
                        "provider": pm.provider,
                        "upstream_model": pm.upstream_model,
                        "thinking_level_map": pm.thinking_level_map,
                        "thinking_format": pm.thinking_format,
                    }));
                }
                out.sort_by(|a, b| {
                    a.get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .cmp(b.get("model").and_then(Value::as_str).unwrap_or_default())
                });
                out
            }
        };
        json!({
            "maps": maps,
            "config_path": crate::config_v2::V2_MODELS_PATH,
        })
    }

    pub fn set_thinking_maps(
        &mut self,
        maps: Vec<(
            String,
            Option<std::collections::HashMap<String, Option<String>>>,
            Option<String>,
        )>,
    ) -> anyhow::Result<Value> {
        let known: HashSet<String> = self.v2.models.keys().cloned().collect();
        for (model, _, _) in &maps {
            if !known.contains(model) {
                anyhow::bail!("unknown physical model: {}", model);
            }
        }
        let mut file = crate::config_v2::load_models_file(crate::config_v2::V2_MODELS_PATH)?;
        for (model, level_map, format) in maps {
            if let Some(pm) = file.models.get_mut(&model) {
                pm.thinking_level_map = level_map;
                pm.thinking_format = format;
            }
        }
        crate::config_v2::write_models_file(crate::config_v2::V2_MODELS_PATH, &file)?;
        let _ = self.reload_v2();
        Ok(self.thinking_snapshot())
    }

    /// 批量保存物理模型完整配置（供应商模型配置面板）：
    /// context_window / max_output_tokens / supports_image / thinking_level_map / thinking_format。
    /// 只更新传入字段；未提供的字段保持不变（物理模型是能力参数的唯一持有者）。
    /// 未注册的 `provider/upstream` 模型（来自 detail 列表）自动注册（provider 已知时）。
    pub fn set_physical_models(
        &mut self,
        models: Vec<PhysicalModelPatch>,
    ) -> anyhow::Result<Value> {
        let cfg = self.v2.clone();
        let known: HashSet<String> = cfg.models.keys().cloned().collect();
        let mut file = crate::config_v2::load_models_file(crate::config_v2::V2_MODELS_PATH)?;
        for patch in &models {
            if known.contains(&patch.model) {
                continue;
            }
            // 自动注册：`provider/upstream` 且 provider 已知
            let Some((provider, upstream)) = patch.model.split_once('/') else {
                anyhow::bail!("unknown physical model: {}", patch.model);
            };
            if !cfg.providers.contains_key(provider) {
                anyhow::bail!("unknown physical model: {}", patch.model);
            }
            file.models.insert(
                patch.model.clone(),
                crate::config_v2::V2PhysicalModel {
                    provider: provider.to_string(),
                    upstream_model: upstream.to_string(),
                    family: None,
                    params: HashMap::new(),
                    context_window: None,
                    max_output_tokens: None,
                    supports_image: None,
                    thinking_level_map: None,
                    thinking_format: None,
                },
            );
        }
        for patch in models {
            let Some(pm) = file.models.get_mut(&patch.model) else {
                continue;
            };
            if patch.context_window.is_some() {
                pm.context_window = patch.context_window;
            }
            if patch.max_output_tokens.is_some() {
                pm.max_output_tokens = patch.max_output_tokens;
            }
            if patch.supports_image.is_some() {
                pm.supports_image = patch.supports_image;
            }
            if patch.thinking_level_map.is_some() {
                pm.thinking_level_map = patch.thinking_level_map.clone().flatten();
            }
            if patch.thinking_format.is_some() {
                pm.thinking_format = patch.thinking_format.clone().flatten();
            }
        }
        crate::config_v2::write_models_file(crate::config_v2::V2_MODELS_PATH, &file)?;
        let _ = self.reload_v2();
        Ok(self.v2_status())
    }

    pub fn set_token_prices(
        &mut self,
        prices: HashMap<String, TokenPrice>,
    ) -> anyhow::Result<Value> {
        self.sync_token_price_defaults();
        let known = self.referenced_physical_model_ids();
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

    pub fn key_secret_snapshot(&mut self) -> anyhow::Result<Value> {
        let refs = self.all_key_refs();
        let stored = self.api_keys_store.load();
        let mut keys = Vec::new();
        for key in refs {
            // 与 upstream_key_value 同源：按 source_setting 严格单源判定，
            // 不再跨源探测（env 模式不看 vault，vault 模式不看 env）。
            let source_setting = match key.source_setting() {
                KeySource::Env => "env",
                KeySource::Vault => "vault",
            };
            let configured = match key.source_setting() {
                KeySource::Env => env::var(&key.env_var)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .is_some(),
                KeySource::Vault => stored
                    .get(&key.name)
                    .filter(|value| !value.is_empty())
                    .is_some(),
            };
            let source = if configured {
                match key.source_setting() {
                    KeySource::Env => "environment",
                    KeySource::Vault => "vault",
                }
            } else {
                "missing"
            };
            keys.push(json!({
                "name": key.name,
                "provider": key.provider,
                "billing_type": key.billing_type,
                "env_var": key.env_var,
                "configured": configured,
                "source_setting": source_setting,
                "source": source,
                "persist": key.persist,
            }));
        }
        Ok(json!({
            "keys": keys,
            "note": "key 取值来源由 source 字段决定：env=只读环境变量（持久化走 agent-env.conf + reload-env），vault=只读 api-keys.json 的 key 名条目（每请求现场读盘，即改即生效）；缺省推断规则：env_var 非空 = env；两源互不回落、互不搬运",
        }))
    }

    pub fn set_key_values(
        &mut self,
        values: HashMap<String, String>,
        delete_names: HashSet<String>,
    ) -> anyhow::Result<Value> {
        let key_by_name: HashMap<String, KeyRef> = self
            .all_key_refs()
            .into_iter()
            .map(|k| (k.name.clone(), k))
            .collect();
        let unknown: Vec<String> = values
            .keys()
            .chain(delete_names.iter())
            .filter(|name| !key_by_name.contains_key(*name))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            anyhow::bail!("unknown key name(s): {}", sorted_join(unknown));
        }
        let mut stored = self.api_keys_store.load();
        // 按 key 的 source 分流写入（2026-11-04 起严格单源，不再互相镜像）：
        // - env 模式：只改运行时环境变量；持久化需运维更新 agent-env.conf
        //   （router 不代写系统 env 文件），重启后以 env 文件为准。
        // - vault 模式：只写 api-keys.json 的 key 名条目（每请求现场读盘，即改即生效）。
        let mut runtime_only: Vec<String> = Vec::new();
        for (name, value) in &values {
            let Some(key) = key_by_name.get(name) else {
                continue;
            };
            if key.source_setting() == KeySource::Env {
                anyhow::ensure!(
                    !key.env_var.is_empty(),
                    "key {} source=env 但 env_var 为空，无法写入",
                    name
                );
            }
            if apply_key_value_write(key, value, &mut stored) {
                runtime_only.push(name.clone());
            }
        }
        for name in &delete_names {
            let Some(key) = key_by_name.get(name) else {
                continue;
            };
            match key.source_setting() {
                KeySource::Env => {
                    if !key.env_var.is_empty() {
                        env::remove_var(&key.env_var);
                    }
                }
                KeySource::Vault => {
                    stored.remove(name.as_str());
                }
            }
        }
        self.api_keys_store.write(&stored)?;
        let mut result = self.key_secret_snapshot()?;
        if !runtime_only.is_empty() {
            // 提示调用方：env 模式写入仅运行时生效，持久化需同步 agent-env.conf。
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "runtime_only_warning".to_string(),
                    json!(format!(
                        "以下 env 模式 key 仅运行时生效（重启后以 agent-env.conf 为准）：{}",
                        sorted_join(runtime_only)
                    )),
                );
            }
        }
        Ok(result)
    }

    pub fn upstream_key_value(&mut self, key: &KeyRef) -> anyhow::Result<Option<String>> {
        // 严格单源（2026-11-04 起，source 显式开关）：env 只读环境变量，
        // vault 只读 api-keys.json 的 key 名条目；互不回落（显式声明时）。
        // 缺省（未配 source）沿用兼容规则：env_var 非空走 env（仍不回落 vault，
        // 与旧逻辑的差别仅在不再有 vault→env 启动注入兜底）。
        match key.source_setting() {
            KeySource::Env => Ok(env::var(&key.env_var)
                .ok()
                .filter(|value| !value.is_empty())),
            KeySource::Vault => Ok(self
                .api_keys_store
                .load()
                .remove(&key.name)
                .filter(|value| !value.is_empty())),
        }
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
        let known_providers: HashSet<String> = self.v2.providers.keys().cloned().collect();
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

/// 单条 key 值写入分流（纯逻辑，供 [`RouterState::set_key_values`] 与单测复用）。
/// 返回 true = 写了运行时环境变量（env 模式，重启后丢失，需同步 agent-env.conf）。
pub(crate) fn apply_key_value_write(
    key: &KeyRef,
    value: &str,
    stored: &mut HashMap<String, String>,
) -> bool {
    match key.source_setting() {
        KeySource::Env => {
            if !key.env_var.is_empty() {
                if value.is_empty() {
                    env::remove_var(&key.env_var);
                } else {
                    env::set_var(&key.env_var, value);
                }
            }
            true
        }
        KeySource::Vault => {
            if value.is_empty() {
                stored.remove(&key.name);
            } else {
                stored.insert(key.name.clone(), value.to_string());
            }
            false
        }
    }
}
