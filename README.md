# LLM Provider Router

OpenAI-compatible key-router proxy for Ark-backed model aliases.

This project is a small replacement path for LiteLLM's key-pool routing. It keeps the stateful parts outside LiteLLM:

- Session affinity: `x-litellm-session-id`, `x-opencode-session-id`, or request metadata binds a session to one key.
- Sliding TTL: active session bindings refresh for 1 hour by default.
- Quota freeze: provider quota errors freeze the selected key until the reset timestamp from the error message.
- Fallback freeze: if the provider gives no reset timestamp, monthly quota freezes for 24 hours and 5-hour quota freezes for 1.5 hours.
- Auth freeze: 401/403 authentication errors (revoked/expired/invalid key, including quota exhaustion surfacing as auth errors) freeze the key for a configurable duration (default 24 hours) so failover routes around it.
- Failover: Ark `*-auto` aliases retry across keys on 401/402/429/5xx; if a bound key is frozen, the same session is rebound to another healthy key.
- Streaming: SSE streams are proxied without buffering the whole response.
- Usage metrics: request/error counts and OpenAI `usage` tokens are tracked in memory by model, key, and status code.

## Intended Deployment

Recommended path:

```text
OpenCode / headroom-proxy -> llm-provider-router front proxy :8789 -> blue/green router backend :8790/:8791 -> Ark OpenAI-compatible API
```

The stable client endpoint remains `http://127.0.0.1:8789`. The front proxy reads
`~/.local/state/llm-provider-router/active-backend.json` and forwards new requests to the
active backend slot. Backends run on `127.0.0.1:8790` (`blue`) and `127.0.0.1:8791`
(`green`), so a deploy can start the inactive slot, health-check it, switch new traffic,
and let existing streaming requests drain on the old slot before stopping it.

## Quick Start

```bash
cd /home/hevin/Developer/tools/llm-provider-router
uv sync
bin/install-service.sh
```

Health check:

```bash
curl http://127.0.0.1:8789/health
curl http://127.0.0.1:8789/_proxy/health
```

Hot deploy after updating code:

```bash
cd /home/hevin/Developer/tools/llm-provider-router
uv sync
bin/hot-deploy-router.py deploy
```

The deploy command starts the inactive backend slot, waits for `/health`, atomically
switches the active backend file, waits for the drain window, and then stops the old slot.
Use `--drain-seconds <n>` to tune the drain period or `bin/hot-deploy-router.py status`
to inspect the current active slot.

Dashboard:

```bash
xdg-open http://127.0.0.1:8789/dashboard
```

The dashboard includes a **Key Weights** panel. Saving weights writes
`config/key-weights.json`, and new requests use the updated ratios immediately
without restarting the service.

Usage metrics API:

```bash
curl http://127.0.0.1:8789/api/usage
curl 'http://127.0.0.1:8789/api/usage?period=today'
curl 'http://127.0.0.1:8789/api/usage?period=month'
curl 'http://127.0.0.1:8789/api/usage?start=2026-07-01&end=2026-07-31'
curl -X POST http://127.0.0.1:8789/api/usage/reset
```

Example request:

```bash
curl -N http://127.0.0.1:8789/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer local-dev' \
  -H 'x-litellm-session-id: demo-session' \
  -d '{"model":"glm-latest-auto","messages":[{"role":"user","content":"say ok"}],"stream":true}'
```

## Configuration

Configuration is environment-variable based. The default aliases mirror the LiteLLM router config in `src/llm_provider_router/config.py`. LiteLLM-style model names keep the `openai/` provider prefix in config, but the proxy sends the stripped model name to the Ark OpenAI-compatible endpoint.

Required Ark key variables:

