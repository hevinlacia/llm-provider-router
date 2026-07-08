from __future__ import annotations

import hashlib
import os
import random
import re
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime

from .config import ALIASES, KeyRef, ModelAlias, Settings
from .key_store import EncryptedKeyConfig
from .usage_store import CustomKeyPoolConfig, KeyWeightConfig, ProviderConfig, UsageStore


KEY_NAME_PATTERN = re.compile(r"^[a-z][a-z0-9-]*$")


@dataclass
class FrozenKey:
    until: float
    reason: str


@dataclass
class SessionBinding:
    key_name: str
    expires_at: float


class RouterState:
    def __init__(self, settings: Settings):
        self.settings = settings
        self.frozen: dict[str, FrozenKey] = {}
        self.bindings: dict[tuple[str, str], SessionBinding] = {}
        self.usage_store = UsageStore(settings.usage_db_path)
        self.weight_config = KeyWeightConfig(settings.weight_config_path, default_key_weights())
        self.provider_config = ProviderConfig(
            settings.provider_config_path,
            default_provider_base_urls(),
        )
        self.custom_key_config = CustomKeyPoolConfig(settings.custom_key_config_path)
        self.key_config = EncryptedKeyConfig(
            settings.key_config_path,
            settings.sops_age_recipient,
            settings.sops_age_key_file,
            self.known_key_names(),
        )

    def cleanup(self) -> None:
        now = time.time()
        self.frozen = {name: item for name, item in self.frozen.items() if item.until > now}
        self.bindings = {key: item for key, item in self.bindings.items() if item.expires_at > now}

    def is_frozen(self, key_name: str) -> bool:
        item = self.frozen.get(key_name)
        if item is None:
            return False
        if item.until <= time.time():
            self.frozen.pop(key_name, None)
            return False
        return True

    def freeze(self, key_name: str, until: float, reason: str) -> None:
        current = self.frozen.get(key_name)
        if current is None or until > current.until:
            self.frozen[key_name] = FrozenKey(until=until, reason=reason)

    def bind(self, alias: str, session_id: str, key_name: str) -> None:
        self.bindings[(alias, session_id)] = SessionBinding(
            key_name=key_name,
            expires_at=time.time() + self.settings.session_ttl_seconds,
        )

    def select_key(self, alias: ModelAlias, session_id: str | None) -> KeyRef:
        return self.select_key_excluding(alias, session_id=session_id, excluded=set())

    def select_key_excluding(
        self,
        alias: ModelAlias,
        session_id: str | None,
        excluded: set[str],
    ) -> KeyRef:
        self.cleanup()
        if session_id:
            binding = self.bindings.get((alias.alias, session_id))
            if (
                binding
                and binding.key_name not in excluded
                and not self.is_frozen(binding.key_name)
            ):
                key = next((item for item in alias.keys if item.name == binding.key_name), None)
                if key is not None:
                    self.bind(alias.alias, session_id, key.name)
                    return key

        candidates = [
            key for key in alias.keys if key.name not in excluded and not self.is_frozen(key.name)
        ]
        if not candidates:
            soonest = min(self.frozen.values(), key=lambda item: item.until, default=None)
            retry_after = int(max(1, (soonest.until - time.time()) if soonest else 60))
            raise NoAvailableKeyError(retry_after=retry_after)

        key = self.usage_adjusted_pick(alias, candidates, session_id=session_id)
        if session_id:
            self.bind(alias.alias, session_id, key.name)
        return key

    def usage_adjusted_pick(
        self,
        alias: ModelAlias,
        candidates: list[KeyRef],
        session_id: str | None,
    ) -> KeyRef:
        token_totals = self.usage_store.key_token_totals_for_model(
            alias.alias,
            [key.name for key in candidates],
        )
        positive_weight_candidates = [key for key in candidates if key.weight > 0]
        if not positive_weight_candidates:
            return weighted_pick(candidates, session_id=session_id, alias=alias.alias)
        min_ratio = min(
            token_totals.get(key.name, 0) / key.weight for key in positive_weight_candidates
        )
        lowest_usage_candidates = [
            key
            for key in positive_weight_candidates
            if token_totals.get(key.name, 0) / key.weight == min_ratio
        ]
        return weighted_pick(lowest_usage_candidates, session_id=session_id, alias=alias.alias)

    def snapshot(self) -> dict:
        self.cleanup()
        now = time.time()
        return {
            "frozen": {
                name: {
                    "seconds_remaining": int(item.until - now),
                    "reason": item.reason,
                }
                for name, item in sorted(self.frozen.items())
            },
            "bindings": len(self.bindings),
        }

    def record_usage(
        self,
        *,
        model: str,
        key_name: str,
        status_code: int,
        usage: dict | None,
    ) -> None:
        self.usage_store.record(
            model=model,
            key_name=key_name,
            status_code=status_code,
            usage=usage,
        )

    def reset_usage(self) -> None:
        self.usage_store.reset()

    def usage_snapshot(
        self,
        *,
        period: str = "all",
        start: str | None = None,
        end: str | None = None,
    ) -> dict:
        return self.usage_store.snapshot(period=period, start=start, end=end)

    def key_weight_overrides(self) -> dict[str, int]:
        self.sync_custom_key_weight_defaults()
        return self.weight_config.get()

    def key_config_snapshot(self) -> dict:
        weights = self.key_weight_overrides()
        provider_urls = self.provider_base_urls()
        aliases = {}
        for alias_name, alias in self.settings_aliases().items():
            effective_alias = alias.with_provider_base_urls(provider_urls).with_key_weights(weights)
            total_weight = sum(max(0, key.weight) for key in effective_alias.keys)
            aliases[alias_name] = {
                "model": alias.litellm_model,
                "base_url": alias.base_url,
                "effective_base_url": effective_alias.base_url,
                "provider": alias.provider,
                "keys": [
                    {
                        "name": key.name,
                        "provider": key.provider,
                        "billing_type": key.billing_type,
                        "default_weight": default_key.weight,
                        "weight": key.weight,
                        "probability": round(key.weight / total_weight, 4)
                        if total_weight > 0 and key.weight > 0
                        else 0.0,
                    }
                    for key, default_key in zip(effective_alias.keys, alias.keys, strict=True)
                ],
            }
        return {
            "aliases": aliases,
            "weights": weights,
            "config_path": str(self.weight_config.path),
        }

    def provider_base_urls(self) -> dict[str, str]:
        return self.provider_config.get()

    def provider_config_snapshot(self) -> dict:
        configured = self.provider_base_urls()
        defaults = default_provider_base_urls()
        providers = [
            {
                "name": name,
                "base_url": configured[name],
                "default_base_url": defaults[name],
            }
            for name in sorted(defaults)
        ]
        return {"providers": providers, "config_path": str(self.provider_config.path)}

    def set_provider_base_urls(self, base_urls: dict[str, str]) -> dict:
        known_names = set(default_provider_base_urls())
        unknown_names = sorted(set(base_urls) - known_names)
        if unknown_names:
            raise ValueError(f"unknown provider(s): {', '.join(unknown_names)}")
        invalid_names = sorted(name for name, base_url in base_urls.items() if not base_url)
        if invalid_names:
            raise ValueError(f"empty base URL for provider(s): {', '.join(invalid_names)}")
        self.provider_config.set(base_urls)
        return self.provider_config_snapshot()

    def set_key_weights(self, weights: dict[str, int]) -> None:
        self.sync_custom_key_weight_defaults()
        known_names = {key.name for alias in self.settings_aliases().values() for key in alias.keys}
        unknown_names = sorted(set(weights) - known_names)
        if unknown_names:
            raise ValueError(f"unknown key name(s): {', '.join(unknown_names)}")
        invalid_names = sorted(name for name, weight in weights.items() if weight < 0)
        if invalid_names:
            raise ValueError(f"negative weight for key(s): {', '.join(invalid_names)}")
        effective_weights = self.weight_config.set(weights)
        self.rebind_zero_weight_sessions(effective_weights)

    def sync_custom_key_weight_defaults(self) -> None:
        self.weight_config.add_defaults({key.name: key.weight for key in self.custom_key_refs()})

    def key_secret_snapshot(self) -> dict:
        configured = self.key_config.safe_snapshot()
        keys = []
        for key in self.all_key_refs():
            encrypted_configured = bool(configured.get(key.name, {}).get("configured"))
            env_configured = bool(__import__("os").environ.get(key.env_var))
            if encrypted_configured and env_configured:
                source = "encrypted_file+runtime_env"
            elif encrypted_configured:
                source = "encrypted_file"
            elif env_configured:
                source = "environment"
            else:
                source = "missing"
            keys.append(
                {
                    "name": key.name,
                    "provider": key.provider,
                    "billing_type": key.billing_type,
                    "env_var": key.env_var,
                    "configured": encrypted_configured or env_configured,
                    "encrypted_configured": encrypted_configured,
                    "env_configured": env_configured,
                    "source": source,
                }
            )
        return {
            "keys": keys,
            "auto_aliases": self.auto_alias_names(),
            "config_path": str(self.key_config.path),
            "custom_key_config_path": str(self.custom_key_config.path),
        }

    def add_key_to_pools(
        self,
        *,
        name: str,
        value: str,
        aliases: list[str],
        weight: int = 1,
        provider: str = "ark",
        billing_type: str = "subscription",
    ) -> dict:
        name = normalize_custom_key_name(name)
        if not KEY_NAME_PATTERN.fullmatch(name):
            raise ValueError("key name must use lowercase letters, numbers, and hyphens")
        if name in self.known_key_names():
            raise ValueError(f"key already exists: {name}")
        if not value:
            raise ValueError("key value is required")
        auto_aliases = set(self.auto_alias_names())
        alias_names = sorted(set(aliases))
        unknown_aliases = sorted(set(alias_names) - auto_aliases)
        if unknown_aliases:
            raise ValueError(f"unknown auto alias(es): {', '.join(unknown_aliases)}")
        if not alias_names:
            raise ValueError("select at least one auto key pool")
        if weight < 0:
            raise ValueError("weight must be zero or greater")

        env_var = f"OPENCODE_AI_ARK_{name.replace('-', '_').upper()}_API_KEY"
        self.key_config.set_known_names(self.known_key_names() | {name})
        self.key_config.set_values({name: value})
        os.environ[env_var] = value
        self.custom_key_config.add_key(
            name=name,
            env_var=env_var,
            provider=provider,
            billing_type=billing_type,
            weight=weight,
            aliases=alias_names,
        )
        self.key_config.set_known_names(self.known_key_names())
        return self.key_secret_snapshot()

    def set_key_values(self, values: dict[str, str], delete_names: set[str] | None = None) -> dict:
        self.key_config.set_values(values, delete_names=delete_names)
        return self.key_secret_snapshot()

    def upstream_key_value(self, key: KeyRef) -> str | None:
        values = self.key_config.get_all()
        return values.get(key.name) or __import__("os").environ.get(key.env_var)

    def alias_with_runtime_weights(self, alias: ModelAlias) -> ModelAlias:
        return alias.with_provider_base_urls(self.provider_base_urls()).with_key_weights(
            self.key_weight_overrides()
        )

    def rebind_zero_weight_sessions(self, weights: dict[str, int]) -> None:
        zero_weight_names = {name for name, weight in weights.items() if weight <= 0}
        if not zero_weight_names:
            return
        self.bindings = {
            binding_key: binding
            for binding_key, binding in self.bindings.items()
            if binding.key_name not in zero_weight_names
        }

    def settings_aliases(self) -> dict[str, ModelAlias]:
        aliases = dict(ALIASES)
        for key in self.custom_key_refs():
            for alias_name in self.custom_key_aliases(key.name):
                alias = aliases.get(alias_name)
                if alias is None:
                    continue
                aliases[alias_name] = ModelAlias(
                    alias=alias.alias,
                    litellm_model=alias.litellm_model,
                    base_url=alias.base_url,
                    keys=(*alias.keys, key),
                    retry_policy=alias.retry_policy,
                )
        return aliases

    def auto_alias_names(self) -> list[str]:
        return sorted(name for name in ALIASES if "auto" in name)

    def custom_key_refs(self) -> list[KeyRef]:
        refs: list[KeyRef] = []
        for name, item in sorted(self.custom_key_config.get().get("keys", {}).items()):
            if not isinstance(item, dict):
                continue
            try:
                weight = int(item.get("weight", 1))
            except (TypeError, ValueError):
                weight = 1
            refs.append(
                KeyRef(
                    str(name),
                    str(item.get("env_var") or f"OPENCODE_AI_ARK_{str(name).upper()}_API_KEY"),
                    weight,
                    str(item.get("provider") or "ark"),
                    str(item.get("billing_type") or "subscription"),
                )
            )
        return refs

    def custom_key_aliases(self, key_name: str) -> list[str]:
        item = self.custom_key_config.get().get("keys", {}).get(key_name, {})
        aliases = item.get("aliases") if isinstance(item, dict) else []
        if not isinstance(aliases, list):
            return []
        return [str(alias) for alias in aliases]

    def all_key_refs(self) -> list[KeyRef]:
        refs: dict[str, KeyRef] = {}
        for alias in self.settings_aliases().values():
            for key in alias.keys:
                refs[key.name] = key
        return [refs[name] for name in sorted(refs)]

    def known_key_names(self) -> set[str]:
        return {key.name for key in self.all_key_refs()}


