from __future__ import annotations

import time

from ark_key_router.config import ARK_KEYS, ModelAlias, Settings
from ark_key_router.state import NoAvailableKeyError, RouterState, parse_quota_reset


def settings() -> Settings:
    return Settings(
        host="127.0.0.1",
        port=8789,
        session_ttl_seconds=3600,
        monthly_quota_fallback_seconds=86400,
        five_hour_quota_fallback_seconds=5400,
        request_timeout_seconds=60,
        local_bearer_token=None,
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
