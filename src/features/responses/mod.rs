//! Responses API 代理子模块。
//!
//! 路由器对外新增 `POST /v1/responses`（最新 OpenAI Responses API），内部翻译成
//! chat completions 走现有路由/选 key/重试/冻结/用量链路，响应再翻译回 Responses 格式。
//!
//! - `translate.rs`：请求/响应/错误双向翻译（纯函数）
//! - `stream.rs`：流式 SSE 翻译状态机
//! - `store.rs`：previous_response_id 内存多轮环

pub mod store;
pub mod stream;
pub mod translate;
