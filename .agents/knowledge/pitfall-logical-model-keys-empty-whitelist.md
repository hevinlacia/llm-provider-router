# logical-models target 配 `"keys": []` 空白名单导致模型永远无可用上游

用于：排查 llm-provider-router 逻辑模型在 `/api/router/capabilities` 显示 `targets=0`、`effective` 窗口为 null、"没有可用模型"，尤其是嵌套逻辑模型跟着不可用的场景。

触发词：keys 空数组、keys []、targets=0、无可用模型、logical-models.json、嵌套逻辑模型、key 白名单、low-model-auto。

不适用：provider 级 key 停用（providers-v2.json 的 `enabled: false`）、key 冻结（frozen_keys）、api-keys.json 缺失——这三种是 key 本身的问题，本文是 target 配置语义问题。

---

## 症状

- `GET /api/router/capabilities` 里该 logical model `targets=0`、`effective.contextWindow/maxTokens` 为 null。
- `/v1/models` 里干脆不返回该模型。
- **嵌套逻辑模型连锁失效**：`low-model-auto` 的 target 指向 `glm-flash-latest-auto`（逻辑模型套逻辑模型），后者失效则前者也 `targets=0`。
- 迷惑点：同一个物理模型 `ark/glm-5.3-flash` 被多个 logical model 引用，`high-model-auto`/`medium-model-auto` 正常，唯独个别引用失效。

## 根因：`V2Target.keys` 是 key 名白名单，空数组 = 零可用 key

`src/config/v2/types.rs` 中 `V2Target.keys: Option<Vec<String>>` 的语义：

| 配置值 | 语义 |
|---|---|
| `null` / 字段缺省 | 使用该 provider **全部 enabled key**（正常情况） |
| `["hevin"]` | 只用 provider 内名为 hevin 的 key（多账号分模型限定的场景） |
| `[]`（空数组） | 白名单为空 → **没有任何 key 可用 → target 永远不可用** |

可用性判定在 `src/features/router/state/v2.rs`（约 L252）：

```rust
let available = cand.model.keys.iter().any(|k| { ... });
```

`iter().any()` 对空数组恒为 `false`，与 provider 实际有几个 enabled key 无关。

历史上 `glm-flash-latest-auto` 和 `deepseek-flash-latest-auto` 的 target 被写成 `"weight": 1, "keys": []`（疑似手配时想把 weight 和 keys 一起补全，空数组被当成"不限制"），于是这两个模型以及嵌套引用它们的 `low-model-auto` 一直 targets=0，而 keys 为 null 的 `high/medium-model-auto` 指向同一个 `ark/glm-5.3-flash` 却正常——对照排障时极易误判为 provider/key 问题。

## 修复

`config/logical-models.json` 中把 target 的 `"keys": []` 改为 `"keys": null`（或直接删掉该字段）。不需要重启服务：

- `src/hot_reload.rs` 以 2 秒间隔轮询 `providers-v2.json` / `models.json` / `logical-models.json` / `virtual-models.json` 的文件戳，改动自动热重载。

## 验证

```bash
sleep 4 && curl -s http://127.0.0.1:8789/api/router/capabilities \
  | python3 -c "import json,sys; [print(m['id'], len(m.get('targets') or [])) for m in json.load(sys.stdin)['models']]"
```

所有 logical model 应 `targets>=1`（除非上游 provider 确实全停）。修复当天（2026-09-28）10 个 auto 模型全部恢复 targets>=1。

## 预防

- 手工编辑 logical-models.json 时，target 三件套写全就用 `"weight": null, "keys": null`，不要用空数组表达"不限制"。
- 排查顺序建议：先对比"同 provider 同物理模型、可用与不可用的 logical model"的 target 配置差异（本例就是这样 5 分钟定位），再查 provider key enabled/冻结状态。
