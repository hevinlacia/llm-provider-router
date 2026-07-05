from __future__ import annotations

import os
from dataclasses import dataclass


DEFAULT_ARK_BASE_URL = "https://ark.cn-beijing.volces.com/api/coding/v3"


@dataclass(frozen=True)
class KeyRef:
    name: str
    env_var: str
    weight: int


@dataclass(frozen=True)
class RetryPolicy:
    max_retry_seconds: int
    retry_delay_seconds: float
    retry_on_status: tuple[int, ...]


@dataclass(frozen=True)
class ModelAlias:
    alias: str
    litellm_model: str
    base_url: str
    keys: tuple[KeyRef, ...]
    retry_policy: RetryPolicy | None = None

    @property
    def upstream_model(self) -> str:
        if self.litellm_model.startswith("openai/"):
            return self.litellm_model.removeprefix("openai/")
        return self.litellm_model


@dataclass(frozen=True)
class Settings:
    host: str
    port: int
    session_ttl_seconds: int
    monthly_quota_fallback_seconds: int
    five_hour_quota_fallback_seconds: int
    request_timeout_seconds: float
    local_bearer_token: str | None
    usage_db_path: str


ARK_KEYS: tuple[KeyRef, ...] = (
    KeyRef("garvin", "OPENCODE_AI_ARK_GARVIN_API_KEY", 6),
    KeyRef("wilford", "OPENCODE_AI_ARK_WILFORD_API_KEY", 3),
    KeyRef("hevin", "OPENCODE_AI_ARK_HEVIN_API_KEY", 5),
    KeyRef("khaine", "OPENCODE_AI_ARK_KHAINE_API_KEY", 6),
    KeyRef("cyril", "OPENCODE_AI_ARK_CYRIL_API_KEY", 4),
    KeyRef("moss", "OPENCODE_AI_ARK_MOSS_API_KEY", 4),
)

OAI_HEVIN_KEYS: tuple[KeyRef, ...] = (KeyRef("oai-hevin", "OPENCODE_AI_OPENAI_HEVIN_API_KEY", 1),)

OAI_RELAY_RETRY_POLICY = RetryPolicy(
    max_retry_seconds=1800,
    retry_delay_seconds=15,
    retry_on_status=(429, 500, 502, 503, 504),
)

DEEPSEEK_OFFICIAL_KEYS: tuple[KeyRef, ...] = (
    KeyRef("deepseek-official", "OPENCODE_AI_DEEPSEEK_API_KEY", 1),
)


ALIASES: dict[str, ModelAlias] = {
    "glm-latest-auto": ModelAlias(
        alias="glm-latest-auto",
        litellm_model="openai/glm-5.2",
        base_url=DEFAULT_ARK_BASE_URL,
        keys=ARK_KEYS,
    ),
    "deepseek-v4-pro-auto": ModelAlias(
        alias="deepseek-v4-pro-auto",
        litellm_model="openai/deepseek-v4-pro",
        base_url=DEFAULT_ARK_BASE_URL,
        keys=ARK_KEYS,
    ),
    "deepseek-v4-flash-auto": ModelAlias(
        alias="deepseek-v4-flash-auto",
        litellm_model="openai/deepseek-v4-flash",
        base_url=DEFAULT_ARK_BASE_URL,
        keys=ARK_KEYS,
    ),
    "minimax-latest-auto": ModelAlias(
        alias="minimax-latest-auto",
        litellm_model="openai/minimax-m3",
        base_url=DEFAULT_ARK_BASE_URL,
        keys=ARK_KEYS,
    ),
    "openai-gpt-5.5-hevin": ModelAlias(
        alias="openai-gpt-5.5-hevin",
        litellm_model="openai/gpt-5.5",
        base_url="https://api.aixhan.com/v1",
        keys=OAI_HEVIN_KEYS,
        retry_policy=OAI_RELAY_RETRY_POLICY,
    ),
    "openai-gpt-5.4-hevin": ModelAlias(
        alias="openai-gpt-5.4-hevin",
        litellm_model="openai/gpt-5.4",
        base_url="https://api.aixhan.com/v1",
        keys=OAI_HEVIN_KEYS,
        retry_policy=OAI_RELAY_RETRY_POLICY,
    ),
    "deepseek-v4-flash-official": ModelAlias(
        alias="deepseek-v4-flash-official",
        litellm_model="openai/deepseek-v4-flash",
        base_url="https://api.deepseek.com",
        keys=DEEPSEEK_OFFICIAL_KEYS,
    ),
    "deepseek-v4-pro-official": ModelAlias(
        alias="deepseek-v4-pro-official",
        litellm_model="openai/deepseek-v4-pro",
        base_url="https://api.deepseek.com",
        keys=DEEPSEEK_OFFICIAL_KEYS,
    ),
}


def load_settings() -> Settings:
    return Settings(
        host=os.getenv("ARK_KEY_ROUTER_HOST", "127.0.0.1"),
        port=int(os.getenv("ARK_KEY_ROUTER_PORT", "8789")),
        session_ttl_seconds=int(os.getenv("ARK_KEY_ROUTER_SESSION_TTL_SECONDS", "3600")),
        monthly_quota_fallback_seconds=int(
            os.getenv("ARK_KEY_ROUTER_MONTHLY_QUOTA_FALLBACK_SECONDS", "86400")
        ),
        five_hour_quota_fallback_seconds=int(
            os.getenv("ARK_KEY_ROUTER_5H_QUOTA_FALLBACK_SECONDS", "5400")
        ),
        request_timeout_seconds=float(os.getenv("ARK_KEY_ROUTER_REQUEST_TIMEOUT_SECONDS", "600")),
        local_bearer_token=os.getenv("ARK_KEY_ROUTER_BEARER_TOKEN")
        or os.getenv("OPENCODE_AI_LITELLM_API_KEY"),
        usage_db_path=os.getenv(
            "ARK_KEY_ROUTER_USAGE_DB_PATH",
            "~/.local/state/ark-key-router/usage.sqlite3",
        ),
    )
