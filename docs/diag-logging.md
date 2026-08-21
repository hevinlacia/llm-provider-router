# 诊断日志（持久化收集期）

> 目标：journal 已部分损坏不可信 + `usage.sqlite` 只有聚合，缺请求级证据。先以**文件持久化**收集一段时间，再回看定位 `muse-spark` 是否为协议兼容问题。

## 落盘位置

```
~/.local/state/llm-provider-router/logs/diag-YYYY-MM-DD.jsonl
~/.local/state/llm-provider-router/logs/diag-YYYY-MM-DD.jsonl.1 ... .N
```

- JSONL，每行一个事件：`{ ts_ms, event, ... }`
- 不含消息正文、密钥、Authorization；仅记 `payload_summary`（model/stream/n/thinking/tool_choice 等）
- 轮转：单文件 `LLM_PROVIDER_ROUTER_DIAG_MAX_BYTES`（默认 10MB）超限后移位，最多 `LLM_PROVIDER_ROUTER_DIAG_MAX_FILES`（默认 50）个文件

## 环境变量（systemd 需 `systemctl --user daemon-reload` 后重启生效）

| 变量 | 默认 | 说明 |
|------|------|------|
| `LLM_PROVIDER_ROUTER_DIAG_DIR` | `~/.local/state/llm-provider-router/logs` | 落盘目录 |
| `LLM_PROVIDER_ROUTER_DIAG_MAX_BYTES` | `10485760` | 单文件上限 |
| `LLM_PROVIDER_ROUTER_DIAG_MAX_FILES` | `50` | 保留文件数，0=禁用诊断 |
| `LLM_PROVIDER_ROUTER_DIAG_SAMPLE_EVERY` | `1` | 采样：每 N 个 request 记录 1 个 |

## 埋点事件

| event | 触发 | 字段 |
|-------|------|------|
| `request.chat_completions` | 每次 `/v1/chat/completions` 入口 | `model`, `summary`(payload_summary) |
| `normalize.muse_spark.thinking_seen` | `muse-spark` 且 payload 含 `thinking/reasoning[_effort]` | `alias/provider/upstream_model`, `summary_before` |
| `normalize.deepseek.applied` | `deepseek-official` 且归一化改写发生 | `before/after` payload_summary |
| `upstream.failure` | 上游 4xx/5xx（stderr 同步落盘一份） | `provider/model/alias/status/error` |
| `stream.incomplete_upstream` | 流式上游缺 `finish_reason` 或 `[DONE]`（已做兜底补齐） | `alias/provider/model/status/saw_finish_reason/saw_done/bytes` |

## 常用查询

```bash
# 按天查看
cat ~/.local/state/llm-provider-router/logs/diag-$(date +%F).jsonl | jq -c 'select(.event=="upstream.failure")'

# muse-spark 且 thinking 透传的请求
jq -c 'select(.event=="normalize.muse_spark.thinking_seen")' ~/.local/state/llm-provider-router/logs/diag-*.jsonl | tail

# 流式不标准上游占比
jq -s 'map(select(.event=="stream.incomplete_upstream")) | group_by(.alias) | map({alias: .[0].alias, count: length})' ~/.local/state/llm-provider-router/logs/diag-*.jsonl

# 4xx/5xx 按模型聚合
jq -s 'map(select(.event=="upstream.failure")) | group_by(.model) | map({model: .[0].model, count: length})' ~/.local/state/llm-provider-router/logs/diag-*.jsonl

# 采样率临时调低（高流量时）
# 在 ~/.config/opencode/agent-config.env 或 systemd dropin 加：
# LLM_PROVIDER_ROUTER_DIAG_SAMPLE_EVERY=10
```

## 收集期后的决策

- 若 `normalize.muse_spark.thinking_seen` 大量出现且同期 `upstream.failure` 的 `muse-spark` 4xx/503 集中，说明 `muse-spark + xhigh(thinking)` 确为根因 → 在 `prepare_upstream_payload` 中对 `is_muse_spark()` 剥离 `thinking/reasoning_effort`。
- 若 `stream.incomplete_upstream` 高频且与失败正相关，考虑对该供应商流做更强归一化。
- 否则回看 `request.chat_completions` 的 `summary` 是否为其他协议字段（`tool_choice/response_format/n`）触发。
