# Architecture v2 — 分层模型 / Provider / Key 架构

> 状态：设计草案（Phase 1 待用户 review）
> 目的：把现状扁平的 ModelAlias（别名 = 上游模型 + base_url + keys 绑死）重构为 供应商 → 物理模型 → 逻辑模型 → key 的分层模型，支持跨供应商回退、配置化、两层负载均衡。

## 1. 设计目标

1. **配置化**：供应商、模型、key 全部收敛到 JSON 配置，不再硬编码在 `config.rs`；新增模型/供应商不改代码。
2. **逻辑模型与物理模型分离**：对外暴露稳定的逻辑模型名（alias），背后可路由到不同供应商的物理模型，支持跨供应商回退与容灾。
3. **两层负载均衡**：供应商/物理模型一层（策略可选）+ 同一供应商内部 key 一层（加权 + session 粘性）。
4. **参数继承与覆写**：逻辑模型定义默认参数，实际路由到的物理模型可覆写。
5. **供应商维度冻结**：key 冻结即供应商冻结，路由时跳过被冻结的供应商。
6. **对外契约不变**：客户端（pi / opencode）仍用现有 alias 名（`deepseek-v4-flash-auto` 等）访问，无需改动。

## 2. 概念模型

| 实体 | 说明 | 是否有 base_url | 是否有 key | 是否有参数 |
|---|---|---|---|---|
| **Provider（供应商）** | 物理上游，如 ark、deepseek-official、openai-relay | ✅ | ✅（key 挂在供应商下） | ❌ |
| **PhysicalModel（物理模型）** | 某供应商下的真实模型，如 `ark/deepseek-v4-flash` | 继承 Provider | 继承 Provider | ✅（可选覆写） |
| **ModelFamily（模型族）** | 跨供应商关联"同一模型"（如 deepseek-v4-flash 族），用于把不同供应商的同族模型绑定到逻辑模型 | ❌ | ❌ | ❌ |
| **LogicalModel（逻辑模型 / 模型组）** | 对外暴露名（alias），无 base_url、无 key，只有路由目标 + 路由策略 + 默认参数 | ❌ | ❌ | ✅（默认参数） |
| **Key** | 供应商下的凭证，env_var + weight + billing_type | — | — | — |

**命名**：采用"逻辑模型 / 模型组（LogicalModel）"，不使用"虚拟供应商"（避免与物理供应商混淆，且它没有 base_url/key 的语义）。

## 3. 数据模型 Schema（草案）

### 3.1 `config/providers.json` — 供应商 + key

> 过渡说明：v2 实现阶段使用 `config/providers-v2.json`（与旧 string-map 版 `providers.json` 并存，不破坏现有读取）；迁移完成后旧文件退役并合并为最终 `providers.json` 结构。

```jsonc
{
  "providers": {
    "ark": {
      "base_url": "https://ark.cn-beijing.volces.com/api/coding/v3",
      "retry": {
        "max_retry_seconds": 300,
        "retry_delay_seconds": 5.0,
        "retry_on_status": [401, 402, 429, 500, 502, 503, 504]
      },
      "keys": {
        "garvin":    { "env_var": "AGENT_AI_ARK_GARVIN_API_KEY",    "weight": 6, "billing_type": "subscription" },
        "wilford":   { "env_var": "AGENT_AI_ARK_WILFORD_API_KEY",   "weight": 3, "billing_type": "subscription", "enabled": false },
        "hevin":     { "env_var": "AGENT_AI_ARK_HEVIN_API_KEY",     "weight": 5, "billing_type": "subscription" },
        "khaine":    { "env_var": "AGENT_AI_ARK_KHAINE_API_KEY",    "weight": 6, "billing_type": "subscription" },
        "cyril":     { "env_var": "AGENT_AI_ARK_CYRIL_API_KEY",     "weight": 4, "billing_type": "subscription" },
        "moss":      { "env_var": "AGENT_AI_ARK_MOSS_API_KEY",      "weight": 4, "billing_type": "subscription" },
        "ronnie":    { "env_var": "AGENT_AI_ARK_RONNIE_API_KEY",    "weight": 4, "billing_type": "subscription" },
        "hevin-private": { "env_var": "AGENT_AI_ARK_HEVIN_PRIVATE_API_KEY", "weight": 6, "billing_type": "subscription" },
        "shell":     { "env_var": "AGENT_AI_ARK_SHELL_API_KEY",     "weight": 6, "billing_type": "subscription" }
      }
    },
    "deepseek-official": {
      "base_url": "https://api.deepseek.com",
      "keys": {
        "deepseek-official": { "env_var": "AGENT_AI_DEEPSEEK_API_KEY", "weight": 1, "billing_type": "payg" }
      }
    },
    "openai-relay": {
      "base_url": "https://api.aixhan.com/v1",
      "retry": { "max_retry_seconds": 1800, "retry_delay_seconds": 15.0, "retry_on_status": [429, 500, 502, 503, 504] },
      "keys": {
        "oai-hevin": { "env_var": "AGENT_AI_OPENAI_HEVIN_API_KEY", "weight": 1, "billing_type": "subscription" }
      }
    }
  }
}
```

