# 报错分类与自动路由（error-rules）

> 2026-10 引入。取代原先散落在 `freeze.rs` 的报错文案匹配判定。
> 起因：ark 把订阅失效文案从 "does not have a valid CodingPlan subscription" 改成
> "lacks a valid CodingPlanEnterprise subscription"，旧匹配失灵后 400 被当成请求级
> 错误直接终止，不再自动换 key，pi 端表现为 `all 1 upstream keys failed`。

## 四类报错与动作

| 分类 | 典型来源 | 动作 |
|---|---|---|
| invalid（失效） | 401/402/403、订阅过期 | 冻结整把 key（`invalid_freeze_seconds`，默认 24h）→ 换下一把 |
| rate_limited（限流） | 429、流内限流错误事件 | 冻结到恢复时刻：`retry-after` 头 → body `reset at` 时间戳（含 zp 中文文案「后可继续使用」，北京时间）→ 不冻结；到点自动恢复 |
| model_unsupported（模型不支持） | 404 | 直接报错，不切 key（用户配置错误）；404+`not support` 文案同时记入 key×模型阶梯退避 |
| transient（其他/兜底） | 5xx、连接失败、未识别的错误 | 同 key 重试 `transient_attempts_per_key` 次（默认 1，即不重试）→ 换下一把 |

分类优先级固定：invalid > rate_limited > model_unsupported > 其余一律 transient。
**未识别的错误永远不会导致"该切 key 不切"** —— 这是对 ark 改文案事故的结构性修复。

## 流内/嵌入错误（HTTP 200 + 错误事件）

2026-10 zp（智谱 bigmodel）5 小时限额事故：限额用尽时上游以 **HTTP 200 + SSE 流内错误事件**
返回中文文案（code=rate_limit_exceeded：
`已达到 5 小时使用上限，2026-10-14 19:14:36 后可继续使用。...`），
此前被当成功流转发：key 不冻结、会话不解绑，粘性会话反复命中坏 key，故障切换失效。

现在四条链路都会扫描嵌入错误并处理（`handle_embedded_failure`）：

- `/v1/responses` 翻译模式：逐 chunk 检测，命中即冻结/解绑 + 下发 Responses 错误事件并终止流；
- `/v1/responses` 透传模式 + 非流式透传：流尾/响应尾扫描（`response.failed`、`{"error":{...}}`、
  `{"type":"error",...}`、无 type 的 `{code,message}` 四种形状）；
- Anthropic 透传流式（`/v1/messages`）：流尾扫描（Anthropic error 事件）。

处理动作与 HTTP 状态错误一致（classify 按关键词命中，无状态码传 200）：
invalid/限流冻结对应 key、解除会话粘性绑定、记入最近报错（status=200，dashboard Errors 页可见）；
流实际失败时跳过差分冻结与"不支持"清除（同行成功前提不成立）。
透传链路无法对本请求换 key 重试（响应已提交），冻结保证**下一次请求**自动故障切换。

内置模板新增 `zp`（智谱 bigmodel GLM Coding Plan）：限流按关键词
`使用上限` / `rate_limit_exceeded` 命中（不限状态码，覆盖流内与 4xx 两种形状）；
default/ark 模板也加了 `rate_limit_error` / `rate_limit_exceeded` 关键词规则，
覆盖其他供应商的流内限流事件。

## 两层兜底（不依赖报错文案）

- **差分冻结**：同一请求内某 key 失败（transient，未冻结）而后续 key 成功
  → 反向冻结失败的 key（`differential_freeze_seconds`，默认 10 分钟，重复失败会续期）。
  供应商再改文案也会自动把坏 key 排除出可用池。
- **全池熔断**：transient 失败耗尽全池后短熔断全部未冻结 key
  （`transient_exhausted_freeze_seconds`，默认 5 分钟，0 = 关闭），
  避免上游整体故障时每个请求都白打一遍全部 key。限流/失效冻结不受影响。

## 配置：config/error-rules.json

热加载（2s 轮询，与 v2 配置共用 watcher）；dashboard Settings 页可视化编辑
（`GET/PUT /api/config/error-rules`）。文件缺失时使用内置预设（default + ark + zp）。

结构：

- `tunables`：上述各时长/次数；缺省项回落 env（invalid 默认回落
  `LLM_PROVIDER_ROUTER_SUBSCRIPTION_INVALID_FREEZE_SECONDS`）。
- `templates`：模板名 → 四类规则集。规则 = 状态码列表（空=任意）+
  `keywords_all`（AND）+ `keywords_any`（OR），关键词小写包含匹配。
- `provider_bindings`：供应商 id → 模板名；未登记供应商先用同名模板（若有），
  再回落 `default`。

已退役：`providers-v2.json` 的 `retry_on_status`（状态码不再决定是否切 key）、
env `LLM_PROVIDER_ROUTER_AUTH_INVALID_FREEZE_SECONDS`（保留字段，不生效）。

## 诊断

- **dashboard Errors 页**（`/api/errors/recent`）：最近 200 条上游失败的
  时间 / 供应商 / key / 模型 / 状态码 / 分类 / 命中规则 / 截断报错原文，
  5s 自动刷新，可按分类过滤。看到误分类 → 去设置页调整对应模板关键词。
- 冻结状态仍走 `/api/state` 的 `frozen` 视图（含 reason），清除冻结按钮
  （`POST /api/frozen/clear`）清空全部冻结。
