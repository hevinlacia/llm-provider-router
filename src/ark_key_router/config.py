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
class ModelAlias:
    alias: str
    litellm_model: str
    base_url: str
    keys: tuple[KeyRef, ...]

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


ARK_KEYS: tuple[KeyRef, ...] = (
    KeyRef("garvin", "OPENCODE_AI_ARK_GARVIN_API_KEY", 6),
    KeyRef("wilford", "OPENCODE_AI_ARK_WILFORD_API_KEY", 3),
    KeyRef("hevin", "OPENCODE_AI_ARK_HEVIN_API_KEY", 5),
    KeyRef("khaine", "OPENCODE_AI_ARK_KHAINE_API_KEY", 6),
    KeyRef("cyril", "OPENCODE_AI_ARK_CYRIL_API_KEY", 4),
    KeyRef("moss", "OPENCODE_AI_ARK_MOSS_API_KEY", 4),
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
        local_bearer_token=os.getenv("ARK_KEY_ROUTER_BEARER_TOKEN"),
    )
