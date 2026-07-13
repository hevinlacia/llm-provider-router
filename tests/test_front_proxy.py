from __future__ import annotations

import json
from pathlib import Path

from fastapi.testclient import TestClient

from llm_provider_router.front_proxy import create_front_proxy_app, write_active_backend


def test_proxy_health_reports_active_backend(monkeypatch, tmp_path: Path) -> None:
    active_file = tmp_path / "active-backend.json"
    monkeypatch.setenv("LLM_PROVIDER_ROUTER_ACTIVE_BACKEND_FILE", str(active_file))
    monkeypatch.setenv("LLM_PROVIDER_ROUTER_BLUE_URL", "http://127.0.0.1:1")
    monkeypatch.setenv("LLM_PROVIDER_ROUTER_GREEN_URL", "http://127.0.0.1:2")

    write_active_backend("green")
    data = json.loads(active_file.read_text())

    assert data["slot"] == "green"
    assert data["base_url"] == "http://127.0.0.1:2"


def test_proxy_rejects_unknown_active_slot(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("LLM_PROVIDER_ROUTER_ACTIVE_BACKEND_FILE", str(tmp_path / "active.json"))
    app = create_front_proxy_app()
    client = TestClient(app)

    response = client.post("/_proxy/active/red")

    assert response.status_code == 400
    assert response.json()["ok"] is False
