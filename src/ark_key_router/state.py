from __future__ import annotations

import hashlib
import random
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime

from .config import KeyRef, ModelAlias, Settings
from .usage_store import UsageStore


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

        key = weighted_pick(candidates, session_id=session_id, alias=alias.alias)
        if session_id:
            self.bind(alias.alias, session_id, key.name)
        return key

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