class NoAvailableKeyError(Exception):
    def __init__(self, retry_after: int):
        super().__init__(f"no available upstream key; retry after {retry_after}s")
        self.retry_after = retry_after


def weighted_pick(keys: list[KeyRef], session_id: str | None, alias: str) -> KeyRef:
    total = sum(max(0, key.weight) for key in keys)
    if total <= 0:
        return keys[0]
    if session_id:
        seed = hashlib.sha256(f"{alias}:{session_id}".encode()).digest()
        target = int.from_bytes(seed[:8], "big") % total
    else:
        target = random.randrange(total)
    running = 0
    for key in keys:
        running += max(0, key.weight)
        if target < running:
            return key
    return keys[-1]


def default_key_weights() -> dict[str, int]:
    weights: dict[str, int] = {}
    for alias in ALIASES.values():
        for key in alias.keys:
            weights[key.name] = key.weight
    return weights


def default_provider_base_urls() -> dict[str, str]:
    base_urls: dict[str, str] = {}
    for alias in ALIASES.values():
        base_urls.setdefault(alias.provider, alias.base_url)
    return base_urls


def all_key_refs() -> list[KeyRef]:
    refs: dict[str, KeyRef] = {}
    for alias in ALIASES.values():
        for key in alias.keys:
            refs[key.name] = key
    return [refs[name] for name in sorted(refs)]


