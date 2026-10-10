//! RouterState route_aliases：请求模型名展开为物理候选列表。

use super::RouterState;
use crate::config::ModelAlias;
use crate::config_v2::{self, TargetCandidate, V2Strategy};
use crate::features::router::selection::{
    drop_exhausted_candidates, order_targets, usage_preferred_index,
};

impl RouterState {
    pub fn route_aliases(&mut self, model_name: &str, session_id: Option<&str>) -> Vec<ModelAlias> {
        {
            // 请求名即逻辑模型名（或 custom alias），resolve_targets 会嵌套展开到物理候选。
            let expanded: Vec<(String, Vec<TargetCandidate>)> = {
                let cfg = &self.v2;
                config_v2::resolve_targets(cfg, model_name)
                    .map(|c| vec![(model_name.to_string(), c)])
                    .unwrap_or_default()
            };
            // custom model aliases（运行时 API 手动新增的扁平逻辑模型）
            let customs = self.custom_alias_models();
            let mut out = Vec::new();
            for (name, candidates) in expanded {
                // priority 策略：耗尽候选软降级（全部耗尽时保留原列表，freeze 兜底）。
                let candidates = if matches!(
                    candidates.first().map(|c| &c.strategy),
                    Some(&V2Strategy::Priority)
                ) {
                    let names: Vec<String> = candidates
                        .iter()
                        .flat_map(|c| c.model.keys.iter().map(|k| k.name.clone()))
                        .collect();
                    match self.usage_store.key_token_totals_today(&names) {
                        Ok(totals) => drop_exhausted_candidates(candidates, &totals),
                        Err(_) => candidates,
                    }
                } else {
                    candidates
                };
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
            out
        }
    }
}