- custom-keys.json 的 hevin-private / shell 并入 ark 的 keys（消除两套并行 key 机制）。
- key 的 `enabled`（默认 true）支持手动停用/启用：停用的 key 不参与负载均衡，也不计入供应商可用性（见 §4.2）。运行时可通过 API 切换并**写回 `providers.json`**（配置级持久化，重启保留）。
- **key 只与供应商关联，与模型无直接关联**：key 可用于该供应商下所有模型。旧 custom-keys 的 aliases 白名单语义取消。

### 3.2 `config/models.json` — 物理模型 + 模型族

```jsonc
{
  "families": {
    "deepseek-v4-flash": { "display_name": "DeepSeek V4 Flash" }
  },
  "models": {
    "ark/deepseek-v4-flash-260801": {
      "provider": "ark",
      "upstream_model": "deepseek-v4-flash-260801",
      "family": "deepseek-v4-flash",
      "params": {}                    // 可选覆写，空 = 继承逻辑模型默认
    },
    "deepseek-official/deepseek-v4-flash": {
      "provider": "deepseek-official",
      "upstream_model": "deepseek-v4-flash",
      "family": "deepseek-v4-flash",
      "params": {}
    },
    "ark/deepseek-v4-pro":        { "provider": "ark", "upstream_model": "deepseek-v4-pro" },
    "ark/glm-5.2":                { "provider": "ark", "upstream_model": "glm-5.2" },
    "ark/minimax-m3":             { "provider": "ark", "upstream_model": "minimax-m3" },
    "ark/ark-code-latest":        { "provider": "ark", "upstream_model": "ark-code-latest" },
    "openai-relay/gpt-5.5":       { "provider": "openai-relay", "upstream_model": "gpt-5.5" },
    "openai-relay/gpt-5.6-sol":   { "provider": "openai-relay", "upstream_model": "gpt-5.6-sol" }
  }
}
```

- 物理模型 id 规范：`<provider>/<upstream_model>`。
- `family` 可选：把不同供应商的同族模型关联起来，供逻辑模型按族路由。

### 3.3 `config/logical-models.json` — 逻辑模型（对外 alias）

