# LLM Provider Router

OpenAI-compatible provider/key router for local model aliases.

The project is now split into:

- **Rust backend** (`axum` + `reqwest` + `rusqlite`) for routing, usage metrics, state persistence, and the blue/green front proxy.
- **TypeScript + React frontend** (`Vite`) for the dashboard and settings UI.

It preserves the old API/config contracts so existing OpenCode/headroom-proxy clients can keep using `http://127.0.0.1:8789`.

## What It Does

- Session affinity: binds `x-litellm-session-id`, `x-opencode-session-id`, or request metadata to one upstream key.
- Sliding TTL: active session bindings refresh for 1 hour by default.
- Quota freeze: provider quota/auth errors freeze the selected key until reset/fallback time.
- Failover: `*-auto` aliases retry across healthy keys and configured model-route fallbacks.
- Streaming: SSE chat completion streams are proxied without buffering the full response.
- Usage metrics: request/error/token counts persist to SQLite by model, key, status, day, and month.
- Hot settings: provider URLs, key weights, model routes, and encrypted API keys can be edited from the dashboard.
- Blue/green deploy: stable front proxy on `:8789` forwards to backend slots `:8790` / `:8791`.

## Intended Deployment

```text
OpenCode / headroom-proxy -> llm-provider-router front proxy :8789 -> blue/green backend :8790/:8791 -> upstream OpenAI-compatible APIs
```

The front proxy reads `~/.local/state/llm-provider-router/active-backend.json` and forwards new requests to the active backend slot.

## Quick Start

```bash
cd /home/hevin/Developer/tools/llm-provider-router
npm --prefix frontend install
npm --prefix frontend run build
cargo build --release
bin/install-service.sh
```

Health checks:

```bash
curl http://127.0.0.1:8789/health
curl http://127.0.0.1:8789/_proxy/health
```

Dashboard:

```bash
xdg-open http://127.0.0.1:8789/dashboard
```

Hot deploy:

```bash
bin/hot-deploy-router.py deploy
bin/hot-deploy-router.py status
```

## Development

Run backend directly:

```bash
cargo run -- backend
```

Run front proxy directly:

```bash
cargo run -- front-proxy
```

Run React dashboard in dev mode:

```bash
npm --prefix frontend run dev
```

Build everything:

```bash
npm run build
```

## API Surface

