//! Responses SSE 事件 -> Anthropic SSE 事件翻译（逐事件映射的纯函数部分）。

use serde_json::{json, Value};

use crate::features::anthropic::error_body;

// ---------------------------------------------------------------------------
// 流式：Responses SSE 事件 -> Anthropic SSE 事件（逐事件映射的纯函数部分）
// ---------------------------------------------------------------------------

/// 单个 Responses SSE 事件 -> 0..n 个 Anthropic SSE 事件（`event:` + `data:` 文本）。
/// 状态（块 index 映射等）由 `super::stream::SseTranslator` 维护。
pub(crate) fn translate_stream_event(event: &Value, state: &mut StreamEventState) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let typ = event.get("type").and_then(Value::as_str).unwrap_or("");
    // responses stream 的错误事件：{"code":..., "message":..., "param":...}（无 type 字段）
    if typ.is_empty() {
        if let Some(message) = event.get("message").and_then(Value::as_str) {
            out.push(anthropic_error_event(message));
        }
        return out;
    }
    match typ {
        "response.created" => {
            let id = event
                .pointer("/response/id")
                .and_then(Value::as_str)
                .unwrap_or("msg_unknown")
                .to_string();
            let input_tokens = event
                .pointer("/response/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            state.message_id = id;
            state.started = true;
            out.push(sse_event(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": state.message_id,
                        "type": "message",
                        "role": "assistant",
                        "model": state.model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {
                            "input_tokens": input_tokens,
                            "output_tokens": 1,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                        }
                    }
                }),
            ));
        }
        "response.output_item.added" => {
            let item = event.get("item").cloned().unwrap_or(json!({}));
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let index = state.next_index;
            let block = match item_type {
                "message" => json!({ "type": "text", "text": "" }),
                "reasoning" => json!({ "type": "thinking", "thinking": "", "signature": "" }),
                "function_call" => {
                    state.has_tool_use = true;
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("id").and_then(Value::as_str))
                        .unwrap_or_default();
                    json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "input": {},
                    })
                }
                _ => return out,
            };
            state.next_index += 1;
            state.item_index.insert(item_id, index);
            state.open_blocks.insert(index);
            out.push(sse_event(
                "content_block_start",
                json!({ "type": "content_block_start", "index": index, "content_block": block }),
            ));
        }
        "response.output_text.delta" => {
            let Some((index, pending)) = state.block_index(event, "text") else {
                return out;
            };
            if let Some(start) = pending {
                out.push(start);
            }
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": delta },
                    }),
                ));
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let Some((index, pending)) = state.block_index(event, "thinking") else {
                return out;
            };
            if let Some(start) = pending {
                out.push(start);
            }
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "thinking_delta", "thinking": delta },
                    }),
                ));
            }
        }
        "response.function_call_arguments.delta" => {
            let Some((index, pending)) = state.block_index(event, "tool_use") else {
                return out;
            };
            if let Some(start) = pending {
                out.push(start);
            }
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                out.push(sse_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": delta },
                    }),
                ));
            }
        }
        "response.output_item.done" => {
            let item_id = event
                .pointer("/item/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(index) = state.item_index.remove(&item_id) {
                state.open_blocks.remove(&index);
                out.push(sse_event(
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": index }),
                ));
            }
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            // 兜底关闭仍未 stop 的块（上游异常结束时可能缺 output_item.done）
            let mut open: Vec<usize> = state.open_blocks.iter().copied().collect();
            open.sort_unstable();
            for index in open {
                out.push(sse_event(
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": index }),
                ));
            }
            state.open_blocks.clear();
            if !state.message_stopped {
                state.message_stopped = true;
                let response = event.get("response").cloned().unwrap_or(json!({}));
                let stop_reason = if typ == "response.incomplete" {
                    "max_tokens"
                } else if typ == "response.failed" {
                    "end_turn"
                } else if state.has_tool_use {
                    "tool_use"
                } else {
                    "end_turn"
                };
                let output_tokens = response
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push(sse_event(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                        "usage": { "output_tokens": output_tokens },
                    }),
                ));
                out.push(sse_event("message_stop", json!({ "type": "message_stop" })));
            }
        }
        _ => {}
    }
    out
}

/// 流式翻译状态：Anthropic 块 index 分配与生命周期。
#[derive(Debug, Default)]
pub(crate) struct StreamEventState {
    pub(crate) message_id: String,
    pub(crate) model: String,
    pub(crate) started: bool,
    pub(crate) message_stopped: bool,
    pub(crate) next_index: usize,
    /// Responses item id -> Anthropic block index
    pub(crate) item_index: std::collections::HashMap<String, usize>,
    /// 已 start 未 stop 的块 index
    pub(crate) open_blocks: std::collections::HashSet<usize>,
    pub(crate) has_tool_use: bool,
}

impl StreamEventState {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            ..Default::default()
        }
    }

    /// 按 delta 事件定位块 index；块未 start 时惰性补建并返回待发的事件文本。
    fn block_index(&mut self, event: &Value, block_type: &str) -> Option<(usize, Option<String>)> {
        let item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(index) = self.item_index.get(&item_id) {
            return Some((*index, None));
        }
        // 惰性补建：item.added 事件缺失时按 delta 隐式开块，start 事件随 delta 一起补发
        let index = self.next_index;
        self.next_index += 1;
        self.item_index.insert(item_id.clone(), index);
        self.open_blocks.insert(index);
        let block = match block_type {
            "text" => json!({ "type": "text", "text": "" }),
            "thinking" => json!({ "type": "thinking", "thinking": "", "signature": "" }),
            _ => json!({ "type": "tool_use", "id": item_id, "name": "", "input": {} }),
        };
        let start = sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": block,
            }),
        );
        Some((index, Some(start)))
    }
}

fn sse_event(event: &str, data: Value) -> String {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string())
    )
}

fn anthropic_error_event(message: &str) -> String {
    sse_event("error", error_body(message, "api_error"))
}
