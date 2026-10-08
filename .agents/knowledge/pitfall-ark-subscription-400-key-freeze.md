# ark 订阅失效以 HTTP 400 到达，旧版 router 不冻结不换 key

用于：排查 llm-provider-router 转 ark（火山方舟 coding plan）时报 "all 1 upstream keys failed"、某把 key errors 率 100% 的问题；以及订阅失效 key 的冻结/恢复机制说明。

触发词：all 1 upstream keys failed、CodingPlan subscription、subscription has expired、SubscriptionNotValid、ark 订阅过期、subscription_invalid、key 冻结、frozen、hevin-private。

不适用：404 "does not support the coding plan feature"（模型级不支持，走 mark_key_model_unsupported 阶梯退避，见 `unsupported` 相关代码）；429 限流（走 retry-after 冻结）；DeepSeek 官方 400（见 `pitfall-deepseek-official-400.md`）。

---

## 现象

- 客户端（pi/opencode 等）报 `Error: all 1 upstream keys failed for glm-flash-latest-auto`（或其它映射到 `ark/glm-5.3-flash` 的别名）。
- `/api/state` → `usage.by_key` 里该 key 的 `errors == requests`，但 `frozen` 为空 → 坏 key 一直留在可用池里反复被选中。
- 失败是间歇性的：粘到坏 key 的会话连续失败，粘到好 key 的会话正常。

## 根因

上游返回（HTTP **400**）：

```
Your account (2102661813) does not have a valid CodingPlan subscription, or your subscription has expired.
```

两个关键点叠加：

1. ark 的 `retry_on_status = [401, 402, 429, 500, 502, 503, 504]` **不含 400**。
2. 旧逻辑把"非 retry_on_status 的 >=400"当作**请求级终态错误**：不冻结、不换 key，直接返回错误事件。

而"账号订阅过期"本质是 **key 级永久故障**（语义等价于 402，但以 400 形态到达）——该账号下所有模型都会失败，应当整把 key 排除出可用池并换下一把，而不是判死整个请求。

## 修复（2026-10-02 已上线）

| 改动 | 位置 |
|---|---|
| `is_subscription_invalid()` 文案识别（"codingplan subscription" / "subscription has expired"，大小写不敏感） | `src/features/router/freeze.rs` |
| `maybe_freeze_key` 命中文案即整把冻结，reason=`subscription_invalid`，时长 `LLM_PROVIDER_ROUTER_SUBSCRIPTION_INVALID_FREEZE_SECONDS`（默认 86400s=24h） | 同上 + `src/config.rs` |
| `key_frozen_now()` 助手：失败处理后 key 若刚被冻结 → key 级失败信号 | `src/features/chat/select.rs` |
| chat 流式/非流式、anthropic messages 流式/非流式、responses 透传/翻译，共 6 处非重试 `>=400` 分支：检测到冻结即 `continue` 换下一把 key；全部耗尽走 NoAvailable fallback 到下一个 target | `src/features/chat/{stream,upstream}.rs`、`src/routes/{messages,responses}.rs`、`src/features/responses/stream/mod.rs` |

设计为**通用机制**：调用点判断的是"key 是否刚被冻结"而非订阅文案本身，以后任何新的 key 级冻结模式（配额、鉴权等）自动获得换 key 行为。

## 恢复手段

- 续费 CodingPlan 后：dashboard「清除冻结」按钮，或 `POST /api/frozen/clear`，立即回到可用池。
- 不续费：`config/providers-v2.json` 里把该 key `enabled: false`（配置热生效，无需重启）。
- 冻结 24h 后自动过期回池——若订阅仍无效，会再次被首个命中请求冻结（自愈循环，代价是每 24h 一次单请求失败）。

## 排查线索

- **诊断日志**：`~/.local/state/llm-provider-router/logs/diag-YYYY-MM-DD.jsonl`，事件 `upstream.failure`，按 `alias`/`status` 过滤（error 字段含上游原文与账号号段）。
- **冻结状态**：`curl -s http://127.0.0.1:8789/api/state | jq .frozen`。
- **上游失败日志**：`journalctl --user -u 'llm-provider-router-backend@*' | grep upstream_failure`。
- **代码位置**：`freeze.rs::maybe_freeze_key`（冻结判定）、`chat/select.rs::key_frozen_now`（换 key 信号）、`chat/stream.rs` 6 处调用点。
- **账号 ↔ key 映射**（2026-10-02 实锤）：账号 `2102661813` = `ark/hevin-private`。