def known_key_names() -> set[str]:
    return {key.name for key in all_key_refs()}


def normalize_custom_key_name(value: str) -> str:
    name = value.strip()
    upper_name = name.upper()
    for prefix in ("OPENCODE_AI_ARK_", "AI_ARK_"):
        if upper_name.startswith(prefix):
            name = name[len(prefix) :]
            break
    if name.upper().endswith("_API_KEY"):
        name = name[: -len("_API_KEY")]
    return name.strip("_-").lower().replace("_", "-")


def parse_retry_after(value: str | None) -> float | None:
    if not value:
        return None
    try:
        return time.time() + max(1, int(value))
    except ValueError:
        pass
    try:
        parsed = parsedate_to_datetime(value)
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.timestamp()
    except (TypeError, ValueError):
        return None


def parse_quota_reset(text: str, settings: Settings) -> tuple[float, str] | None:
    lowered = text.lower()
    monthly = "you have exceeded the monthly usage quota" in lowered
    five_hour = "you have exceeded the 5-hour usage quota" in lowered
    if not monthly and not five_hour:
        return None

    reset_at = parse_reset_timestamp(text)
    if reset_at is not None:
        return reset_at, "monthly_quota" if monthly else "five_hour_quota"
    if monthly:
        return time.time() + settings.monthly_quota_fallback_seconds, "monthly_quota"
    return time.time() + settings.five_hour_quota_fallback_seconds, "five_hour_quota"


def parse_reset_timestamp(text: str) -> float | None:
    import re

    match = re.search(
        r"reset at (\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2}) ([+-]\d{4})",
        text,
        re.IGNORECASE,
    )
    if not match:
        return None
    reset_at = datetime.strptime(" ".join(match.groups()), "%Y-%m-%d %H:%M:%S %z")
    return reset_at.timestamp()
