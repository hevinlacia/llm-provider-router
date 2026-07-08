from __future__ import annotations

import time
from unittest.mock import AsyncMock, patch

import httpx
from fastapi.testclient import TestClient

from llm_provider_router.config import ALIASES, ARK_KEYS, ModelAlias, RetryPolicy, Settings
from llm_provider_router.proxy import call_upstream, create_app
from llm_provider_router.state import NoAvailableKeyError, RouterState, parse_quota_reset


def settings(usage_db_path: str = ":memory:", weight_config_path: str = ":memory:") -> Settings:
    return Settings(
        host="127.0.0.1",
        port=8789,
        session_ttl_seconds=3600,
        monthly_quota_fallback_seconds=86400,
        five_hour_quota_fallback_seconds=5400,
        request_timeout_seconds=60,
        local_bearer_token=None,
        usage_db_path=usage_db_path,
        weight_config_path=weight_config_path,
        provider_config_path=":memory:",
        custom_key_config_path=":memory:",
        router_auth_config_path=":memory:",
        key_config_path=":memory:",
        sops_age_key_file="~/.config/sops/age/keys.txt",
        sops_age_recipient="age1test",
    )


def alias() -> ModelAlias:
    return ModelAlias(
        alias="glm-latest-auto",
        litellm_model="openai/glm-5.2",
        base_url="https://example.invalid/v1",
        keys=ARK_KEYS[:2],
    )


def test_session_sticks_to_same_key() -> None:
    state = RouterState(settings())
    first = state.select_key(alias(), "session-a")
    second = state.select_key(alias(), "session-a")
    assert second.name == first.name


def test_frozen_key_rebinds_session() -> None:
    state = RouterState(settings())
    first = state.select_key(alias(), "session-a")
    state.freeze(first.name, time.time() + 3600, "quota")
    second = state.select_key(alias(), "session-a")
    assert second.name != first.name


def test_all_keys_frozen_raises_retry_after() -> None:
    state = RouterState(settings())
    for key in alias().keys:
        state.freeze(key.name, time.time() + 123, "quota")
    try:
        state.select_key(alias(), "session-a")
    except NoAvailableKeyError as exc:
        assert 1 <= exc.retry_after <= 123
    else:
        raise AssertionError("expected NoAvailableKeyError")


def test_select_key_excluding_rebinds_session() -> None:
    state = RouterState(settings())
    first = state.select_key(alias(), "session-a")
    second = state.select_key_excluding(alias(), "session-a", {first.name})
    assert second.name != first.name


def test_existing_session_binding_still_wins_over_usage_balancing() -> None:
    state = RouterState(settings())
    state.bind("glm-latest-auto", "session-a", "garvin")
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=200,
        usage={"total_tokens": 600},
    )

    selected = state.select_key(alias(), "session-a")

    assert selected.name == "garvin"


def test_new_session_prefers_alias_key_with_lowest_weighted_token_usage() -> None:
    state = RouterState(settings())
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=200,
        usage={"total_tokens": 600},
    )
    state.record_usage(
        model="glm-latest-auto",
        key_name="wilford",
        status_code=200,
        usage={"total_tokens": 60},
    )
    state.record_usage(
        model="glm-latest-auto",
        key_name="cyril",
        status_code=200,
        usage={"total_tokens": 0},
    )

    selected = state.select_key(alias(), "session-b")

    assert selected.name == "wilford"


def test_select_key_excluding_all_candidates_raises_retry_after() -> None:
    state = RouterState(settings())
    try:
        state.select_key_excluding(alias(), "session-a", {key.name for key in alias().keys})
    except NoAvailableKeyError as exc:
        assert exc.retry_after == 60
    else:
        raise AssertionError("expected NoAvailableKeyError")


def test_parse_quota_reset_timestamp() -> None:
    result = parse_quota_reset(
        "You have exceeded the 5-hour usage quota. It will reset at 2099-07-02 19:01:27 +0800 CST.",
        settings(),
    )
    assert result is not None
    until, reason = result
    assert until > time.time()
    assert reason == "five_hour_quota"


