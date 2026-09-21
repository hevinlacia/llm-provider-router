use super::types::{V2Config, V2Target};
use super::validate::validate;
use crate::config::{KeyRef, ModelAlias, RetryPolicy};
use anyhow::anyhow;
use std::collections::{HashMap, HashSet};
/// 返回 None 表示无法折叠到任何物理模型（如全环引用，或首个可达 target 的 keys 白名单
/// 过滤后无可用 enabled key）。
/// 返回命中的 target：调用方需要它的 model id 与 keys 白名单。
fn first_physical_target<'a>(
    cfg: &'a V2Config,
    targets: &'a [V2Target],
    visited: &mut HashSet<String>,
) -> Option<&'a V2Target> {
    for target in targets {
        if cfg.models.contains_key(&target.model) {
            if target_keys_usable(cfg, target) {
                return Some(target);
            }
            continue;
        }
        if let Some(nested) = cfg.logical_models.get(&target.model) {
            if visited.insert(target.model.clone()) {
                if let Some(found) = first_physical_target(cfg, &nested.route.targets, visited) {
                    return Some(found);
                }
                visited.remove(&target.model);
            }
        }
    }
    None
}

/// target 的 keys 白名单（若设置）在该 provider 下是否至少命中一个 enabled key。
fn target_keys_usable(cfg: &V2Config, target: &V2Target) -> bool {
    let Some(allow) = target.keys.as_deref() else {
        return true;
    };
    let Some(model) = cfg.models.get(&target.model) else {
        return false;
    };
    let Some(prov) = cfg.providers.get(&model.provider) else {
        return false;
    };
    prov.keys
        .iter()
        .any(|(key_name, key)| key.enabled && allow.iter().any(|n| n == key_name))
}

pub fn fold_to_aliases(cfg: &V2Config) -> anyhow::Result<HashMap<String, ModelAlias>> {
    validate(cfg)?;
    let mut aliases = HashMap::new();
    for (alias, lm) in &cfg.logical_models {
        // 首 target 可能是另一个逻辑模型：递归展开到第一个可达物理模型（防环）。
        let mut visited = HashSet::new();
        visited.insert(alias.clone());
        let Some(first) = first_physical_target(cfg, &lm.route.targets, &mut visited) else {
            continue; // 全环引用 / 无可用 key，无法折叠，跳过该逻辑模型
        };
        let model = &cfg.models[&first.model];
        let provider = cfg.providers.get(&model.provider).ok_or_else(|| {
            anyhow!(
                "model '{}': missing provider '{}'",
                first.model,
                model.provider
            )
        })?;

        let keys: Vec<KeyRef> = provider
            .keys
            .iter()
            .filter(|(_, key)| key.enabled)
            .filter(|(key_name, _)| {
                first
                    .keys
                    .as_deref()
                    .is_none_or(|names| names.iter().any(|n| n == *key_name))
            })
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
