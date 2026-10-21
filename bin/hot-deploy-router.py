#!/usr/bin/env python3
from __future__ import annotations

import argparse
import gzip
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from deploy_lock import HeldLock, LockHeldError, acquire as lock_acquire, probe as lock_probe, release as lock_release  # noqa: E402

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


def verify_after_switch(model: str = "low-model-auto") -> tuple[bool, list[str]]:
    """通过 front-proxy 校验核心 API 未被 dashboard HTML fallback 兜住。

    判定：响应 Content-Type 不含 `text/html` 即视为路由正常（注册路由的 4xx 如
    PUT-only 的 405 不算 fallback）。另跑头语义与 SSE 完整性探针（2026-10-21 事故后新增）。
    返回 (是否全部通过, 失败明细)。
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
    failures += header_probe(base)
    verdict, detail = sse_probe(base, model)
    if verdict == "fail":
        failures.append(f"SSE probe: {detail}")
    return (not failures, failures)


# ---------- 部署探针（2026-10-21 两起事故后新增） ----------
# 口径：不信任单组件测试与人工冒烟，对“客户端可见结果”做验证。
# - SSE 完整性：逐行 JSON 可解析、事件无重复、终态存在、SSE 不被压缩。
#   结构性损坏 = fail（阻塞部署/触发回滚）；上游原因（超时/上游错误）= skip（不阻塞）。
# - 头语义：JSON 压缩带头且可解压；dashboard no-cache。


def _http(url: str, method: str = "GET", data: bytes | None = None,
          headers: dict | None = None, timeout: int = 8):
    req = urllib.request.Request(url, method=method, data=data)
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    return urllib.request.urlopen(req, timeout=timeout)


def sse_probe(base_url: str, model: str, timeout: int = 25) -> tuple[str, str]:
    """流式 /v1/responses 字节级完整性探针。返回 (verdict, detail)。

    verdict: "pass" | "fail"（结构性损坏，阻塞）| "skip"（上游原因，不阻塞）。
    """
    try:
        payload = json.dumps(
            {"model": model, "stream": True, "input": "hi", "max_output_tokens": 32}
        ).encode("utf-8")
        headers = {"Content-Type": "application/json", "Accept": "text/event-stream"}
        # 上游偶发 4xx/抖动（如 Ark 瞬时 400）会污染探针：非 200 重试一次再定 skip
        last_error: Exception | None = None
        resp = status = ctype = enc = None
        for attempt in range(2):
            try:
                resp = _http(
                    f"{base_url.rstrip('/')}/v1/responses",
                    "POST",
                    payload,
                    headers,
                    timeout,
                )
                status = resp.status
                if status == 200:
                    break
            except Exception as exc:  # noqa: BLE001
                last_error = exc
            time.sleep(1)
        if resp is None or status != 200:
            hint = f"last_error={last_error}" if last_error else f"HTTP {status}"
            return "skip", f"transport error after retry (upstream unhealthy?): {hint}"
        ctype = resp.headers.get("Content-Type", "")
        enc = resp.headers.get("Content-Encoding")
        chunks = []
        while True:
            piece = resp.read(8192)
            if not piece:
                break
            chunks.append(piece)
        body = b"".join(chunks).decode("utf-8", errors="replace")
    except Exception as exc:  # noqa: BLE001
        return "skip", f"transport error (upstream unhealthy?): {exc}"
    if status != 200:
        return "skip", f"HTTP {status}"
    if "text/event-stream" not in ctype:
        return "fail", f"SSE content-type wrong: {ctype}"
    if enc:
        return "fail", f"SSE must not be compressed, got content-encoding={enc}"
    if 'upstream_error' in body:
        return "skip", "router returned upstream_error (provider unhealthy; not a deploy blocker)"
    data_lines = [line for line in body.split("\n") if line.startswith("data: ")]
    if not data_lines:
        return "skip", "no data lines (empty stream)"
    seen: set = set()
    types: list[str] = []
    for line in data_lines:
        payload = line[len("data: "):]
        if payload == "[DONE]":
            types.append("[DONE]")
            continue
        try:
            event = json.loads(payload)
        except json.JSONDecodeError as exc:
            return "fail", f"malformed SSE JSON: {exc}: {payload[:120]!r}"
        etype = event.get("type", "?") if isinstance(event, dict) else "?"
        key = (etype, event.get("sequence_number") if isinstance(event, dict) else None)
        if key in seen:
            return "fail", f"duplicated SSE event {key} (byte corruption)"
        seen.add(key)
        types.append(etype)
    if types.count("response.created") != 1:
        return "fail", f"response.created count={types.count('response.created')} (expect 1)"
    terminal = {"response.completed", "response.incomplete", "response.failed", "[DONE]"}
    if not (set(types) & terminal):
        return "skip", f"no terminal event; tail={types[-4:]}"
    return "pass", f"{len(data_lines)} data lines ok"


def header_probe(base_url: str) -> list[str]:
    """响应头语义探针（2026-10-21 事故二回归）。返回失败明细列表。"""
    failures: list[str] = []
    base = base_url.rstrip("/")
    try:
        with _http(f"{base}/api/state", headers={"Accept-Encoding": "gzip"}) as resp:
            encoding = resp.headers.get("Content-Encoding")
            body = resp.read()
        if encoding != "gzip":
            failures.append(
                f"/api/state with AE gzip: content-encoding={encoding} (expect gzip; front-proxy 不得剥头)"
            )
        else:
            try:
                json.loads(gzip.decompress(body))
            except Exception as exc:  # noqa: BLE001
                failures.append(f"/api/state gzip body invalid: {exc}")
    except Exception as exc:  # noqa: BLE001
        failures.append(f"/api/state probe: {exc}")
    try:
        with _http(f"{base}/api/state") as resp:
            if resp.headers.get("Content-Encoding"):
                failures.append("/api/state without AE: unexpected content-encoding")
    except Exception as exc:  # noqa: BLE001
        failures.append(f"/api/state plain probe: {exc}")
    try:
        with _http(f"{base}/") as resp:
            cc = resp.headers.get("Cache-Control")
            ctype = resp.headers.get("Content-Type", "")
        if "text/html" in ctype and cc != "no-cache":
            failures.append(f"/ cache-control={cc} (expect no-cache)")
    except Exception as exc:  # noqa: BLE001
        failures.append(f"/ probe: {exc}")
    return failures


def precheck_target(slot: str, model: str) -> tuple[list[str], list[str]]:
    """切流前对新槽直接跑探针（不经过 front-proxy）。返回 (硬失败, 警告)。"""
    url = SLOTS[slot]["url"]
    failures = header_probe(url)
    verdict, detail = sse_probe(url, model)
    warnings: list[str] = []
    if verdict == "fail":
        failures.append(f"SSE probe: {detail}")
    elif verdict == "skip":
        warnings.append(f"SSE probe skipped: {detail}")
    return failures, warnings


def precheck_cmd(args: argparse.Namespace) -> int:
    model = args.model or os.environ.get("LLM_PROVIDER_ROUTER_PROBE_MODEL", "low-model-auto")
    url = args.url or SLOTS[args.slot]["url"]
    print(f"precheck {url} (model={model})", flush=True)
    header_failures = header_probe(url)
    for failure in header_failures:
        print(f"HEADER FAIL: {failure}", file=sys.stderr)
    verdict, detail = sse_probe(url, model)
    if verdict == "fail":
        header_failures.append(f"SSE probe: {detail}")
        print(f"SSE FAIL: {detail}", file=sys.stderr)
    elif verdict == "skip":
        print(f"SSE SKIP: {detail}")
    else:
        print(f"SSE PASS: {detail}")
    if header_failures:
        return 1
    print("precheck ok")
    return 0



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


def switch_via_proxy(slot: str, token: str | None) -> dict:
    """通过 front-proxy 切流（携带部署锁 token；proxy 侧校验失败会返回 409）。"""
    ensure_proxy_running()
    url = f"{proxy_url().rstrip('/')}/_proxy/active/{slot}"
    req = urllib.request.Request(url, method="POST", data=b"")
    if token:
        req.add_header("X-Deploy-Lock", token)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        try:
            detail = json.loads(body)
        except json.JSONDecodeError:
            detail = {"error": body[:200]}
        lock = detail.get("lock") or {}
        raise RuntimeError(
            f"proxy rejected switch (HTTP {exc.code}): {detail.get('error', body[:120])}"
            + (f"; lock held by {lock.get('holder')} pid={lock.get('holder_pid')}" if lock else "")
        ) from None


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
    try:
        # 部署互斥锁（2026-10-08 事故：两会话同时部署互相覆盖槽位状态）：
        # 整个重启目标槽→切流→复检→（可能回滚）流程持锁，其他部署等待或失败
        with HeldLock(args.lock_wait, holder=f"deploy(pid:{os.getpid()})") as lock_token:
            # 部署必强制重启目标 slot，确保加载当前二进制（start 对已运行服务是 no-op）
            bring_up_backend(target, args.health_timeout, restart=True)
            # 切流前探针（2026-10-21 事故后新增）：对新槽直接验证头语义 + SSE 完整性，
            # 硬失败中止部署（目标槽已验证异常，不得切流）；skip 类仅警告。
            precheck_failures, precheck_warnings = precheck_target(target, args.probe_model)
            for warning in precheck_warnings:
                print(f"PRECHECK SKIP: {warning}", file=sys.stderr)
            if precheck_failures:
                for failure in precheck_failures:
                    print(f"PRECHECK FAIL: {failure}", file=sys.stderr)
                stop_backend(target)
                status()
                return 1
            print(f"precheck {target} ok")
            # 必须携带自己的 token：proxy 对持锁状态的切流请求校验 token，不带会 409
            switch_via_proxy(target, lock_token)
            print(f"switched active backend to {target} ({SLOTS[target]['url']})")

            # 切换后立即用 front-proxy 复检（AGENTS.md）；任一失败即回滚，不得停留在未验证状态
            ok, failures = verify_after_switch(args.probe_model)
            if not ok:
                for failure in failures:
                    print(f"VERIFY FAIL: {failure}", file=sys.stderr)
                if old:
                    print(f"rolling back to {old}...", file=sys.stderr)
                    switch_via_proxy(old, lock_token)
                    bring_up_backend(old, args.health_timeout, restart=False)
                stop_backend(target)
                status()
                return 1
    except LockHeldError as exc:
        print(f"ERROR: another deploy is in progress: {exc}", file=sys.stderr)
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


def stage(args: argparse.Namespace) -> int:
    """两段式部署阶段 1：重启非活跃槽加载新二进制，获取并持有部署锁（跨调用存活）。

    锁由 fork 出的 holder 子进程持有，stage→probe→switch 之间不释放；
    switch 后用 `lock-release --token <T>` 释放。
    """
    ensure_proxy_running()
    current = read_active_slot()
    target = args.slot or inactive_slot(current)
    try:
        token = lock_acquire(args.lock_wait, holder=f"stage(slot:{target}, pid:{os.getpid()})")
    except LockHeldError as exc:
        print(f"ERROR: another deploy is in progress: {exc}", file=sys.stderr)
        return 1
    bring_up_backend(target, args.health_timeout, restart=True)
    print(f"staged slot={target} health=ok (current active={current or 'unset'} unchanged)")
    print(f"lock_token={token}")
    print(f"next: probe the slot, then `hot-deploy-router.py switch --slot {target} --lock-token {token}`, finally `lock-release --token {token}`")
    return 0


def switch(args: argparse.Namespace) -> int:
    """两段式部署阶段 2：校验锁持有后经 front-proxy 切流并复检。"""
    state = lock_probe()
    if not state["locked"]:
        print(
            "ERROR: no deploy lock is held; run `lock-acquire` (or one-shot `deploy`) first — "
            "unlocked switches are how two sessions overwrite each other (2026-10-08)",
            file=sys.stderr,
        )
        return 1
    holder = state["holder"] or {}
    if not args.lock_token or args.lock_token != holder.get("token"):
        print(
            f"ERROR: deploy lock is held by {holder.get('holder', 'unknown')} "
            f"(pid={holder.get('holder_pid')}) and the provided token does not match",
            file=sys.stderr,
        )
        return 1
    current = read_active_slot()
    if args.slot == current:
        print(f"slot {args.slot} is already active; nothing to do")
        return 0
    result = switch_via_proxy(args.slot, args.lock_token)
    print(f"switched active backend to {args.slot} ({result.get('backend', '')})")
    ok, failures = verify_after_switch(args.probe_model)
    if not ok:
        for failure in failures:
            print(f"VERIFY FAIL: {failure}", file=sys.stderr)
        print(f"rolling back to {current}...", file=sys.stderr)
        switch_via_proxy(current, args.lock_token)
        bring_up_backend(current, args.health_timeout, restart=False)
        return 1
    print(f"lock_token={args.lock_token} still held; release with `lock-release --token {args.lock_token}` when done")
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


def lock_release_cmd(a: argparse.Namespace) -> None:
    if not lock_release(a.token, a.force):
        raise SystemExit(1)


def _print_lock(fn) -> None:
    """lock 子命令统一出口：异常转 stderr + 退出码，成功打印返回值。"""
    try:
        result = fn()
        if isinstance(result, dict):
            holder = result.get("holder") or {}
            if result.get("locked"):
                print(f"locked by {holder.get('holder', 'unknown')} (pid={holder.get('holder_pid', '?')}, since={holder.get('acquired_at', '?')}, token={holder.get('token', '?')})")
            else:
                print("free")
        elif result is not None:
            print(result)
    except LockHeldError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        raise SystemExit(1) from None


def main() -> int:
    parser = argparse.ArgumentParser(description="Blue/green deploy llm-provider-router without dropping active streams.")
    sub = parser.add_subparsers(dest="command", required=True)

    p_boot = sub.add_parser("bootstrap", help="Start an initial backend slot and point the proxy at it.")
    p_boot.add_argument("--slot", choices=sorted(SLOTS), default="blue")
    p_boot.add_argument("--health-timeout", type=int, default=30)
    p_boot.add_argument("--stop-other", action="store_true")
    p_boot.set_defaults(func=bootstrap)

    p_deploy = sub.add_parser("deploy", help="Restart inactive slot with current binary, switch traffic, verify, keep old slot as warm standby. Holds the deploy mutex for the whole run.")
    p_deploy.add_argument("--slot", choices=sorted(SLOTS), help="Target slot. Defaults to inactive slot.")
    p_deploy.add_argument("--health-timeout", type=int, default=30)
    p_deploy.add_argument("--lock-wait", type=int, default=300, help="Seconds to wait when another deploy holds the lock (0=fail fast, default 300).")
    p_deploy.add_argument("--probe-model", default=None, help="Model for the SSE integrity probe (default $LLM_PROVIDER_ROUTER_PROBE_MODEL or low-model-auto).")
    p_deploy.set_defaults(func=deploy)

    p_stage = sub.add_parser("stage", help="Two-phase deploy step 1: restart inactive slot with current binary; acquires and HOLDS the deploy lock across invocations.")
    p_stage.add_argument("--slot", choices=sorted(SLOTS), help="Target slot. Defaults to inactive slot.")
    p_stage.add_argument("--health-timeout", type=int, default=30)
    p_stage.add_argument("--lock-wait", type=int, default=300, help="Seconds to wait when another deploy holds the lock (0=fail fast, default 300).")
    p_stage.set_defaults(func=stage)

    p_switch = sub.add_parser("switch", help="Two-phase deploy step 2: verify deploy lock token, switch traffic via front-proxy, verify, keep old slot warm.")
    p_switch.add_argument("--slot", choices=sorted(SLOTS), required=True, help="Target slot.")
    p_switch.add_argument("--lock-token", required=True, help="Token printed by `stage`/`lock-acquire`.")
    p_switch.add_argument("--health-timeout", type=int, default=30)
    p_switch.add_argument("--probe-model", default=None)
    p_switch.set_defaults(func=switch)

    p_lacq = sub.add_parser("lock-acquire", help="Acquire the deploy lock without touching slots; prints token.")
    p_lacq.add_argument("--wait", type=int, default=300)
    p_lacq.add_argument("--holder", default=f"manual(pid:{os.getpid()})")
    p_lacq.set_defaults(func=lambda a: _print_lock(lambda: lock_acquire(a.wait, a.holder)))

    p_lrel = sub.add_parser("lock-release", help="Release the deploy lock (token must match; --force for emergencies).")
    p_lrel.add_argument("--token", required=True)
    p_lrel.add_argument("--force", action="store_true")
    p_lrel.set_defaults(func=lock_release_cmd)

    p_lstat = sub.add_parser("lock-status", help="Show deploy lock holder or free.")
    p_lstat.set_defaults(func=lambda _a: _print_lock(lock_probe))

    p_status = sub.add_parser("status", help="Print active slot, services, and health.")
    p_status.set_defaults(func=lambda _args: status())

    p_pre = sub.add_parser("precheck", help="Run header-semantics and SSE-integrity probes against a slot/URL without deploying.")
    p_pre.add_argument("--slot", choices=sorted(SLOTS))
    p_pre.add_argument("--url", help="Probe an arbitrary base URL instead of a slot.")
    p_pre.add_argument("--model", default=None, help="Model for the SSE probe (default $LLM_PROVIDER_ROUTER_PROBE_MODEL or low-model-auto).")
    p_pre.set_defaults(func=precheck_cmd)

    args = parser.parse_args()
    if getattr(args, "probe_model", None) is None:
        args.probe_model = os.environ.get("LLM_PROVIDER_ROUTER_PROBE_MODEL", "low-model-auto")
    try:
        result = args.func(args)
        # 子命令返回 int（0/1）时透传退出码；返回 None 视为成功
        return result if isinstance(result, int) else 0
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
