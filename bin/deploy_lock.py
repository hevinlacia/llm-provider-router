#!/usr/bin/env python3
"""部署互斥锁：flock 文件锁，防止两个会话/进程同时部署 llm-provider-router。

背景（2026-10-08 事故）：两个 agent 会话同时执行部署/切流，槽位状态互相覆盖，
active 槽被翻回旧版本二进制，设置页/新端点整体 405/HTML fallback。

机制：
- 锁文件 `~/.local/state/llm-provider-router/deploy.lock`（可用环境变量
  `LLM_PROVIDER_ROUTER_DEPLOY_LOCK_FILE` 覆盖），内容为持锁者信息 JSON。
- 互斥靠 `flock(LOCK_EX)`：持有者进程死亡（含 kill -9）内核自动释放，无残留死锁；
  锁文件内容残留不代表仍持锁，判定一律以"能否抢到 flock"为准。
- token：acquire 时生成随机 token 写入锁文件；持锁者切流时通过
  `X-Deploy-Lock: <token>` 请求头向 front-proxy 证明身份，proxy 侧校验
  "锁被持有 && token 不匹配" → 409 拒绝切流。
- 跨进程长持锁：stage→probe→switch 分多次调用脚本，acquire 成功后 fork 子进程
  继承已锁 fd 并驻留，锁跨调用存活；release 用 SIGTERM 结束 holder 子进程。

CLI（也可 import DeployLock 当库用）：
  deploy_lock.py acquire  [--wait N] [--holder NAME]   # 成功打印 token
  deploy_lock.py release  --token T [--force]          # token 匹配才释放；force 应急
  deploy_lock.py status                                # free / held(含持有者信息)
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import secrets
import signal
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_LOCK_PATH = "~/.local/state/llm-provider-router/deploy.lock"


def lock_path() -> Path:
    return Path(
        os.environ.get(
            "LLM_PROVIDER_ROUTER_DEPLOY_LOCK_FILE",
            DEFAULT_LOCK_PATH,
        ).replace("~", os.path.expanduser("~"), 1)
        if os.environ.get("LLM_PROVIDER_ROUTER_DEPLOY_LOCK_FILE", "").startswith("~")
        else os.environ.get("LLM_PROVIDER_ROUTER_DEPLOY_LOCK_FILE", DEFAULT_LOCK_PATH)
    ).expanduser()


class LockHeldError(RuntimeError):
    """锁被其他持有者占用（等待超时）。"""

    def __init__(self, holder: dict | None):
        self.holder = holder or {}
        desc = self.holder.get("holder", "unknown")
        pid = self.holder.get("holder_pid", "?")
        since = self.holder.get("acquired_at", "?")
        super().__init__(
            f"deploy lock held by {desc} (pid={pid}, since={since}); "
            f"等待其释放或 kill {pid} 后重试，应急可 lock-release --force"
        )


def _write_holder(fd: int, token: str, holder_pid: int, holder: str) -> None:
    os.lseek(fd, 0, os.SEEK_SET)
    os.ftruncate(fd, 0)
    payload = {
        "token": token,
        "holder": holder,
        "holder_pid": holder_pid,
        "acquired_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }
    os.write(fd, (json.dumps(payload, indent=2) + "\n").encode("utf-8"))


def read_holder(path: Path | None = None) -> dict | None:
    """读锁文件内容（仅供参考，是否真持锁以 flock 为准）。"""
    try:
        return json.loads((path or lock_path()).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def probe(path: Path | None = None) -> dict:
    """探测锁状态：{'locked': bool, 'holder': dict|None}。

    通过对独立 fd 抢非阻塞 flock 判定：抢得到 = 空闲（内容是残留）；
    抢不到 = 真被持有。返回前先释放自己抢到的锁。
    """
    path = path or lock_path()
    try:
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    except OSError:
        return {"locked": False, "holder": None}
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(fd, fcntl.LOCK_UN)
            return {"locked": False, "holder": None}
        except OSError:
            return {"locked": True, "holder": read_holder(path)}
    finally:
        os.close(fd)


def acquire(wait_secs: int = 300, holder: str = "unknown", poll: float = 0.5) -> str:
    """获取锁并保持（fork holder 子进程驻留），返回 token。

    - wait_secs>0：被占用时轮询等待，超时抛 LockHeldError（含持有者信息）。
    - wait_secs=0：立即失败（fail fast）。
    """
    path = lock_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    deadline = time.monotonic() + max(wait_secs, 0)
    while True:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except OSError:
            current = read_holder(path)
            if time.monotonic() >= deadline:
                raise LockHeldError(current) from None
            print(
                f"deploy lock held by {(current or {}).get('holder', 'unknown')}, "
                f"waiting up to {int(deadline - time.monotonic())}s...",
                file=sys.stderr,
            )
            time.sleep(poll)

    token = secrets.token_hex(8)
    child = os.fork()
    if child == 0:
        # holder 子进程：脱离调用方 stdio（否则继承的 stdout 管道会让 $(acquire)
        # 永远等不到 EOF）并脱离进程组，然后驻留直到收到 SIGTERM。
        try:
            os.setsid()
        except OSError:
            pass
        devnull = os.open(os.devnull, os.O_RDWR)
        for target_fd in (0, 1, 2):
            os.dup2(devnull, target_fd)
        if devnull > 2:
            os.close(devnull)
        signal.signal(signal.SIGTERM, lambda *_: os._exit(0))
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        while True:
            time.sleep(3600)

    _write_holder(fd, token, child, holder)
    return token


class HeldLock:
    """单进程内持有的锁（一次性命令用，如 deploy 全流程），退出/异常自动释放。"""

    def __init__(self, wait_secs: int = 300, holder: str = "unknown"):
        self.wait_secs = wait_secs
        self.holder = holder
        self.token = ""
        self._fd: int | None = None

    def __enter__(self) -> str:
        path = lock_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
        deadline = time.monotonic() + max(self.wait_secs, 0)
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    os.close(fd)
                    raise LockHeldError(read_holder(path)) from None
                time.sleep(0.5)
        self._fd = fd
        self.token = secrets.token_hex(8)
        _write_holder(fd, self.token, os.getpid(), self.holder)
        return self.token

    def __exit__(self, *_exc) -> None:
        if self._fd is not None:
            try:
                fcntl.flock(self._fd, fcntl.LOCK_UN)
            finally:
                os.close(self._fd)
                self._fd = None


def release(token: str, force: bool = False) -> bool:
    """释放锁：token 匹配（或 force）时 SIGTERM holder 子进程，flock 随之释放。"""
    info = read_holder()
    if not info:
        print("deploy lock: no lock file content (already free)")
        return True
    if info.get("token") != token and not force:
        print(
            f"ERROR: token mismatch; lock held by {info.get('holder')} "
            f"(pid={info.get('holder_pid')}); 应急释放用 --force",
            file=sys.stderr,
        )
        return False
    pid = info.get("holder_pid")
    if isinstance(pid, int) and pid > 0:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    # 锁文件内容留着（下次 acquire 覆盖）；状态以 flock 为准
    print(f"deploy lock released (holder pid={pid})")
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description="llm-provider-router deploy mutex lock (flock)")
    sub = parser.add_subparsers(dest="command", required=True)

    p_acq = sub.add_parser("acquire", help="获取部署锁并保持（fork holder 驻留），打印 token")
    p_acq.add_argument("--wait", type=int, default=300, help="被占用时最多等待秒数（0=fail fast，默认 300）")
    p_acq.add_argument("--holder", default=f"pid:{os.getpid()}", help="持锁者标识（如 agent 会话名）")

    p_rel = sub.add_parser("release", help="释放部署锁")
    p_rel.add_argument("--token", required=True)
    p_rel.add_argument("--force", action="store_true", help="应急：忽略 token 不匹配强制释放")

    sub.add_parser("status", help="查看锁状态与持有者")
    args = parser.parse_args()

    if args.command == "acquire":
        token = acquire(args.wait, args.holder)
        print(token)
        return 0
    if args.command == "release":
        return 0 if release(args.token, args.force) else 1
    state = probe()
    if state["locked"]:
        info = state["holder"] or {}
        print(
            f"locked by {info.get('holder', 'unknown')} "
            f"(pid={info.get('holder_pid', '?')}, since={info.get('acquired_at', '?')})"
        )
    else:
        print("free")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
