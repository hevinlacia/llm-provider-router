from __future__ import annotations

import json
import os
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

    @app.post("/api/usage/reset")
    async def api_usage_reset() -> dict[str, Any]:
        state.reset_usage()
        return {"ok": True, "usage": state.usage_snapshot()}

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
        alias = ALIASES.get(model_name)
        if alias is None:
            raise HTTPException(status_code=404, detail=f"unsupported model alias: {model_name}")

        session_id = extract_session_id(payload, x_litellm_session_id, x_opencode_session_id)
        stream = bool(payload.get("stream"))

        try:
            key = state.select_key(alias, session_id=session_id)
        except NoAvailableKeyError as exc:
            return JSONResponse(
                status_code=429,
                content={"error": {"message": str(exc), "type": "all_keys_frozen"}},
                headers={"Retry-After": str(exc.retry_after)},
            )

        upstream_payload = dict(payload)
        upstream_payload["model"] = alias.upstream_model

        if stream:
            return StreamingResponse(
                stream_upstream(alias, key, upstream_payload, settings, state),
                media_type="text/event-stream",
            )
        return await call_upstream(alias, key, upstream_payload, settings, state)

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
    key: KeyRef,
    payload: dict[str, Any],
    settings: Settings,
    state: RouterState,
) -> JSONResponse:
    async with httpx.AsyncClient(timeout=settings.request_timeout_seconds) as client:
        response = await client.post(
            f"{alias.base_url.rstrip('/')}/chat/completions",
            headers=upstream_headers(key),
            json=payload,
        )
    body_text = response.text
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
    key: KeyRef,
    payload: dict[str, Any],
    settings: Settings,
    state: RouterState,
):
    chunks: list[bytes] = []
    body_text = ""
    status_code = 599
    async with httpx.AsyncClient(timeout=settings.request_timeout_seconds) as client:
        async with client.stream(
            "POST",
            f"{alias.base_url.rstrip('/')}/chat/completions",
            headers=upstream_headers(key),
            json=payload,
        ) as response:
            status_code = response.status_code
            async for chunk in response.aiter_bytes():
                chunks.append(chunk)
                yield chunk
            body_text = b"".join(chunks).decode("utf-8", "replace")
            maybe_freeze_key(
                key, response.status_code, response.headers, body_text, settings, state
            )
    state.record_usage(
        model=alias.alias,
        key_name=key.name,
        status_code=status_code,
        usage=extract_usage_from_stream(body_text),
    )


def upstream_headers(key: KeyRef) -> dict[str, str]:
    value = os.environ.get(key.env_var)
    if not value:
        raise HTTPException(status_code=503, detail=f"missing upstream key env: {key.env_var}")
    return {"Authorization": f"Bearer {value}", "Content-Type": "application/json"}


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
