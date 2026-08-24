use super::types::{
    ProviderModelsFile, V2Config, V2LogicalModelsFile, V2ModelsFile, V2ProviderFile,
    V2VirtualModelsFile, V2_LOGICAL_MODELS_PATH, V2_MODELS_PATH, V2_PROVIDERS_PATH,
    V2_VIRTUAL_MODELS_PATH,
};
use super::validate::validate;
use anyhow::{anyhow, Context};
use serde::Deserialize;
use std::fs;
use std::path::Path;
/// 虚拟模型文件缺失时视为空。
pub fn load_v2_config() -> anyhow::Result<V2Config> {
    // 一次性迁移：把 logical-models.json 遗留的能力参数（thinking map/format/reasoning/input）
    // 下沉到其路由的物理模型，并清理逻辑模型字段（逻辑模型不再持有能力参数）。
    migrate_legacy_logical_caps(&V2_MODELS_PATH, &V2_LOGICAL_MODELS_PATH)?;
    load_v2_config_from(
        &V2_PROVIDERS_PATH,
        &V2_MODELS_PATH,
        &V2_LOGICAL_MODELS_PATH,
        &V2_VIRTUAL_MODELS_PATH,
    )
}

/// 供测试注入路径的加载入口。
pub fn load_v2_config_from(
    providers_path: &str,
    models_path: &str,
    logical_models_path: &str,
    virtual_models_path: &str,
) -> anyhow::Result<V2Config> {
    let providers: V2ProviderFile = read_json(providers_path)?;
    let models: V2ModelsFile = read_json(models_path)?;
    let logical: V2LogicalModelsFile = read_json(logical_models_path)?;
    let virtual_file: V2VirtualModelsFile = match read_json(virtual_models_path) {
        Ok(file) => file,
        Err(_) => V2VirtualModelsFile::default(),
    };

    let cfg = V2Config {
        providers: providers.providers,
        models: models.models,
        logical_models: logical.logical_models,
        virtual_models: virtual_file.virtual_models,
    };
    validate(&cfg)?;
    Ok(cfg)
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &str) -> anyhow::Result<T> {
    let file = Path::new(path);
    if !file.is_file() {
        return Err(anyhow!("v2 config file not found: {path}"));
    }
    let raw = fs::read_to_string(file).with_context(|| format!("read {path}"))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {path}"))
}

