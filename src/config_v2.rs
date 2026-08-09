//! v2 分层配置：Provider / PhysicalModel / ModelFamily / LogicalModel / Key。
//!
//! 架构目标见 `docs/architecture-v2.md`：
//! - Provider：物理上游（base_url + retry + keys），key 只与供应商关联，支持 enabled 手动停用。
//! - PhysicalModel：某供应商下的真实模型（upstream_model + 可选 family + 可选参数覆写）。
//! - ModelFamily：跨供应商关联"同一模型"。
//! - LogicalModel：对外暴露名（alias），无 base_url / 无 key，只有路由目标 + 策略 + 默认参数。
//!
//! Phase 1：提供解析器 + 折叠为旧 `ModelAlias` 的适配器 + 校验 + 单测。
//! 尚未接入运行时路由（Phase 2 切换），运行时默认仍走 `config::aliases()` 旧逻辑。

use crate::config::{KeyRef, ModelAlias, RetryPolicy};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

pub const V2_PROVIDERS_PATH: &str = "config/providers-v2.json";
pub const V2_MODELS_PATH: &str = "config/models.json";
pub const V2_LOGICAL_MODELS_PATH: &str = "config/logical-models.json";

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Key {
    pub env_var: String,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default = "default_billing")]
    pub billing_type: String,
    /// 手动启用/停用开关（默认 true）。停用的 key 不参与负载均衡。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// key 值是否可持久化到 api-keys.json（env-only key 设 false）。
    #[serde(default = "default_persist")]
    pub persist: bool,
}

