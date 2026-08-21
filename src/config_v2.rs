//! v2 分层配置入口（兼容层）。
//!
//! 实现已迁移到 `config::v2/`（types / io / mutate / validate / fold / resolve），
//! 本文件仅 re-export，保持 `crate::config_v2::*` 旧路径兼容，避免改动全部调用点。

pub use crate::config::v2::*;
