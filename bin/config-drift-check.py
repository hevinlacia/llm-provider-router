#!/usr/bin/env python3
"""config-drift-check.py — 本机仓库 config/ 与部署实例 config/ 漂移检查。

背景（2026-10-29 zp 限额事故）：部署流程只 scp 二进制，不含 config/*.json 同步，
仓库里已修复的 error-rules.json（zp 模板）没上到 LXC，导致线上按旧规则分类、
限额报错不冻结不切 key。本脚本在部署前后检测这类「二进制新、配置旧」漂移。

用法（workstation 侧运行，repo 为 SSOT）：
  python3 bin/config-drift-check.py --host root@192.168.6.245          # 检查全部配置
  python3 bin/config-drift-check.py --host root@... --diff             # 漂移文件显示 diff
  python3 bin/config-drift-check.py --host root@... --sync             # 推送本地版本（先远端备份）
  python3 bin/config-drift-check.py --host root@... --file error-rules.json
  python3 bin/config-drift-check.py --self-test                        # 纯逻辑自测（无网络）

行为：
- 默认比对本地 config/ 下全部 *.json（跳过密钥文件，见 SECRET_DENY）与远端目录同名文件。
- 状态：ok / drift / remote-missing（仓库有、部署没有）/ local-missing（部署有、仓库没有）。
- 退出码：0 = 无漂移；1 = 有漂移；2 = 运行错误（ssh 失败等）。可作部署前置门禁。
- --sync 方向固定 本地 → 远端：先把远端现存文件备份为 <name>.bak-<UTC 时间戳> 再覆盖，
  不删除远端多余文件（只报告）。dashboard 运行时可编辑部分配置（error-rules 等），
  远端可能领先仓库——先看 --diff 再决定是否 --sync。

密钥安全：api-keys.json / api-keys.sops.json 永不比对/推送（vault 流程单独管理，
见 justfile vault-encrypt/vault-decrypt）；显式 --file 传密钥文件名也会被拒绝。
"""

from __future__ import annotations

import argparse
import difflib
import json
import shlex
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_LOCAL_DIR = REPO_ROOT / "config"
DEFAULT_REMOTE_DIR = "/opt/llm-provider-router/config"

# 永不比对/推送的密钥文件（vault 流程单独管理）。
SECRET_DENY = ("api-keys.json", "api-keys.sops.json")

SSH_OPTS = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]

# 状态常量。
OK = "ok"
DRIFT = "drift"
REMOTE_MISSING = "remote-missing"
LOCAL_MISSING = "local-missing"


def is_secret(name: str) -> bool:
    return name in SECRET_DENY


def classify(local_hash: str | None, remote_hash: str | None) -> str:
    """按本地/远端哈希判定漂移状态（None = 文件缺失）。"""
    if local_hash is not None and remote_hash is None:
        return REMOTE_MISSING
    if local_hash is None and remote_hash is not None:
        return LOCAL_MISSING
    if local_hash is None and remote_hash is None:
        # 双侧都没有不该发生（调用方保证本地存在），按漂移处理引人注意。
        return DRIFT
    return OK if local_hash == remote_hash else DRIFT


def parse_sha256sum(output: str) -> dict[str, str]:
    """解析 `sha256sum f...` 输出为 {文件名: 哈希}；忽略报错行（文件缺失）。"""
    hashes: dict[str, str] = {}
    for line in output.splitlines():
        parts = line.strip().split(None, 1)
        if len(parts) != 2 or len(parts[0]) != 64 or not all(
            c in "0123456789abcdef" for c in parts[0].lower()
        ):
            continue
        name = parts[1].strip()
        # sha256sum 对空格/特殊名会转义；本目录文件名均为简单 ASCII，直接还原。
        if name.startswith("*"):
            name = name[1:]
        hashes[name] = parts[0].lower()
    return hashes


def local_sha256(path: Path) -> str | None:
    import hashlib

    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except FileNotFoundError:
        return None


def run_ssh(host: str, remote_cmd: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["ssh", *SSH_OPTS, host, remote_cmd],
        capture_output=True,
        text=True,
        check=check,
    )


