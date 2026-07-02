# Ark Key Router

OpenAI-compatible key-router proxy for Ark-backed model aliases.

This project is a small replacement path for LiteLLM's key-pool routing. It keeps the stateful parts outside LiteLLM:

- Session affinity: `x-litellm-session-id`, `x-opencode-session-id`, or request metadata binds a session to one key.
- Sliding TTL: active session bindings refresh for 1 hour by default.
- Quota freeze: provider quota errors freeze the selected key until the reset timestamp from the error message.
- Fallback freeze: if the provider gives no reset timestamp, monthly quota freezes for 24 hours and 5-hour quota freezes for 1.5 hours.
- Failover: if a bound key is frozen, the same session is rebound to another healthy key.
- Streaming: SSE streams are proxied without buffering the whole response.

## Intended Deployment

Recommended path:

```text
OpenCode / headroom-proxy -> ark-key-router -> Ark OpenAI-compatible API
```

LiteLLM can stay online for non-Ark providers while Ark `*-auto` aliases move here first.

## Quick Start

```bash
cd /home/hevin/Developer/playground/ark-key-router
uv sync
uv run ark-key-router
```

Health check:

```bash
curl http://127.0.0.1:8789/health
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

Configuration is environment-variable based. The default aliases mirror the LiteLLM router config in `src/ark_key_router/config.py`. LiteLLM-style model names keep the `openai/` provider prefix in config, but the proxy sends the stripped model name to the Ark OpenAI-compatible endpoint.

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
ARK_KEY_ROUTER_HOST=127.0.0.1
ARK_KEY_ROUTER_PORT=8789
ARK_KEY_ROUTER_SESSION_TTL_SECONDS=3600
ARK_KEY_ROUTER_MONTHLY_QUOTA_FALLBACK_SECONDS=86400
ARK_KEY_ROUTER_5H_QUOTA_FALLBACK_SECONDS=5400
ARK_KEY_ROUTER_REQUEST_TIMEOUT_SECONDS=600
```

No real key values should be committed or printed.

## Current Model Aliases

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
