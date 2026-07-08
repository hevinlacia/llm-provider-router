from __future__ import annotations

import asyncio
import json
import time
from typing import Any

import httpx
from fastapi import FastAPI, Header, HTTPException, Request, Response
from fastapi.responses import HTMLResponse, JSONResponse, StreamingResponse

from .config import ALIASES, KeyRef, ModelAlias, Settings, load_settings
from .dashboard import DASHBOARD_HTML
from .state import NoAvailableKeyError, RouterState, parse_quota_reset, parse_retry_after


def create_app(settings: Settings | None = None) -> FastAPI:
    settings = settings or load_settings()
    state = RouterState(settings)
    app = FastAPI(title="Ark Key Router", version="0.1.0")

    @app.get("/health")
    async def health() -> dict[str, Any]:
        return {"ok": True, **state.snapshot()}

    @app.get("/")
    async def dashboard() -> HTMLResponse:
        return HTMLResponse(DASHBOARD_HTML)

    @app.get("/dashboard")
    async def dashboard_alias() -> HTMLResponse:
        return HTMLResponse(DASHBOARD_HTML)

    @app.get("/api/state")
    async def api_state(
        period: str = "all",
        start: str | None = None,
        end: str | None = None,
    ) -> dict[str, Any]:
        return {
            "ok": True,
            **state.snapshot(),
            "usage": state.usage_snapshot(period=period, start=start, end=end),
        }

    @app.get("/api/usage")
    async def api_usage(
        period: str = "all",
        start: str | None = None,
        end: str | None = None,
    ) -> dict[str, Any]:
        return state.usage_snapshot(period=period, start=start, end=end)

    @app.get("/v1/models")
    async def models(authorization: str | None = Header(default=None)) -> dict[str, Any]:
        validate_auth(settings, authorization)
        return {
            "object": "list",
            "data": [
                {
                    "id": alias.alias,
                    "object": "model",
                    "created": 0,
                    "owned_by": "ark-key-router",
                }
                for alias in ALIASES.values()
            ],
        }

    @app.post("/api/usage/reset")
    async def api_usage_reset() -> dict[str, Any]:
        state.reset_usage()
        return {"ok": True, "usage": state.usage_snapshot()}

    @app.get("/api/config/weights")
    async def api_config_weights() -> dict[str, Any]:
        return {"ok": True, **state.key_config_snapshot()}

    @app.put("/api/config/weights")
    async def api_config_weights_update(request: Request) -> dict[str, Any]:
        payload = await request.json()
        weights = payload.get("weights") if isinstance(payload, dict) else None
        if not isinstance(weights, dict):
            raise HTTPException(status_code=400, detail="weights must be an object")
        try:
            parsed_weights = {str(name): int(weight) for name, weight in weights.items()}
            state.set_key_weights(parsed_weights)
        except (TypeError, ValueError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"ok": True, **state.key_config_snapshot()}

    @app.get("/api/config/providers")
    async def api_config_providers() -> dict[str, Any]:
        return {"ok": True, **state.provider_config_snapshot()}

    @app.put("/api/config/providers")
    async def api_config_providers_update(request: Request) -> dict[str, Any]:
        payload = await request.json()
        providers = payload.get("providers") if isinstance(payload, dict) else None
        if not isinstance(providers, dict):
            raise HTTPException(status_code=400, detail="providers must be an object")
        try:
            snapshot = state.set_provider_base_urls(
                {str(name): str(base_url) for name, base_url in providers.items()}
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"ok": True, **snapshot}

    @app.get("/api/config/keys")
    async def api_config_keys() -> dict[str, Any]:
        return {"ok": True, **state.key_secret_snapshot()}

    @app.put("/api/config/keys")
    async def api_config_keys_update(request: Request) -> dict[str, Any]:
        payload = await request.json()
        values = payload.get("keys") if isinstance(payload, dict) else None
        delete_names = payload.get("delete") if isinstance(payload, dict) else None
        if values is None:
            values = {}
        if delete_names is None:
            delete_names = []
        if not isinstance(values, dict):
            raise HTTPException(status_code=400, detail="keys must be an object")
        if not isinstance(delete_names, list):
            raise HTTPException(status_code=400, detail="delete must be a list")
        try:
            clean_values = {str(name): str(value) for name, value in values.items() if str(value)}
            snapshot = state.set_key_values(clean_values, {str(name) for name in delete_names})
        except (RuntimeError, ValueError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"ok": True, **snapshot}

    @app.post("/api/config/keys")
    async def api_config_keys_add(request: Request) -> dict[str, Any]:
        payload = await request.json()
        if not isinstance(payload, dict):
            raise HTTPException(status_code=400, detail="payload must be an object")
        aliases = payload.get("aliases")
        if not isinstance(aliases, list):
            raise HTTPException(status_code=400, detail="aliases must be a list")
        try:
            snapshot = state.add_key_to_pools(
                name=str(payload.get("name") or ""),
                value=str(payload.get("value") or ""),
                aliases=[str(alias) for alias in aliases],
                weight=int(payload.get("weight") or 1),
            )
        except (RuntimeError, ValueError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"ok": True, **snapshot}

    @app.post("/v1/chat/completions")
    async def chat_completions(
        request: Request,
        authorization: str | None = Header(default=None),
        x_litellm_session_id: str | None = Header(default=None),
        x_opencode_session_id: str | None = Header(default=None),
    ) -> Response:
        validate_auth(settings, authorization)
        payload = await request.json()
        model_name = payload.get("model")
        base_alias = ALIASES.get(model_name)
        alias = state.alias_with_runtime_weights(base_alias) if base_alias is not None else None
        if alias is None:
            raise HTTPException(status_code=404, detail=f"unsupported model alias: {model_name}")

        session_id = extract_session_id(payload, x_litellm_session_id, x_opencode_session_id)
        stream = bool(payload.get("stream"))

        upstream_payload = dict(payload)
        upstream_payload["model"] = alias.upstream_model

        if stream:
            return StreamingResponse(
                stream_upstream(alias, session_id, upstream_payload, settings, state),
                media_type="text/event-stream",
            )
        try:
            return await call_upstream(alias, session_id, upstream_payload, settings, state)
        except NoAvailableKeyError as exc:
            return all_keys_frozen_response(exc)

    return app