def remote_snapshot(host: str, remote_dir: str, names: list[str]) -> tuple[dict[str, str], list[str]]:
    """一次 ssh 拿远端哈希表 + 远端目录现存 *.json 清单。"""
    name_args = " ".join(shlex.quote(n) for n in names) or "''"
    cmd = (
        f"cd {shlex.quote(remote_dir)} && "
        f"sha256sum {name_args} 2>/dev/null; echo '---LIST---'; "
        f"ls -- *.json 2>/dev/null || true"
    )
    proc = run_ssh(host, cmd)
    out = proc.stdout
    if "---LIST---" not in out:
        raise RuntimeError(f"ssh {host} 输出异常（远端目录不存在?）: {proc.stderr.strip()[:200]}")
    hash_part, list_part = out.split("---LIST---", 1)
    hashes = parse_sha256sum(hash_part)
    remote_files = [line.strip() for line in list_part.splitlines() if line.strip().endswith(".json")]
    return hashes, remote_files


def fetch_remote_text(host: str, remote_dir: str, name: str) -> str:
    proc = run_ssh(host, f"cat {shlex.quote(f'{remote_dir}/{name}')}")
    return proc.stdout


def backup_and_push(
    host: str, remote_dir: str, local_dir: Path, name: str, tag: str, dry: bool
) -> None:
    """备份远端现存文件后推送本地版本；失败抛异常由调用方统一处理。"""
    remote_path = f"{remote_dir}/{name}"
    if dry:
        print(f"  [dry-run] 备份远端 {name} → {name}.bak-{tag}（若存在）并推送本地版本")
        return
    # 远端存在则先备份（cp -p 保留属性；不存在则跳过）。
    run_ssh(
        host,
        f"if [ -f {shlex.quote(remote_path)} ]; then "
        f"cp -p {shlex.quote(remote_path)} {shlex.quote(remote_path)}.bak-{tag}; fi",
    )
    subprocess.run(
        ["scp", "-q", *SSH_OPTS, str(local_dir / name), f"{host}:{remote_path}"],
        check=True,
    )