def test_litellm_openai_prefix_is_removed_for_upstream() -> None:
    assert alias().litellm_model == "openai/glm-5.2"
    assert alias().upstream_model == "glm-5.2"


def test_call_upstream_retries_next_key_after_connect_error() -> None:
    state = RouterState(settings())
    request = httpx.Request("POST", "https://example.invalid/v1/chat/completions")
    response = httpx.Response(200, json={"usage": {"total_tokens": 1}}, request=request)
    post = AsyncMock(side_effect=[httpx.ConnectError("boom", request=request), response])

    class Client:
        def __init__(self, timeout: float):
            self.timeout = timeout
            self.post = post

        async def __aenter__(self):
            return self

        async def __aexit__(self, exc_type, exc, tb):
            return None

    async def run_test() -> None:
        with patch.dict("os.environ", {key.env_var: "test-key" for key in alias().keys}):
            with patch("llm_provider_router.proxy.httpx.AsyncClient", Client):
                result = await call_upstream(
                    alias(),
                    "session-a",
                    {"model": "glm-5.2", "messages": []},
                    settings(),
                    state,
                )
        assert result.status_code == 200

    import asyncio

    asyncio.run(run_test())
    assert post.await_count == 2
    usage = state.usage_snapshot()
    assert usage["total"]["requests"] == 2
    assert usage["by_status"]["599"]["errors"] == 1
    assert len(usage["by_key"]) == 2


def test_usage_stats_are_grouped_by_model_key_and_status() -> None:
    state = RouterState(settings())
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=200,
        usage={
            "prompt_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 4},
            "completion_tokens": 15,
            "total_tokens": 25,
        },
    )
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=429,
        usage=None,
    )

    usage = state.usage_snapshot()
    assert usage["total"]["requests"] == 2
    assert usage["total"]["errors"] == 1
    assert usage["total"]["total_tokens"] == 25
    assert usage["total"]["cached_tokens"] == 4
    assert usage["total"]["cache_hit_rate"] == 0.4
    assert usage["by_model"]["glm-latest-auto"]["requests"] == 2
    assert usage["by_key"]["garvin"]["prompt_tokens"] == 10
    assert usage["by_status"]["429"]["errors"] == 1


def test_usage_stats_can_reset() -> None:
    state = RouterState(settings())
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=200,
        usage={"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
    )

    state.reset_usage()

    usage = state.usage_snapshot()
    assert usage["total"]["requests"] == 0
    assert usage["by_model"] == {}


def test_usage_stats_persist_to_sqlite(tmp_path) -> None:
    db_path = str(tmp_path / "usage.sqlite3")
    state = RouterState(settings(db_path))
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=200,
        usage={"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5},
    )

    restored = RouterState(settings(db_path))
    usage = restored.usage_snapshot()
    assert usage["total"]["requests"] == 1
    assert usage["total"]["total_tokens"] == 5

    restored.reset_usage()
    cleared = RouterState(settings(db_path)).usage_snapshot()
    assert cleared["total"]["requests"] == 0


def test_usage_stats_support_period_and_timeseries_filters(tmp_path) -> None:
    db_path = str(tmp_path / "usage.sqlite3")
    state = RouterState(settings(db_path))
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=200,
        usage={"prompt_tokens": 2, "cached_tokens": 1, "completion_tokens": 3, "total_tokens": 5},
    )

    usage = state.usage_snapshot(period="today")

    assert usage["range"]["period"] == "today"
    assert usage["total"]["requests"] == 1
    assert usage["total"]["cache_hit_rate"] == 0.5
    assert list(usage["by_day"].values())[0]["requests"] == 1
    assert list(usage["by_month"].values())[0]["total_tokens"] == 5


