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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const V2_PROVIDERS_PATH: &str = "config/providers-v2.json";
pub const V2_MODELS_PATH: &str = "config/models.json";
pub const V2_LOGICAL_MODELS_PATH: &str = "config/logical-models.json";
pub const V2_VIRTUAL_MODELS_PATH: &str = "config/virtual-models.json";

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
    /// Anthropic 兼容 API 端点（可选）。供应商同时提供 Anthropic 协议时配置，
    /// 模型能力探测优先走 Anthropic `/v1/models`（返回精确 context_window，零成本）。
    #[serde(default)]
    pub anthropic_base_url: Option<String>,
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
    /// 上游真实上下文窗口（tokens）。缺省时由 Router 推断或取保守默认值。
    #[serde(default)]
    pub context_window: Option<u32>,
    /// 上游单次最大输出 tokens。缺省时取保守默认值。
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// 是否支持图片输入（视觉模态）。
    #[serde(default)]
    pub supports_image: Option<bool>,
    /// 思考强度映射（物理粒度）：标准档位 -> 上游 wire 值（None=不支持该档位）。
    #[serde(default)]
    pub thinking_level_map: Option<HashMap<String, Option<String>>>,
    /// 思考字段协议（reasoning_effort 等），物理粒度覆写。
    #[serde(default)]
    pub thinking_format: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Family {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderModelsFile {
    pub providers: HashMap<String, ProviderModelsEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderModelsEntry {
    pub models: Vec<String>,
    #[serde(default)]
    pub fetched_at: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
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
    /// 人类可读展示名。
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2LogicalModelsFile {
    pub logical_models: HashMap<String, V2LogicalModel>,
}

/// 虚拟模型文件：虚拟名（全局抽象名）→ { 供应商 → 实际上游模型名 }。
/// 同一虚拟名可跨供应商映射，便于模型池统一不同供应商的同一模型命名。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct V2VirtualModelsFile {
    pub virtual_models: HashMap<String, HashMap<String, String>>,
}

/// 聚合后的 v2 配置视图。
#[derive(Clone, Debug, Default)]
pub struct V2Config {
    pub providers: HashMap<String, V2Provider>,
    pub models: HashMap<String, V2PhysicalModel>,
    pub logical_models: HashMap<String, V2LogicalModel>,
    /// 虚拟名 → { 供应商 → 上游模型名 }
    pub virtual_models: HashMap<String, HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// 加载
// ---------------------------------------------------------------------------