```text
OPENCODE_AI_ARK_GARVIN_API_KEY
OPENCODE_AI_ARK_WILFORD_API_KEY
OPENCODE_AI_ARK_HEVIN_API_KEY
OPENCODE_AI_ARK_KHAINE_API_KEY
OPENCODE_AI_ARK_CYRIL_API_KEY
OPENCODE_AI_ARK_MOSS_API_KEY
```

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
LLM_PROVIDER_ROUTER_WEIGHT_CONFIG_PATH=config/key-weights.json
LLM_PROVIDER_ROUTER_PROVIDER_CONFIG_PATH=config/providers.json
LLM_PROVIDER_ROUTER_AUTH_CONFIG_PATH=config/router-auth.json
LLM_PROVIDER_ROUTER_KEY_CONFIG_PATH=config/api-keys.sops.json
LLM_PROVIDER_ROUTER_SOPS_AGE_RECIPIENT=age1n4kxrm8969pqaax2u63akszmdgvu5dr2tfnwpt2d957ewtwx4sescvvz7d
SOPS_AGE_KEY_FILE=~/.config/sops/age/keys.txt
```

No real key values should be committed or printed.

API keys can be managed from the dashboard Settings page. Values are written to
`config/api-keys.sops.json` encrypted with SOPS age recipient
`age1n4kxrm8969pqaax2u63akszmdgvu5dr2tfnwpt2d957ewtwx4sescvvz7d`; the router decrypts
that file locally with `SOPS_AGE_KEY_FILE` when sending upstream requests. The API and
dashboard only expose whether a key is configured, never the plaintext key value.
Keys are grouped by provider in Settings and include a billing marker. Current billing
types are `subscription` for Ark/OpenAI relay keys and `payg` for the official DeepSeek key.

Provider base URLs are stored in `config/providers.json` and can be edited from Settings.
New requests pick up provider URL changes immediately without restarting the router.

Model tier routes are stored in `config/model-routes.json`. The router exposes
`high-model-auto`, `medium-model-auto`, and `low-model-auto` as stable virtual model
names; each route points to an existing concrete model alias and can optionally list
fallback aliases. Defaults are:

```json
{
  "high-model-auto": {"target": "openai-gpt-5.5-hevin", "fallbacks": ["glm-latest-auto"]},
  "medium-model-auto": {"target": "glm-latest-auto", "fallbacks": ["deepseek-v4-pro-auto"]},
  "low-model-auto": {"target": "deepseek-v4-flash-auto", "fallbacks": ["glm-latest-auto"]}
}
```

Fallbacks are tried in order after the primary route has no available upstream key. Leave
`fallbacks` empty to disable fallback routing.

The router's own bearer token (used to authenticate incoming `Authorization: Bearer ...`
requests from OpenCode and the dashboard) is read in this order:

1. `LLM_PROVIDER_ROUTER_BEARER_TOKEN` environment variable.
2. `config/router-auth.json` — a plaintext file `{ "bearer_token": "..." }` shipped
   in the repository. Unlike the upstream `api-keys.sops.json` keys, this is a
   low-risk local-only token and is committed to git so it syncs across machines
   together with the router itself. Override the path with
   `LLM_PROVIDER_ROUTER_AUTH_CONFIG_PATH`.
3. `LLM_PROVIDER_ROUTER_API_KEY` for compatibility with existing OpenCode provider
   configuration.

Key routing weights are stored in `config/key-weights.json` by default, so the
preferred local ratios can be committed and synced to GitHub without committing
any secrets. The dashboard can update this file through `/api/config/weights`;
new requests pick up the updated weights immediately without restarting the
router. Existing session bindings continue until they expire unless a key is set
to weight `0`, which drops active bindings for that key so it stops receiving
new routed requests.

Usage metrics are persisted to SQLite by default and can be filtered by `period`, `start`,
and `end`. The API returns total request counts, token counts, daily/monthly rollups, and
cache hit rate from `prompt_tokens_details.cached_tokens` when the upstream returns it.
PostgreSQL is not required for the local single-instance deployment; SQLite keeps the service
self-contained while still surviving restarts. Move to PostgreSQL only if multiple router
instances need to share the same metrics store or if long-term cross-host reporting becomes
necessary.

## Current Model Aliases

- `high-model-auto` -> configurable route, default `openai/gpt-5.5`, fallback `glm-latest-auto`
- `medium-model-auto` -> configurable route, default `glm-latest-auto`, fallback `deepseek-v4-pro-auto`
- `low-model-auto` -> configurable route, default `deepseek-v4-flash-auto`, fallback `glm-latest-auto`
- `glm-latest-auto` -> `openai/glm-5.2`
- `deepseek-v4-pro-auto` -> `openai/deepseek-v4-pro`
- `deepseek-v4-flash-auto` -> `openai/deepseek-v4-flash`
- `minimax-latest-auto` -> `openai/minimax-m3`

## Replacement Plan

1. Run this router on a new local port.
2. Point only one Ark auto alias through it.
3. Validate session stickiness and quota freeze behavior for 1-2 days.
4. Move the remaining Ark auto aliases.
5. Keep LiteLLM for non-Ark providers until this proxy covers all needed traffic.