def self_test() -> int:
    assert classify("a", "a") == OK
    assert classify("a", "b") == DRIFT
    assert classify("a", None) == REMOTE_MISSING
    assert classify(None, "b") == LOCAL_MISSING
    assert is_secret("api-keys.json") and is_secret("api-keys.sops.json")
    assert not is_secret("error-rules.json")
    h = parse_sha256sum("abc def\n" + ("f" * 64) + "  error-rules.json\nsha256sum: x: No such file\n")
    assert list(h) == ["error-rules.json"], h
    assert h["error-rules.json"] == "f" * 64
    print("self-test: all passed")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="仓库 config/ 与部署实例 config/ 漂移检查")
    ap.add_argument("--host", default=None, help="部署实例 ssh 目标，如 root@192.168.6.245（--self-test 时可省）")
    ap.add_argument("--remote-dir", default=DEFAULT_REMOTE_DIR, help=f"远端 config 目录（默认 {DEFAULT_REMOTE_DIR}）")
    ap.add_argument("--local-dir", default=str(DEFAULT_LOCAL_DIR), help=f"本地 config 目录（默认 {DEFAULT_LOCAL_DIR}）")
    ap.add_argument("--file", action="append", default=[], help="只检查指定文件（可重复）；默认全部 *.json")
    ap.add_argument("--diff", action="store_true", help="对漂移文件输出 unified diff（本地 → 远端）")
    ap.add_argument("--sync", action="store_true", help="把本地版本推送到远端（先远端备份 <name>.bak-<ts>）")
    ap.add_argument("--dry-run", action="store_true", help="配合 --sync：只显示将执行的动作")
    ap.add_argument("--json", action="store_true", help="机器可读输出")
    ap.add_argument("--self-test", action="store_true", help="纯逻辑自测，不访问网络")
    args = ap.parse_args()

    if args.self_test:
        return self_test()
    if not args.host:
        ap.error("--host 必填（或用 --self-test）")

    local_dir = Path(args.local_dir)
    if not local_dir.is_dir():
        print(f"error: 本地目录不存在: {local_dir}", file=sys.stderr)
        return 2

    if args.file:
        names = []
        for f in args.file:
            if is_secret(f):
                print(f"error: 拒绝操作密钥文件 {f}（vault 流程单独管理）", file=sys.stderr)
                return 2
            names.append(f)
    else:
        names = sorted(
            p.name
            for p in local_dir.glob("*.json")
            if not is_secret(p.name)
        )
    if not names:
        print("error: 本地无可比对文件", file=sys.stderr)
        return 2

    try:
        remote_hashes, remote_all = remote_snapshot(args.host, args.remote_dir, names)
    except (subprocess.CalledProcessError, RuntimeError) as exc:
        print(f"error: ssh/远端读取失败: {exc}", file=sys.stderr)
        return 2

    results = []
    for name in names:
        lh = local_sha256(local_dir / name)
        rh = remote_hashes.get(name)
        results.append({"file": name, "status": classify(lh, rh), "local_sha256": lh, "remote_sha256": rh})

    # 远端多出的非密钥 json（仓库没有对应文件）——仅全量模式报告（--file 子集下无意义）。
    remote_only = (
        sorted(n for n in remote_all if n not in names and not is_secret(n))
        if not args.file
        else []
    )

    drifted = [r for r in results if r["status"] != OK]
    exit_code = 1 if drifted else 0

    if args.json:
        print(json.dumps({"host": args.host, "remote_dir": args.remote_dir,
                          "results": results, "remote_only": remote_only,
                          "drifted": len(drifted)}, ensure_ascii=False, indent=1))
    else:
        icon = {OK: "ok             ", DRIFT: "DRIFT          ", REMOTE_MISSING: "REMOTE-MISSING ", LOCAL_MISSING: "LOCAL-MISSING  "}
        print(f"host={args.host} remote_dir={args.remote_dir}")
        for r in results:
            print(f"  [{icon[r['status']]}] {r['file']}")
        if remote_only:
            print(f"  [remote-only   ] （仓库无对应文件，仅报告）: {', '.join(remote_only)}")
        print(f"结论: {'有漂移 ' + str(len(drifted)) + ' 个文件' if drifted else '无漂移'}")

    if args.diff and drifted:
        for r in drifted:
            if r["status"] == LOCAL_MISSING:
                continue  # 本地没有内容可比
            name = r["file"]
            remote_text = fetch_remote_text(args.host, args.remote_dir, name)
            local_text = (local_dir / name).read_text()
            diff = list(difflib.unified_diff(
                remote_text.splitlines(), local_text.splitlines(),
                fromfile=f"remote:{name}", tofile=f"local:{name}", lineterm=""))
            print(f"\n===== diff {name} (remote → local) =====")
            print("\n".join(diff) if diff else f"（内容一致但哈希不同：换行符/编码差异？{name}）")

    if args.sync:
        tag = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        mode = "dry-run" if args.dry_run else "执行"
        print(f"\n--sync 目标（{mode}，远端备份后缀 .bak-{tag}）:")
        sync_failed = False
        for r in drifted:
            if r["status"] == LOCAL_MISSING:
                print(f"  skip（本地缺失，不动远端）: {r['file']}")
                continue
            try:
                backup_and_push(args.host, args.remote_dir, local_dir, r["file"], tag, args.dry_run)
            except (subprocess.CalledProcessError, RuntimeError) as exc:
                print(f"  error: 推送 {r['file']} 失败: {exc}", file=sys.stderr)
                sync_failed = True
                continue
            if not args.dry_run:
                print(f"  push {r['file']} ← 本地版本（远端原文件备份为 {r['file']}.bak-{tag}，若存在）")
        if args.dry_run:
            return exit_code
        # 同步后重拉远端快照复验，退出码反映同步结果而非同步前状态。
        try:
            remote_hashes2, _ = remote_snapshot(args.host, args.remote_dir, names)
        except (subprocess.CalledProcessError, RuntimeError) as exc:
            print(f"error: 同步后复验失败: {exc}", file=sys.stderr)
            return 2
        still = [
            r["file"]
            for r in results
            if r["status"] != LOCAL_MISSING and classify(local_sha256(local_dir / r["file"]), remote_hashes2.get(r["file"])) != OK
        ]
        if still:
            print(f"error: 同步后仍有漂移: {', '.join(still)}", file=sys.stderr)
            return 1
        if not sync_failed:
            print("sync 完成且复验一致；backend 2s 热加载自动生效，可用 journalctl 确认 hot-reload 行。")
            exit_code = 0

    return exit_code


if __name__ == "__main__":
    sys.exit(main())