def validate_auth(settings: Settings, authorization: str | None) -> None:
    if settings.local_bearer_token is None:
        return
    expected = f"Bearer {settings.local_bearer_token}"
    if authorization != expected:
        raise HTTPException(status_code=401, detail="invalid local bearer token")


def extract_session_id(
    payload: dict[str, Any],
    x_litellm_session_id: str | None,
    x_opencode_session_id: str | None,
) -> str | None:
    metadata = payload.get("metadata") if isinstance(payload.get("metadata"), dict) else {}
    litellm_metadata = (
        payload.get("litellm_metadata") if isinstance(payload.get("litellm_metadata"), dict) else {}
    )
    return (
        x_litellm_session_id
        or x_opencode_session_id
        or metadata.get("session_id")
        or metadata.get("trace_id")
        or litellm_metadata.get("session_id")
        or litellm_metadata.get("trace_id")
    )


async def call_upstream(
    alias: ModelAlias,
    session_id: str | None,
    payload: dict[str, Any],
    settings: Settings,
    state: RouterState,
) -> JSONResponse:
    retry_policy = alias.retry_policy
    deadline = time.time() + retry_policy.max_retry_seconds if retry_policy else 0.0

    tried: set[str] = set()
    last_error: httpx.RequestError | HTTPException | OSError | None = None
    last_retriable_status: int | None = None
    last_retriable_content: Any = None
    last_retry_after: float | None = None

    while True:
        try:
            key = state.select_key_excluding(alias, session_id=session_id, excluded=tried)
        except NoAvailableKeyError:
            if retry_policy and time.time() < deadline:
                if isinstance(last_error, (httpx.RequestError, OSError)):
                    delay = 2.0
                else:
                    delay = _compute_retry_delay(retry_policy, deadline, last_retry_after)
                if delay > 0:
                    await asyncio.sleep(delay)
                tried.clear()
                continue
            if last_retriable_status is not None:
                return JSONResponse(
                    status_code=last_retriable_status, content=last_retriable_content
                )
            if last_error is not None and tried:
                return upstream_unavailable_response(alias, tried, last_error)
            raise
        tried.add(key.name)
        try:
            async with httpx.AsyncClient(timeout=settings.request_timeout_seconds) as client:
                response = await client.post(
                    f"{alias.base_url.rstrip('/')}/chat/completions",
                    headers=upstream_headers(key, state),
                    json=payload,
                )
        except (httpx.RequestError, HTTPException, OSError) as exc:
            last_error = exc
            state.record_usage(model=alias.alias, key_name=key.name, status_code=599, usage=None)
            continue
        body_text = response.text

        if retry_policy and response.status_code in retry_policy.retry_on_status:
            try:
                content = response.json()
            except json.JSONDecodeError:
                content = {"error": {"message": body_text, "type": "upstream_error"}}
            state.record_usage(
                model=alias.alias,
                key_name=key.name,
                status_code=response.status_code,
                usage=extract_usage(content),
            )
            last_retriable_status = response.status_code
            last_retriable_content = content
            last_retry_after = parse_retry_after(response.headers.get("retry-after"))
            continue

        maybe_freeze_key(key, response.status_code, response.headers, body_text, settings, state)
        try:
            content = response.json()
        except json.JSONDecodeError:
            content = {"error": {"message": body_text, "type": "upstream_error"}}
        state.record_usage(
            model=alias.alias,
            key_name=key.name,
            status_code=response.status_code,
            usage=extract_usage(content),
        )
        return JSONResponse(status_code=response.status_code, content=content)