```jsonc
{
  "logical_models": {
    "deepseek-v4-flash-auto": {
      "params": { "temperature": 1.0, "thinking": true },   // 默认参数（可选）
      "route": {
        "strategy": "weighted",                             // priority | weighted | usage-aware
        "targets": [
          { "model": "ark/deepseek-v4-flash-260801", "weight": 8 },
          { "model": "deepseek-official/deepseek-v4-flash", "weight": 2 }
        ]
      }
    },
    "deepseek-v4-flash-260801": {
      "route": { "strategy": "priority", "targets": [ { "model": "ark/deepseek-v4-flash-260801" } ] }
    },
    "deepseek-v4-pro-auto": {
      "route": { "strategy": "priority", "targets": [ { "model": "ark/deepseek-v4-pro" } ] }
    },
    "glm-latest-auto": {
      "route": { "strategy": "priority", "targets": [ { "model": "ark/glm-5.2" } ] }
    },
    "minimax-latest-auto": {
      "route": { "strategy": "priority", "targets": [ { "model": "ark/minimax-m3" } ] }
    },
    "ark-code-latest-auto": {
      "route": { "strategy": "priority", "targets": [ { "model": "ark/ark-code-latest" } ] }
    },
    "openai-gpt-5.5-hevin": {
      "route": { "strategy": "priority", "targets": [ { "model": "openai-relay/gpt-5.5" } ] }
    },
    "openai-gpt-5.6-sol-hevin": {
      "route": { "strategy": "priority", "targets": [ { "model": "openai-relay/gpt-5.6-sol" } ] }
    },
    "deepseek-v4-flash-official": {
      "route": { "strategy": "priority", "targets": [ { "model": "deepseek-official/deepseek-v4-flash" } ] }
    },
    "deepseek-v4-pro-official": {
      "route": { "strategy": "priority", "targets": [ { "model": "deepseek-official/deepseek-v4-pro" } ] }
    }
  }
}
```

- **注意**：`deepseek-v4-flash-auto` 的主目标是 `ark/deepseek-v4-flash-260801`（固定 260801 版本，用户决策 #4），同时 `deepseek-v4-flash-official` 指向 deepseek 官方 —— 通过族把它们并到同一个逻辑模型的候选目标里，Phase 2 实现跨供应商回退。

### 3.4 保留 / 调整的文件

| 文件 | 处理 |
|---|---|
| `config/model-routes.json` | 保留：`low/high/medium-model-auto` 是**路由档位**（意图层），`target`/`fallbacks` 改为指向逻辑模型名 |
| `config/token-prices.json` | 保留，双口径统计：支持"按逻辑模型"和"按实际模型（物理模型）"两种统计视图，通过开关切换或分两个页面展示（用户决策 #3） |
| `config/key-weights.json` | 并入 `providers.json` 的 keys[].weight；文件可退役或保留为运行时覆盖层 |
| `config/custom-keys.json` | 并入 `providers.json`；退役 |
| `config/custom-model-aliases.json` | 保留：运行时 API 手动新增的逻辑模型补充 |
| `config/api-keys.json` | 保留：key 值持久化（persist=true 时），只存值不存结构 |

## 4. 关键机制

### 4.1 参数继承链（用户决策 #2）

```
客户端请求参数 ──> LogicalModel.params（默认，可选）──> PhysicalModel.params（覆写，可选）──> 上游请求
```

- 未配置参数的层不干预，透传客户端参数。
- 物理模型 `params` 为空 = 继承逻辑模型默认参数；逻辑模型默认参数为空 = 纯透传客户端。
- 路由到哪个物理模型，就应用该物理模型的参数覆写。

### 4.2 key 可用性与供应商冻结（用户决策 #3 + key 启用/停用）

**key 可用性** = `enabled && !frozen`：
- `enabled`：手动开关（配置 `enabled` 字段，默认 true；运行时 API 可切换并写回），停用的 key 不参与负载均衡。
- `frozen`：自动状态（月度/5 小时配额超限，由上游 429/403 响应触发，沿用现状 parse_quota_reset / freeze 逻辑）。

**供应商冻结判定**：仅当该供应商下**全部** key 都不可用（停用或冻结）时，该供应商才被视为冻结；逻辑模型选目标时跳过它，切到下一个可用供应商。

### 4.3 两层负载均衡（用户决策 #6）

```
第 1 层（供应商/物理模型间）：LogicalModel.route
  strategy = priority   → 按 targets 顺序取第一个"可用"（供应商未冻结且 key 池非空）
  strategy = weighted   → 按 targets[].weight 加权随机（session 粘性，复用现有 weighted_pick）
  strategy = usage-aware → 按用量选最低的可用目标（复用现有 usage_adjusted_pick）

第 2 层（供应商内部 key 间）：Provider.keys
  加权随机 + session 粘性（现有 weighted_pick），冻结 key 不参与
```