fn default_weight() -> i64 {
    1
}
fn default_billing() -> String {
    "subscription".to_string()
}
fn default_enabled() -> bool {
    true
}
fn default_persist() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Retry {
    pub max_retry_seconds: u64,
    pub retry_delay_seconds: f64,
    #[serde(default)]
    pub retry_on_status: Vec<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Provider {
    pub base_url: String,
    #[serde(default)]
    pub retry: Option<V2Retry>,
    pub keys: HashMap<String, V2Key>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2ProviderFile {
    pub providers: HashMap<String, V2Provider>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2PhysicalModel {
    pub provider: String,
    pub upstream_model: String,
    #[serde(default)]
    pub family: Option<String>,
    /// 参数覆写（可选）：实际路由到该物理模型时应用；空 = 继承逻辑模型默认参数。
    #[serde(default)]
    pub params: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Family {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2ModelsFile {
    #[serde(default)]
    pub families: HashMap<String, V2Family>,
    pub models: HashMap<String, V2PhysicalModel>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum V2Strategy {
    /// 按 targets 顺序取第一个可用目标
    Priority,
    /// 按 targets[].weight 加权随机（session 粘性）
    Weighted,
    /// 按用量选最低的可用目标
    UsageAware,
}

impl Default for V2Strategy {
    fn default() -> Self {
        V2Strategy::Priority
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Target {
    /// 物理模型 id：`<provider>/<upstream_model>`
    pub model: String,
    #[serde(default)]
    pub weight: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Route {
    #[serde(default)]
    pub strategy: V2Strategy,
    pub targets: Vec<V2Target>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2LogicalModel {
    /// 默认参数（可选）：未配置的层不干预客户端参数。
    #[serde(default)]
    pub params: HashMap<String, serde_json::Value>,
    pub route: V2Route,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2LogicalModelsFile {
    pub logical_models: HashMap<String, V2LogicalModel>,
}

/// 聚合后的 v2 配置视图。
#[derive(Clone, Debug, Default)]
pub struct V2Config {
    pub providers: HashMap<String, V2Provider>,
    pub models: HashMap<String, V2PhysicalModel>,
    pub logical_models: HashMap<String, V2LogicalModel>,
}

// ---------------------------------------------------------------------------
// 加载
// ---------------------------------------------------------------------------

/// 从三个 v2 配置文件加载聚合配置。任一文件缺失/解析失败返回 Err。
pub fn load_v2_config() -> anyhow::Result<V2Config> {
    load_v2_config_from(&V2_PROVIDERS_PATH, &V2_MODELS_PATH, &V2_LOGICAL_MODELS_PATH)
}

/// 供测试注入路径的加载入口。
pub fn load_v2_config_from(
    providers_path: &str,
    models_path: &str,
    logical_models_path: &str,
) -> anyhow::Result<V2Config> {
    let providers: V2ProviderFile = read_json(providers_path)?;
    let models: V2ModelsFile = read_json(models_path)?;
    let logical: V2LogicalModelsFile = read_json(logical_models_path)?;

    let cfg = V2Config {
        providers: providers.providers,
        models: models.models,
        logical_models: logical.logical_models,
    };
    validate(&cfg)?;
    Ok(cfg)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &str) -> anyhow::Result<T> {
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

/// 供应商改名时同步 `models.json`：把引用旧 provider 的物理模型改指新 provider，
/// 并重写模型 id 前缀 `<old>/` → `<new>/`。
pub fn rename_provider_in_models(
    new_name: &str,
    old_name: &str,
    path: &str,
) -> anyhow::Result<()> {
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

// ---------------------------------------------------------------------------
// 校验
// ---------------------------------------------------------------------------

pub fn validate(cfg: &V2Config) -> anyhow::Result<()> {
    for (provider_name, provider) in &cfg.providers {
        if provider.base_url.is_empty() {
            return Err(anyhow!("provider {provider_name}: base_url is empty"));
        }
        for (key_name, key) in &provider.keys {
            if key.env_var.is_empty() {
                return Err(anyhow!("provider {provider_name} key {key_name}: env_var is empty"));
            }
        }
    }
    for (model_id, model) in &cfg.models {
        if !cfg.providers.contains_key(&model.provider) {
            return Err(anyhow!(
                "model {model_id}: references unknown provider '{}'",
                model.provider
            ));
        }
        if let Some(family) = &model.family {
            // family 允许指向未显式声明的族（隐式族），不强制校验。
            let _ = family;
        }
    }
    for (alias, lm) in &cfg.logical_models {
        if lm.route.targets.is_empty() {
            return Err(anyhow!("logical model {alias}: route has no targets"));
        }
        for target in &lm.route.targets {
            let known_physical = cfg.models.contains_key(&target.model);
            let known_logical =
                cfg.logical_models.contains_key(&target.model) && target.model.as_str() != alias.as_str();
            if !known_physical && !known_logical {
                return Err(anyhow!(
                    "logical model {alias}: target references unknown physical model or logical model '{}'",
                    target.model
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 折叠适配器：V2Config -> 旧 HashMap<String, ModelAlias>
// ---------------------------------------------------------------------------

/// 把 v2 配置折叠为旧 ModelAlias 视图，供现有路由/API 复用。
///
/// 折叠规则（Phase 1 语义，等价于现状"一逻辑模型一物理目标"）：
/// - 每个逻辑模型取 `route.targets` 第一个目标的物理模型；
/// - litellm_model = `openai/<upstream_model>`；
/// - base_url / retry / keys 来自该物理模型所属供应商；
/// - keys 过滤 `enabled=false` 的 key。
///
/// 跨供应商回退（多个 target）在 Phase 2 由路由引擎消费，折叠视图只表达主目标。
pub fn fold_to_aliases(cfg: &V2Config) -> anyhow::Result<HashMap<String, ModelAlias>> {
    validate(cfg)?;
    let mut aliases = HashMap::new();
    for (alias, lm) in &cfg.logical_models {
        let Some(first) = lm.route.targets.first() else {
            continue;
        };
        let model = cfg
            .models
            .get(&first.model)
            .ok_or_else(|| anyhow!("logical model {alias}: missing physical model '{}'", first.model))?;
        let provider = cfg
            .providers
            .get(&model.provider)
            .ok_or_else(|| anyhow!("model '{}': missing provider '{}'", first.model, model.provider))?;

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

        aliases.insert(
            alias.clone(),
            ModelAlias::new(
                alias,
                &format!("openai/{}", model.upstream_model),
                &provider.base_url,
                keys,
                retry,
            ),
        );
    }
    Ok(aliases)
}

// ---------------------------------------------------------------------------
// 第 1 层路由：逻辑模型 → 物理模型候选列表
// ---------------------------------------------------------------------------

/// 单个物理模型候选（静态：enabled 已过滤，frozen 由第 2 层选 key 时处理）。
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
        } else if let Some(nested) = resolve_targets_inner(cfg, &target.model, visited) {
            candidates.extend(nested);
        }
    }
    visited.remove(alias);
    Some(candidates)
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
        RetryPolicy::new(r.max_retry_seconds, r.retry_delay_seconds, &r.retry_on_status)
    });

    let model = ModelAlias::new(
        alias,
        &format!("openai/{}", pm.upstream_model),
        &prov.base_url,
        keys,
        retry,
    )
    .with_params(merge_params(&lm_params_default(cfg, alias), &pm.params));

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

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDERS: &str = r#"{
      "providers": {
        "ark": {
          "base_url": "https://ark.cn-beijing.volces.com/api/coding/v3",
          "retry": {
            "max_retry_seconds": 300,
            "retry_delay_seconds": 5.0,
            "retry_on_status": [401, 402, 429, 500, 502, 503, 504]
          },
          "keys": {
            "hevin":   { "env_var": "AGENT_AI_ARK_HEVIN_API_KEY",   "weight": 5, "billing_type": "subscription" },
            "wilford": { "env_var": "AGENT_AI_ARK_WILFORD_API_KEY", "weight": 3, "billing_type": "subscription", "enabled": false }
          }
        },
        "deepseek-official": {
          "base_url": "https://api.deepseek.com",
          "keys": {
            "deepseek-official": { "env_var": "AGENT_AI_DEEPSEEK_API_KEY", "weight": 1, "billing_type": "payg" }
          }
        }
      }
    }"#;

    const MODELS: &str = r#"{
      "families": {
        "deepseek-v4-flash": { "display_name": "DeepSeek V4 Flash" }
      },
      "models": {
        "ark/deepseek-v4-flash": {
          "provider": "ark",
          "upstream_model": "deepseek-v4-flash",
          "family": "deepseek-v4-flash"
        },
        "deepseek-official/deepseek-v4-flash": {
          "provider": "deepseek-official",
          "upstream_model": "deepseek-v4-flash",
          "family": "deepseek-v4-flash"
        }
      }
    }"#;

    const LOGICAL: &str = r#"{
      "logical_models": {
        "deepseek-v4-flash-auto": {
          "params": { "temperature": 1.0 },
          "route": {
            "strategy": "weighted",
            "targets": [
              { "model": "ark/deepseek-v4-flash", "weight": 8 },
              { "model": "deepseek-official/deepseek-v4-flash", "weight": 2 }
            ]
          }
        },
        "deepseek-v4-flash-official": {
          "route": {
            "strategy": "priority",
            "targets": [ { "model": "deepseek-official/deepseek-v4-flash" } ]
          }
        }
      }
    }"#;

    fn write_temp(dir: &Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    /// 每次调用使用唯一临时目录，避免并行测试互相覆盖同名配置文件。
    fn load_test() -> V2Config {
        let seq = {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        };
        let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-{seq}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = write_temp(&dir, "providers.json", PROVIDERS);
        let m = write_temp(&dir, "models.json", MODELS);
        let l = write_temp(&dir, "logical.json", LOGICAL);
        load_v2_config_from(&p, &m, &l).unwrap()
    }

    #[test]
    fn parses_provider_key_enabled_defaults() {
        let cfg = load_test();
        let ark = cfg.providers.get("ark").unwrap();
        assert_eq!(
            ark.keys.get("hevin").unwrap().enabled,
            true,
            "enabled 缺省应为 true"
        );
        assert_eq!(ark.keys.get("wilford").unwrap().enabled, false);
        assert_eq!(ark.keys.get("wilford").unwrap().weight, 3);
        // deepseek-official 缺省 billing/weight/enabled
        let dsk = cfg.providers.get("deepseek-official").unwrap();
        let dk = dsk.keys.get("deepseek-official").unwrap();
        assert_eq!(dk.billing_type, "payg");
        assert_eq!(dk.weight, 1);
        assert_eq!(dk.enabled, true);
    }

    #[test]
    fn folds_to_aliases_takes_first_target_and_filters_disabled_keys() {
        let cfg = load_test();
        let aliases = fold_to_aliases(&cfg).unwrap();

        let flash = aliases.get("deepseek-v4-flash-auto").unwrap();
        assert_eq!(flash.litellm_model, "openai/deepseek-v4-flash");
        assert_eq!(
            flash.base_url,
            "https://ark.cn-beijing.volces.com/api/coding/v3"
        );
        // ark 的 wilford 被停用，不应出现在 keys 里
        let key_names: Vec<&str> = flash.keys.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(key_names, vec!["hevin"]);
        assert_eq!(flash.keys[0].env_var, "AGENT_AI_ARK_HEVIN_API_KEY");
        assert!(flash.retry_policy.is_some(), "ark retry 应被折叠");

        let official = aliases.get("deepseek-v4-flash-official").unwrap();
        assert_eq!(official.base_url, "https://api.deepseek.com");
        assert_eq!(official.keys.len(), 1);
        assert_eq!(official.keys[0].provider, "deepseek-official");
        assert!(official.retry_policy.is_none(), "deepseek-official 无 retry 配置");
    }

    #[test]
    fn validates_unknown_references() {
        let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-bad", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = write_temp(&dir, "providers.json", PROVIDERS);
        let m = write_temp(&dir, "models.json", MODELS);
        let l = write_temp(
            &dir,
            "logical.json",
            r#"{"logical_models":{"x":{"route":{"targets":[{"model":"nope/missing"}]}}}}"#,
        );
        let result = load_v2_config_from(&p, &m, &l);
        assert!(result.is_err(), "引用不存在的物理模型应报错");
        assert!(result.unwrap_err().to_string().contains("unknown physical model"));
    }

    #[test]
    fn missing_files_return_error() {
        let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-missing", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("providers.json").to_string_lossy().to_string();
        let m = dir.join("models.json").to_string_lossy().to_string();
        let l = dir.join("logical.json").to_string_lossy().to_string();
        let result = load_v2_config_from(&p, &m, &l);
        assert!(result.is_err(), "文件缺失应返回 Err");
    }

    /// 读取仓库真实迁移配置文件（providers-v2/models/logical-models），
    /// 验证折叠结果与旧 config::aliases() 的主目标完全一致（迁移等价性）。    /// 需要仓库内的真实配置，默认 ignore，用 `cargo test -- --ignored` 显式运行。
    #[test]
    #[ignore = "requires real config files in repo"]
    fn real_repo_config_folds_equivalent_to_legacy() {
        let cfg = load_v2_config().expect("v2 配置文件应可加载");
        let aliases = fold_to_aliases(&cfg).expect("折叠应成功");

        for expected in [
            "deepseek-v4-flash-auto",
            "deepseek-v4-flash-260801",
            "deepseek-v4-flash-official",
            "deepseek-v4-pro-auto",
            "deepseek-v4-pro-official",
            "glm-latest-auto",
            "minimax-latest-auto",
            "ark-code-latest-auto",
            "openai-gpt-5.5-hevin",
            "openai-gpt-5.6-sol-hevin",
        ] {
            assert!(aliases.contains_key(expected), "v2 折叠缺少 alias: {expected}");
        }

        // 与旧 aliases() 主目标等价性校验：litellm_model / base_url 应一致
        let legacy = crate::config::aliases();
        for (name, alias) in &aliases {
            let Some(old) = legacy.get(name) else { continue };
            assert_eq!(
                alias.litellm_model, old.litellm_model,
                "alias {name} 上游模型不一致"
            );
            assert_eq!(alias.base_url, old.base_url, "alias {name} base_url 不一致");
        }

        // v2 语义校验（与旧逻辑的预期差异，见 architecture-v2.md §5）：
        // - custom keys（hevin-private/shell）并入 ark，key 只与供应商关联，可用于 ark 所有模型；
        // - wilford 用 enabled=false 表达停用（旧逻辑用 weight=0），折叠后应被过滤。
        let flash = aliases.get("deepseek-v4-flash-auto").unwrap();
        let key_names: Vec<&str> = flash.keys.iter().map(|k| k.name.as_str()).collect();
        assert!(key_names.contains(&"hevin-private"), "hevin-private 应并入 ark 模型");
        assert!(key_names.contains(&"shell"), "shell 应并入 ark 模型");
        assert!(
            !key_names.contains(&"wilford"),
            "enabled=false 的 wilford 应被过滤"
        );
        assert_eq!(
            flash.keys.len(),
            8,
            "ark 应有 9 key 减 1 个停用 = 8 个可用 key"
        );
    }

    #[test]
    fn merge_params_overrides_defaults() {
        let mut defaults = HashMap::new();
        defaults.insert("temperature".into(), serde_json::json!(0.7));
        defaults.insert("thinking".into(), serde_json::json!(true));
        let mut overrides = HashMap::new();
        overrides.insert("temperature".into(), serde_json::json!(0.3));
        overrides.insert("max_tokens".into(), serde_json::json!(4096));

        let merged = merge_params(&defaults, &overrides);
        assert_eq!(merged.get("temperature"), Some(&serde_json::json!(0.3)), "physical 应覆写 logical");
        assert_eq!(merged.get("thinking"), Some(&serde_json::json!(true)), "未覆写字段保留 logical");
        assert_eq!(merged.get("max_tokens"), Some(&serde_json::json!(4096)), "新增字段来自 physical");
    }

    #[test]
    fn resolve_targets_expands_multi_provider_with_params_merge() {
        let cfg = load_test();
        let candidates = resolve_targets(&cfg, "deepseek-v4-flash-auto").expect("应解析出候选");
        assert_eq!(candidates.len(), 2, "auto 应展开为 ark + official 两个物理模型");

        let ark = &candidates[0];
        assert_eq!(ark.model.base_url, "https://ark.cn-beijing.volces.com/api/coding/v3");
        assert_eq!(ark.model.litellm_model, "openai/deepseek-v4-flash");
        // ark 的 wilford enabled=false 应被过滤
        let names: Vec<&str> = ark.model.keys.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, vec!["hevin"]);
        // logical.params(temperature=1.0) 应被继承
        assert_eq!(ark.model.params.get("temperature"), Some(&serde_json::json!(1.0)));
        assert_eq!(ark.weight, Some(8));

        let official = &candidates[1];
        assert_eq!(official.model.base_url, "https://api.deepseek.com");
        assert_eq!(official.model.litellm_model, "openai/deepseek-v4-flash");
        assert_eq!(official.weight, Some(2));
    }

    #[test]
    fn resolve_targets_unknown_alias_returns_none() {
        let cfg = load_test();
        assert!(resolve_targets(&cfg, "no-such-model").is_none());
    }

    #[test]
    fn resolve_targets_nested_logical_model_expands() {
        let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-nested", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = write_temp(&dir, "providers.json", PROVIDERS);
        let m = write_temp(&dir, "models.json", MODELS);
        let l = write_temp(
            &dir,
            "logical.json",
            r#"{
              "logical_models": {
                "alias-a": { "route": { "strategy": "priority", "targets": [ { "model": "deepseek-official/deepseek-v4-flash" } ] } },
                "alias-b": { "route": { "strategy": "priority", "targets": [ { "model": "alias-a" }, { "model": "ark/deepseek-v4-flash" } ] } }
              }
            }"#,
        );
        let cfg = load_v2_config_from(&p, &m, &l).unwrap();
        let candidates = resolve_targets(&cfg, "alias-b").unwrap();
        assert_eq!(candidates.len(), 2, "alias-b 应展开 = alias-a 的 official + ark 物理候选");
        assert_eq!(candidates[0].model.base_url, "https://api.deepseek.com");
        assert_eq!(candidates[1].model.base_url, "https://ark.cn-beijing.volces.com/api/coding/v3");
    }

    #[test]
    fn resolve_targets_cycle_is_bounded() {
        let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-cycle", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = write_temp(&dir, "providers.json", PROVIDERS);
        let m = write_temp(&dir, "models.json", MODELS);
        let l = write_temp(
            &dir,
            "logical.json",
            r#"{
              "logical_models": {
                "alias-a": { "route": { "strategy": "priority", "targets": [ { "model": "alias-b" } ] } },
                "alias-b": { "route": { "strategy": "priority", "targets": [ { "model": "alias-a" } ] } }
              }
            }"#,
        );
        let cfg = load_v2_config_from(&p, &m, &l).unwrap();
        // 纯环：resolve 应能返回（空候选），不无限递归
        let candidates = resolve_targets(&cfg, "alias-a").unwrap();
        assert!(candidates.is_empty(), "纯环无物理候选");
    }

    #[test]
    fn rename_provider_updates_models_and_logical_references() {
        let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-rename", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let models_path = write_temp(
            &dir,
            "models.json",
            r#"{
              "models": {
                "ark/deepseek-v4-flash-260801": { "provider": "ark", "upstream_model": "deepseek-v4-flash-260801", "family": "deepseek-v4-flash" },
                "deepseek-official/deepseek-v4-flash": { "provider": "deepseek-official", "upstream_model": "deepseek-v4-flash" }
              }
            }"#,
        );
        let logical_path = write_temp(
            &dir,
            "logical.json",
            r#"{
              "logical_models": {
                "deepseek-v4-flash-auto": {
                  "route": { "strategy": "weighted", "targets": [
                    { "model": "ark/deepseek-v4-flash-260801", "weight": 8 },
                    { "model": "deepseek-official/deepseek-v4-flash", "weight": 2 }
                  ]}
                }
              }
            }"#,
        );
        rename_provider_in_models("ark-renamed", "ark", &models_path).unwrap();
        rename_provider_in_logical("ark-renamed", "ark", &logical_path).unwrap();

        let models: V2ModelsFile =
            serde_json::from_str(&fs::read_to_string(&models_path).unwrap()).unwrap();
        assert!(models.models.contains_key("ark-renamed/deepseek-v4-flash-260801"));
        assert_eq!(
            models.models["ark-renamed/deepseek-v4-flash-260801"].provider,
            "ark-renamed"
        );
        assert!(models.models.contains_key("deepseek-official/deepseek-v4-flash"));

        let logical: V2LogicalModelsFile =
            serde_json::from_str(&fs::read_to_string(&logical_path).unwrap()).unwrap();
        let targets = &logical.logical_models["deepseek-v4-flash-auto"].route.targets;
        assert_eq!(targets[0].model, "ark-renamed/deepseek-v4-flash-260801");
        assert_eq!(targets[1].model, "deepseek-official/deepseek-v4-flash");
    }
}
