//! RouterState v2 分层配置：providers / logical models / virtual models 编辑与视图。

use super::RouterState;
use crate::config::{aliases, KeyRef, ModelAlias, DEFAULT_ARK_BASE_URL};
use crate::config_v2::{self, is_provider_scoped_virtual, V2Strategy};
use crate::state_store::now_seconds;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

impl RouterState {
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
    pub(super) fn custom_alias_models(&mut self) -> HashMap<String, ModelAlias> {
        let provider_urls = self.provider_base_urls();
        let mut out = HashMap::new();
        for custom in self.model_alias_config.get() {
            let base_url = provider_urls
                .get(&custom.provider)
                .cloned()
                .or_else(|| {
                    self.v2.as_ref().and_then(|cfg| {
                        cfg.providers
                            .get(&custom.provider)
                            .map(|p| p.base_url.clone())
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
    pub fn v2_status(&mut self) -> Value {
        let Some(cfg) = self.v2.as_ref() else {
            return json!({ "v2_enabled": false });
        };
        let mut providers = serde_json::Map::new();
        let provider_models =
            config_v2::load_provider_models_file(&self.settings.provider_models_path);
        for (name, prov) in &cfg.providers {
            let enabled: Vec<_> = prov.keys.iter().filter(|(_, k)| k.enabled).collect();
            let frozen_count = enabled
                .iter()
                .filter(|(kname, _)| {
                    let kid = format!("{}/{}", name, kname);
                    self.frozen.contains_key(&kid) || self.frozen.contains_key(*kname)
                })
                .count();
            let mut keys = serde_json::Map::new();
            for (kname, key) in &prov.keys {
                let kid = format!("{}/{}", name, kname);
                let frozen = self.frozen.contains_key(&kid) || self.frozen.contains_key(kname);
                let reason = self
                    .frozen
                    .get(&kid)
                    .or_else(|| self.frozen.get(kname))
                    .map(|f| f.reason.clone());
                keys.insert(
                    kname.clone(),
                    json!({
                        "env_var": key.env_var,
                        "weight": key.weight,
                        "billing_type": key.billing_type,
                        "enabled": key.enabled,
                        "frozen": frozen,
                        "frozen_reason": reason,
                    }),
                );
            }
            providers.insert(
                name.clone(),
                json!({
                    "base_url": prov.base_url,
                    "responses_base_url": prov.responses_base_url,
                    "anthropic_base_url": prov.anthropic_base_url,
                    "key_total": prov.keys.len(),
                    "key_enabled": enabled.len(),
                    "key_frozen": frozen_count,
                    "available": enabled.len() - frozen_count > 0,
                    "keys": keys,
                    "models": provider_models
                        .providers
                        .get(name)
                        .map(|e| e.models.clone())
                        .unwrap_or_default(),
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
                "context_window": pm.context_window,
                "max_output_tokens": pm.max_output_tokens,
                "supports_image": pm.supports_image,
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
        let mut virtual_models: serde_json::Map<String, Value> = serde_json::Map::new();
        let mut vlist: Vec<(String, String, String)> = Vec::new(); // (name, provider, upstream)
        for (name, mappings) in &cfg.virtual_models {
            for (provider, upstream) in mappings {
                vlist.push((name.clone(), provider.clone(), upstream.clone()));
            }
        }
        vlist.sort();
        for (name, provider, upstream) in vlist {
            let obj = virtual_models
                .entry(name)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("virtual_models entry is object");
            obj.insert(provider, Value::String(upstream));
        }
        json!({
            "v2_enabled": true,
            "providers": providers,
            "models": models,
            "logical_models": logical,
            "virtual_models": virtual_models,
        })
    }

    /// 动态上下文协商：聚合各逻辑模型的有效窗口（保守 min）及每物理目标窗口。
    /// `effective` 取“可用目标”的最小窗口（跨供应商取 min，available=false 的目标不计入）；
    /// 全部不可用时回退到全部目标的 min；未声明窗口的目标按 0 处理（不拉大 min）。
    pub fn router_capabilities(&mut self) -> Value {
        let Some(cfg) = self.v2.as_ref().cloned() else {
            return json!({ "v2_enabled": false, "models": [] });
        };
        // 为了读 frozen 需要 &mut，但 cfg 已克隆，避免借用冲突
        let mut models_out: Vec<Value> = Vec::new();
        let mut aliases: Vec<String> = cfg.logical_models.keys().cloned().collect();
        aliases.sort();
        for alias in aliases {
            let lm = match cfg.logical_models.get(&alias) {
                Some(v) => v,
                None => continue,
            };
            let strategy = match lm.route.strategy {
                V2Strategy::Priority => "priority",
                V2Strategy::Weighted => "weighted",
                V2Strategy::UsageAware => "usage-aware",
            };
            let candidates = match config_v2::resolve_targets(&cfg, &alias) {
                Some(c) => c,
                None => continue,
            };
            let mut targets_json: Vec<Value> = Vec::new();
            let mut available_cw: Vec<u32> = Vec::new();
            let mut available_mo: Vec<u32> = Vec::new();
            let mut all_cw: Vec<u32> = Vec::new();
            let mut all_mo: Vec<u32> = Vec::new();
            // 能力聚合（池取最低/最保守）：supports_image / reasoning / thinking map
            let mut any_supports_image: bool = false;
            let mut all_supports_image: bool = true;
            let mut any_reasoning_map: bool = false;
            let mut all_reasoning_map: bool = true;
            let mut formats: Vec<String> = Vec::new();
            let mut level_maps: Vec<HashMap<String, Option<String>>> = Vec::new();
            for cand in &candidates {
                let cw = cand.model.context_window;
                let mo = cand.model.max_output_tokens;
                // 物理 id：尝试从 cfg.models 反查，否则用 upstream；能力参数从物理模型读
                let physical_id = cfg
                    .models
                    .iter()
                    .find(|(_, pm)| {
                        pm.provider == cand.model.provider()
                            && cand.model.upstream_model() == pm.upstream_model
                    })
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| {
                        format!("{}/{}", cand.model.provider(), cand.model.upstream_model())
                    });
                let pm = cfg.models.get(&physical_id);
                let supports_image = pm.and_then(|p| p.supports_image).unwrap_or(false);
                any_supports_image |= supports_image;
                all_supports_image &= supports_image;
                let has_reasoning_map = pm
                    .and_then(|p| p.thinking_level_map.as_ref())
                    .is_some_and(|m| !m.is_empty());
                any_reasoning_map |= has_reasoning_map;
                all_reasoning_map &= has_reasoning_map;
                if let Some(fmt) = pm.and_then(|p| p.thinking_format.clone()) {
                    formats.push(fmt);
                }
                if let Some(map) = pm.and_then(|p| p.thinking_level_map.clone()) {
                    level_maps.push(map);
                }
                // 可用：至少一个 key 未冻结且权重>0
                let available = cand.model.keys.iter().any(|k| {
                    let kid = format!("{}/{}", k.provider, k.name);
                    !(self.frozen.contains_key(&kid) || self.frozen.contains_key(&k.name))
                        && k.weight > 0
                });
                if let Some(v) = cw {
                    all_cw.push(v);
                    if available {
                        available_cw.push(v);
                    }
                }
                if let Some(v) = mo {
                    all_mo.push(v);
                    if available {
                        available_mo.push(v);
                    }
                }
                targets_json.push(json!({
                    "id": physical_id,
                    "provider": cand.model.provider(),
                    "upstream_model": cand.model.upstream_model(),
                    "context_window": cw,
                    "max_output_tokens": mo,
                    "supports_image": supports_image,
                    "available": available,
                    "weight": cand.weight,
                }));
            }
            let eff_cw = if !available_cw.is_empty() {
                available_cw.into_iter().min()
            } else {
                all_cw.into_iter().min()
            };
            let eff_mo = if !available_mo.is_empty() {
                available_mo.into_iter().min()
            } else {
                all_mo.into_iter().min()
            };
            // 能力聚合（池取最低/最保守）：
            // - input/supports_image：所有候选都支持图片才对外声明图片模态。
            // - reasoning：所有候选都配置了非空思考映射才对外声明推理。
            // - thinking_level_map：取所有候选映射的公共档位交集（任一候选不支持则该档位不暴露），
            //   对外统一为 OpenAI 标准身份映射（xhigh:xhigh），真实上游方言保留在内部 fold。
            // - thinking_format：取第一个非空候选（无交集语义，仅协议名）。
            let input_modes = if any_supports_image && all_supports_image {
                vec!["text".to_string(), "image".to_string()]
            } else {
                vec!["text".to_string()]
            };
            let reasoning = any_reasoning_map && all_reasoning_map;
            let exposed_map = if level_maps.is_empty() {
                None
            } else {
                let mut m = std::collections::HashMap::new();
                for k in ["minimal", "low", "medium", "high", "xhigh"] {
                    let all_support = level_maps
                        .iter()
                        .all(|orig| matches!(orig.get(k), Some(Some(_))));
                    let any_declared = level_maps.iter().any(|orig| orig.contains_key(k));
                    // 仅当所有候选都声明且都支持该档位才暴露；任一候选不支持则整体不暴露
                    if any_declared && all_support {
                        m.insert(k.to_string(), Some(k.to_string()));
                    }
                }
                if m.is_empty() {
                    None
                } else {
                    Some(serde_json::to_value(&m).unwrap())
                }
            };
            let thinking_format = formats.first().cloned();
            let mut entry = json!({
                "id": alias,
                "name": lm.display_name.clone().unwrap_or_else(|| alias.clone()),
                "display_name": lm.display_name.clone(),
                "reasoning": reasoning,
                "input": input_modes,
                "strategy": strategy,
                "effective": {
                    "contextWindow": eff_cw,
                    "maxTokens": eff_mo,
                },
                "targets": targets_json,
            });
            if let Some(v) = exposed_map {
                entry["thinking_level_map"] = v;
            }
            if let Some(tf) = thinking_format {
                entry["thinking_format"] = json!(tf);
            }
            models_out.push(entry);
        }
        json!({
            "ok": true,
            "v2_enabled": true,
            "generated_at": now_seconds(),
            "models": models_out,
        })
    }

    /// v2 供应商探测信息：Chat Completions API 地址（base_url）+ Responses API 地址 + enabled key 的 env_var 列表（用于拉取模型名列表）。
    /// 探测统一走 Chat Completions API：模型名拉取优先 base_url，未配置时回退 responses_base_url。
    pub fn v2_provider_probe(&self, name: &str) -> Option<(String, Option<String>, Vec<String>)> {
        let cfg = self.v2.as_ref()?;
        let provider = cfg.providers.get(name)?;
        let mut env_vars: Vec<String> = provider
            .keys
            .iter()
            .filter(|(_, key)| key.enabled)
            .map(|(_, key)| key.env_var.clone())
            .collect();
        if env_vars.is_empty() {
            env_vars = provider
                .keys
                .values()
                .map(|key| key.env_var.clone())
                .collect();
        }
        Some((
            provider.base_url.clone(),
            provider.responses_base_url.clone(),
            env_vars,
        ))
    }

    /// 编辑 v2 供应商：改名 / base_url / keys（新增、删除、启用停用）。
    /// 改名时同步 `models.json` 与 `logical-models.json` 的引用，完成后热加载并返回最新视图。
    pub fn update_v2_provider(
        &mut self,
        old_name: &str,
        new_name: &str,
        base_url: &str,
        responses_base_url: Option<String>,
        anthropic_base_url: Option<String>,
        keys: HashMap<String, config_v2::V2Key>,
    ) -> anyhow::Result<Value> {
        if new_name.trim().is_empty() {
            anyhow::bail!("provider name must not be empty");
        }
        // 三类地址至少填一种：Chat Completions API(base_url) / Responses API(responses_base_url) / Anthropic API(anthropic_base_url)
        if base_url.trim().is_empty()
            && responses_base_url
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            && anthropic_base_url
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            anyhow::bail!("at least one of Chat Completions API / Responses API / Anthropic API base URL must not be empty");
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
        provider.responses_base_url = responses_base_url
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        provider.anthropic_base_url = anthropic_base_url
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        provider.keys = keys;
        providers.providers.insert(new_name.to_string(), provider);
        config_v2::write_providers_file(config_v2::V2_PROVIDERS_PATH, &providers)?;

        if renamed {
            config_v2::rename_provider_in_models(new_name, old_name, config_v2::V2_MODELS_PATH)?;
            config_v2::rename_provider_in_logical(
                new_name,
                old_name,
                config_v2::V2_LOGICAL_MODELS_PATH,
            )?;
        }
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 新增 v2 供应商：name / base_url / keys（可先不填 key，创建后再通过编辑补 key）。
    /// 新供应商不引用任何物理模型，无需同步 models.json / logical-models.json，写回后热加载。
    pub fn create_v2_provider(
        &mut self,
        name: &str,
        base_url: &str,
        responses_base_url: Option<String>,
        anthropic_base_url: Option<String>,
        keys: HashMap<String, config_v2::V2Key>,
    ) -> anyhow::Result<Value> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("provider name must not be empty");
        }
        // 三类地址至少填一种：Chat Completions API(base_url) / Responses API(responses_base_url) / Anthropic API(anthropic_base_url)
        if base_url.trim().is_empty()
            && responses_base_url
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            && anthropic_base_url
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            anyhow::bail!("at least one of Chat Completions API / Responses API / Anthropic API base URL must not be empty");
        }
        let mut providers = config_v2::load_providers_file(config_v2::V2_PROVIDERS_PATH)?;
        if providers.providers.contains_key(name) {
            anyhow::bail!("provider {name} already exists");
        }
        providers.providers.insert(
            name.to_string(),
            config_v2::V2Provider {
                base_url: base_url.trim().to_string(),
                responses_base_url: responses_base_url
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                anthropic_base_url: anthropic_base_url
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                retry: None,
                keys,
            },
        );
        config_v2::write_providers_file(config_v2::V2_PROVIDERS_PATH, &providers)?;
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 编辑 v2 逻辑模型：路由策略 + 目标（物理模型或嵌套逻辑模型）。
    /// 能力参数（上下文/输出/图片/思考映射）只属于物理模型，逻辑模型聚合见 router_capabilities。
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
        Self::auto_register_target_models(self, &cfg, &targets)?;
        let cfg = config_v2::load_v2_config()?;
        Self::validate_targets(&cfg, name, &targets)?;
        let mut logical = config_v2::load_logical_models_file(config_v2::V2_LOGICAL_MODELS_PATH)?;
        let lm = logical
            .logical_models
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("logical model {name} not found"))?;
        lm.route.strategy = strategy;
        lm.route.targets = targets;
        lm.params = params;
        config_v2::write_logical_models_file(config_v2::V2_LOGICAL_MODELS_PATH, &logical)?;
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 新增模型池（逻辑模型）：名字不能与物理模型 / 虚拟模型 / 已有逻辑模型冲突。
    /// 未注册的 `provider/upstream` target 会自动注册为物理模型（provider 已知时）。
    pub fn create_v2_logical_model(
        &mut self,
        name: &str,
        strategy: config_v2::V2Strategy,
        params: HashMap<String, serde_json::Value>,
        targets: Vec<config_v2::V2Target>,
    ) -> anyhow::Result<Value> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("logical model name must not be empty");
        }
        if targets.is_empty() {
            anyhow::bail!("route must have at least one target");
        }
        let cfg = config_v2::load_v2_config()?;
        if cfg.logical_models.contains_key(name) {
            anyhow::bail!("logical model {name} already exists");
        }
        if cfg.models.contains_key(name) {
            anyhow::bail!("logical model name conflicts with physical model id: {name}");
        }
        if cfg.virtual_models.contains_key(name) {
            anyhow::bail!("logical model name conflicts with virtual model name: {name}");
        }
        Self::auto_register_target_models(self, &cfg, &targets)?;
        let cfg = config_v2::load_v2_config()?;
        Self::validate_targets(&cfg, name, &targets)?;
        let mut logical = config_v2::load_logical_models_file(config_v2::V2_LOGICAL_MODELS_PATH)?;
        logical.logical_models.insert(
            name.to_string(),
            config_v2::V2LogicalModel {
                params,
                route: config_v2::V2Route { strategy, targets },
                display_name: None,
            },
        );
        config_v2::write_logical_models_file(config_v2::V2_LOGICAL_MODELS_PATH, &logical)?;
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 删除模型池（逻辑模型）：同时从其他模型池的 targets 移除对该池的引用，
    /// 避免 validate 因悬空引用失败。
    pub fn delete_v2_logical_model(&mut self, name: &str) -> anyhow::Result<Value> {
        let mut logical = config_v2::load_logical_models_file(config_v2::V2_LOGICAL_MODELS_PATH)?;
        if !logical.logical_models.contains_key(name) {
            anyhow::bail!("logical model {name} not found");
        }
        logical.logical_models.remove(name);
        for lm in logical.logical_models.values_mut() {
            lm.route.targets.retain(|t| t.model != name);
        }
        config_v2::write_logical_models_file(config_v2::V2_LOGICAL_MODELS_PATH, &logical)?;
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 校验 targets 引用（物理模型 / 虚拟模型 / 其他逻辑模型，不含自身）。
    /// 自动注册 `provider/upstream` 形式的未注册物理模型 target。
    /// 仅注册 provider 已知的组合（宽松接受任意 upstream；上游不存在时路由期失败并走 fallback，不阻塞配置保存）。
    fn auto_register_target_models(
        &mut self,
        cfg: &config_v2::V2Config,
        targets: &[config_v2::V2Target],
    ) -> anyhow::Result<()> {
        let known_providers: HashSet<&str> = cfg.providers.keys().map(String::as_str).collect();
        let unregistered: Vec<String> = targets
            .iter()
            .filter_map(|t| {
                let model = t.model.trim();
                if model
                    .split_once('/')
                    .is_some_and(|(p, _)| cfg.providers.contains_key(p))
                    && !cfg.models.contains_key(model)
                {
                    Some(model.to_string())
                } else {
                    None
                }
            })
            .collect();
        if !unregistered.is_empty() {
            config_v2::register_physical_models(
                config_v2::V2_MODELS_PATH,
                &unregistered,
                &known_providers,
            )?;
            self.reload_v2();
        }
        Ok(())
    }

    fn validate_targets(
        cfg: &config_v2::V2Config,
        name: &str,
        targets: &[config_v2::V2Target],
    ) -> anyhow::Result<()> {
        for target in targets {
            let ok = cfg.models.contains_key(&target.model)
                || (cfg.logical_models.contains_key(&target.model) && target.model != name)
                || cfg.virtual_models.contains_key(&target.model)
                || is_provider_scoped_virtual(cfg, &target.model);
            if !ok {
                anyhow::bail!(
                    "target {}: unknown physical model, virtual model or logical model (or self-reference)",
                    target.model
                );
            }
        }
        Ok(())
    }

    /// 新增/更新虚拟模型映射：`virtual_models[name][provider] = upstream_model`。
    /// 同名虚拟模型在多个供应商下可分别映射实际模型名。
    pub fn upsert_v2_virtual_model(
        &mut self,
        name: &str,
        provider: &str,
        upstream_model: &str,
    ) -> anyhow::Result<Value> {
        let name = name.trim();
        let provider = provider.trim();
        let upstream = upstream_model.trim();
        if name.is_empty() {
            anyhow::bail!("virtual model name must not be empty");
        }
        if !provider
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!("invalid provider name: {provider}");
        }
        if upstream.is_empty() {
            anyhow::bail!("upstream model must not be empty");
        }
        // 校验供应商存在
        let cfg = config_v2::load_v2_config()?;
        if !cfg.providers.contains_key(provider) {
            anyhow::bail!("unknown provider: {provider}");
        }
        // 校验不能与物理模型 id / 逻辑模型名冲突
        if cfg.models.contains_key(name) {
            anyhow::bail!("virtual model name conflicts with physical model id: {name}");
        }
        if cfg.logical_models.contains_key(name) {
            anyhow::bail!("virtual model name conflicts with logical model name: {name}");
        }

        let mut file = config_v2::load_virtual_models_file(config_v2::V2_VIRTUAL_MODELS_PATH);
        file.virtual_models
            .entry(name.to_string())
            .or_default()
            .insert(provider.to_string(), upstream.to_string());
        config_v2::write_virtual_models_file(config_v2::V2_VIRTUAL_MODELS_PATH, &file)?;
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 删除虚拟模型映射。删掉某个供应商的映射后若有其他供应商映射则保留虚拟名。
    pub fn delete_v2_virtual_model_mapping(
        &mut self,
        name: &str,
        provider: &str,
    ) -> anyhow::Result<Value> {
        let mut file = config_v2::load_virtual_models_file(config_v2::V2_VIRTUAL_MODELS_PATH);
        let Some(mappings) = file.virtual_models.get_mut(name) else {
            anyhow::bail!("virtual model {name} not found");
        };
        if mappings.remove(provider).is_none() {
            anyhow::bail!("virtual model {name} has no mapping for provider {provider}");
        }
        if mappings.is_empty() {
            file.virtual_models.remove(name);
        }
        config_v2::write_virtual_models_file(config_v2::V2_VIRTUAL_MODELS_PATH, &file)?;
        self.reload_v2();
        Ok(self.v2_status())
    }

    /// 重新加载 v2 配置（供应商编辑写回后热生效）。
    pub(super) fn reload_v2(&mut self) {
        if self.settings.v2_config_enabled {
            // 逻辑模型不再持有能力参数；编辑回写后如残留 legacy 字段（旧版本文件）一并迁移。
            let _ = config_v2::migrate_legacy_logical_caps(
                config_v2::V2_MODELS_PATH,
                config_v2::V2_LOGICAL_MODELS_PATH,
            );
            self.v2 = config_v2::load_v2_config().ok();
        }
    }
}
