use super::io::{read_json, write_models_file};
use super::types::{V2LogicalModelsFile, V2ModelsFile, V2PhysicalModel};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
pub fn register_physical_models(
    models_path: &str,
    ids: &[String],
    known_providers: &HashSet<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut file: V2ModelsFile = read_json(models_path)?;
    let mut registered = Vec::new();
    for id in ids {
        let Some((provider, upstream)) = id.split_once('/') else {
            continue;
        };
        if !known_providers.contains(provider) {
            continue;
        }
        if file.models.contains_key(id) {
            continue;
        }
        file.models.insert(
            id.clone(),
            V2PhysicalModel {
                provider: provider.to_string(),
                upstream_model: upstream.to_string(),
                family: None,
                params: HashMap::new(),
                context_window: None,
                max_output_tokens: None,
                thinking_level_map: None,
                thinking_format: None,
            },
        );
        registered.push(id.clone());
    }
    if !registered.is_empty() {
        write_models_file(models_path, &file)?;
    }
    Ok(registered)
}

/// 供应商改名时同步 `models.json`：把引用旧 provider 的物理模型改指新 provider，
/// 并重写模型 id 前缀 `<old>/` → `<new>/`。
pub fn rename_provider_in_models(new_name: &str, old_name: &str, path: &str) -> anyhow::Result<()> {
    let mut models: V2ModelsFile = read_json(path)?;
    let mut renamed = HashMap::new();
    for (id, mut model) in models.models {
        if model.provider == old_name {
            model.provider = new_name.to_string();
            let rest = id.split('/').nth(1).unwrap_or(id.as_str());
            renamed.insert(format!("{new_name}/{rest}"), model);
        } else {
            renamed.insert(id, model);
        }
    }
    models.models = renamed;
    let raw = serde_json::to_string_pretty(&models)?;
    fs::write(Path::new(path), format!("{raw}\n"))?;
    Ok(())
}

/// 供应商改名时同步 `logical-models.json`：重写 target.model 的 `<old>/` → `<new>/`。
pub fn rename_provider_in_logical(
    new_name: &str,
    old_name: &str,
    path: &str,
) -> anyhow::Result<()> {
    let mut logical: V2LogicalModelsFile = read_json(path)?;
    let prefix = format!("{old_name}/");
    for lm in logical.logical_models.values_mut() {
        for target in &mut lm.route.targets {
            if let Some(rest) = target.model.strip_prefix(&prefix) {
                target.model = format!("{new_name}/{rest}");
            }
        }
    }
    let raw = serde_json::to_string_pretty(&logical)?;
    fs::write(Path::new(path), format!("{raw}\n"))?;
    Ok(())
}
