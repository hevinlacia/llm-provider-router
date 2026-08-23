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
