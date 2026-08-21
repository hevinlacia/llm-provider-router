//! Chat completions 代理主链路。
//!
//! - `payload.rs`：上游载荷归一化（纯函数）
//! - `upstream.rs`：非流式调用（选 key / 重试 / 冻结 / 用量）
//! - `stream.rs`：流式 SSE 转发
//! - `select.rs`：key 选择 / 冻结 / 用量记录辅助

pub mod payload;
pub mod select;
pub mod stream;
pub mod upstream;