### 4.4 路由决策伪代码

```
fn resolve(alias, session):
    lm = logical_models[alias]                       # 不存在 → 未知 alias
    for target in pick_targets(lm.route, session):   # 第 1 层策略决定顺序
        pm = models[target.model]
        prov = providers[pm.provider]
        keys = prov.keys 过滤可用 (enabled && !frozen)
        if keys.empty: continue                     # 全部 key 不可用 = 供应商冻结，跳过
        key = weighted_pick(keys, session, alias)    # 第 2 层
        base_url = prov.base_url
        params = lm.params merge pm.params           # 参数继承链
        return (base_url, pm.upstream_model, key, params)
    → 所有目标不可用：返回错误/回退
```

### 4.5 key 管理（启用/停用）

- `enabled`：每把 key 的手动启用/停用开关，停用即不参与负载均衡（见 §4.2）。运行时 API 切换后写回 `providers.json` 持久化。
- key 只与供应商关联，无模型白名单；key 可用于该供应商下所有模型。

## 5. 迁移映射（现状 → v2）

| 现状 | v2 |
|---|---|
| `providers.json`（仅 base_url map） | `providers.json`（+ retry + keys） |
| `config.rs aliases()` 硬编码 ModelAlias | 拆为 `models.json`（物理）+ `logical-models.json`（逻辑） |
| ark 系 8 个 alias（共用 ark_keys） | 8 个 LogicalModel + 8 个 ark 物理模型 |
| openai-relay 系（high-model-auto 等） | 2-3 个 LogicalModel + openai-relay 物理模型 |
| deepseek-official 系 | 2 个 LogicalModel + deepseek-official 物理模型 |
| `custom-keys.json`（hevin-private/shell + aliases 白名单） | 并入 `providers.ark.keys`；aliases 白名单语义取消（key 只与供应商关联） |
| `key-weights.json` | 并入 `providers.*.keys[].weight` |
| `model-routes.json`（档位） | 保留，target/fallback 指逻辑模型名 |
| `token-prices.json`（按 alias） | 保留，按逻辑模型名 |
| `custom-model-aliases.json` | 保留（运行时逻辑模型） |
| frozen（per-key） | 保持 per-key 冻结；供应商冻结 = 全部 key 冻结的聚合判定 |
| 模型参数（无） | 新增 LogicalModel.params / PhysicalModel.params |

**兼容策略**：v2 加载器先读新 schema；若 `logical-models.json` / `models.json` 不存在，回退到 `aliases()` 硬编码（双轨），迁移完成后再移除硬编码。

## 6. 对外契约

- 客户端访问的 alias 名（`deepseek-v4-flash-auto`、`low-model-auto`、`high-model-auto` 等）**保持不变**，逻辑模型名即对外 alias。
- HTTP API（`/v1/chat/completions`、`/api/config/model-aliases`、`/api/config/model-routes` 等）路径不变，内部实现替换。
- 前端页面需要新增加：供应商管理、物理模型管理、逻辑模型路由配置视图（Phase 4）。

## 7. 实施计划

