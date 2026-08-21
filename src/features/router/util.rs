//! 配置校验与文本工具（纯函数）。

use std::collections::{HashMap, HashSet};

pub(crate) fn validate_weight_names(
    weights: &HashMap<String, i64>,
    known: &HashSet<String>,
) -> anyhow::Result<()> {
    let unknown = weights
        .keys()
        .filter(|name| !known.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        anyhow::bail!("unknown key name(s): {}", sorted_join(unknown));
    }
    let invalid = weights
        .iter()
        .filter(|(_, weight)| **weight < 0)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        anyhow::bail!("negative weight for key(s): {}", sorted_join(invalid));
    }
    Ok(())
}

pub(crate) fn sorted_join(mut values: Vec<String>) -> String {
    values.sort();
    values.dedup();
    values.join(", ")
}
