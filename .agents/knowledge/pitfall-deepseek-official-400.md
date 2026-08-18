# DeepSeek 官方 API 严格校验导致 router 转发 400

用于：排查 llm-provider-router 转发到 DeepSeek 官方（api.deepseek.com）时出现 HTTP 400、请求失败率高的问题，以及 DeepSeek 官方缓存率与 router 展示不一致的疑问。

触发词：deepseek 400、deepseek-official、json_object 报错、json_schema、tool_choice 不支持、reasoning_content 必须回传、n>1 不支持、DeepSeek 缓存率、cache hit 不一致。

不适用：ark（火山方舟）等其他供应商的 400 问题（ark 对下述字段更宽松）；DeepSeek 官方直连调用方自身的报错（本文是 router 转发层兼容方案）。

---

## 症状

- router 的 `/api/usage` 按 key 统计里，`deepseek-official` 请求失败率高（实测一天 89 个请求 59 个 400）。
- 同一份 payload 走 ark（火山方舟）成功、落到 DeepSeek 官方就 400。
- 用户质疑：DeepSeek 官方后台显示输入缓存命中 90%+，router 却显示 66%，怀疑 router 缓存统计不准（实为统计样本不同，见文末）。

## 根因：DeepSeek 官方对 OpenAI 兼容字段的额外严格校验

DeepSeek 官方（api.deepseek.com）比 ark 等供应商更严格，以下请求直接 400：

| 触发条件 | DeepSeek 报错原文 |
|---|---|
| `response_format: {"type":"json_object"}` 但整个 prompt 不含 "json" 字样 | `Prompt must contain the word 'json' in some form to use 'response_format' of type 'json_object'.` |
| `response_format: {"type":"json_schema",...}` | `This response_format type is unavailable now` |
| `n > 1` | `Invalid n value (currently only n = 1 is supported)` |
| thinking 模式下 `tool_choice: "required"` 或 function 对象 | `Thinking mode does not support this tool_choice` |
| thinking 模式下 assistant 消息带 `tool_calls` 但没回传 `reasoning_content` | `The reasoning_content in the thinking mode must be passed back to the API.` |
| 消息里出现 `role: "developer"` | `unknown variant 'developer'`（router 已转 system，无需处理） |

关键点：

- DeepSeek 对 "json" 字样检查是**大小写不敏感**的（"JSON"/"json"/"Json" 都放行）。
- `tool_choice` 只有 `"auto"` / `"none"` 在 thinking 模式下放行；`"required"` 和 function 对象都 400。
- `deepseek-v4-flash` 是推理模型，**thinking 默认开启**；只要没显式 `"thinking":{"type":"disabled"}`，上面 thinking 相关的严格校验就会触发。
- `n=1` 显式传没问题，只有 `n>1` 才 400。

## 修复（已上线，2026-08-12）

在 `src/proxy.rs` 的 `prepare_upstream_payload` 中对 **deepseek-official 供应商**（`alias.provider()=="deepseek-official"` 或 base_url 含 `deepseek.com`）做最小化兼容归一化，只在请求必然 400 时才改写：

1. `response_format` type `json_schema` → `json_object`（DeepSeek 不支持 json_schema）。
2. `json_object` 模式且 prompt 无 "json" 字样 → 在首条 system 消息前补 `Respond in JSON format.`（首条非 system 或 content 为数组时，在 messages 头部插入新 system 消息）。
3. `n > 1` → 钳到 1。
4. thinking 开启时，若 `tool_choice` 为强制值（非 auto/none），或存在 assistant `tool_calls` 但缺 `reasoning_content` → 设 `"thinking":{"type":"disabled"}` 绕开校验（保留工具调用契约，牺牲该请求的推理）。

辅助函数：`is_deepseek_official` / `normalize_deepseek_official` / `prompt_mentions_json` / `ensure_json_hint` / `thinking_enabled` / `forced_tool_choice` / `assistant_tool_calls_missing_reasoning`。

同时新增上游失败日志：`log_upstream_failure` 把 4xx/5xx 的 provider/model/status/error message 打到 stderr（systemd 下进 journald）：
`journalctl --user -u 'llm-provider-router-backend@*' | grep upstream_failure`

单元测试：`proxy::tests` 下 12 个用例覆盖全部归一化分支（`cargo test --bin llm-provider-router`）。

## 验证方法

- 走真实链路（前代理 8789 → deepseek-v4-flash-official，纯官方 key）发四种之前必 400 的 payload，全部应返回 200：
  - `response_format:json_object` + prompt 无 "json" 字样
  - `tool_choice:"required"`
  - assistant `tool_calls` 缺 `reasoning_content`
  - `n:3`
- 单测 + clippy + `cargo build --release`。
- 蓝绿热部署：`python3 bin/hot-deploy-router.py deploy`（起 inactive 槽 → 切流量 → 排空旧槽，不中断在途请求）。

## 部署环境备忘

- 服务：`llm-provider-router-backend@blue`（8790）/ `@green`（8791），前代理 `llm-provider-router.service`（8789）。
- active slot 记录在 `~/.local/state/llm-provider-router/active-backend.json`。
- 构建产物 `target/release/llm-provider-router` 是服务实际运行的二进制，改代码后必须 `cargo build --release` 再 deploy。
- `config/` 下 `providers-v2.json` / `logical-models.json` 的改动直接热生效（服务运行中读取），不在二进制里。

## 附：DeepSeek 官方缓存率 90%+ vs router 显示 66% 的说明

不是 router 统计 bug，是统计口径/样本不同：

- router 的 `extract_cached_tokens` 读 `usage.prompt_tokens_details.cached_tokens`（DeepSeek 流式/非流式都返回该字段，实测确认），公式 `cached/prompt_tokens` 与 DeepSeek 官方 `hit/(hit+miss)` 数学等价（DeepSeek 的 `prompt_tokens = hit + miss` 恒成立）。
- router 只统计它自己转发的请求；DeepSeek 官方后台是整账号聚合。deepseek-official 在 auto 池里权重仅 1/18，一天只有几十个请求，样本小且是 agent 交互负载（上下文前缀频繁变化），命中率天然低。
- 相同前缀重复请求也不是 100% 命中（实测 179 tokens 二次命中 128，约 71.5%），每请求永远有尾部不命中。
- 逐小时命中率在 1%~92% 之间波动，属负载类型差异，非测量误差。
