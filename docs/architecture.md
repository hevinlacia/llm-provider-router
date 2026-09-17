# Architecture — 当前实际架构总览

> 用于：新会话快速建立项目心智模型——模块地图、进程模型、配置权威来源、验证命令。
> 触发词：架构、模块、进程、部署、配置文件、front-proxy、blue/green、验证命令。
> 不适用：v2 分层设计的历史决策与推导过程（见 `architecture-v2.md`）、部署操作细则（见项目 `AGENTS.md`）。

## 进程模型

一个二进制（`target/release/llm-provider-router`）两种运行模式，由首个 CLI 参数决定：

```
front-proxy (:8789)                 backend (@blue / @green, systemd user unit)
┌─────────────────────┐   proxy    ┌──────────────────────────────────┐
│ 流量入口 + 槽生命周期管理 │ ────────→ │ OpenAI/Anthropic/Responses 兼容 API │
│ /_proxy/health       │            │ + dashboard 静态页 + /api/* 管理  │
│ idle 自动下线/拉起    │            │ usage.sqlite3 / state.sqlite3    │
└─────────────────────┘            └──────────────────────────────────┘
```

- **front-proxy**（`src/front_proxy.rs` + `src/slot_manager.rs`）：唯一对外端口。管理 blue/green 双槽生命周期——活跃槽异常时 `ensure_running` 另一槽并短等重试；非活跃槽连续无流量超过 `LLM_PROVIDER_ROUTER_IDLE_SHUTDOWN_SECONDS`（默认 900s）自动下线，切回时拉起。
- **backend**（`src/main.rs` → `app.rs` → `routes/serve`）：承载全部业务。blue/green 部署由 `bin/hot-deploy-router.py` 编排（先验证后切换，细则见 `AGENTS.md` Deploy & Switch 节）。
- **优雅退出**：SIGTERM 后停接新连接、等存量流（85s 看门狗 < systemd TimeoutStopSec 90s）。

## 模块地图（src/）

```
src/
├── main.rs                 # 入口：分发 backend / front-proxy 模式
├── app.rs                  # AppState 组装（锁 + 各 config/store + HTTP client）
├── config.rs               # Settings、路径常量、KeyRef/ModelAlias/RetryPolicy 基础类型
├── routes/                 # HTTP 薄层（handler 只做提取/状态码映射）
│   ├── mod.rs              #   serve(): 路由表 + 中间件（路由表即 API 清单）
│   ├── chat.rs             #   POST /v1/chat/completions、/v1/search
│   ├── messages.rs         #   POST /v1/messages（Anthropic）
│   ├── responses.rs        #   POST /v1/responses 及子资源
│   ├── models.rs           #   /v1/models、/api/router/capabilities
│   ├── config.rs           #   v1/通用配置 API（权重/别名/价格/keys/搜索供应商）
│   ├── config_v2.rs        #   v2 配置管理 API（供应商/逻辑模型/虚拟模型/物理模型）
│   └── resp.rs             #   共享响应工具
├── features/
│   ├── chat/               # 核心转发链：payload 准备→选 alias/key→upstream→SSE 流
│   ├── router/             # 路由状态机：state/（v1+v2 权威状态）、selection、
│   │                       #   costing、freeze（key 冻结）、keys
│   ├── responses/          # Responses API 协议适配
│   │   ├── translate/{request,response}.rs   #   请求/响应纯函数翻译
│   │   └── stream/                           #   SSE 翻译状态机
│   └── anthropic/          # Anthropic Messages 协议适配（透传/翻译两种模式）
│       └── translate/{request,response,sse}.rs
├── config/v2/              # v2 分层配置：types/io/validate/fold/resolve/mutate
├── json_config.rs          # v1 配置文件读写（TokenPrice/别名/keys 活跃；权重/供应商已旁路）
├── front_proxy.rs          # 前置代理（转发 + 槽选择）
├── slot_manager.rs         # blue/green 槽生命周期
├── hot_reload.rs           # config/*.json 文件监听热加载
├── shutdown.rs             # 优雅退出
├── usage_store.rs          # 用量统计（sqlite）
├── state_store.rs          # 运行时状态持久化（key 冻结等）
├── search.rs               # /v1/search 聚合搜索（tavily/exa/brave）
└── diag.rs                 # 诊断日志
```

## 配置文件权威来源（config/）

| 文件 | 状态 | 说明 |
|---|---|---|
| `providers-v2.json` | **v2 权威** | 供应商 + base_url + keys（weight/enabled/billing_type） |
| `models.json` | **v2 权威** | 物理模型（`<provider>/<upstream_model>`）+ 模型族 + 窗口参数 |
| `logical-models.json` | **v2 权威** | 逻辑模型（对外 alias）：route strategy/targets + 默认 params |
| `custom-model-aliases.json` | 活跃 | 运行时 API 手动新增的逻辑模型补充 |
| `token-prices.json` | 活跃 | 计价（双口径：逻辑模型/物理模型） |
| `api-keys.json` | 活跃 | key 值持久化（gitignore，persist=true 时写回） |
| `search-providers.json` | 活跃 | 搜索供应商配置 |
| `provider-models.json` | 活跃（缓存） | 供应商 `/models` 拉取缓存（gitignore） |
| `providers.json` / `custom-keys.json` / `key-weights.json` | **v1 遗留** | v2 模式下运行时已旁路（见下节退役计划） |

配置变更：手工编辑 `config/*.json` 会被 `hot_reload.rs` 监听自动重载；API 修改直接写回文件。**config 运行时快照会随服务运行漂移并以 git commit 形式同步**（提交信息 `config(router): 本机运行快照`），属预期行为。

## v1 配置退役计划（未完成项）

v2 上线后以下 v1 结构仍在代码中但已旁路（`features/router/state/config.rs` 明确 v2 以 `providers-v2.json` 为权威、不套旧覆盖层）：

- 仍被持有的死重：`AppState` 的 `weight_config` / `provider_config` / `custom_key_config` 字段（`json_config.rs` 的 `KeyWeightConfig` / `ProviderConfig` / `CustomKeyPoolConfig`）；`/api/config/weights`、`/api/config/providers` 两个 endpoint 及前端对应面板；`config.rs` 的 `DEFAULT_WEIGHT_CONFIG_PATH` 等常量。
- 退役步骤：确认前端 Settings 面板对应 UI 下线 → 删 endpoint + AppState 字段 + json_config 死结构 + 三个 v1 JSON 文件 → 回归 v1 回退路径（`LLM_PROVIDER_ROUTER_V2=0`）随退役一并删除的决策。
- 前置条件：v1 回退开关（`LLM_PROVIDER_ROUTER_V2=0`）确认废弃后才能删 v1 读取链路。

## 验证命令

```bash
cargo fmt && cargo clippy --all-targets && cargo test   # 后端（96 单测）
cd frontend && npm run build                             # 前端构建
python3 bin/hot-deploy-router.py status                  # 部署后槽健康（AGENTS.md 规则）
```

## 前端（frontend/）

React + Vite 单页（`src/` 扁平 + `features/`）：`home`（用量看板）、`analytics`、`settings`（v2 管理面板 / 价格 / 别名 / keys / 搜索供应商）。API 层集中在 `src/api.ts`，类型在 `src/types.ts`。
