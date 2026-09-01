//! Anthropic Messages API（`/v1/messages`）对外入口的协议翻译层。
//!
//! Router 对外暴露两种 API：Anthropic API 与 Responses API。Chat Completions 是
//! 内部翻译语言。本模块负责 Anthropic 协议与内部协议之间的转换：
//!
//! - **透传**：供应商配置了 `anthropic_base_url`（原生支持 Anthropic 协议）时，
//!   `/v1/messages` 请求只改写 model 名后透传到 `{anthropic_base_url}/v1/messages`，
//!   响应/SSE 字节原样返回；
//! - **翻译**：未配置时，把 Anthropic 请求翻译成 Responses 请求，走现有
//!   `/v1/responses` 机制（内部再透传到供应商 Responses 端点或翻译成 chat completions），
//!   响应再翻译回 Anthropic 格式。
//!
//! 字段映射（Anthropic Messages -> Responses）：
//! - `system`（string / text blocks）-> `instructions`
//! - `messages`（string / content blocks）-> `input`（`input_text` / `output_text` /
//!   `input_image` 项 + `function_call` / `function_call_output` 独立项；
//!   `thinking` / `redacted_thinking` 丢弃——上游推理由上游重新生成）
//! - `max_tokens` -> `max_output_tokens`；`tools[].input_schema` -> `tools[].parameters`
//! - `tool_choice`（auto / any / tool）-> `auto` / `required` / `{type:function,name}`
//! - `thinking.budget_tokens` -> `reasoning.effort`（>=32k high / >=8k medium / 其余 low）
//! - `stop_sequences`：Responses API 无对应字段，静默丢弃
//!
//! 响应映射（Responses -> Anthropic）：
//! - `output` 项：`message.output_text` -> `text` 块、`reasoning.summary` -> `thinking` 块
//!   （signature 置空，翻译模式无真实签名；回传时被输入翻译丢弃）、
//!   `function_call` -> `tool_use` 块
//! - `status`：`incomplete` -> `stop_reason: max_tokens`；含 `tool_use` -> `tool_use`；
//!   其余 -> `end_turn`
//! - `usage`：`input_tokens/output_tokens/cached_tokens` -> Anthropic usage 字段

pub(crate) mod stream;
pub(crate) mod translate;

/// Anthropic 错误响应体：`{"type":"error","error":{"type":...,"message":...}}`。
pub(crate) fn error_body(message: &str, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "error": { "type": error_type, "message": message }
    })
}

/// OpenAI 风格错误类型 -> Anthropic 错误类型（就近映射，未知归 api_error）。
pub(crate) fn map_error_type(openai_type: &str) -> &'static str {
    match openai_type {
        "invalid_request_error" => "invalid_request_error",
        "authentication_error" => "authentication_error",
        "permission_error" | "forbidden" => "permission_error",
        "not_found_error" => "not_found_error",
        "rate_limit_error" | "rate_limit_exceeded" => "rate_limit_error",
        "overloaded_error" => "overloaded_error",
        "all_keys_frozen" => "rate_limit_error",
        _ => "api_error",
    }
}

/// Anthropic usage（input_tokens/output_tokens）归一化成内部统计字段
/// （usage_store 读 `prompt_tokens` / `completion_tokens`）。
pub(crate) fn normalize_usage(usage: &serde_json::Value) -> serde_json::Value {
    let input = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            usage
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            usage
                .get("completion_tokens")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0);
    serde_json::json!({ "prompt_tokens": input, "completion_tokens": output })
}