async def stream_upstream(
    alias: ModelAlias,
    session_id: str | None,
    payload: dict[str, Any],
    settings: Settings,
    state: RouterState,
):
    retry_policy = alias.retry_policy
    deadline = time.time() + retry_policy.max_retry_seconds if retry_policy else 0.0

    tried: set[str] = set()
    last_error: httpx.RequestError | HTTPException | OSError | None = None
    last_retry_after: float | None = None

    while True:
        try:
            key = state.select_key_excluding(alias, session_id=session_id, excluded=tried)
        except NoAvailableKeyError:
            if retry_policy and time.time() < deadline:
                if isinstance(last_error, (httpx.RequestError, OSError)):
                    delay = 2.0
                else:
                    delay = _compute_retry_delay(retry_policy, deadline, last_retry_after)
                if delay > 0:
                    await asyncio.sleep(delay)
                tried.clear()
                continue
            yield stream_error_event(alias, tried, last_error)
            return
        tried.add(key.name)
        chunks: list[bytes] = []
        body_text = ""
        status_code = 599
        data_sent = False
        try:
            async with httpx.AsyncClient(timeout=settings.request_timeout_seconds) as client:
                async with client.stream(
                    "POST",
                    f"{alias.base_url.rstrip('/')}/chat/completions",
                    headers=upstream_headers(key, state),
                    json=payload,
                ) as response:
                    status_code = response.status_code
                    if (
                        retry_policy
                        and status_code in retry_policy.retry_on_status
                        and not data_sent
                    ):
                        async for chunk in response.aiter_bytes():
                            chunks.append(chunk)
                        body_text = b"".join(chunks).decode("utf-8", "replace")
                        state.record_usage(
                            model=alias.alias,
                            key_name=key.name,
                            status_code=status_code,
                            usage=extract_usage_from_stream(body_text),
                        )
                        last_retry_after = parse_retry_after(response.headers.get("retry-after"))
                        continue
                    async for chunk in response.aiter_bytes():
                        chunks.append(chunk)
                        yield chunk
                        data_sent = True
                    body_text = b"".join(chunks).decode("utf-8", "replace")
                    maybe_freeze_key(
                        key, response.status_code, response.headers, body_text, settings, state
                    )
        except (httpx.RequestError, HTTPException, OSError) as exc:
            if data_sent:
                yield stream_error_event(alias, tried, exc)
                return
            last_error = exc
            state.record_usage(model=alias.alias, key_name=key.name, status_code=599, usage=None)
            continue
        state.record_usage(
            model=alias.alias,
            key_name=key.name,
            status_code=status_code,
            usage=extract_usage_from_stream(body_text),
        )
        return


