//! RouterState key 引用与 v2 物理模型引用推导、rebind 清理。

use super::RouterState;
use crate::config::{aliases, KeyRef};
use crate::config_v2;
use crate::features::router::costing::default_token_prices;
use crate::json_config::{KeyWeightsConfigData, TokenPrice};
use std::collections::{HashMap, HashSet};

impl RouterState {
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

    /// v2 模式下的 key 视图：从 providers-v2.json 构建，key 名带 provider 前缀
    /// （`provider/key`），避免不同供应商同名 key 在 keys/usage 视图中合并。
    pub(super) fn v2_key_refs(&self) -> Vec<KeyRef> {
        let mut refs = Vec::new();
        if let Some(cfg) = &self.v2 {
            for (provider_name, provider) in &cfg.providers {
                for (key_name, key) in &provider.keys {
                    refs.push(KeyRef {
                        name: format!("{provider_name}/{key_name}"),
                        env_var: key.env_var.clone(),
                        weight: key.weight,
                        provider: provider_name.clone(),
                        billing_type: key.billing_type.clone(),
                        persist: key.persist,
                    });
                }
            }
        }
        refs.sort_by_key(|key| key.name.clone());
        refs
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

    pub(super) fn sync_custom_key_weight_defaults(&mut self) {
        let defaults = self
            .all_key_refs()
            .into_iter()
            .map(|key| (key.name, key.weight))
            .collect();
        self.weight_config.add_defaults(defaults);
    }

    pub(super) fn known_model_names(&mut self) -> HashSet<String> {
        self.settings_aliases().keys().cloned().collect()
    }

    /// 仅保留模型池实际引用的供应商真实模型（物理模型 id）。
    pub(super) fn referenced_physical_model_ids(&self) -> HashSet<String> {
        let Some(cfg) = self.v2.as_ref() else {
            return aliases().keys().cloned().collect();
        };
        let physical_ids: HashSet<&String> = cfg.models.keys().collect();
        let mut referenced = HashSet::new();
        fn collect(
            cfg: &config_v2::V2Config,
            alias: &str,
            referenced: &mut HashSet<String>,
            visited: &mut HashSet<String>,
            physical_ids: &HashSet<&String>,
        ) {
            if !visited.insert(alias.to_string()) {
                return;
            }
            let Some(lm) = cfg.logical_models.get(alias) else {
                return;
            };
            for target in &lm.route.targets {
                if physical_ids.contains(&target.model) {
                    referenced.insert(target.model.clone());
                } else if let Some(mappings) = cfg.virtual_models.get(&target.model) {
                    for (provider, upstream) in mappings {
                        referenced.insert(format!("{}/{}", provider, upstream));
                    }
                } else if target.model.contains('/') {
                    if let Some((provider, rest)) = target.model.split_once('/') {
                        if let Some(mappings) = cfg.virtual_models.get(rest) {
                            if let Some(upstream) = mappings.get(provider) {
                                referenced.insert(format!("{}/{}", provider, upstream));
                                continue;
                            }
                        }
                    }
                    // 未注册但形如 provider/upstream 的物理 id，也视为真实模型
                    referenced.insert(target.model.clone());
                } else if cfg.logical_models.contains_key(&target.model) {
                    collect(cfg, &target.model, referenced, visited, physical_ids);
                }
            }
        }
        for alias in cfg.logical_models.keys() {
            let mut visited = HashSet::new();
            collect(cfg, alias, &mut referenced, &mut visited, &physical_ids);
        }
        // 仅保留已在 models.json 中定义的物理模型（未定义的虚拟展开已单独插入）
        // 但用户要求“供应商实际模型”即物理层面的模型，保留所有 referenced（含虚拟展开的 provider/upstream）
        referenced
    }

    fn first_physical_for_logical(&self, alias: &str) -> Option<String> {
        let cfg = self.v2.as_ref()?;
        fn dfs(
            cfg: &config_v2::V2Config,
            current: &str,
            visited: &mut HashSet<String>,
        ) -> Option<String> {
            if !visited.insert(current.to_string()) {
                return None;
            }
            let lm = cfg.logical_models.get(current)?;
            for target in &lm.route.targets {
                if cfg.models.contains_key(&target.model) {
                    return Some(target.model.clone());
                }
                if let Some(mappings) = cfg.virtual_models.get(&target.model) {
                    let mut providers: Vec<&String> = mappings.keys().collect();
                    providers.sort();
                    if let Some(provider) = providers.first() {
                        if let Some(upstream) = mappings.get(*provider) {
                            return Some(format!("{}/{}", provider, upstream));
                        }
                    }
                }
                if target.model.contains('/') {
                    if let Some((provider, rest)) = target.model.split_once('/') {
                        if let Some(mappings) = cfg.virtual_models.get(rest) {
                            if let Some(upstream) = mappings.get(provider) {
                                return Some(format!("{}/{}", provider, upstream));
                            }
                        }
                    }
                    // 形如 provider/upstream 的未注册物理也直接返回
                    if target.model.contains('/') {
                        return Some(target.model.clone());
                    }
                }
                if cfg.logical_models.contains_key(&target.model) {
                    if let Some(found) = dfs(cfg, &target.model, visited) {
                        return Some(found);
                    }
                }
            }
            None
        }
        let mut visited = HashSet::new();
        dfs(cfg, alias, &mut visited)
    }

    pub(super) fn expanded_prices_for_cost(&mut self) -> HashMap<String, TokenPrice> {
        let physical_prices = self.token_price_config.get();
        let mut expanded = physical_prices.clone();
        if let Some(cfg) = self.v2.clone() {
            for logical in cfg.logical_models.keys() {
                if expanded.contains_key(logical) {
                    continue;
                }
                if let Some(first) = self.first_physical_for_logical(logical) {
                    if let Some(price) = physical_prices.get(&first) {
                        expanded.insert(logical.clone(), price.clone());
                    }
                }
            }
        }
        expanded
    }

    pub(super) fn sync_token_price_defaults(&mut self) {
        if self.v2.is_some() {
            let known = self.referenced_physical_model_ids();
            self.token_price_config.sync_to_known(&known);
        } else {
            let defaults = default_token_prices();
            self.token_price_config.add_defaults(defaults);
        }
    }

    pub(super) fn rebind_disabled_sessions(
        &mut self,
        weights: &KeyWeightsConfigData,
    ) -> anyhow::Result<()> {
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
