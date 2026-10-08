# 报错分类与自动路由（error-rules）

> 2026-10 引入。取代原先散落在 `freeze.rs` 的报错文案匹配判定。
> 起因：ark 把订阅失效文案从 "does not have a valid CodingPlan subscription" 改成
> "lacks a valid CodingPlanEnterprise subscription"，旧匹配失灵后 400 被当成请求级
> 错误直接终止，不再自动换 key，pi 端表现为 `all 1 upstream keys failed`。

## 四类报错与动作

| 分类 | 典型来源 | 动作 |
|---|---|---|
| invalid（失效） | 401/402/403、订阅过期 | 冻结整把 key（`invalid_freeze_seconds`，默认 24h）→ 换下一把 |
| rate_limited（限流） | 429 | 冻结到恢复时刻：`retry-after` 头 → body `reset at` 时间戳 → 不冻结；到点自动恢复 |
| model_unsupported（模型不支持） | 404 | 直接报错，不切 key（用户配置错误）；404+`not support` 文案同时记入 key×模型阶梯退避 |
| transient（其他/兜底） | 5xx、连接失败、未识别的错误 | 同 key 重试 `transient_attempts_per_key` 次（默认 1，即不重试）→ 换下一把 |

分类优先级固定：invalid > rate_limited > model_unsupported > 其余一律 transient。
**未识别的错误永远不会导致"该切 key 不切"** —— 这是对 ark 改文案事故的结构性修复。

## 两层兜底（不依赖报错文案）

- **差分冻结**：同一请求内某 key 失败（transient，未冻结）而后续 key 成功
  → 反向冻结失败的 key（`differential_freeze_seconds`，默认 10 分钟，重复失败会续期）。
  供应商再改文案也会自动把坏 key 排除出可用池。
- **全池熔断**：transient 失败耗尽全池后短熔断全部未冻结 key
  （`transient_exhausted_freeze_seconds`，默认 5 分钟，0 = 关闭），
  避免上游整体故障时每个请求都白打一遍全部 key。限流/失效冻结不受影响。

## 配置：config/error-rules.json

热加载（2s 轮询，与 v2 配置共用 watcher）；dashboard Settings 页可视化编辑
（`GET/PUT /api/config/error-rules`）。文件缺失时使用内置预设（default + ark）。

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