def upstream_headers(key: KeyRef, state: RouterState) -> dict[str, str]:
    value = state.upstream_key_value(key)
    if not value:
        raise HTTPException(status_code=503, detail=f"missing upstream key: {key.name}")
    return {"Authorization": f"Bearer {value}", "Content-Type": "application/json"}


def _compute_retry_delay(
    retry_policy: Any,
    deadline: float,
    last_retry_after: float | None,
) -> float:
    delay = retry_policy.retry_delay_seconds
    if last_retry_after is not None:
        remaining_to_retry_after = last_retry_after - time.time()
        if remaining_to_retry_after > delay:
            delay = remaining_to_retry_after
    remaining_to_deadline = max(0.0, deadline - time.time())
    return min(delay, remaining_to_deadline)


def maybe_freeze_key(
    key: KeyRef,
    status_code: int,
    headers: httpx.Headers,
    body_text: str,
    settings: Settings,
    state: RouterState,
) -> None:
    if status_code < 400:
        return
    quota = parse_quota_reset(body_text, settings)
    if quota is not None:
        until, reason = quota
        state.freeze(key.name, until=until, reason=reason)
        return
    retry_until = parse_retry_after(headers.get("retry-after"))
    if status_code == 429 and retry_until is not None:
        state.freeze(key.name, until=retry_until, reason="retry_after")


def all_keys_frozen_response(exc: NoAvailableKeyError) -> JSONResponse:
    return JSONResponse(
        status_code=429,
        content={"error": {"message": str(exc), "type": "all_keys_frozen"}},
        headers={"Retry-After": str(exc.retry_after)},
    )


def upstream_unavailable_response(
    alias: ModelAlias,
    tried: set[str],
    exc: httpx.RequestError | HTTPException | OSError,
) -> JSONResponse:
    return JSONResponse(
        status_code=503,
        content={
            "error": {
                "message": f"all {len(tried)} upstream keys failed for {alias.alias}",
                "type": "upstream_connect_error",
                "last_error": type(exc).__name__,
            }
        },
    )


def stream_error_event(
    alias: ModelAlias,
    tried: set[str],
    exc: httpx.RequestError | HTTPException | OSError | None,
) -> bytes:
    error = {
        "error": {
            "message": f"all {len(tried)} upstream keys failed for {alias.alias}",
            "type": "upstream_connect_error",
            "last_error": type(exc).__name__ if exc is not None else "NoAvailableKeyError",
        }
    }
    return f"data: {json.dumps(error)}\n\ndata: [DONE]\n\n".encode()


def extract_usage(content: Any) -> dict | None:
    if not isinstance(content, dict):
        return None
    usage = content.get("usage")
    return usage if isinstance(usage, dict) else None


def extract_usage_from_stream(body_text: str) -> dict | None:
    usage: dict | None = None
    for line in body_text.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        data = line.removeprefix("data:").strip()
        if not data or data == "[DONE]":
            continue
        try:
            payload = json.loads(data)
        except json.JSONDecodeError:
            continue
        chunk_usage = extract_usage(payload)
        if chunk_usage is not None:
            usage = chunk_usage
    return usage
