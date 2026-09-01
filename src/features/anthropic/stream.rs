//! Responses SSE 字节流 -> Anthropic SSE 字节流的流式翻译器。
//!
//! [`SseTranslator`] 以 chunk 为单位喂入 Responses SSE 原始字节（跨 chunk 的
//! 半行由内部缓冲），输出对应的 Anthropic SSE 字节文本：
//! - `response.created` -> `message_start`
//! - `output_item.added` -> `content_block_start`（message/reasoning/function_call
//!   项分别映射 text/thinking/tool_use 块）
//! - `output_text.delta` / `reasoning_summary_text.delta` / `function_call_arguments.delta`
//!   -> `content_block_delta`（text_delta / thinking_delta / input_json_delta）
//! - `output_item.done` -> `content_block_stop`
//! - `response.completed` -> `message_delta`（stop_reason + usage）+ `message_stop`
//! - `response.incomplete` -> `message_delta`（stop_reason=max_tokens）+ `message_stop`
//! - 错误事件（无 type 字段的 `{code,message}`）-> Anthropic `error` 事件
//! - `data: [DONE]` 忽略（Anthropic 以 `message_stop` 结束）

use super::translate::{translate_stream_event, StreamEventState};

pub(crate) struct SseTranslator {
    state: StreamEventState,
    line_buf: String,
}

impl SseTranslator {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            state: StreamEventState::new(model),
            line_buf: String::new(),
        }
    }

    /// 喂入一个 Responses SSE 字节 chunk，返回翻译后的 Anthropic SSE 文本。
    pub(crate) fn feed(&mut self, chunk: &str) -> String {
        self.line_buf.push_str(chunk);
        let mut out = String::new();
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].trim().to_string();
            self.line_buf.drain(..=pos);
            out.push_str(&self.handle_line(&line));
        }
        out
    }

    /// 流结束：冲洗残余缓冲行（理论上 Responses SSE 以换行结尾，此为兜底）。
    pub(crate) fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.line_buf);
        self.handle_line(rest.trim())
    }

    fn handle_line(&mut self, line: &str) -> String {
        let Some(data) = line.strip_prefix("data:") else {
            return String::new();
        };
        let body = data.trim();
        if body.is_empty() || body == "[DONE]" {
            return String::new();
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(body) else {
            return String::new();
        };
        translate_stream_event(&event, &mut self.state).concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_data(event: &serde_json::Value) -> String {
        format!("data: {}\n\n", serde_json::to_string(event).unwrap())
    }

    #[test]
    fn translates_chunked_stream_including_split_lines() {
        let mut t = SseTranslator::new("my-model");
        let created = sse_data(&serde_json::json!({
            "type": "response.created", "response": { "id": "resp_1" }
        }));
        let added = sse_data(&serde_json::json!({
            "type": "response.output_item.added",
            "item": { "type": "message", "id": "item_1" }
        }));
        let delta = sse_data(&serde_json::json!({
            "type": "response.output_text.delta", "item_id": "item_1", "delta": "hello"
        }));
        let completed = sse_data(&serde_json::json!({
            "type": "response.completed",
            "response": { "id": "resp_1", "status": "completed", "usage": { "output_tokens": 9 } }
        }));

        // 故意把 chunk 切在事件中间，验证跨 chunk 缓冲
        let combined = format!("{created}{added}{delta}{completed}");
        let mid = combined.len() / 2;
        let out1 = t.feed(&combined[..mid]);
        let out2 = t.feed(&combined[mid..]);
        let out = format!("{out1}{out2}{}", t.finish());

        assert!(out.contains("event: message_start"));
        assert!(out.contains("\"model\":\"my-model\""));
        assert!(out.contains("event: content_block_start"));
        assert!(out.contains("\"text\":\"hello\""));
        assert!(out.contains("event: content_block_stop"));
        assert!(out.contains("\"stop_reason\":\"end_turn\""));
        assert!(out.contains("\"output_tokens\":9"));
        assert!(out.contains("event: message_stop"));
        assert!(!out.contains("data: [DONE]"));
    }

    #[test]
    fn ignores_done_and_unknown_lines() {
        let mut t = SseTranslator::new("m");
        assert_eq!(t.feed("data: [DONE]\n\n"), "");
        assert_eq!(t.feed(": comment\n\n"), "");
        assert_eq!(t.feed("event: ping\ndata: {\"type\":\"ignored\"}\n\n"), "");
    }
}
