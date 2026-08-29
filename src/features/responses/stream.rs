//! Responses API 流式代理：按供应商配置决定透传或翻译。
//!
//! - **透传**（供应商配置了 `responses_base_url`，原生支持 Responses）：上游 SSE 字节原样转发。
//! - **翻译**（供应商只有 `base_url`）：把上游 chat completions SSE 翻译成 Responses API SSE 事件。
//!
//! 翻译事件序列（对照 openai SDK `ResponseStreamEvent`）：
//!   response.created
//!   -> response.output_item.added（message / reasoning / function_call 项）
//!   -> response.content_part.added / response.reasoning_summary_part.added
//!   -> response.output_text.delta / response.reasoning_summary_text.delta
//!      / response.function_call_arguments.delta
//!   -> 结束时 response.output_text.done / response.content_part.done
//!      / response.reasoning_summary_text.done / response.reasoning_summary_part.done
//!      / response.reasoning.done / response.function_call_arguments.done
//!      / response.output_item.done（每项）
//!   -> response.completed（带完整 output 与 usage）-> data: [DONE]
//!
//! 选 key / 重试 / 冻结 / 用量记录与 chat 流式主链路一致（复用 select 辅助）。

use crate::app::AppState;
use crate::config::ModelAlias;
use crate::features::chat::payload::{log_upstream_failure, prepare_upstream_payload};
use crate::features::chat::select::{
    alias_with_runtime_weights_locked, extract_usage_from_stream, freeze_maybe, record_usage,
    select_key_locked, upstream_key_value_locked, usage_key_name,
};
use crate::features::responses::store;
use crate::features::responses::translate;
use crate::routes::resp::internal_error;
use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashSet;

