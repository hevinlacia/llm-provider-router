//! 透传流的零输出守卫（zero-output guard）。
//!
//! 背景：Ark 等上游网关在容量异常时会收下请求、发出 `response.created` 后
//! 长时间零产出，最后以 `response.incomplete(incomplete_details.reason =
//! "network_error")` 终止（实测约 180s）。旧的透传逻辑逐 chunk 原样转发，
//! 客户端（pi-ai）直接看到 `Response incomplete: network_error` 报错，
//! 且流已提交无法换 key 重试。
//!
//! 守卫策略：首个内容事件到达前只缓冲不转发。
//! - 缓冲期收到可重试的异常终止（`response.incomplete` 且 reason 不是
//!   `max_output_tokens` / `content_filter`）→ 客户端尚无任何字节，
//!   通知调用方丢弃缓冲、换 key/target 静默重试；
//! - 收到首个内容事件（delta / output_item / …）→ 把缓冲整体冲刷给客户端，
//!   之后转回逐 chunk 原样转发（流已提交，不可重试）。
//!
//! 仅按行解析 `data:` JSON 的 `type` 字段做分类，不改写任何字节；
//! 无法识别的载荷一律按"已提交"处理（宁可透传，不误重试）。

use serde_json::Value;

/// 单个 SSE 行的分类结果。
enum LineClass {
    /// 元事件（response.created / in_progress / ping 等）：继续缓冲。
    Meta,
    /// 内容或语义性终止：流对客户端已提交。
    Content,
    /// 零输出可重试终止，附带终止描述（incomplete reason）。
    Retry(String),
}

/// feed() 对单个上游 chunk 的转发决策。
#[derive(Debug)]
pub(crate) enum FeedAction {
    /// 把 `prefix`（此前缓冲的字节）接当前 chunk 一起转发；
    /// 守卫结束后 prefix 恒为空。
    Forward { prefix: Vec<u8> },
    /// 守卫期：已缓冲，暂不转发。
    Buffered,
    /// 零输出可重试终止：不向客户端发出任何字节，可换 key/target 重试。
    RetryZeroOutput(String),
}

/// incomplete reason 是否为语义性结果（重试无意义，需原样透传）。
fn is_semantic_incomplete_reason(reason: &str) -> bool {
    matches!(reason, "max_output_tokens" | "content_filter")
}

pub(crate) struct PassthroughGuard {
    flushed: bool,
    buffer: Vec<u8>,
    line: Vec<u8>,
}

impl PassthroughGuard {
    pub(crate) fn new() -> Self {
        Self {
            flushed: false,
            buffer: Vec::new(),
            line: Vec::new(),
        }
    }

    /// 是否已开始向客户端转发（流已提交）。仅测试断言用。
    #[cfg(test)]
    pub(crate) fn is_flushed(&self) -> bool {
        self.flushed
    }

    /// 喂入一个上游 chunk，返回转发决策。
    ///
    /// 跨 chunk 的 SSE 行由内部行缓冲拼接；冲刷后本守卫退化为直通。
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> FeedAction {
        if self.flushed {
            return FeedAction::Forward { prefix: Vec::new() };
        }
        // 全量缓冲：冲刷时按序整体交给客户端（含当前 chunk 冲刷行之后的部分）。
        self.buffer.extend_from_slice(chunk);
        let mut pos = 0;
        while let Some(rel) = chunk[pos..].iter().position(|&b| b == b'\n') {
            let line_end = pos + rel;
            self.line.extend_from_slice(&chunk[pos..=line_end]);
            pos = line_end + 1;
            let line = std::mem::take(&mut self.line);
            match Self::classify_line(&line) {
                LineClass::Meta => {}
                LineClass::Content => {
                    self.flushed = true;
                    let prefix = std::mem::take(&mut self.buffer);
                    return FeedAction::Forward { prefix };
                }
                LineClass::Retry(reason) => return FeedAction::RetryZeroOutput(reason),
            }
        }
        self.line.extend_from_slice(&chunk[pos..]);
        FeedAction::Buffered
    }

