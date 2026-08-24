//! v2 分层配置：Provider / PhysicalModel / ModelFamily / LogicalModel / Key。
//!
//! 架构目标见 `docs/architecture-v2.md`。拆分说明：
//! - `types.rs`：数据模型 + 路径常量
//! - `io.rs`：配置文件读写
//! - `mutate.rs`：物理模型自动注册 / 供应商改名同步
//! - `validate.rs`：配置校验
//! - `fold.rs`：逻辑模型折叠为旧 ModelAlias 适配
//! - `resolve.rs`：路由目标解析（逻辑模型 → 物理候选）
//! - `tests.rs`：共同测试

mod fold;
pub(crate) mod io;
mod mutate;
pub(crate) mod resolve;
#[cfg(test)]
mod tests;
mod types;
mod validate;

pub use fold::fold_to_aliases;
#[cfg(test)]
pub use io::load_v2_config_from;
pub use io::{
    load_logical_models_file, load_models_file, load_provider_models_file, load_providers_file,
    load_v2_config, load_virtual_models_file, migrate_legacy_logical_caps,
    write_logical_models_file, write_models_file, write_provider_models_file, write_providers_file,
    write_virtual_models_file,
};
pub use mutate::{register_physical_models, rename_provider_in_logical, rename_provider_in_models};
pub use resolve::{resolve_targets, TargetCandidate};
pub use types::{
    ProviderModelsEntry, V2Config, V2Key, V2LogicalModel, V2PhysicalModel, V2Provider, V2Route,
    V2Strategy, V2Target, V2_LOGICAL_MODELS_PATH, V2_MODELS_PATH, V2_PROVIDERS_PATH,
    V2_VIRTUAL_MODELS_PATH,
};
pub use validate::is_provider_scoped_virtual;
