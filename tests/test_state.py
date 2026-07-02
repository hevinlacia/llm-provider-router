from __future__ import annotations

import time

from ark_key_router.config import ARK_KEYS, ModelAlias, Settings
from ark_key_router.state import NoAvailableKeyError, RouterState, parse_quota_reset


def settings(usage_db_path: str = ":memory:") -> Settings:
    return Settings(
        host="127.0.0.1",
        port=8789,
        session_ttl_seconds=3600,
        monthly_quota_fallback_seconds=86400,
        five_hour_quota_fallback_seconds=5400,
        request_timeout_seconds=60,
        local_bearer_token=None,
        usage_db_path=usage_db_path,
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
