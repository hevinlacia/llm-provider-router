//! RouterState route_aliases：请求模型名展开为物理候选列表。

use super::RouterState;
use crate::config::ModelAlias;
use crate::config_v2::{self, TargetCandidate, V2Strategy};
use crate::features::router::selection::{order_targets, usage_preferred_index};

impl RouterState {
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
}
