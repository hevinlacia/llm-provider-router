from __future__ import annotations

import json
from pathlib import Path

import pytest

from ark_key_router.config import (
    DEFAULT_ROUTER_AUTH_CONFIG_PATH,
    _load_router_bearer_token,
    load_settings,
)


def test_load_router_bearer_token_reads_plaintext(tmp_path: Path) -> None:
    config_path = tmp_path / "router-auth.json"
    config_path.write_text(json.dumps({"bearer_token": "local-dev"}), encoding="utf-8")

    assert _load_router_bearer_token(str(config_path)) == "local-dev"


def test_load_router_bearer_token_resolves_relative_path(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    (tmp_path / "router-auth.json").write_text(
        json.dumps({"bearer_token": "from-cwd"}), encoding="utf-8"
    )

    assert _load_router_bearer_token("router-auth.json") == "from-cwd"


def test_load_router_bearer_token_expands_user(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    config_path = tmp_path / "router-auth.json"
    config_path.write_text(json.dumps({"bearer_token": "from-home"}), encoding="utf-8")

    assert _load_router_bearer_token("~/router-auth.json") == "from-home"


@pytest.mark.parametrize(
    "contents",
    [
        "",
        "not json",
        json.dumps([]),
        json.dumps({"other": "x"}),
        json.dumps({"bearer_token": ""}),
        json.dumps({"bearer_token": 123}),
    ],
)
def test_load_router_bearer_token_returns_none_for_invalid(contents: str, tmp_path: Path) -> None:
    config_path = tmp_path / "router-auth.json"
    config_path.write_text(contents, encoding="utf-8")

    assert _load_router_bearer_token(str(config_path)) is None


def test_load_router_bearer_token_missing_file(tmp_path: Path) -> None:
    assert _load_router_bearer_token(str(tmp_path / "missing.json")) is None


def test_default_router_auth_path() -> None:
    assert DEFAULT_ROUTER_AUTH_CONFIG_PATH == "config/router-auth.json"


def test_load_settings_prefers_env_token(monkeypatch, tmp_path: Path) -> None:
    config_path = tmp_path / "router-auth.json"
    config_path.write_text(json.dumps({"bearer_token": "from-file"}), encoding="utf-8")
    monkeypatch.delenv("ARK_KEY_ROUTER_BEARER_TOKEN", raising=False)
    monkeypatch.setenv("ARK_KEY_ROUTER_AUTH_CONFIG_PATH", str(config_path))
    monkeypatch.setenv("OPENCODE_AI_LITELLM_API_KEY", "from-litellm-env")

    assert load_settings().local_bearer_token == "from-file"


def test_load_settings_env_overrides_file(monkeypatch, tmp_path: Path) -> None:
    config_path = tmp_path / "router-auth.json"
    config_path.write_text(json.dumps({"bearer_token": "from-file"}), encoding="utf-8")
    monkeypatch.setenv("ARK_KEY_ROUTER_BEARER_TOKEN", "from-env")
    monkeypatch.setenv("ARK_KEY_ROUTER_AUTH_CONFIG_PATH", str(config_path))
    monkeypatch.setenv("OPENCODE_AI_LITELLM_API_KEY", "from-litellm-env")

    assert load_settings().local_bearer_token == "from-env"


def test_load_settings_falls_back_to_litellm_env(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.delenv("ARK_KEY_ROUTER_BEARER_TOKEN", raising=False)
    monkeypatch.setenv("ARK_KEY_ROUTER_AUTH_CONFIG_PATH", str(tmp_path / "missing.json"))
    monkeypatch.setenv("OPENCODE_AI_LITELLM_API_KEY", "from-litellm-env")

    assert load_settings().local_bearer_token == "from-litellm-env"


def test_load_settings_returns_none_when_no_source(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.delenv("ARK_KEY_ROUTER_BEARER_TOKEN", raising=False)
    monkeypatch.setenv("ARK_KEY_ROUTER_AUTH_CONFIG_PATH", str(tmp_path / "missing.json"))
    monkeypatch.delenv("OPENCODE_AI_LITELLM_API_KEY", raising=False)

    assert load_settings().local_bearer_token is None