def test_usage_stats_custom_range_can_exclude_events(tmp_path) -> None:
    db_path = str(tmp_path / "usage.sqlite3")
    state = RouterState(settings(db_path))
    state.record_usage(
        model="glm-latest-auto",
        key_name="garvin",
        status_code=200,
        usage={"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5},
    )

    usage = state.usage_snapshot(start="2999-01-01", end="2999-01-31")

    assert usage["total"]["requests"] == 0
    assert usage["by_day"] == {}


def test_settings_accepts_llm_provider_router_api_key() -> None:
    from llm_provider_router.config import load_settings

    with patch.dict("os.environ", {"LLM_PROVIDER_ROUTER_API_KEY": "local-token"}, clear=True):
        assert load_settings().local_bearer_token == "local-token"


def test_models_endpoint_returns_openai_compatible_list() -> None:
    client = TestClient(create_app(settings()))

    response = client.get("/v1/models")

    assert response.status_code == 200
    body = response.json()
    assert body["object"] == "list"
    assert {item["id"] for item in body["data"]} >= {
        "glm-latest-auto",
        "minimax-latest-auto",
        "high-model-auto",
        "low-model-auto",
        "picture-model-auto",
    }


def test_model_tier_aliases_route_to_expected_upstreams() -> None:
    assert ALIASES["high-model-auto"].upstream_model == "gpt-5.5"
    assert ALIASES["high-model-auto"].keys[0].provider == "openai-relay"
    assert ALIASES["low-model-auto"].upstream_model == "deepseek-v4-flash"
    assert ALIASES["picture-model-auto"].upstream_model == "minimax-m3"


def test_models_endpoint_validates_local_token() -> None:
    client = TestClient(
        create_app(
            Settings(
                host="127.0.0.1",
                port=8789,
                session_ttl_seconds=3600,
                monthly_quota_fallback_seconds=86400,
                five_hour_quota_fallback_seconds=5400,
                request_timeout_seconds=60,
                local_bearer_token="local-token",
                usage_db_path=":memory:",
                weight_config_path=":memory:",
                provider_config_path=":memory:",
                custom_key_config_path=":memory:",
                router_auth_config_path=":memory:",
                key_config_path=":memory:",
                sops_age_key_file="~/.config/sops/age/keys.txt",
                sops_age_recipient="age1test",
            )
        )
    )

    assert client.get("/v1/models").status_code == 401
    assert (
        client.get("/v1/models", headers={"Authorization": "Bearer local-token"}).status_code == 200
    )


def test_runtime_key_weights_are_persisted_and_applied(tmp_path) -> None:
    config_path = str(tmp_path / "key-weights.json")
    state = RouterState(settings(weight_config_path=config_path))

    state.set_key_weights({"garvin": 0, "wilford": 10})

    restored = RouterState(settings(weight_config_path=config_path))
    weighted_alias = restored.alias_with_runtime_weights(alias())
    weights = {key.name: key.weight for key in weighted_alias.keys}
    assert weights["garvin"] == 0
    assert weights["wilford"] == 10
    assert restored.key_config_snapshot()["config_path"] == config_path


def test_zero_weight_drops_existing_session_binding(tmp_path) -> None:
    config_path = str(tmp_path / "key-weights.json")
    state = RouterState(settings(weight_config_path=config_path))
    state.bind("glm-latest-auto", "session-a", "garvin")

    state.set_key_weights({"garvin": 0})

    selected = state.select_key(alias().with_key_weights(state.key_weight_overrides()), "session-a")

    assert selected.name != "garvin"


def test_provider_base_url_can_be_changed_at_runtime() -> None:
    state = RouterState(settings())

    state.set_provider_base_urls({"ark": "https://ark.example.invalid/v1"})

    weighted_alias = state.alias_with_runtime_weights(alias())
    snapshot = state.provider_config_snapshot()

    assert weighted_alias.base_url == "https://ark.example.invalid/v1"
    assert snapshot["providers"][0]["name"] == "ark"


def test_key_metadata_groups_provider_and_billing_type() -> None:
    state = RouterState(settings())

    snapshot = state.key_secret_snapshot()
    deepseek = next(item for item in snapshot["keys"] if item["name"] == "deepseek-official")
    garvin = next(item for item in snapshot["keys"] if item["name"] == "garvin")

    assert deepseek["provider"] == "deepseek-official"
    assert deepseek["billing_type"] == "payg"
    assert garvin["provider"] == "ark"
    assert garvin["billing_type"] == "subscription"


def test_encrypted_key_config_overrides_environment(monkeypatch) -> None:
    state = RouterState(settings())
    monkeypatch.setenv("OPENCODE_AI_ARK_GARVIN_API_KEY", "env-key")

    state.set_key_values({"garvin": "configured-key"})

    assert state.upstream_key_value(ARK_KEYS[0]) == "configured-key"
    snapshot = state.key_secret_snapshot()
    garvin = next(item for item in snapshot["keys"] if item["name"] == "garvin")
    assert garvin["source"] == "encrypted_file+runtime_env"


def test_encrypted_key_config_can_remove_value(monkeypatch) -> None:
    state = RouterState(settings())
    monkeypatch.setenv("OPENCODE_AI_ARK_GARVIN_API_KEY", "env-key")
    state.set_key_values({"garvin": "configured-key"})

    state.set_key_values({}, {"garvin"})

    assert state.upstream_key_value(ARK_KEYS[0]) == "env-key"
    snapshot = state.key_secret_snapshot()
    garvin = next(item for item in snapshot["keys"] if item["name"] == "garvin")
    assert garvin["source"] == "environment"


def test_custom_key_can_be_added_to_selected_auto_pools() -> None:
    state = RouterState(settings())

    snapshot = state.add_key_to_pools(
        name="new-ark",
        value="configured-key",
        aliases=["glm-latest-auto", "minimax-latest-auto"],
        weight=2,
    )

    key = next(item for item in snapshot["keys"] if item["name"] == "new-ark")
    assert key["configured"] is True
    assert key["source"] == "encrypted_file+runtime_env"
    assert state.upstream_key_value(state.custom_key_refs()[0]) == "configured-key"
    glm_keys = [key.name for key in state.settings_aliases()["glm-latest-auto"].keys]
    flash_keys = [key.name for key in state.settings_aliases()["deepseek-v4-flash-auto"].keys]
    minimax_keys = [key.name for key in state.settings_aliases()["minimax-latest-auto"].keys]
    assert "new-ark" in glm_keys
    assert "new-ark" not in flash_keys
    assert "new-ark" in minimax_keys


def test_custom_key_rejects_non_auto_pool() -> None:
    state = RouterState(settings())

    try:
        state.add_key_to_pools(
            name="new-ark",
            value="configured-key",
            aliases=["openai-gpt-5.5-hevin"],
        )
    except ValueError as exc:
        assert "unknown auto alias" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_custom_key_name_accepts_env_var_style() -> None:
    state = RouterState(settings())

    snapshot = state.add_key_to_pools(
        name="AI_ARK_SHELL_API_KEY",
        value="configured-key",
        aliases=["glm-latest-auto"],
    )

    key = next(item for item in snapshot["keys"] if item["name"] == "shell")
    assert key["env_var"] == "OPENCODE_AI_ARK_SHELL_API_KEY"
    assert key["source"] == "encrypted_file+runtime_env"


def test_custom_key_add_updates_runtime_environment(monkeypatch) -> None:
    monkeypatch.delenv("OPENCODE_AI_ARK_SHELL_API_KEY", raising=False)
    state = RouterState(settings())

    state.add_key_to_pools(
        name="shell",
        value="configured-key",
        aliases=["glm-latest-auto"],
    )

    assert __import__("os").environ["OPENCODE_AI_ARK_SHELL_API_KEY"] == "configured-key"
    shell_ref = next(key for key in state.custom_key_refs() if key.name == "shell")
    assert state.upstream_key_value(shell_ref) == "configured-key"


def test_custom_key_appears_in_key_weights() -> None:
    state = RouterState(settings())

    state.add_key_to_pools(
        name="shell",
        value="configured-key",
        aliases=["glm-latest-auto"],
        weight=3,
    )

    snapshot = state.key_config_snapshot()
    assert snapshot["weights"]["shell"] == 3


def test_custom_key_weight_can_be_saved() -> None:
    state = RouterState(settings())

    state.add_key_to_pools(
        name="shell",
        value="configured-key",
        aliases=["glm-latest-auto"],
        weight=1,
    )
    state.set_key_weights({"shell": 7})

    snapshot = state.key_config_snapshot()
    weighted_alias = state.alias_with_runtime_weights(state.settings_aliases()["glm-latest-auto"])
    shell = next(key for key in weighted_alias.keys if key.name == "shell")
    assert snapshot["weights"]["shell"] == 7
    assert shell.weight == 7


def _oai_alias() -> ModelAlias:
    from llm_provider_router.config import KeyRef

    return ModelAlias(
        alias="openai-gpt-5.5-hevin",
        litellm_model="openai/gpt-5.5",
        base_url="https://example.invalid/v1",
        keys=(KeyRef("oai-hevin", "OPENCODE_AI_OPENAI_HEVIN_API_KEY", 1),),
        retry_policy=RetryPolicy(
            max_retry_seconds=1800,
            retry_delay_seconds=15,
            retry_on_status=(429, 500, 502, 503, 504),
        ),
    )


def test_call_upstream_retries_on_429_with_retry_policy() -> None:
    state = RouterState(settings())
    request = httpx.Request("POST", "https://example.invalid/v1/chat/completions")
    error_resp = httpx.Response(429, json={"error": {"message": "rate limited"}}, request=request)
    ok_resp = httpx.Response(200, json={"usage": {"total_tokens": 1}}, request=request)
    post = AsyncMock(side_effect=[error_resp, ok_resp])

    class Client:
        def __init__(self, timeout: float):
            self.timeout = timeout
            self.post = post

        async def __aenter__(self):
            return self

        async def __aexit__(self, exc_type, exc, tb):
            return None

    async def run_test() -> None:
        with patch.dict("os.environ", {"OPENCODE_AI_OPENAI_HEVIN_API_KEY": "test-key"}):
            with patch("llm_provider_router.proxy.httpx.AsyncClient", Client):
                with patch("llm_provider_router.proxy.asyncio.sleep", new_callable=AsyncMock):
                    result = await call_upstream(
                        _oai_alias(),
                        "session-a",
                        {"model": "gpt-5.5", "messages": []},
                        settings(),
                        state,
                    )
        assert result.status_code == 200

    import asyncio

    asyncio.run(run_test())
    assert post.await_count == 2
    usage = state.usage_snapshot()
    assert usage["total"]["requests"] == 2
    assert usage["by_status"]["429"]["errors"] == 1
    assert usage["by_status"]["200"]["requests"] == 1


def test_call_upstream_returns_last_retriable_status_when_deadline_exceeded() -> None:
    state = RouterState(settings())
    request = httpx.Request("POST", "https://example.invalid/v1/chat/completions")
    error_resp = httpx.Response(503, json={"error": {"message": "unavailable"}}, request=request)

    alias_with_short_deadline = ModelAlias(
        alias="openai-gpt-5.5-hevin",
        litellm_model="openai/gpt-5.5",
        base_url="https://example.invalid/v1",
        keys=_oai_alias().keys,
        retry_policy=RetryPolicy(
            max_retry_seconds=0,
            retry_delay_seconds=1,
            retry_on_status=(429, 500, 502, 503, 504),
        ),
    )

    post = AsyncMock(side_effect=[error_resp])

    class Client:
        def __init__(self, timeout: float):
            self.timeout = timeout
            self.post = post

        async def __aenter__(self):
            return self

        async def __aexit__(self, exc_type, exc, tb):
            return None

    async def run_test() -> None:
        with patch.dict("os.environ", {"OPENCODE_AI_OPENAI_HEVIN_API_KEY": "test-key"}):
            with patch("llm_provider_router.proxy.httpx.AsyncClient", Client):
                result = await call_upstream(
                    alias_with_short_deadline,
                    "session-a",
                    {"model": "gpt-5.5", "messages": []},
                    settings(),
                    state,
                )
        assert result.status_code == 503

    import asyncio

    asyncio.run(run_test())
    assert post.await_count == 1
