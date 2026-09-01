use super::types::{V2Config, V2Strategy};
use crate::config::{KeyRef, ModelAlias, RetryPolicy};
use std::collections::{HashMap, HashSet};
#[derive(Clone, Debug)]
pub struct TargetCandidate {
    /// 折叠好的物理模型（base_url/keys/retry 来自其供应商，params 为 logical+physical 合并）
    pub model: ModelAlias,
    /// route.targets[].weight（priority 策略下为 None）
    pub weight: Option<i64>,
    pub strategy: V2Strategy,
}

/// 参数合并：`overrides`（物理模型覆写）覆盖 `defaults`（逻辑模型默认）。
pub fn merge_params(
    defaults: &HashMap<String, serde_json::Value>,
    overrides: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    let mut merged = defaults.clone();
    for (key, value) in overrides {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

/// 把逻辑模型展开为物理模型候选（route.targets 顺序，未排序）。
/// target.model 可以是物理模型 id（`<provider>/<upstream>`）或另一个逻辑模型别名（嵌套展开，防环）。
/// 返回 None 表示该逻辑模型不存在或引用全部失效。
pub fn resolve_targets(cfg: &V2Config, alias: &str) -> Option<Vec<TargetCandidate>> {
    let mut visited = HashSet::new();
    resolve_targets_inner(cfg, alias, &mut visited)
}

fn resolve_targets_inner(
    cfg: &V2Config,
    alias: &str,
    visited: &mut HashSet<String>,
) -> Option<Vec<TargetCandidate>> {
    let lm = cfg.logical_models.get(alias)?;
    if !visited.insert(alias.to_string()) {
        return Some(Vec::new()); // 环：跳过，避免无限递归
    }
    let mut candidates = Vec::with_capacity(lm.route.targets.len());
    for target in &lm.route.targets {
        if cfg.models.contains_key(&target.model) {
            if let Some(candidate) =
                physical_candidate(cfg, alias, &target.model, target.weight, &lm.route.strategy)
            {
                candidates.push(candidate);
            }
        } else if let Some(virtual_cands) =
            virtual_candidates(cfg, alias, &target.model, target.weight, &lm.route.strategy)
        {
            candidates.extend(virtual_cands);
        } else if let Some(nested) = resolve_targets_inner(cfg, &target.model, visited) {
            candidates.extend(nested);
        }
    }
    visited.remove(alias);
    Some(candidates)
}

/// 虚拟模型展开：
/// - `target` 为纯虚拟名 → 展开所有映射供应商的物理候选；
/// - `target` 为 `provider/virtual` → 仅展开该供应商下的该虚拟模型。
/// 返回 None 表示不是虚拟模型引用。
fn virtual_candidates(
    cfg: &V2Config,
    alias: &str,
    target: &str,
    weight: Option<i64>,
    strategy: &V2Strategy,
) -> Option<Vec<TargetCandidate>> {
    let mut out = Vec::new();
    if let Some(mappings) = cfg.virtual_models.get(target) {
        // 纯虚拟名：展开所有供应商映射
        let mut providers: Vec<&String> = mappings.keys().collect();
        providers.sort();
        for provider in providers {
            if let Some(upstream) = mappings.get(provider) {
                if let Some(candidate) =
                    virtual_candidate(cfg, alias, provider, upstream, weight, strategy)
                {
                    out.push(candidate);
                }
            }
        }
        return Some(out);
    }
    // provider/virtual 形式
    if let Some((provider, rest)) = target.split_once('/') {
        if let Some(upstream) = cfg.virtual_models.get(rest).and_then(|m| m.get(provider)) {
            if let Some(candidate) =
                virtual_candidate(cfg, alias, provider, upstream, weight, strategy)
            {
                out.push(candidate);
            }
            return Some(out);
        }
    }
    None
}

/// 虚拟模型 → 单个物理候选（base_url/keys/retry 来自所属供应商）。
fn virtual_candidate(
    cfg: &V2Config,
    alias: &str,
    provider_name: &str,
    upstream_model: &str,
    weight: Option<i64>,
    strategy: &V2Strategy,
) -> Option<TargetCandidate> {
    let prov = cfg.providers.get(provider_name)?;

    let keys: Vec<KeyRef> = prov
        .keys
        .iter()
        .filter(|(_, key)| key.enabled)
        .map(|(key_name, key)| KeyRef {
            name: key_name.clone(),
            env_var: key.env_var.clone(),
            weight: key.weight,
            provider: provider_name.to_string(),
            billing_type: key.billing_type.clone(),
            persist: key.persist,
        })
        .collect();
    if keys.is_empty() {
        return None;
    }

    let retry = prov.retry.as_ref().map(|r| {
        RetryPolicy::new(
            r.max_retry_seconds,
            r.retry_delay_seconds,
            &r.retry_on_status,
        )
    });

    let mut model = ModelAlias::new(
        alias,
        &format!("openai/{}", upstream_model),
        &prov.base_url,
        keys,
        retry,
    )
    .with_params(lm_params_default(cfg, alias))
    .with_responses_base_url(prov.responses_base_url.clone())
    .with_anthropic_base_url(prov.anthropic_base_url.clone());
    // 虚拟映射无独立物理记录，尝试从 models 表按 provider+upstream 复用窗口声明
    if let Some(pm) = cfg
        .models
        .values()
        .find(|pm| pm.provider == provider_name && pm.upstream_model == upstream_model)
    {
        model = model.with_windows(pm.context_window, pm.max_output_tokens);
    }

    Some(TargetCandidate {
        model,
        weight,
        strategy: strategy.clone(),
    })
}

/// 单个物理模型候选（enabled 已过滤，params 为逻辑默认 + 物理覆写合并）。
fn physical_candidate(
    cfg: &V2Config,
    alias: &str,
    model_id: &str,
    weight: Option<i64>,
    strategy: &V2Strategy,
) -> Option<TargetCandidate> {
    let pm = cfg.models.get(model_id)?;
    let prov = cfg.providers.get(&pm.provider)?;

    let keys: Vec<KeyRef> = prov
        .keys
        .iter()
        .filter(|(_, key)| key.enabled)
        .map(|(key_name, key)| KeyRef {
            name: key_name.clone(),
            env_var: key.env_var.clone(),
            weight: key.weight,
            provider: pm.provider.clone(),
            billing_type: key.billing_type.clone(),
            persist: key.persist,
        })
        .collect();

    let retry = prov.retry.as_ref().map(|r| {
        RetryPolicy::new(
            r.max_retry_seconds,
            r.retry_delay_seconds,
            &r.retry_on_status,
        )
    });

    let model = ModelAlias::new(
        alias,
        &format!("openai/{}", pm.upstream_model),
        &prov.base_url,
        keys,
        retry,
    )
    .with_params(merge_params(&lm_params_default(cfg, alias), &pm.params))
    .with_windows(pm.context_window, pm.max_output_tokens)
    .with_responses_base_url(prov.responses_base_url.clone())
    .with_anthropic_base_url(prov.anthropic_base_url.clone());

    Some(TargetCandidate {
        model,
        weight,
        strategy: strategy.clone(),
    })
}

/// 读取逻辑模型默认参数（嵌套展开时用于外层逻辑模型的默认 params）。
fn lm_params_default(cfg: &V2Config, alias: &str) -> HashMap<String, serde_json::Value> {
    cfg.logical_models
        .get(alias)
        .map(|lm| lm.params.clone())
        .unwrap_or_default()
}