pub(crate) async fn stream_responses_route(
    app: AppState,
    aliases: Vec<ModelAlias>,
    session_id: Option<String>,
    original_payload: Value,
    chat_payload: Option<Value>,
) -> Response {
    // 流式响应头取首选候选的保守窗口提示（与 chat 流式一致）
    let stream_headers: Option<(String, String, String, Option<u32>, Option<u32>)> =
        aliases.first().map(|first| {
            (
                first.alias.clone(),
                first.provider(),
                first.upstream_model(),
                first.context_window,
                first.max_output_tokens,
            )
        });

    let stream = async_stream::stream! {
        let mut last_error: Option<String> = None;
        let mut total_tried: usize = 0;
        let mut failed_alias: String = aliases.first().map(|a| a.alias.clone()).unwrap_or_else(|| "router".to_string());
        for base_alias in aliases {
            let alias = match alias_with_runtime_weights_locked(&app, &base_alias) {
                Ok(alias) => alias,
                Err(message) => {
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(sse_error_event("router", 0, &message)));
                    return;
                }
            };
            let is_passthrough = alias.supports_responses();
            // 空地址防护：所选模式对应的供应商地址未配置时给出明确错误，而不是发向坏 URL
            let endpoint_missing = if is_passthrough {
                alias
                    .responses_base_url
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
            } else {
                alias.base_url.trim().is_empty()
            };
            if endpoint_missing {
                yield Ok(Bytes::from(sse_error_message(if is_passthrough {
                    "provider responses_base_url is empty; cannot pass through /v1/responses"
                } else {
                    "provider has no chat completions base_url configured; cannot translate /v1/responses"
                })));
                return;
            }
            let (endpoint, upstream_payload) = if is_passthrough {
                // 透传：只改写 model 名，原样发到供应商 Responses 端点
                let responses_base = alias
                    .responses_base_url
                    .clone()
                    .unwrap_or_default();
                (
                    format!("{}/responses", responses_base.trim_end_matches('/')),
                    translate::prepare_passthrough_payload(&original_payload, &alias),
                )
            } else {
                // 翻译：Responses 请求已转成 chat 载荷，发到 chat 端点
                let chat = chat_payload
                    .as_ref()
                    .expect("translate-mode stream requires chat_payload")
                    .clone();
                (
                    format!("{}/chat/completions", alias.base_url.trim_end_matches('/')),
                    prepare_upstream_payload(&chat, &alias),
                )
            };
            let upstream_model = alias.upstream_model();
            let mut tried = HashSet::new();
            let retry_policy = alias.retry_policy.clone();

            loop {
                let selected_key = match select_key_locked(&app, &alias, session_id.as_deref(), &tried) {
                    Ok(result) => result,
                    Err(message) => {
                        yield Ok(Bytes::from(sse_error_event(&alias.alias, tried.len(), &message)));
                        return;
                    }
                };
                let key = match selected_key {
                    Ok(key) => key,
                    Err(_) => break, // key 全冻结/不可用：fallback 下一个 target
                };
                tried.insert(key.name.clone());
                total_tried += 1;
                failed_alias = alias.alias.clone();
                let key_value = match upstream_key_value_locked(&app, &key) {
                    Ok(value) => value,
                    Err(message) => {
                        yield Ok(Bytes::from(sse_error_event(&alias.alias, tried.len(), &message)));
                        return;
                    }
                };
                let Some(key_value) = key_value else {
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
                    last_error = Some(format!("missing key value for {}", usage_key_name(&app, &key)));
                    continue;
                };

                let mut response: Option<reqwest::Response> = None;
                let mut last_exc: Option<String> = None;
                for attempt in 0..2 {
                    match app
                        .client
                        .post(&endpoint)
                        .bearer_auth(key_value.clone())
                        .header(CONTENT_TYPE, "application/json")
                        .json(&upstream_payload)
                        .send()
                        .await
                    {
                        Ok(resp) => {
                            response = Some(resp);
                            break;
                        }
                        Err(exc) => {
                            let retryable = exc.is_connect() || exc.is_timeout() || exc.is_request();
                            last_exc = Some(exc.to_string());
                            if attempt == 0 && retryable {
                                continue;
                            }
                            break;
                        }
                    }
                }
                let response = match response {
                    Some(r) => r,
                    None => {
                        record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), 599, None);
                        last_error = Some(last_exc.unwrap_or_else(|| "upstream connect error".to_string()));
                        continue;
                    }
                };

                let status = response.status().as_u16();
                let headers = response.headers().clone();
                if retry_policy.as_ref().is_some_and(|p| p.retry_on_status.contains(&status)) {
                    let body_text = response.text().await.unwrap_or_default();
                    freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                    let usage = extract_usage_from_stream(&body_text)
                        .or_else(|| serde_json::from_str::<Value>(&body_text).ok().and_then(|v| v.get("usage").filter(|u| u.is_object()).cloned()));
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref());
                    log_upstream_failure(&alias, status, &body_text);
                    continue;
                }

                if status >= 400 {
                    let body_text = response.text().await.unwrap_or_default();
                    freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                    record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, None);
                    log_upstream_failure(&alias, status, &body_text);
                    // 转成 Responses SSE error 事件
                    let err = translate::upstream_error_to_responses(&body_text);
                    let message = err.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("upstream error");
                    yield Ok(Bytes::from(sse_error_message(message)));
                    return;
                }

                // 正常流：按模式处理（透传原样转发 / 翻译成 Responses 事件）
                let mut bytes_stream = response.bytes_stream();
                let mut body_text = Vec::new();
                if is_passthrough {
                    // 透传：字节原样转发（上游已是 Responses SSE），仅收集 usage
                    while let Some(item) = bytes_stream.next().await {
                        match item {
                            Ok(chunk) => {
                                body_text.extend_from_slice(&chunk);
                                yield Ok(chunk);
                            }
                            Err(exc) => {
                                yield Ok(Bytes::from(sse_error_event(&alias.alias, tried.len(), &exc.to_string())));
                                return;
                            }
                        }
                    }
                } else {
                    // 翻译：chat SSE -> Responses 事件
                    let echo = translate::response_echo_fields(&original_payload);
                    let mut sse = ResponsesSse::with_echo(&upstream_model, &echo);
                    while let Some(item) = bytes_stream.next().await {
                        match item {
                            Ok(chunk) => {
                                body_text.extend_from_slice(&chunk);
                                let text = String::from_utf8_lossy(&chunk);
                                for line in text.lines() {
                                    let trimmed = line.trim();
                                    let Some(data) = trimmed.strip_prefix("data:") else { continue };
                                    let body = data.trim();
                                    if body == "[DONE]" {
                                        continue;
                                    }
                                    if let Ok(value) = serde_json::from_str::<Value>(body) {
                                        let events = sse.feed(&value);
                                        if !events.is_empty() {
                                            yield Ok(Bytes::from(events));
                                        }
                                    }
                                }
                            }
                            Err(exc) => {
                                yield Ok(Bytes::from(sse_error_event(&alias.alias, tried.len(), &exc.to_string())));
                                return;
                            }
                        }
                    }
                    let (events, (resp_id, history, response)) = sse.finish();
                    if !events.is_empty() {
                        yield Ok(Bytes::from(events));
                    }
                    let input_items = translate::extract_input_items(&original_payload);
                    store::put_full(&resp_id, history, response, input_items);
                }
                let body_text = String::from_utf8_lossy(&body_text).to_string();
                freeze_maybe(&app.state, &key, status, &headers, &body_text, &app.settings);
                let usage = extract_usage_from_stream(&body_text)
                    .or_else(|| extract_responses_usage_from_stream(&body_text));
                record_usage(&app.state, &alias.alias, &usage_key_name(&app, &key), status, usage.as_ref());
                log_upstream_failure(&alias, status, &body_text);
                return;
            }
        }
        if let Some(error) = last_error {
            let shown = if total_tried > 0 { total_tried } else { 1 };
            let alias = if total_tried > 0 { failed_alias } else { "router".to_string() };
            yield Ok(Bytes::from(sse_error_event(&alias, shown, &error)));
        }
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream");
    if let Some((alias, provider, upstream, context_window, max_output)) = stream_headers {
        if let Ok(hv) = HeaderValue::from_str(&alias) {
            builder = builder.header("x-llm-router-model", hv);
        }
        if let Ok(hv) = HeaderValue::from_str(&provider) {
            builder = builder.header("x-llm-router-provider", hv);
        }
        if let Ok(hv) = HeaderValue::from_str(&upstream) {
            builder = builder.header("x-llm-router-upstream-model", hv);
        }
        if let Some(v) = context_window {
            if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
                builder = builder.header("x-llm-router-context-window", hv);
            }
        }
        if let Some(v) = max_output {
            if let Ok(hv) = HeaderValue::from_str(&v.to_string()) {
                builder = builder.header("x-llm-router-max-output", hv);
            }
        }
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error("failed to create streaming response"))
}

