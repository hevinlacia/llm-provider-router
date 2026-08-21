//! 选路排序与加权采样（纯函数）：order_targets / weighted_pick / 自定义 key 名归一。

use crate::config::{KeyRef, ModelAlias};
use crate::config_v2::{TargetCandidate, V2Strategy};
use crate::usage_store::UsageStore;
use rand::Rng;
use sha2::{Digest, Sha256};

/// 第 1 层路由排序：把物理模型候选排成"首选在前，其余作为回退"。
/// - priority：按 route.targets 原序；
/// - weighted / usage-aware：`preferred` 有值则用其作为首选（usage-aware 由调用方按用量算），
///   否则加权采样首选（session 粘性）；其余按 weight 降序作为回退。
pub fn order_targets(
    candidates: Vec<TargetCandidate>,
    session_id: Option<&str>,
    preferred: Option<usize>,
) -> Vec<ModelAlias> {
    let Some(first) = candidates.first() else {
        return Vec::new();
    };
    let strategy = first.strategy.clone();
    let alias = first.model.alias.clone();
    let mut items: Vec<(i64, ModelAlias)> = candidates
        .into_iter()
        .map(|c| (c.weight.unwrap_or(1).max(0), c.model))
        .collect();
    match strategy {
        V2Strategy::Priority => items.into_iter().map(|(_, m)| m).collect(),
        V2Strategy::Weighted | V2Strategy::UsageAware => {
            let total: i64 = items.iter().map(|(w, _)| w).sum();
            if total <= 0 {
                return items.into_iter().map(|(_, m)| m).collect();
            }
            let picked = match preferred {
                Some(idx) if idx < items.len() => idx,
                _ => weighted_first_index(&items, session_id, &alias),
            };
            let (_, first_model) = items.remove(picked);
            items.sort_by(|a, b| b.0.cmp(&a.0));
            let mut out = Vec::with_capacity(items.len() + 1);
            out.push(first_model);
            out.extend(items.into_iter().map(|(_, m)| m));
            out
        }
    }
}

/// 按 weight 加权采样首选的下标（session 粘性：同 session 结果稳定）。
fn weighted_first_index(
    items: &[(i64, ModelAlias)],
    session_id: Option<&str>,
    alias: &str,
) -> usize {
    let total: i64 = items.iter().map(|(w, _)| w).sum();
    if total <= 0 {
        return 0;
    }
    let mut target = if let Some(session) = session_id {
        let mut hasher = Sha256::new();
        hasher.update(format!("{alias}:{session}").as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) % total as u64) as i64
    } else {
        rand::thread_rng().gen_range(0..total)
    };
    for (i, (w, _)) in items.iter().enumerate() {
        target -= w;
        if target < 0 {
            return i;
        }
    }
    items.len() - 1
}

/// usage-aware 第 1 层：跨供应商按用量选首选。
/// 每个物理模型取其 key 池内最低 tokens/weight 比（池内最紧张的 key），
/// 选该比值最低的候选作为首选（即当前用量相对最宽裕的供应商）。
pub(crate) fn usage_preferred_index(
    usage: &UsageStore,
    alias: &str,
    candidates: &[TargetCandidate],
) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, candidate) in candidates.iter().enumerate() {
        let names: Vec<String> = candidate
            .model
            .keys
            .iter()
            .map(|k| k.name.clone())
            .collect();
        let Ok(totals) = usage.key_token_totals_for_model(alias, &names) else {
            continue;
        };
        let mut min_ratio = f64::INFINITY;
        let mut any = false;
        for key in &candidate.model.keys {
            let weight = key.weight.max(0) as f64;
            if weight <= 0.0 {
                continue;
            }
            any = true;
            let tokens = totals.get(&key.name).copied().unwrap_or(0) as f64;
            min_ratio = min_ratio.min(tokens / weight);
        }
        if any && min_ratio < best.as_ref().map(|(_, r)| *r).unwrap_or(f64::INFINITY) {
            best = Some((i, min_ratio));
        }
    }
    best.map(|(i, _)| i)
}

pub fn weighted_pick(keys: &[KeyRef], session_id: Option<&str>, alias: &str) -> Option<KeyRef> {
    let total: i64 = keys.iter().map(|key| key.weight.max(0)).sum();
    if total <= 0 {
        return None;
    }
    let mut target = if let Some(session_id) = session_id {
        let mut hasher = Sha256::new();
        hasher.update(format!("{alias}:{session_id}").as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) % total as u64) as i64
    } else {
        rand::thread_rng().gen_range(0..total)
    };
    for key in keys {
        target -= key.weight.max(0);
        if target < 0 {
            return Some(key.clone());
        }
    }
    keys.last().cloned()
}

pub fn normalize_custom_key_name(value: &str) -> String {
    let mut name = value.trim().to_string();
    let upper = name.to_uppercase();
    for prefix in ["AGENT_AI_ARK_", "AI_ARK_"] {
        if upper.starts_with(prefix) {
            name = name[prefix.len()..].to_string();
            break;
        }
    }
    if name.to_uppercase().ends_with("_API_KEY") {
        let len = name.len() - "_API_KEY".len();
        name.truncate(len);
    }
    name.trim_matches(&['_', '-'][..])
        .to_lowercase()
        .replace('_', "-")
}
