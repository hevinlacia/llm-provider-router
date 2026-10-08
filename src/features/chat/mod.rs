//! 代理主链路共享设施（chat 客户端协议已于 2026-10 下线，模块保留共享辅助）。
//!
//! - `payload.rs`：上游载荷归一化（responses/messages 翻译层复用）
//! - `select.rs`：key 选择 / 记账 / 成功路径辅助
//!
//! 失败分类与动作统一在 `features/router/failure`。

pub mod payload;
pub mod select;