    fn classify_line(line: &[u8]) -> LineClass {
        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim();
        // event:/id:/注释/空行不参与分类；分类只看 data 载荷的 type 字段。
        let Some(data) = trimmed.strip_prefix("data:") else {
            return LineClass::Meta;
        };
        let body = data.trim();
        if body == "[DONE]" {
            return LineClass::Content;
        }
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            return LineClass::Content;
        };
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            return LineClass::Content;
        };
        match event_type {
            "response.created" | "response.in_progress" | "ping" => LineClass::Meta,
            "response.incomplete" => {
                // reason 在 response.incomplete_details 下（OpenAI/Ark 实际形状）；
                // 兼容少数网关的顶层扁平写法。
                let reason = value["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .or_else(|| value["incomplete_details"]["reason"].as_str())
                    .unwrap_or_default();
                if is_semantic_incomplete_reason(reason) {
                    return LineClass::Content;
                }
                let desc = if reason.is_empty() {
                    "incomplete without reason".to_string()
                } else {
                    reason.to_string()
                };
                LineClass::Retry(desc)
            }
            _ => LineClass::Content,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(guard: &mut PassthroughGuard, chunks: &[&str]) -> FeedAction {
        let mut last = FeedAction::Buffered;
        for c in chunks {
            last = guard.feed(c.as_bytes());
        }
        last
    }

    fn forward_prefix(action: &FeedAction) -> String {
        match action {
            FeedAction::Forward { prefix } => String::from_utf8_lossy(prefix).to_string(),
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn zero_output_network_error_is_retryable() {
        let mut g = PassthroughGuard::new();
        let action = feed_all(
            &mut g,
            &[
                "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
                "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"network_error\"}}}\n\n",
            ],
        );
        match action {
            FeedAction::RetryZeroOutput(reason) => assert_eq!(reason, "network_error"),
            other => panic!("expected RetryZeroOutput, got {other:?}"),
        }
        assert!(!g.is_flushed());
    }

    #[test]
    fn terminal_split_across_chunks_still_detected() {
        let mut g = PassthroughGuard::new();
        let action = feed_all(
            &mut g,
            &[
                "data: {\"type\":\"response.created\"}\n\n",
                "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"net",
                "work_error\"}}}\n\n",
            ],
        );
        assert!(matches!(action, FeedAction::RetryZeroOutput(_)));
    }

    #[test]
    fn content_event_flushes_buffer_and_commits() {
        let mut g = PassthroughGuard::new();
        // 第一段：只有 created → 缓冲
        assert!(matches!(
            g.feed(b"data: {\"type\":\"response.created\"}\n\n"),
            FeedAction::Buffered
        ));
        // 第二段：内容事件 → 冲刷缓冲（含本 chunk）
        let action = g.feed(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"he\"}\n\n",
        );
        let prefix = forward_prefix(&action);
        assert!(prefix.contains("response.created"));
        assert!(prefix.contains("output_text.delta"));
        assert!(g.is_flushed());
        // 提交后：后续 chunk 原样直通
        let action =
            g.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n\n");
        assert!(matches!(action, FeedAction::Forward { prefix } if prefix.is_empty()));
        // 提交后的 incomplete 原样放行（不可重试）
        let action = g.feed(b"data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"network_error\"}}}\n\n");
        assert!(matches!(action, FeedAction::Forward { prefix } if prefix.is_empty()));
    }

    #[test]
    fn semantic_incomplete_reason_not_retryable() {
        for reason in ["max_output_tokens", "content_filter"] {
            let mut g = PassthroughGuard::new();
            let body = format!(
                "data: {{\"type\":\"response.created\"}}\n\ndata: {{\"type\":\"response.incomplete\",\"response\":{{\"incomplete_details\":{{\"reason\":\"{reason}\"}}}}}}\n\n"
            );
            let action = g.feed(body.as_bytes());
            let prefix = forward_prefix(&action);
            assert!(prefix.contains(reason));
        }
    }

    #[test]
    fn completed_with_zero_output_is_forwarded_not_retried() {
        let mut g = PassthroughGuard::new();
        assert!(matches!(
            g.feed(b"data: {\"type\":\"response.created\"}\n\n"),
            FeedAction::Buffered
        ));
        // completed 触发冲刷：prefix 含 created + completed
        let action = g.feed(
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        let prefix = forward_prefix(&action);
        assert!(prefix.contains("response.created"));
        assert!(prefix.contains("response.completed"));
        // 后续 [DONE] 直通
        let action = g.feed(b"data: [DONE]\n\n");
        assert!(matches!(action, FeedAction::Forward { prefix } if prefix.is_empty()));
    }

    #[test]
    fn incomplete_without_reason_is_retryable() {
        let mut g = PassthroughGuard::new();
        let action = feed_all(
            &mut g,
            &[
                "data: {\"type\":\"response.created\"}\n\n",
                "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}\n\n",
            ],
        );
        match action {
            FeedAction::RetryZeroOutput(reason) => assert_eq!(reason, "incomplete without reason"),
            other => panic!("expected RetryZeroOutput, got {other:?}"),
        }
    }

    #[test]
    fn ping_and_comments_stay_buffered() {
        let mut g = PassthroughGuard::new();
        assert!(matches!(
            feed_all(
                &mut g,
                &[
                    ": ping\n\n",
                    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n"
                ]
            ),
            FeedAction::Buffered
        ));
    }

    #[test]
    fn unparseable_data_line_commits_stream() {
        let mut g = PassthroughGuard::new();
        let action = g.feed(b"data: not-json\n\ndata: {\"type\":\"response.created\"}\n\n");
        let prefix = forward_prefix(&action);
        assert!(prefix.contains("not-json"));
    }
}