// ---------------------------------------------------------------------------
// SSE 状态机
// ---------------------------------------------------------------------------

struct MessageState {
    item_id: String,
    text: String,
    output_index: usize,
    content_index: usize,
}

struct ReasoningState {
    item_id: String,
    text: String,
    output_index: usize,
    content_index: usize,
    summary_index: usize,
}

struct ToolState {
    index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
}

pub(crate) struct ResponsesSse {
    response_id: String,
    created_at: i64,
    model: String,
    echo: Value,
    output_index: usize,
    message: Option<MessageState>,
    reasoning: Option<ReasoningState>,
    tool_calls: Vec<ToolState>,
    usage: Option<Value>,
    emitted_created: bool,
    sequence: u64,
}

impl ResponsesSse {
    /// 带请求回显字段构造（echo 来自 translate::response_echo_fields）。
    /// 不传回显时用空对象（等价于历史 `new` 行为）。
    pub(crate) fn with_echo(model: &str, echo: &Value) -> Self {
        Self {
            response_id: translate::next_id("resp"),
            created_at: translate::now_ts(),
            model: model.to_string(),
            echo: if echo.is_null() { json!({}) } else { echo.clone() },
            output_index: 0,
            message: None,
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            emitted_created: false,
            sequence: 0,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    /// 处理一个上游 chat chunk，返回要下发的 Responses SSE 文本（可能为空）。
    pub(crate) fn feed(&mut self, chunk: &Value) -> String {
        let mut out = String::new();
        if !self.emitted_created {
            self.emitted_created = true;
            out.push_str(&sse_event(
                "response.created",
                json!({ "response": self.response_snapshot("in_progress") }),
            ));
        }
        // 上游可能把 usage 放在最终 chunk
        if let Some(usage) = chunk.get("usage").filter(|v| v.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return out;
        };
        let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));

        // 推理内容（deepseek reasoning_content / 部分网关 delta.reasoning）
        if let Some(rc) = delta.get("reasoning_content").and_then(Value::as_str) {
            if !rc.is_empty() {
                self.push_reasoning_delta(&mut out, rc);
            }
        }
        if let Some(rc) = delta.get("reasoning").and_then(Value::as_str) {
            if !rc.is_empty() {
                self.push_reasoning_delta(&mut out, rc);
            }
        }
        // 正文
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.push_message_delta(&mut out, text);
            }
        }
        // 工具调用
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                self.feed_tool_call(tc, &mut out);
            }
        }
        // finish_reason

        out
    }

    /// 收尾：下发所有 done 事件 + response.completed + [DONE]。
    /// 返回 (sse 文本, (response_id, 供 previous_response_id 回填的 chat 消息, 完整 Response 对象))。
    pub(crate) fn finish(mut self) -> (String, (String, Vec<Value>, Value)) {
        let mut out = String::new();
        if !self.emitted_created {
            self.emitted_created = true;
            out.push_str(&sse_event(
                "response.created",
                json!({ "response": self.response_snapshot("in_progress") }),
            ));
        }
        let mut items: Vec<(usize, Value)> = Vec::new();

        // reasoning
        if let Some(rs) = self.reasoning.take() {
            self.push_seq(
                &mut out,
                "response.reasoning_summary_text.done",
                json!({
                    "item_id": rs.item_id,
                    "output_index": rs.output_index,
                    "summary_index": rs.summary_index,
                    "text": rs.text,
                }),
            );
            self.push_seq(
                &mut out,
                "response.reasoning_summary_part.done",
                json!({
                    "item_id": rs.item_id,
                    "output_index": rs.output_index,
                    "summary_index": rs.summary_index,
                    "part": { "type": "summary_text", "text": rs.text },
                }),
            );
            self.push_seq(
                &mut out,
                "response.reasoning.done",
                json!({
                    "item_id": rs.item_id,
                    "output_index": rs.output_index,
                    "content_index": rs.content_index,
                    "text": rs.text,
                }),
            );
            let item = json!({
                "id": rs.item_id,
                "type": "reasoning",
                "status": "completed",
                "summary": [{ "type": "summary_text", "text": rs.text }],
            });
            items.push((rs.output_index, item.clone()));
            self.push_seq(
                &mut out,
                "response.output_item.done",
                json!({ "output_index": rs.output_index, "item": item }),
            );
        }

        // message
        if let Some(m) = self.message.take() {
            self.push_seq(
                &mut out,
                "response.output_text.done",
                json!({
                    "item_id": m.item_id,
                    "output_index": m.output_index,
                    "content_index": m.content_index,
                    "text": m.text,
                }),
            );
            self.push_seq(
                &mut out,
                "response.content_part.done",
                json!({
                    "item_id": m.item_id,
                    "output_index": m.output_index,
                    "content_index": m.content_index,
                    "part": { "type": "output_text", "text": m.text, "annotations": [] },
                }),
            );
            let item = json!({
                "id": m.item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": m.text, "annotations": [] }],
            });
            items.push((m.output_index, item.clone()));
            self.push_seq(
                &mut out,
                "response.output_item.done",
                json!({ "output_index": m.output_index, "item": item }),
            );
        }

        // tool calls
        let tool_calls: Vec<ToolState> = self.tool_calls.drain(..).collect();
        for tc in tool_calls {
            self.push_seq(
                &mut out,
                "response.function_call_arguments.done",
                json!({
                    "item_id": tc.item_id,
                    "output_index": tc.output_index,
                    "arguments": tc.arguments,
                }),
            );
            let item = json!({
                "id": tc.item_id,
                "type": "function_call",
                "status": "completed",
                "call_id": tc.call_id,
                "name": tc.name,
                "arguments": tc.arguments,
            });
            items.push((tc.output_index, item.clone()));
            self.push_seq(
                &mut out,
                "response.output_item.done",
                json!({ "output_index": tc.output_index, "item": item }),
            );
        }

        items.sort_by_key(|(idx, _)| *idx);
        let output: Vec<Value> = items.into_iter().map(|(_, v)| v).collect();
        let history = self.history_from_output(&output);
        let response = self.response_with("completed", output);
        self.push_seq(
            &mut out,
            "response.completed",
            json!({ "response": response }),
        );
        out.push_str("data: [DONE]\n\n");
        (out, (self.response_id.clone(), history, response))
    }

    fn ensure_message(&mut self, out: &mut String) {
        if self.message.is_some() {
            return;
        }
        let item_id = translate::next_id("msg");
        let output_index = self.output_index;
        self.output_index += 1;
        self.message = Some(MessageState {
            item_id: item_id.clone(),
            text: String::new(),
            output_index,
            content_index: 0,
        });
        self.push_seq(
            out,
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                },
            }),
        );
        self.push_seq(
            out,
            "response.content_part.added",
            json!({
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] },
            }),
        );
    }

    /// 追加一段正文到当前 message 项并下发 response.output_text.delta。
    fn push_message_delta(&mut self, out: &mut String, delta_text: &str) {
        self.ensure_message(out);
        let meta = if let Some(m) = self.message.as_mut() {
            m.text.push_str(delta_text);
            Some((m.item_id.clone(), m.output_index, m.content_index))
        } else {
            None
        };
        if let Some((item_id, output_index, content_index)) = meta {
            self.push_seq(
                out,
                "response.output_text.delta",
                json!({
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": content_index,
                    "delta": delta_text,
                }),
            );
        }
    }

    /// 追加一段推理文本到当前 reasoning 项并下发 response.reasoning_summary_text.delta。
    fn push_reasoning_delta(&mut self, out: &mut String, delta_text: &str) {
        self.ensure_reasoning(out);
        let meta = if let Some(rs) = self.reasoning.as_mut() {
            rs.text.push_str(delta_text);
            Some((rs.item_id.clone(), rs.output_index, rs.summary_index))
        } else {
            None
        };
        if let Some((item_id, output_index, summary_index)) = meta {
            self.push_seq(
                out,
                "response.reasoning_summary_text.delta",
                json!({
                    "item_id": item_id,
                    "output_index": output_index,
                    "summary_index": summary_index,
                    "delta": delta_text,
                }),
            );
        }
    }

    fn ensure_reasoning(&mut self, out: &mut String) {
        if self.reasoning.is_some() {
            return;
        }
        let item_id = translate::next_id("rs");
        let output_index = self.output_index;
        self.output_index += 1;
        self.reasoning = Some(ReasoningState {
            item_id: item_id.clone(),
            text: String::new(),
            output_index,
            content_index: 0,
            summary_index: 0,
        });
        self.push_seq(
            out,
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": [],
                },
            }),
        );
        self.push_seq(
            out,
            "response.reasoning_summary_part.added",
            json!({
                "item_id": item_id,
                "output_index": output_index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
        );
    }

    fn feed_tool_call(&mut self, tc: &Value, out: &mut String) {
        let index = tc.get("index").and_then(Value::as_i64).unwrap_or(0) as usize;
        let existing_pos = self.tool_calls.iter().position(|t| t.index == index);
        let arguments = tc
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match existing_pos {
            // 已有调用：累积 arguments 增量
            Some(p) => {
                if !arguments.is_empty() {
                    let meta = if let Some(t) = self.tool_calls.get_mut(p) {
                        t.arguments.push_str(arguments);
                        Some((t.item_id.clone(), t.output_index))
                    } else {
                        None
                    };
                    if let Some((item_id, output_index)) = meta {
                        self.push_seq(
                            out,
                            "response.function_call_arguments.delta",
                            json!({
                                "item_id": item_id,
                                "output_index": output_index,
                                "delta": arguments,
                            }),
                        );
                    }
                }
            }
            // 新调用：id 可能首帧带，也可能缺省，都先建项
            None => {
                let call_id = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let call_id = if call_id.is_empty() {
                    translate::next_id("call")
                } else {
                    call_id
                };
                let name = tc
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let item_id = translate::next_id("fc");
                let output_index = self.output_index;
                self.output_index += 1;
                self.tool_calls.push(ToolState {
                    index,
                    item_id: item_id.clone(),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.to_string(),
                    output_index,
                });
                self.push_seq(
                    out,
                    "response.output_item.added",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                        },
                    }),
                );
                if !arguments.is_empty() {
                    self.push_seq(
                        out,
                        "response.function_call_arguments.delta",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": arguments,
                        }),
                    );
                }
            }
        }
    }

    fn push_seq(&mut self, out: &mut String, event_type: &str, mut data: Value) {
        let seq = self.next_seq();
        if let Some(obj) = data.as_object_mut() {
            obj.insert("type".to_string(), json!(event_type));
            obj.insert("sequence_number".to_string(), json!(seq));
        }
        out.push_str(&sse_event(event_type, data));
    }

    fn response_snapshot(&self, status: &str) -> Value {
        self.response_with(status, Vec::new())
    }

    fn response_with(&self, status: &str, output: Vec<Value>) -> Value {
        // output_text = 所有 message 项 output_text 部分的拼接（与真实 Responses API 语义一致）
        let output_text: String = output
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
            .filter_map(|item| {
                item.get("content").and_then(Value::as_array).map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
            })
            .collect();
        let echo = &self.echo;
        let instructions = echo.get("instructions").cloned().unwrap_or(Value::Null);
        let metadata = echo.get("metadata").cloned().unwrap_or(Value::Null);
        let temperature = echo.get("temperature").cloned().unwrap_or(Value::Null);
        let top_p = echo.get("top_p").cloned().unwrap_or(Value::Null);
        let max_output_tokens = echo.get("max_output_tokens").cloned().unwrap_or(Value::Null);
        let parallel_tool_calls = echo
            .get("parallel_tool_calls")
            .cloned()
            .unwrap_or(json!(true));
        let tool_choice = echo.get("tool_choice").cloned().unwrap_or(json!("auto"));
        let tools = echo.get("tools").cloned().unwrap_or(json!([]));
        let service_tier = echo.get("service_tier").cloned().unwrap_or(Value::Null);
        let truncation = echo.get("truncation").cloned().unwrap_or(Value::Null);
        let reasoning = echo.get("reasoning").cloned().unwrap_or(Value::Null);
        let text = echo.get("text").cloned().unwrap_or(Value::Null);
        let background = echo.get("background").cloned().unwrap_or(Value::Null);
        let conversation = echo.get("conversation").cloned().unwrap_or(Value::Null);
        let max_tool_calls = echo.get("max_tool_calls").cloned().unwrap_or(Value::Null);
        let top_logprobs = echo.get("top_logprobs").cloned().unwrap_or(Value::Null);
        let user = echo.get("user").cloned().unwrap_or(Value::Null);
        let store = echo.get("store").cloned().unwrap_or(json!(true));
        let previous_response_id = echo
            .get("previous_response_id")
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "completed_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": Value::Array(output),
            "output_text": output_text,
            "usage": self.usage.clone().unwrap_or_else(translate::zero_usage),
            "error": Value::Null,
            "incomplete_details": Value::Null,
            "instructions": instructions,
            "metadata": metadata,
            "parallel_tool_calls": parallel_tool_calls,
            "temperature": temperature,
            "tool_choice": tool_choice,
            "tools": tools,
            "top_p": top_p,
            "max_output_tokens": max_output_tokens,
            "max_tool_calls": max_tool_calls,
            "background": background,
            "conversation": conversation,
            "previous_response_id": previous_response_id,
            "store": store,
            "service_tier": service_tier,
            "truncation": truncation,
            "reasoning": reasoning,
            "text": text,
            "user": user,
            "top_logprobs": top_logprobs,
            "prompt": Value::Null,
            "prompt_cache_key": Value::Null,
            "prompt_cache_options": Value::Null,
            "prompt_cache_retention": Value::Null,
            "moderation": Value::Null,
            "safety_identifier": Value::Null
        })
    }

    /// 从完整 output 项生成 assistant chat 消息（previous_response_id 回填用）。
    fn history_from_output(&self, output: &[Value]) -> Vec<Value> {
        let mut text = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for p in parts {
                            if p.get("type").and_then(Value::as_str) == Some("output_text") {
                                if let Some(t) = p.get("text").and_then(Value::as_str) {
                                    text.push_str(t);
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    tool_calls.push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }));
                }
                _ => {}
            }
        }
        if text.is_empty() && tool_calls.is_empty() {
            return Vec::new();
        }
        let mut msg = json!({ "role": "assistant" });
        if !text.is_empty() {
            msg["content"] = json!(text);
        } else {
            msg["content"] = Value::Null;
        }
        if !tool_calls.is_empty() {
            msg["tool_calls"] = Value::Array(tool_calls);
        }
        vec![msg]
    }
}

