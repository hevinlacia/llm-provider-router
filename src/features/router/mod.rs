//! 路由核心：RouterState（会话粘性 / 限额冻结 / 用量均衡）+ 选路排序 + 计费。
//!
//! 拆分说明（行为保持，v2.1 结构整理）：
//! - `state/`：RouterState 定义与核心逻辑（含字段私有访问的多个 impl 块）
//!   - `state/mod.rs`   生命周期、冻结、绑定、选 key、快照
//!   - `state/config.rs` 权重/供应商/价格/等价组/自定义别名配置读写
//!   - `state/v2.rs`     v2 分层配置（providers/logical/virtual）编辑与视图
//!   - `state/routing.rs` route_aliases：请求模型名展开为物理候选
//!   - `state/keys.rs`   key 引用、已知名集合、v2 物理引用推导、rebind 清理
//! - `selection.rs` 排序/加权采样/自定义 key 名归一（纯函数）
//! - `freeze.rs`    配额/鉴权失败/retry-after 解析与冻结判定（纯函数）
//! - `costing.rs`    用量快照计费（纯函数）
//! - `util.rs`       权重名校验、排序 join（纯函数）
//! - `tests.rs`      RouterState/选路/解析共同测试

pub(crate) mod costing;
pub(crate) mod freeze;
pub(crate) mod probe;
pub(crate) mod selection;
pub(crate) mod state;
#[cfg(test)]
mod tests;
pub(crate) mod util;

pub use freeze::maybe_freeze_key;
pub(crate) use state::config::PhysicalModelPatch;
pub use state::{NoAvailableKeyError, RouterState};