/// 一次性迁移（幂等）：逻辑模型不再持有能力参数，
/// 把 legacy 的 thinking_level_map / thinking_format / reasoning / input 下沉到其路由的物理模型：
/// - thinking_level_map / thinking_format → 物理模型对应字段（仅当物理模型未配置时写入）。
/// - input 含 "image" → 物理模型 supports_image=true（仅当未配置时写入）。
/// - reasoning 不再单独保存（由物理模型 thinking map 推导）；
///   若 legacy reasoning=false 且物理模型无 map，则物理模型保持无 map（= 不声明推理）。
/// 完成后清理逻辑模型的 legacy 字段并写回。
pub fn migrate_legacy_logical_caps(
    models_path: &str,
    logical_models_path: &str,
) -> anyhow::Result<bool> {
    let logical_file = Path::new(logical_models_path);
    if !logical_file.is_file() {
        return Ok(false);
    }
    let raw =
        fs::read_to_string(logical_file).with_context(|| format!("read {logical_models_path}"))?;
    let mut logical: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {logical_models_path}"))?;
    let mut models: V2ModelsFile = read_json(models_path)?;

    let logical_models = logical
        .get_mut("logical_models")
        .and_then(serde_json::Value::as_object_mut);
    let Some(logical_models) = logical_models else {
        return Ok(false);
    };

    // 先收集要迁移的 legacy 字段（避免借用冲突：先取数据再改 models）
    #[derive(Clone, Default)]
    struct Legacy {
        thinking_level_map: Option<std::collections::HashMap<String, Option<String>>>,
        thinking_format: Option<String>,
        supports_image: Option<bool>,
    }
    let mut to_migrate: Vec<(String, Legacy)> = Vec::new();
    let mut changed = false;
    for (name, lm) in logical_models.iter_mut() {
        let has_legacy = lm.get("thinking_level_map").is_some()
            || lm.get("thinking_format").is_some()
            || lm.get("reasoning").is_some()
            || lm.get("input").is_some();
        if !has_legacy {
            continue;
        }
        let mut legacy = Legacy::default();
        if let Some(v) = lm.get("thinking_level_map") {
            if !v.is_null() {
                if let Ok(map) = serde_json::from_value(v.clone()) {
                    legacy.thinking_level_map = Some(map);
                }
            }
        }
        if let Some(v) = lm.get("thinking_format") {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    legacy.thinking_format = Some(s.to_string());
                }
            }
        }
        if let Some(v) = lm.get("input") {
            let has_image = v
                .as_array()
                .is_some_and(|arr| arr.iter().any(|m| m.as_str() == Some("image")));
            if has_image {
                legacy.supports_image = Some(true);
            }
        }
        to_migrate.push((name.clone(), legacy));
        // 清理逻辑模型 legacy 字段
        if let Some(obj) = lm.as_object_mut() {
            if obj.remove("thinking_level_map").is_some() {
                changed = true;
            }
            if obj.remove("thinking_format").is_some() {
                changed = true;
            }
            if obj.remove("reasoning").is_some() {
                changed = true;
            }
            if obj.remove("input").is_some() {
                changed = true;
            }
        }
    }

    if !changed {
        return Ok(false);
    }

    // 展开每个逻辑模型的路由目标到物理模型 id，并把 legacy 能力下沉
    let mut models_changed = false;
    for (name, legacy) in to_migrate {
        let targets: Vec<String> = logical_models
            .get(&name)
            .and_then(|lm| lm.get("route"))
            .and_then(|r| r.get("targets"))
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        t.get("model")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // 物理目标 = 直接命中的物理 id + 嵌套逻辑模型的物理目标（递归一层展开足够）
        let mut physical_ids: Vec<String> = Vec::new();
        for t in &targets {
            if models.models.contains_key(t) {
                physical_ids.push(t.clone());
            } else if let Some(nested) = logical_models.get(t) {
                if let Some(route) = nested.get("route") {
                    if let Some(arr) = route.get("targets").and_then(serde_json::Value::as_array) {
                        for nt in arr {
                            if let Some(m) = nt.get("model").and_then(serde_json::Value::as_str) {
                                if models.models.contains_key(m) {
                                    physical_ids.push(m.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        for pid in physical_ids {
            let Some(pm) = models.models.get_mut(&pid) else {
                continue;
            };
            if legacy.thinking_level_map.is_some() && pm.thinking_level_map.is_none() {
                pm.thinking_level_map = legacy.thinking_level_map.clone();
                models_changed = true;
            }
            if legacy.thinking_format.is_some() && pm.thinking_format.is_none() {
                pm.thinking_format = legacy.thinking_format.clone();
                models_changed = true;
            }
            if legacy.supports_image.is_some() && pm.supports_image.is_none() {
                pm.supports_image = legacy.supports_image;
                models_changed = true;
            }
        }
    }

    if models_changed {
        write_models_file(models_path, &models)?;
    }
    let raw = serde_json::to_string_pretty(&logical)?;
    fs::write(Path::new(logical_models_path), format!("{raw}\n"))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// 写回：供应商编辑（改名 / base_url / keys 增删与启用停用）
// ---------------------------------------------------------------------------

/// 读取原始 providers-v2.json（保留未折叠结构）。
pub fn load_providers_file(path: &str) -> anyhow::Result<V2ProviderFile> {
    read_json(path)
}

pub fn write_providers_file(path: &str, file: &V2ProviderFile) -> anyhow::Result<()> {
    let raw = serde_json::to_string_pretty(file)?;
    fs::write(Path::new(path), format!("{raw}\n"))?;
    Ok(())
}

/// 读取原始 logical-models.json。
pub fn load_logical_models_file(path: &str) -> anyhow::Result<V2LogicalModelsFile> {
    read_json(path)
}

pub fn write_logical_models_file(path: &str, file: &V2LogicalModelsFile) -> anyhow::Result<()> {
    let raw = serde_json::to_string_pretty(file)?;
    fs::write(Path::new(path), format!("{raw}\n"))?;
    Ok(())
}

/// 读取虚拟模型文件（缺失时返回空文件）。
pub fn load_virtual_models_file(path: &str) -> V2VirtualModelsFile {
    match read_json::<V2VirtualModelsFile>(path) {
        Ok(file) => file,
        Err(_) => V2VirtualModelsFile::default(),
    }
}

pub fn write_virtual_models_file(path: &str, file: &V2VirtualModelsFile) -> anyhow::Result<()> {
    let raw = serde_json::to_string_pretty(file)?;
    fs::write(Path::new(path), format!("{raw}\n"))?;
    Ok(())
}

/// 读取供应商模型列表缓存（不存在或损坏时返回空文件）。
pub fn load_provider_models_file(path: &str) -> ProviderModelsFile {
    match read_json::<ProviderModelsFile>(path) {
        Ok(file) => file,
        Err(_) => ProviderModelsFile::default(),
    }
}

pub fn write_provider_models_file(path: &str, file: &ProviderModelsFile) -> anyhow::Result<()> {
    let raw = serde_json::to_string_pretty(file)?;
    fs::write(Path::new(path), format!("{raw}\n"))?;
    Ok(())
}

pub fn write_models_file(path: &str, file: &V2ModelsFile) -> anyhow::Result<()> {
    let raw = serde_json::to_string_pretty(file)?;
    fs::write(Path::new(path), format!("{raw}\n"))?;
    Ok(())
}

pub fn load_models_file(path: &str) -> anyhow::Result<V2ModelsFile> {
    read_json(path)
}
