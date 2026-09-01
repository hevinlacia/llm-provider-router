use super::types::{V2Config, V2Target};
use super::validate::validate;
use crate::config::{KeyRef, ModelAlias, RetryPolicy};
use anyhow::anyhow;
use std::collections::{HashMap, HashSet};
/// 返回 None 表示无法折叠到任何物理模型（如全环引用）。
pub(crate) fn first_physical_model<'a>(
    cfg: &'a V2Config,
    targets: &'a [V2Target],
    visited: &mut HashSet<String>,
) -> Option<&'a str> {
    for target in targets {
        if cfg.models.contains_key(&target.model) {
            return Some(&target.model);
        }
        if let Some(nested) = cfg.logical_models.get(&target.model) {
            if visited.insert(target.model.clone()) {
                if let Some(found) = first_physical_model(cfg, &nested.route.targets, visited) {
                    return Some(found);
                }
                visited.remove(&target.model);
            }
        }
    }
    None
}

pub fn fold_to_aliases(cfg: &V2Config) -> anyhow::Result<HashMap<String, ModelAlias>> {
    validate(cfg)?;
    let mut aliases = HashMap::new();
    for (alias, lm) in &cfg.logical_models {
        // 首 target 可能是另一个逻辑模型：递归展开到第一个可达物理模型（防环）。
        let mut visited = HashSet::new();
        visited.insert(alias.clone());
        let Some(first) = first_physical_model(cfg, &lm.route.targets, &mut visited) else {
            continue; // 全环引用，无法折叠，跳过该逻辑模型
        };
        let model = &cfg.models[first];
        let provider = cfg
            .providers
            .get(&model.provider)
            .ok_or_else(|| anyhow!("model '{}': missing provider '{}'", first, model.provider))?;

        let keys: Vec<KeyRef> = provider
            .keys
            .iter()
            .filter(|(_, key)| key.enabled)
            .map(|(key_name, key)| KeyRef {
                name: key_name.clone(),
                env_var: key.env_var.clone(),
                weight: key.weight,
                provider: model.provider.clone(),
                billing_type: key.billing_type.clone(),
                persist: key.persist,
            })
            .collect();

        let retry = provider.retry.as_ref().map(|r| {
            RetryPolicy::new(
                r.max_retry_seconds,
                r.retry_delay_seconds,
                &r.retry_on_status,
            )
        });

        let mut alias_obj = ModelAlias::new(
            alias,
            &format!("openai/{}", model.upstream_model),
            &provider.base_url,
            keys,
            retry,
        );
        alias_obj = alias_obj.with_windows(model.context_window, model.max_output_tokens);
        alias_obj = alias_obj.with_responses_base_url(provider.responses_base_url.clone());
        alias_obj = alias_obj.with_anthropic_base_url(provider.anthropic_base_url.clone());
        // 思考强度：能力参数只属于物理模型，逻辑模型不再持有（池聚合见 router_capabilities）
        let thinking_map = model.thinking_level_map.clone();
        let thinking_fmt = model.thinking_format.clone();
        alias_obj = alias_obj.with_thinking(thinking_map, thinking_fmt);
        aliases.insert(alias.clone(), alias_obj);
    }
    Ok(aliases)
}