fn sse_event(event_type: &str, data: Value) -> String {
    format!("event: {event_type}\ndata: {}\n\n", data)
}

fn sse_error_message(message: &str) -> String {
    let mut out = String::new();
    out.push_str(&sse_event(
        "error",
        json!({ "code": "upstream_error", "message": message, "param": Value::Null }),
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

fn sse_error_event(alias: &str, tried: usize, exc: &str) -> String {
    let message = format!("all {tried} upstream keys failed for {alias}: {exc}");
    sse_error_message(&message)
}

/// 从 Responses SSE 流中提取 `response.completed` 事件的 usage（透传模式用量记录）。
fn extract_responses_usage_from_stream(body_text: &str) -> Option<Value> {
    let mut usage = None;
    for line in body_text.lines().map(str::trim) {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            if let Some(u) = value
                .get("response")
                .and_then(|r| r.get("usage"))
                .filter(|u| u.is_object())
            {
                usage = Some(u.clone());
            }
        }
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 收集所有 SSE 事件的 type。
    fn event_types(sse: &str) -> Vec<String> {
        sse.lines()
            .filter_map(|l| l.strip_prefix("event: "))
            .map(|s| s.to_string())
            .collect()
    }

    fn last_event_json(sse: &str, wanted: &str) -> Value {
        let mut found = json!(null);
        let mut current_event = String::new();
        let mut current_data = String::new();
        for line in sse.lines() {
            if let Some(ev) = line.strip_prefix("event: ") {
                current_event = ev.to_string();
                current_data.clear();
            } else if let Some(data) = line.strip_prefix("data: ") {
                current_data.push_str(data);
                if current_event == wanted {
                    if let Ok(v) = serde_json::from_str::<Value>(&current_data) {
                        found = v;
                    }
                }
            }
        }
        found
    }

    #[test]
    fn plain_text_stream_emits_expected_events() {
        let mut sse = ResponsesSse::with_echo("test-model", &json!({}));
        let mut collected = String::new();
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" }, "finish_reason": null }]
        })));
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "content": "Hel" }, "finish_reason": null }]
        })));
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "content": "lo" }, "finish_reason": null }]
        })));
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
        })));
        let (finish_sse, (resp_id, history, _response)) = sse.finish();
        collected.push_str(&finish_sse);

        let types = event_types(&collected);
        assert!(types.contains(&"response.created".to_string()));
        assert!(types.contains(&"response.output_item.added".to_string()));
        assert!(types.contains(&"response.content_part.added".to_string()));
        assert_eq!(
            types
                .iter()
                .filter(|t| t.as_str() == "response.output_text.delta")
                .count(),
            2
        );
        assert!(types.contains(&"response.output_text.done".to_string()));
        assert!(types.contains(&"response.content_part.done".to_string()));
        assert!(types.contains(&"response.output_item.done".to_string()));
        assert!(types.contains(&"response.completed".to_string()));

        let completed = last_event_json(&collected, "response.completed");
        assert_eq!(completed["response"]["status"], "completed");
        let msg = &completed["response"]["output"][0];
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["content"][0]["text"], "Hello");
        assert_eq!(completed["response"]["output_text"], "Hello");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 0);

        assert!(!resp_id.is_empty());
        assert_eq!(history[0]["role"], "assistant");
        assert_eq!(history[0]["content"], "Hello");
    }

    #[test]
    fn reasoning_and_tool_call_stream() {
        let mut sse = ResponsesSse::with_echo("test-model", &json!({}));
        let mut collected = String::new();
        // 推理内容 + 工具调用首帧（带 id/name）
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "role": "assistant", "reasoning_content": "think" }, "finish_reason": null }]
        })));
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": { "reasoning_content": "ing" }, "finish_reason": null }]
        })));
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": {
                "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "f", "arguments": "{\"a\":" } }]
            }, "finish_reason": null }]
        })));
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": {
                "tool_calls": [{ "index": 0, "function": { "arguments": "1}" } }]
            }, "finish_reason": null }]
        })));
        collected.push_str(&sse.feed(&json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
        })));
        let (finish_sse, (_, history, _response)) = sse.finish();
        collected.push_str(&finish_sse);

        let types = event_types(&collected);
        assert!(types.contains(&"response.reasoning_summary_part.added".to_string()));
        assert!(types.contains(&"response.reasoning_summary_text.delta".to_string()));
        assert!(types.contains(&"response.function_call_arguments.delta".to_string()));
        assert!(types.contains(&"response.function_call_arguments.done".to_string()));
        assert!(types.contains(&"response.reasoning.done".to_string()));

        let completed = last_event_json(&collected, "response.completed");
        let output = completed["response"]["output"].as_array().unwrap();
        let types_out: Vec<&str> = output.iter().filter_map(|o| o["type"].as_str()).collect();
        assert_eq!(types_out, vec!["reasoning", "function_call"]);
        assert_eq!(output[0]["summary"][0]["text"], "thinking");
        assert_eq!(output[1]["call_id"], "call_1");
        assert_eq!(output[1]["arguments"], "{\"a\":1}");

        // previous_response_id 历史：assistant 消息带 tool_calls
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
    }

    #[test]
    fn finish_without_any_chunk_still_emits_created() {
        let sse = ResponsesSse::with_echo("m", &json!({}));
        let (out, (_resp_id, history, _response)) = sse.finish();
        assert!(out.contains("response.created"));
        assert!(out.contains("response.completed"));
        assert!(history.is_empty());
    }
}
