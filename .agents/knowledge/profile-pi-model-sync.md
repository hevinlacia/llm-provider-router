# Pi agent 模型列表托管：models.json 最小条目 + router-context-sync 协商

用于：理解/维护 Pi agent（`~/.pi/agent`）侧 llm-provider-router 模型列表的同步机制——为什么 models.json 里 auto 模型没有 contextWindow/图片字段、加新 auto 模型要改哪里、协商 extension 如何工作。

触发词：models.json、auto 系列、router-context-sync、协商、contextWindow、图片支持 input、enabledModels、模型列表同步、pi 模型注册。

不适用：其他直连 provider（deepseek-official / oai-relay / minimax-official）的静态配置（它们不走协商）；router 侧 logical-models.json 的路由配置本身（见 `docs/architecture-v2.md`）。

---

## 分工

| 层 | 文件 | 职责 |
|---|---|---|
| Router（唯一事实源） | `config/logical-models.json` | 定义 auto 模型、targets、路由策略；`/api/router/capabilities` 输出 effective 窗口与 input 能力 |
| Pi 协商 extension | `~/.pi/agent/extensions/router-context-sync.ts`（源码在 `pi-extensions/`） | 启动时 fetch capabilities，`pi.registerProvider` 全量注册模型（含 contextWindow/maxTokens/input/reasoning/thinkingLevelMap/compat）；**过滤只注册 id 含 `auto` 的模型** |
| Pi 静态兜底 | `~/.pi/agent/models.json` | llm-provider-router.models 只放 auto 系列最小条目 `{ "id", "name" }`；extension 挂掉时兜底，不写窗口/图片元数据 |

设计要点（2026-09-28 定稿）：

1. **上下文长度和图片支持由协商 extension 负责**：models.json 条目不写 `contextWindow`/`maxTokens`/`input`（pi 的 models entry 最少只需 id，见 pi docs models.md）。窗口滑动最小由 capabilities `effective` 下发，非流式响应头 `x-llm-router-context-window` / `x-llm-router-max-output` 还会精确下修。
2. **auto 过滤**（`feat/pi-extension-auto-filter`）：sync 入口 `caps.filter(m => m.id.includes("auto"))`；过滤后为空则跳过注册，保留静态兜底不清空 provider。变更检测/重注册均基于过滤后集合。
3. **兜底链**：extension 的 `FALLBACK_PROVIDER`（baseUrl/apiKey/api 内置）保证 models.json 为空也能注册；静态 models.json 保证 extension 挂掉时仍有模型可用。

## 加新 auto 模型的操作

1. 改 router `config/logical-models.json` 加 logical model（target 的 `keys` 用 `null`，别用 `[]`，见 pitfall-logical-model-keys-empty-whitelist.md）——热重载自动生效，capabilities 出现新模型。
2. Pi 侧**自动跟随**（下次 extension sync 注册），无需改 models.json；若想静态兜底也覆盖，手动在 models.json 补一行 `{ "id": "...", "name": "..." }`。
3. `settings.json` 的 `enabledModels`（Ctrl+P 循环池）与 `defaultModel` 引用**需手动同步**。

## 坑

- **permission-gate 硬拦 write/edit 到 `~/.pi/agent/models.json`**（SENSITIVE_WRITE_PREFIXES，防凭证文件被 AI 写坏）。AI 更新它需走 bash+python 写入；内容保持 `$ENV` 引用形式，不落真实 key。
- **api 字段会被覆盖为 openai-completions**：extension 全量注册带 `FALLBACK_PROVIDER`（api=completions），运行时以它为准；models.json 里写的 `openai-responses` 仅静态兜底时生效。router 两种 API 都支持。
- **双协商 extension 会打架**：旧 `llm-router-dynamic-models.ts`（refreshModels 机制、/v1/models 口径）与 router-context-sync 同时注册会互相覆盖且口径不一致（/v1/models 不含无上游模型），已禁用为 `.disabled`。删配置文件里的模型 id 时，先 grep `~/.pi/agent/extensions/*.ts` 是否有引用（如 auto-title.ts 的 `L2_MODEL_ID` 二级模型）。
- models.json 静态列表不会自动增删，长期漂移靠 extension 协商兜底，但 enabledModels 引用已删除的 id 会在 /model 里出现无效项。

## 验证方法

```bash
# 1. 静态配置合法
python3 -c "import json; d=json.load(open('/home/hevin/.pi/agent/models.json')); print([m['id'] for m in d['providers']['llm-provider-router']['models']])"

# 2. 全链路（defaultModel 走 router）
pi -p "只回复: ok"

# 3. 运行时协商生效：rpc get_state 的 model 应带 models.json 里没有的 contextWindow/input
(echo '{"type":"get_state"}'; sleep 12) | pi --mode rpc | grep -o '"contextWindow":[0-9]*'
```
