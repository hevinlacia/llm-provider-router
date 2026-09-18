//! RouterState key 引用推导、物理模型引用与 token 价格默认同步。

use super::RouterState;
use crate::config::KeyRef;
use crate::config_v2;
use crate::json_config::TokenPrice;
use std::collections::{HashMap, HashSet};

impl RouterState {
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

    /// 仅保留模型池实际引用的供应商真实模型（物理模型 id）。
    pub(super) fn referenced_physical_model_ids(&self) -> HashSet<String> {
        let cfg = &self.v2;
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
        referenced
    }

    fn first_physical_for_logical(&self, alias: &str) -> Option<String> {
        let cfg = &self.v2;
        fn dfs(
            cfg: &config_v2::V2Config,
            current: &str,
            visited: &mut HashSet<String>,
        ) -> Option<String> {
            if !visited.insert(current.to_string()) {
                return None;
            }
            let Some(lm) = cfg.logical_models.get(current) else {
                return None;
            };
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
        for logical in self.v2.logical_models.keys() {
            if expanded.contains_key(logical) {
                continue;
            }
            if let Some(first) = self.first_physical_for_logical(logical) {
                if let Some(price) = physical_prices.get(&first) {
                    expanded.insert(logical.clone(), price.clone());
                }
            }
        }
        expanded
    }

    pub(super) fn sync_token_price_defaults(&mut self) {
        let known = self.referenced_physical_model_ids();
        self.token_price_config.sync_to_known(&known);
    }
}
