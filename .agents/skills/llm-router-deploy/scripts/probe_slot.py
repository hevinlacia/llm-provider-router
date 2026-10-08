#!/usr/bin/env python3
"""llm-provider-router 协议级探针：验证指定槽/入口的客户端协议端点真实可用。

用途：部署流程 llm-router-deploy 的阶段 2（直连槽端口）与阶段 3（经 front-proxy 复检）。
防止"health=ok 但客户端协议端点已被删/被改"导致的部署后 405（2026-10-08 事故）。

用法：
    python3 probe_slot.py --port 8791 [--base http://127.0.0.1] [--model glm-flash-latest-auto]
                          [--allow-chat-removal] [--smoke] [--timeout 30]

退出码：0 = 全部 PASS；1 = 存在 FAIL。
探针矩阵与判定规则见 skill 的 references/probes.md。
"""
from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request

JSON_HEADERS = {"Content-Type": "application/json"}


def request(base: str, method: str, path: str, body: dict | None, timeout: int) -> tuple[int, str, str]:
    """返回 (status, content_type, body_text)。连接失败返回 (0, '', exc)。"""
    url = f"{base.rstrip('/')}{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=JSON_HEADERS, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.headers.get("Content-Type", ""), resp.read().decode(errors="replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.headers.get("Content-Type", "") if exc.headers else "", (exc.read() or b"").decode(errors="replace")
    except Exception as exc:  # noqa: BLE001 — 探针需要把任何传输层失败记为结果
        return 0, "", f"{type(exc).__name__}: {exc}"


def is_json_response(body: str) -> bool:
    return body.lstrip()[:1] == "{"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0] if __doc__ else "")
    parser.add_argument("--port", type=int, required=True, help="目标端口（槽端口 8790/8791，或 front-proxy 8789）")
    parser.add_argument("--base", default="http://127.0.0.1")
    parser.add_argument("--model", default="glm-flash-latest-auto", help="探针/冒烟用的逻辑模型名")
    parser.add_argument("--allow-chat-removal", action="store_true",
                        help="阶段 0 审计已确认无客户端使用 chat 协议时，/v1/chat/completions 405 降级为 WARN")
    parser.add_argument("--smoke", action="store_true", help="追加真实模型冒烟（会产生少量上游调用）")
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()

    base = f"{args.base.rstrip('/')}:{args.port}"
    results: list[tuple[str, str, str]] = []  # (判定, 名称, 说明)

    def record(name: str, ok: bool, note: str, *, warn_as_pass: bool = False) -> None:
        verdict = "PASS" if ok else ("WARN" if warn_as_pass else "FAIL")
        results.append((verdict, name, note))

    # —— 协议端点探针（存在性 + 方法匹配）——
    # 用不存在的模型名：handler 会以 400/404 拒绝（不产生真实上游调用），
    # 但足以证明"路由命中 handler"。405 = 路由层破坏（请求未进 handler，空 body）。
    probe_model = "llm-router-probe-nonexistent"

    def endpoint_probe(name: str, path: str, body: dict) -> None:
        status, _, resp_body = request(base, "POST", path, body, args.timeout)
        if status == 405:
            record(name, False, "405：端点缺失/方法不匹配（协议破坏，客户端会全部失败）",
                   warn_as_pass=(path == "/v1/chat/completions" and args.allow_chat_removal))
        elif status == 0:
            record(name, False, f"传输层失败: {resp_body[:120]}")
        else:
            # 200/400/401/404/429/5xx = handler 执行过，端点存在；模型/参数/上游问题不掩盖路由结论
            record(name, True, f"{status}: {resp_body[:100]}")

    endpoint_probe("POST /v1/responses", "/v1/responses",
                   {"model": probe_model, "input": "hi", "max_output_tokens": 16})
    endpoint_probe("POST /v1/chat/completions", "/v1/chat/completions",
                   {"model": probe_model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 8})
    endpoint_probe("POST /v1/messages", "/v1/messages",
                   {"model": probe_model, "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]})

    # —— 管理 API 探针：必须返回 JSON；返回 HTML 说明路由落到了 dashboard fallback ——
    for path in ("/api/state", "/api/errors/recent", "/api/config/error-rules",
                 "/api/router/capabilities", "/api/config/token-prices"):
        status, ctype, body = request(base, "GET", path, None, args.timeout)
        ok = status == 200 and is_json_response(body)
        record(f"GET {path}", ok, f"{status} ct={ctype.split(';')[0] if ctype else '-'} "
                                 f"{'json' if is_json_response(body) else '非JSON(可能dashboard fallback)'}")

    # —— 真实模型冒烟（可选）：走通上游 key 链路 ——
    if args.smoke:
        status, _, body = request(base, "POST", "/v1/responses",
                                  {"model": args.model, "input": "回复OK", "max_output_tokens": 16}, args.timeout)
        try:
            state = json.loads(body).get("status")
        except Exception:  # noqa: BLE001
            state = "?"
        record(f"冒烟 POST /v1/responses ({args.model})",
               status == 200 and state in ("completed", "incomplete"),
               f"{status} status={state}")

    failed = [r for r in results if r[0] == "FAIL"]
    for verdict, name, note in results:
        print(f"[{verdict}] {name} — {note}")
    print(f"\n结果: {len(results) - len(failed)}/{len(results)} PASS, {len(failed)} FAIL "
          f"({base}, {time.strftime('%Y-%m-%d %H:%M:%S')})")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
