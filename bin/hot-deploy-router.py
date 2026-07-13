#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

SLOTS = {
    "blue": {"port": 8790, "url": "http://127.0.0.1:8790"},
    "green": {"port": 8791, "url": "http://127.0.0.1:8791"},
}
STATE_DIR = Path(os.path.expanduser("~/.local/state/llm-provider-router"))
ACTIVE_FILE = STATE_DIR / "active-backend.json"
PROXY_SERVICE = "llm-provider-router.service"
BACKEND_TEMPLATE = "llm-provider-router-backend@{}.service"


def run(args: list[str], *, check: bool = True, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        check=check,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
    )


def systemctl(*args: str, check: bool = True, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return run(["systemctl", "--user", *args], check=check, capture=capture)


def systemctl_quiet(*args: str) -> subprocess.CompletedProcess[str]:
    return run(["systemctl", "--user", *args], check=False, capture=True)


def read_active_slot() -> str | None:
    if not ACTIVE_FILE.exists():
        return None
    try:
        data = json.loads(ACTIVE_FILE.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    slot = data.get("slot")
    return slot if slot in SLOTS else None


def write_active_slot(slot: str) -> None:
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    payload = {"slot": slot, "base_url": SLOTS[slot]["url"], "updated_at": int(time.time())}
    tmp = ACTIVE_FILE.with_suffix(f".json.{os.getpid()}.tmp")
    tmp.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    tmp.replace(ACTIVE_FILE)


def inactive_slot(active: str | None) -> str:
    return "green" if active == "blue" else "blue"


def service_name(slot: str) -> str:
    return BACKEND_TEMPLATE.format(slot)


def is_active(service: str) -> bool:
    result = systemctl("is-active", service, check=False, capture=True)
    return (result.stdout or "").strip() == "active"


def health_url(url: str) -> bool:
    try:
        with urllib.request.urlopen(f"{url.rstrip('/')}/health", timeout=2) as response:
            return 200 <= response.status < 300
    except Exception:
        return False


def wait_healthy(slot: str, timeout: int) -> None:
    deadline = time.time() + timeout
    url = SLOTS[slot]["url"]
    while time.time() < deadline:
        if health_url(url):
            return
        time.sleep(1)
    raise RuntimeError(f"backend {slot} did not become healthy at {url}/health within {timeout}s")


def ensure_proxy_running() -> None:
    systemctl("enable", "--now", PROXY_SERVICE)


def start_backend(slot: str, timeout: int) -> None:
    systemctl_quiet("reset-failed", service_name(slot))
    systemctl("start", service_name(slot))
    wait_healthy(slot, timeout)


def stop_backend(slot: str) -> None:
    systemctl_quiet("stop", service_name(slot))



def status() -> None:
    active = read_active_slot()
    print(f"active_slot={active or 'unset'}")
    for slot, meta in SLOTS.items():
        print(
            f"{slot}: service={'active' if is_active(service_name(slot)) else 'inactive'} "
            f"health={'ok' if health_url(meta['url']) else 'fail'} url={meta['url']}"
        )
    print(f"proxy: service={'active' if is_active(PROXY_SERVICE) else 'inactive'}")
    print(f"active_file={ACTIVE_FILE}")


def deploy(args: argparse.Namespace) -> None:
    ensure_proxy_running()
    current = read_active_slot()
    target = args.slot or inactive_slot(current)
    old = current if current in SLOTS and current != target else None

    print(f"current={current or 'unset'} target={target} old={old or 'none'}")
    start_backend(target, args.health_timeout)
    write_active_slot(target)
    print(f"switched active backend to {target} ({SLOTS[target]['url']})")

    if old and args.drain_seconds >= 0:
        print(f"draining old backend {old} for {args.drain_seconds}s")
        time.sleep(args.drain_seconds)
        stop_backend(old)
        print(f"stopped old backend {old}")

    status()


def bootstrap(args: argparse.Namespace) -> None:
    slot = args.slot
    start_backend(slot, args.health_timeout)
    write_active_slot(slot)
    ensure_proxy_running()
    other = inactive_slot(slot)
    if args.stop_other:
        stop_backend(other)
    status()


def main() -> int:
    parser = argparse.ArgumentParser(description="Blue/green deploy llm-provider-router without dropping active streams.")
    sub = parser.add_subparsers(dest="command", required=True)

    p_boot = sub.add_parser("bootstrap", help="Start an initial backend slot and point the proxy at it.")
    p_boot.add_argument("--slot", choices=sorted(SLOTS), default="blue")
    p_boot.add_argument("--health-timeout", type=int, default=30)
    p_boot.add_argument("--stop-other", action="store_true")
    p_boot.set_defaults(func=bootstrap)

    p_deploy = sub.add_parser("deploy", help="Start inactive slot, switch traffic, then drain old slot.")
    p_deploy.add_argument("--slot", choices=sorted(SLOTS), help="Target slot. Defaults to inactive slot.")
    p_deploy.add_argument("--health-timeout", type=int, default=30)
    p_deploy.add_argument("--drain-seconds", type=int, default=120)
    p_deploy.set_defaults(func=deploy)

    p_status = sub.add_parser("status", help="Print active slot, services, and health.")
    p_status.set_defaults(func=lambda _args: status())

    args = parser.parse_args()
    try:
        args.func(args)
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