- **Phase 1** ✅：新 schema 加载器（`src/config_v2.rs`：Provider / PhysicalModel / LogicalModel 结构 + 校验 + 折叠适配 + 双轨 `effective_aliases`）+ 迁移文件落地（`providers-v2.json` / `models.json` / `logical-models.json`）。验证：单测 5 个（含真实配置等价性，`cargo test -- --ignored`）+ `cargo check` / `cargo build --release`。**尚未接入运行时**（仍走旧 `aliases()`，零回归）；Phase 2 切换。
- **Phase 2** ✅：逻辑模型展开 + 第 1 层路由策略 + 参数继承链，已接入运行时并部署验证。
  - `ModelAlias` 新增 `params`（服务器侧默认/覆写参数，应用时只填充客户端缺失字段）。
  - `config_v2::resolve_targets` 把逻辑模型展开为物理模型候选（多供应商、enabled 过滤、params 合并）；`router_state::order_targets` 按策略排序（priority 原序 / weighted 加权首选 + 降序回退）。
  - `RouterState` 集成 v2：`base_aliases`/`route_aliases` v2 模式展开，`alias_with_runtime_weights` 跳过旧覆盖层，`V2Key.persist` 保持 env-only 语义。
  - 开关：`LLM_PROVIDER_ROUTER_V2`（默认启用，设 0 回退旧逻辑）；API 快照含 `v2_enabled`。
  - 验证：19 单测 + 真实配置等价性测试 + 端到端请求（`deepseek-v4-flash-auto` → ark `deepseek-v4-flash-260801`，已部署）。
  - **Phase 2 已知限制（已解决）**：第 1 层 `usage-aware` 暂以 weighted 展开 → Phase 3 精细化；v2 模式下 custom model aliases 暂不生效 → Phase 4 接入；token-prices 默认值仍基于旧 `aliases()`（同名逻辑模型命中不受影响）。
- **Phase 3** ✅：第 1 层 `usage-aware` 精细化 + 供应商冻结/启用状态可视化数据。
  - `usage_preferred_index`：usage-aware 时跨供应商按用量选首选（每个物理模型取 key 池内最低 tokens/weight 比，选最宽裕的供应商）；`order_targets` 支持 `preferred` 注入（usage-aware 用，其余策略仍加权/原序）。
  - 新增 API `GET /api/config/v2`：v2 架构完整视图（供应商 key_total/key_enabled/key_frozen/available + 各 key enabled/frozen/reason、物理模型、逻辑模型 strategy/targets），为 Phase 4 前端管理页提供数据。
  - 清理：删除未接入的 `effective_aliases`（实际由 `load_v2_config` + `resolve_targets`/`fold_to_aliases` 取代）。
  - 验证：21 单测（含 order_targets preferred/usage-aware）+ 部署后 `v2_enabled:True`、供应商聚合（ark 8/9、garvin frozen、wilford disabled）、weighted session 粘性端到端请求。
- **Phase 4** ✅：custom model aliases 接入 v2 + 前端 v2 管理面板。
  - 后端：`custom_alias_models()` 把运行时 API 手动新增的 custom alias 作为扁平逻辑模型接入 v2（base_url/keys 取自声明 provider 的 v2 供应商，retry 用 custom 自身配置）；`v2_aliases` / `route_aliases` 均合并。
  - 前端：新增 `Architecture (v2)` 面板（Settings 页顶部），展示供应商（base_url / keys / available + 每 key enabled/frozen badge）、逻辑模型（strategy + targets）；`types.ts`/`api.ts` 增加 `V2Status` 与 `v2Status()`。
  - 验证：21 单测 + 前端 `npm run build` + 部署后：前端页面正常、v2 API 正常、临时 custom alias `my-test-model` 出现在 base_aliases 并端到端路由成功（已清理恢复）。

## 8. 已确认决策

1. 供应商冻结粒度：仅当该供应商**全部** key 都被冻结才冻结供应商；内部 key 均衡跳过冻结 key。
2. token-prices：双口径（逻辑模型 / 实际模型）统计，开关切换或分页展示。
3. `deepseek-v4-flash-auto`：保留 "260801" 固定版本逻辑（ark 上游固定指向 260801）。

## 9. 待确认

无 —— 所有设计决策已确认：
1. 供应商冻结粒度：全部 key 不可用（停用或冻结）才冻结供应商；内部均衡跳过不可用 key。
2. token-prices：双口径（逻辑模型 / 实际模型）统计，开关切换或分页展示。
3. `deepseek-v4-flash-auto`：保留 "260801" 固定版本逻辑（ark 上游固定指向 260801）。
4. key 白名单：取消，key 只与供应商关联。
5. key `enabled`（启用/停用）：写回 `providers.json` 持久化。
