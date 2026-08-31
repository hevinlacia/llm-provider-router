#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
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


def bring_up_backend(slot: str, timeout: int, *, restart: bool) -> None:
    """拉起一个后端 slot 并等待健康。

    - `restart=True`（部署路径必用）：强制 `systemctl restart`。
      蓝绿部署下两个 slot 通常都保持运行（温备），`systemctl start` 对已在运行的
      服务是 no-op，会导致目标 slot 继续跑旧二进制、切换后仍是旧版本（曾真实踩中）。
    - `restart=False`（bootstrap 首次拉起）：`systemctl start` 即可，已运行则幂等。
    """
    systemctl_quiet("reset-failed", service_name(slot))
    if restart:
        systemctl("restart", service_name(slot))
    else:
        systemctl("start", service_name(slot))
    wait_healthy(slot, timeout)


def stop_backend(slot: str) -> None:
    systemctl_quiet("stop", service_name(slot))


def proxy_url() -> str:
    host = os.environ.get("LLM_PROVIDER_ROUTER_PROXY_HOST", "127.0.0.1")
    port = os.environ.get("LLM_PROVIDER_ROUTER_PROXY_PORT", "8789")
    return f"http://{host}:{port}"


# AGENTS.md：切换后必须立即用 front-proxy 入口验证核心 API 返回正常 JSON（而非 dashboard HTML fallback）。
VERIFY_PATHS = [
    "/health",
    "/api/config/token-prices",
    "/api/config/v2/physical-models",
    "/api/router/capabilities",
]


def verify_after_switch() -> tuple[bool, list[str]]:
    """通过 front-proxy 校验核心 API 未被 dashboard HTML fallback 兜住。

    判定：响应 Content-Type 不含 `text/html` 即视为路由正常（注册路由的 4xx 如
    PUT-only 的 405 不算 fallback）。返回 (是否全部通过, 失败明细)。
    """
    base = proxy_url().rstrip("/")
    failures: list[str] = []
    for path in VERIFY_PATHS:
        url = f"{base}{path}"
        try:
            req = urllib.request.Request(url, method="GET")
            with urllib.request.urlopen(req, timeout=5) as resp:
                ctype = resp.headers.get("Content-Type", "")
                status = resp.status
        except urllib.error.HTTPError as exc:
            ctype = exc.headers.get("Content-Type", "")
            status = exc.code
        except Exception as exc:  # noqa: BLE001 - 网络/解析类错误统一归为验证失败
            failures.append(f"{path}: {exc}")
            continue
        if "text/html" in ctype:
            failures.append(
                f"{path}: dashboard HTML fallback (HTTP {status}, content-type={ctype})"
            )
    return (not failures, failures)



def fetch_proxy_slot_management() -> dict | None:
    """从 front-proxy /_proxy/health 拉取槽生命周期管理快照（新版本字段，缺失则 None）。"""
    try:
        with urllib.request.urlopen(f"{proxy_url().rstrip('/')}/_proxy/health", timeout=3) as resp:
            data = json.loads(resp.read().decode("utf-8"))
            return data.get("slot_management")
    except Exception:
        return None


def status() -> None:
    active = read_active_slot()
    mgmt = fetch_proxy_slot_management()
    print(f"active_slot={active or 'unset'}")
    idle_cfg = f"idle_shutdown={mgmt['idle_shutdown_secs']}s" if mgmt else "idle_shutdown=?s(旧 proxy,需升级)"
    print(f"slot_mgmt: {idle_cfg} check_interval={mgmt['check_interval_secs'] if mgmt else '?'}s auto_heal={'on' if mgmt and mgmt['auto_heal'] else 'off'}")
    for slot, meta in SLOTS.items():
        detail = ""
        if mgmt:
            info = next((s for s in mgmt["slots"] if s["slot"] == slot), None)
            if info:
                detail = (
                    f" idle_for={info['idle_for_secs']}s"
                    f" is_active={info['is_active']}"
                    f" last_action={info['last_action']}"
                )
        print(
            f"{slot}: service={'active' if is_active(service_name(slot)) else 'inactive'} "
            f"health={'ok' if health_url(meta['url']) else 'fail'} url={meta['url']}{detail}"
        )
    print(f"proxy: service={'active' if is_active(PROXY_SERVICE) else 'inactive'}")
    print(f"active_file={ACTIVE_FILE}")


def deploy(args: argparse.Namespace) -> int:
    ensure_proxy_running()
    current = read_active_slot()
    target = args.slot or inactive_slot(current)
    if target == current:
        print(
            f"ERROR: target slot {target} is the current active slot; deploy must target "
            f"the inactive slot ({inactive_slot(current)})",
            file=sys.stderr,
        )
        return 1
    old = current if current in SLOTS and current != target else None

    print(f"current={current or 'unset'} target={target} old={old or 'none'}")
    # 部署必强制重启目标 slot，确保加载当前二进制（start 对已运行服务是 no-op）
    bring_up_backend(target, args.health_timeout, restart=True)
    write_active_slot(target)
    print(f"switched active backend to {target} ({SLOTS[target]['url']})")

    # 切换后立即用 front-proxy 复检（AGENTS.md）；任一失败即回滚，不得停留在未验证状态
    ok, failures = verify_after_switch()
    if not ok:
        for failure in failures:
            print(f"VERIFY FAIL: {failure}", file=sys.stderr)
        if old:
            print(f"rolling back to {old}...", file=sys.stderr)
            write_active_slot(old)
            bring_up_backend(old, args.health_timeout, restart=False)
        stop_backend(target)
        status()
        return 1

    if old:
        # 旧槽自动下线：切换后旧槽继续运行（无 drain 等待），由 front-proxy 生命周期管理
        # 在连续无流量超过 idle_shutdown_secs（默认 15 分钟）后自动 systemctl stop 下线；
        # 切回该槽时入口自动拉起。需手动停某槽仍可 systemctl --user stop backend@{old}。
        print(
            f"old backend {old} kept running; front-proxy will auto-stop it after "
            f"idle (default 15min) with no traffic"
        )

    status()
    return 0


def bootstrap(args: argparse.Namespace) -> None:
    slot = args.slot
    bring_up_backend(slot, args.health_timeout, restart=False)
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

    p_deploy = sub.add_parser("deploy", help="Restart inactive slot with current binary, switch traffic, verify, keep old slot as warm standby.")
    p_deploy.add_argument("--slot", choices=sorted(SLOTS), help="Target slot. Defaults to inactive slot.")
    p_deploy.add_argument("--health-timeout", type=int, default=30)
    p_deploy.set_defaults(func=deploy)

    p_status = sub.add_parser("status", help="Print active slot, services, and health.")
    p_status.set_defaults(func=lambda _args: status())

    args = parser.parse_args()
    try:
        result = args.func(args)
        # 子命令返回 int（0/1）时透传退出码；返回 None 视为成功
        return result if isinstance(result, int) else 0
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
