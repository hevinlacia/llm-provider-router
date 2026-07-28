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
- `GET/PUT /api/config/model-routes` — virtual model route targets/fallbacks.
- `GET/PUT /api/config/providers` — provider base URLs.
- `GET/PUT/POST /api/config/keys` — encrypted key metadata/update/add.
- `GET /v1/models` — OpenAI-compatible model list.
- `POST /v1/chat/completions` — OpenAI-compatible chat completions, streaming and non-streaming.
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
LLM_PROVIDER_ROUTER_BEARER_TOKEN=<optional; falls back to config/router-auth.json, then LLM_PROVIDER_ROUTER_API_KEY>
LLM_PROVIDER_ROUTER_USAGE_DB_PATH=~/.local/state/llm-provider-router/usage.sqlite3
LLM_PROVIDER_ROUTER_STATE_DB_PATH=~/.local/state/llm-provider-router/state.sqlite3
LLM_PROVIDER_ROUTER_WEIGHT_CONFIG_PATH=config/key-weights.json
LLM_PROVIDER_ROUTER_PROVIDER_CONFIG_PATH=config/providers.json
LLM_PROVIDER_ROUTER_CUSTOM_KEY_CONFIG_PATH=config/custom-keys.json
LLM_PROVIDER_ROUTER_MODEL_ROUTE_CONFIG_PATH=config/model-routes.json
LLM_PROVIDER_ROUTER_AUTH_CONFIG_PATH=config/router-auth.json
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
