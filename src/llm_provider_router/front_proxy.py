from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass
from pathlib import Path
from typing import AsyncIterator

import httpx
import uvicorn
from fastapi import FastAPI, Request, Response
from fastapi.responses import JSONResponse, StreamingResponse


DEFAULT_STATE_DIR = "~/.local/state/llm-provider-router"
DEFAULT_ACTIVE_BACKEND_FILE = "~/.local/state/llm-provider-router/active-backend.json"
HOP_BY_HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}
REQUEST_SKIP_HEADERS = HOP_BY_HOP_HEADERS | {"host", "content-length"}
RESPONSE_SKIP_HEADERS = HOP_BY_HOP_HEADERS | {"content-length", "content-encoding", "date", "server"}


@dataclass(frozen=True)
class Backend:
    slot: str
    base_url: str


def configured_backends() -> dict[str, Backend]:
    return {
        "blue": Backend("blue", os.getenv("LLM_PROVIDER_ROUTER_BLUE_URL", "http://127.0.0.1:8790")),
        "green": Backend(
            "green",
            os.getenv("LLM_PROVIDER_ROUTER_GREEN_URL", "http://127.0.0.1:8791"),
        ),
    }


def active_backend_path() -> Path:
    return Path(os.path.expanduser(os.getenv("LLM_PROVIDER_ROUTER_ACTIVE_BACKEND_FILE", DEFAULT_ACTIVE_BACKEND_FILE)))


def read_active_slot() -> str:
    path = active_backend_path()
    if not path.exists():
        return os.getenv("LLM_PROVIDER_ROUTER_DEFAULT_SLOT", "blue")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return os.getenv("LLM_PROVIDER_ROUTER_DEFAULT_SLOT", "blue")
    slot = data.get("slot") if isinstance(data, dict) else None
    return slot if slot in configured_backends() else os.getenv("LLM_PROVIDER_ROUTER_DEFAULT_SLOT", "blue")


def ordered_backends() -> list[Backend]:
    backends = configured_backends()
    active_slot = read_active_slot()
    ordered = [backends[active_slot]] if active_slot in backends else []
    ordered.extend(backend for slot, backend in backends.items() if slot != active_slot)
    return ordered


def write_active_backend(slot: str) -> None:
    backends = configured_backends()
    if slot not in backends:
        raise ValueError(f"unknown backend slot: {slot}")
    path = active_backend_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "slot": slot,
        "base_url": backends[slot].base_url,
        "updated_at": int(time.time()),
    }
    tmp = path.with_suffix(path.suffix + f".{os.getpid()}.tmp")
    tmp.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    tmp.replace(path)


def clean_request_headers(headers: httpx.Headers) -> dict[str, str]:
    return {key: value for key, value in headers.items() if key.lower() not in REQUEST_SKIP_HEADERS}


def clean_response_headers(headers: httpx.Headers) -> dict[str, str]:
    return {key: value for key, value in headers.items() if key.lower() not in RESPONSE_SKIP_HEADERS}


async def health_probe(backend: Backend, timeout: float = 2.0) -> dict:
    try:
        async with httpx.AsyncClient(timeout=timeout) as client:
            response = await client.get(f"{backend.base_url.rstrip('/')}/health")
        return {"slot": backend.slot, "base_url": backend.base_url, "ok": response.is_success, "status": response.status_code}
    except httpx.HTTPError as exc:
        return {"slot": backend.slot, "base_url": backend.base_url, "ok": False, "error": type(exc).__name__}


def create_front_proxy_app() -> FastAPI:
    app = FastAPI(title="LLM Provider Router Front Proxy", version="0.1.0")

    @app.get("/_proxy/health")
    async def proxy_health() -> dict:
        probes = [await health_probe(backend) for backend in ordered_backends()]
        active = read_active_slot()
        return {
            "ok": any(item.get("ok") and item.get("slot") == active for item in probes),
            "active_slot": active,
            "active_backend_file": str(active_backend_path()),
            "backends": probes,
        }

    @app.post("/_proxy/active/{slot}")
    async def set_active(slot: str) -> dict:
        if slot not in configured_backends():
            return JSONResponse(status_code=400, content={"ok": False, "error": f"unknown slot: {slot}"})
        write_active_backend(slot)
        return {"ok": True, "active_slot": slot, "backend": configured_backends()[slot].base_url}

    @app.api_route("/{path:path}", methods=["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"])
    async def proxy(path: str, request: Request) -> Response:
        body = await request.body()
        errors: list[dict[str, str]] = []
        for backend in ordered_backends():
            target = f"{backend.base_url.rstrip('/')}/{path}"
            if request.url.query:
                target = f"{target}?{request.url.query}"
            client = httpx.AsyncClient(timeout=None)
            try:
                upstream_request = client.build_request(
                    request.method,
                    target,
                    headers=clean_request_headers(request.headers),
                    content=body,
                )
                upstream_response = await client.send(upstream_request, stream=True)
            except httpx.HTTPError as exc:
                await client.aclose()
                errors.append({"slot": backend.slot, "error": type(exc).__name__})
                continue

            async def stream_body(
                response: httpx.Response = upstream_response,
                client_to_close: httpx.AsyncClient = client,
            ) -> AsyncIterator[bytes]:
                try:
                    async for chunk in response.aiter_raw():
                        yield chunk
                finally:
                    await response.aclose()
                    await client_to_close.aclose()

            return StreamingResponse(
                stream_body(),
                status_code=upstream_response.status_code,
                headers=clean_response_headers(upstream_response.headers),
                media_type=upstream_response.headers.get("content-type"),
            )
        return JSONResponse(
            status_code=503,
            content={"error": {"message": "no llm-provider-router backend available", "details": errors}},
        )

    return app


def main() -> None:
    host = os.getenv("LLM_PROVIDER_ROUTER_PROXY_HOST", "127.0.0.1")
    port = int(os.getenv("LLM_PROVIDER_ROUTER_PROXY_PORT", "8789"))
    uvicorn.run(create_front_proxy_app(), host=host, port=port)


if __name__ == "__main__":
    main()