- `GET /health` — backend health, frozen keys, binding count.
- `GET /`, `GET /dashboard` — React dashboard shell.
- `GET /api/state` — state + usage snapshot.
- `GET /api/usage` — usage metrics; supports `period`, `start`, and `end`.
- `POST /api/usage/reset` — clear usage events.
- `POST /api/frozen/clear` — clear frozen keys.
- `GET/PUT /api/config/weights` — key routing weights.
- `GET/PUT /api/config/providers` — provider base URLs.
- `GET/PUT/POST /api/config/keys` — encrypted key metadata/update/add.
- `GET /v1/models` — OpenAI-compatible model list (enriched with `context_window`/`max_output_tokens` for dynamic context negotiation).
- `GET /api/router/capabilities` — dynamic context negotiation view: per logical model `effective: {contextWindow,maxTokens}` (conservative min across available physical targets) + per-target windows/availability.
- `POST /v1/chat/completions` — OpenAI-compatible chat completions (non-streaming responses include `x-llm-router-*` headers for per-request precise window; streaming includes conservative hint), streaming and non-streaming.
- `POST /v1/search` — unified web search proxy (search key pool): authenticated with the same local bearer token, routes to Tavily/Exa/Brave by key pool. See [Search Key Pool](#search-key-pool).
- `GET/PUT /api/config/search-providers` — inspect/update the search key pool configuration.
- `GET /_proxy/health` — front-proxy backend health.
- `POST /_proxy/active/{slot}` — switch active blue/green slot.

## Configuration

The Rust backend keeps the same environment variables and JSON files as the previous implementation.

Common settings:

```text
LLM_PROVIDER_ROUTER_HOST=127.0.0.1
LLM_PROVIDER_ROUTER_PORT=8789
LLM_PROVIDER_ROUTER_SESSION_TTL_SECONDS=3600
LLM_PROVIDER_ROUTER_MONTHLY_QUOTA_FALLBACK_SECONDS=86400
LLM_PROVIDER_ROUTER_5H_QUOTA_FALLBACK_SECONDS=5400
LLM_PROVIDER_ROUTER_AUTH_INVALID_FREEZE_SECONDS=86400
LLM_PROVIDER_ROUTER_REQUEST_TIMEOUT_SECONDS=600
LLM_PROVIDER_ROUTER_BEARER_TOKEN=<optional; local auth token, read from environment only; falls back to LLM_PROVIDER_ROUTER_API_KEY>
LLM_PROVIDER_ROUTER_USAGE_DB_PATH=~/.local/state/llm-provider-router/usage.sqlite3
LLM_PROVIDER_ROUTER_STATE_DB_PATH=~/.local/state/llm-provider-router/state.sqlite3
LLM_PROVIDER_ROUTER_WEIGHT_CONFIG_PATH=config/key-weights.json
LLM_PROVIDER_ROUTER_PROVIDER_CONFIG_PATH=config/providers.json
LLM_PROVIDER_ROUTER_CUSTOM_KEY_CONFIG_PATH=config/custom-keys.json
LLM_PROVIDER_ROUTER_SEARCH_PROVIDERS_PATH=config/search-providers.json
```

## Search Key Pool

The router exposes a **unified web search endpoint** `POST /v1/search` that hides multiple search providers (Tavily / Exa / Brave) behind the router's single local bearer token — clients only need one key.

### Configuration (`config/search-providers.json`)

```jsonc
{
  "providers": {
    "tavily": {
      "base_url": "https://api.tavily.com",          // optional, defaults to official endpoint
      "keys": {
        "hevin":  { "env_var": "AGENT_SEARCH_TAVILY_HEVIN_API_KEY",  "weight": 5, "enabled": true },
        "backup": { "env_var": "AGENT_SEARCH_TAVILY_BACKUP_API_KEY", "weight": 3, "enabled": true }
      }
    },
    "exa":   { "keys": { "hevin": { "env_var": "AGENT_SEARCH_EXA_HEVIN_API_KEY",  "weight": 5 } } },
    "brave": { "keys": { "hevin": { "env_var": "AGENT_SEARCH_BRAVE_HEVIN_API_KEY", "weight": 5 } } }
  }
}
```

- Provider names must be one of `tavily` / `exa` / `brave`; `base_url` is optional (official endpoint is the default).
- `keys.<name>.env_var` reads the actual key value from the environment; `weight` controls weighted random selection inside the provider; `enabled: false` takes the key out of rotation.
- Key values are never written to disk by the router — configure them in the environment (e.g. `~/.config/opencode/agent-secrets.env`).

### Request

```bash
curl -X POST http://127.0.0.1:8789/v1/search \
  -H "Authorization: Bearer <router-local-token>" \
  -H "Content-Type: application/json" \
  -d '{
    "query": "Spring Boot 4 requirements",
    "max_results": 5,
    "provider": "auto",                 // auto | tavily | exa | brave
    "search_depth": "basic",            // tavily: basic | advanced
    "topic": "general",                 // tavily: general | news | finance
    "time_range": "week",               // tavily: optional day|week|month|year
    "include_answer": false,
    "include_domains": ["docs.spring.io"],
    "exclude_domains": ["reddit.com"]
  }'
```

- `provider` defaults to `auto`: the router picks a provider weighted by the sum of its available key weights, then picks a key inside the provider.
- With an explicit provider, only that provider's pool is used.

### Response (unified)

```json
{
  "provider": "tavily",
  "query": "Spring Boot 4 requirements",
  "results": [
    { "title": "...", "url": "...", "snippet": "...", "published_date": "...", "score": 0.9 }
  ],
  "answer": "..."     // present only when include_answer=true and the provider supports it
}
```

### Manage the pool

```bash
# Inspect (shows env_var/weight/enabled/configured for each key)
curl http://127.0.0.1:8789/api/config/search-providers -H "Authorization: Bearer <token>"

# Update (whole file, persisted back to config/search-providers.json)
curl -X PUT http://127.0.0.1:8789/api/config/search-providers \
  -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"providers":{...}}'
```

### Consume from pi

Point the pi `web_search` extension at the router instead of a raw provider:

```bash
export TAVILY_API_KEY=<router-local-token>   # extension auth key
# and point the extension base URL at http://127.0.0.1:8789 if you customize it
```

Front proxy settings:

```text
LLM_PROVIDER_ROUTER_PROXY_HOST=127.0.0.1
LLM_PROVIDER_ROUTER_PROXY_PORT=8789
LLM_PROVIDER_ROUTER_BLUE_URL=http://127.0.0.1:8790
LLM_PROVIDER_ROUTER_GREEN_URL=http://127.0.0.1:8791
LLM_PROVIDER_ROUTER_ACTIVE_BACKEND_FILE=~/.local/state/llm-provider-router/active-backend.json
LLM_PROVIDER_ROUTER_DEFAULT_SLOT=blue
```

API key values are read from environment variables loaded from `~/.config/opencode/agent-secrets.env`. The encrypted source of truth is now `~/Developer/vault`; restore it with `python3 ~/Developer/vault/scripts/vault.py restore`. The dashboard can still show whether keys are configured and can update runtime env values, but persistent key changes should be made through the vault/env file flow.

## Persistence

Two SQLite files are used by default:

- `usage.sqlite3` — usage event log and startup timestamp.
- `state.sqlite3` — frozen keys and session bindings.

The Rust schema intentionally matches the previous SQLite tables so existing local state survives the migration.

## Current Model Aliases

- `high-model-auto` -> configurable route, default `openai-gpt-5.5-hevin`, fallback `glm-latest-auto`
- `medium-model-auto` -> configurable route, default `glm-latest-auto`, fallback `deepseek-v4-pro-auto`
- `low-model-auto` -> configurable route, default `deepseek-v4-flash-auto`, fallback `glm-latest-auto`
- `glm-latest-auto` -> `openai/glm-5.2`
- `deepseek-v4-pro-auto` -> `openai/deepseek-v4-pro`
- `deepseek-v4-flash-auto` -> `openai/deepseek-v4-flash`
- `minimax-latest-auto` -> `openai/minimax-m3`

## Dynamic Context Negotiation (Pi × Router)

`*-auto` models route across suppliers with different real `contextWindow`/`maxOutput`. Pi was using static `~/.pi/agent/models.json` thresholds, causing `compaction`/`isContextOverflow` mismatch. The router now negotiates dynamically:

- `GET /api/router/capabilities` → per logical model `effective` (min across available targets) + `targets[]` detail.
- `GET /v1/models` → enriched with `context_window`/`max_output_tokens` (from capabilities effective, fallback to physical declaration).
- `POST /v1/chat/completions` → non-streaming responses carry `x-llm-router-{model,upstream-model,provider,context-window,max-output}` for precise per-request correction; streaming carries conservative hint from preferred target.

Pi extension: `pi-extensions/router-context-sync.ts` (copy to `~/.pi/agent/extensions/` and `/reload`) polls `capabilities` (fallback `v1/models`) and `after_provider_response` headers, then hot-patches via `pi.registerProvider(llm-provider-router, {modelOverrides})` — no Pi core change. Settings UI shows `Context Negotiation` panel with effective windows.

## Pi: B 全量托管（推荐）

让模型与窗口完全由 Router 定，Pi 只保留 provider 声明：

```bash
# 1) 精简 Pi 模型配置（备份原有）
cp ~/.pi/agent/models.json ~/.pi/agent/models.json.bak
cp pi-models.minimal.json ~/.pi/agent/models.json
# 2) 安装托管扩展
cp pi-extensions/router-context-sync.ts ~/.pi/agent/extensions/router-context-sync.ts
# 3) 重启 pi 或 /reload
```

`settings.json` 保留 `enabledModels` 白名单即可（`"llm-provider-router/low-model-auto"` 等），窗口与 reasoning/input/thinkingLevelMap 由 Router 的 `logical-models.json` 统一管理；切换模型或 Router 改窗口后，Pi 下次启动/轮询/命中响应头即自动热更新。
