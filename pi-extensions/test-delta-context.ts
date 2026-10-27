/**
 * delta-context 扩展状态机测试：mock ExtensionAPI，真实 import 扩展源码。
 * 覆盖：协商握手、增量确认、429 死亡自愈（pendingDelta）、miss、非 router 2xx 回退。
 * 运行：node --experimental-strip-types /tmp/test-delta-context.ts
 */
type Handler = (event: any) => any;
const handlers = new Map<string, Handler[]>();
const pi = {
  on: (event: string, handler: Handler) => {
    const list = handlers.get(event) ?? [];
    list.push(handler);
    handlers.set(event, list);
  },
};
const emit = (event: string, payload: any) => {
  let out: any = payload;
  for (const h of handlers.get(event) ?? []) {
    const r = h({ type: event, ...payload });
    if (r !== undefined) out = r;
  }
  return out;
};

const mod = await import("/home/hevin/.pi/agent/extensions/delta-context.ts");
await mod.default(pi);

let passed = 0;
function check(name: string, cond: boolean) {
  if (!cond) {
    console.error(`FAIL: ${name}`);
    process.exitCode = 1;
  } else {
    passed++;
    console.log(`ok: ${name}`);
  }
}

// 断言辅助：handler 返回 undefined 时 emit 返回事件包装对象 → 用 x_delta 有无判断
const isDelta = (out: any) => !!(out && out.x_delta);
const deltaOf = (out: any) => (out && out.x_delta ? (out.x_delta as any) : undefined);

// 单调增长的会话：每轮在上一轮基础上追加 1 条
const history: unknown[] = [{ role: "user", content: "hi" }];
const nextPayload = (instructions = "sys") => ({
  model: "glm-test",
  instructions,
  input: history.map((m) => ({ ...(m as any) })),
  store: false,
});
const grow = (role: string, content: string) => history.push({ role, content });

// 1) 首笔全量：不改写
let out = emit("before_provider_request", { payload: nextPayload() });
check("full request passes through (no x_delta)", !isDelta(out));
emit("after_provider_response", {
  status: 200,
  headers: { "x-delta-cache": "ready", "x-delta-token": "tok1", "x-delta-base-hash": "h1" },
});

// 2) 第二笔（+2 条）：delta 包
grow("assistant", "a"); grow("user", "b");
out = emit("before_provider_request", { payload: nextPayload() });
check("second request becomes delta (tok1)", deltaOf(out)?.token === "tok1");
check("delta strips input/instructions", out && !("input" in out) && !("instructions" in out));
emit("after_provider_response", { status: 200, headers: { "x-delta-cache": "advance", "x-delta-base-hash": "h2" } });

// 3) 第三笔（+1 条）：新 base_hash
grow("assistant", "c");
out = emit("before_provider_request", { payload: nextPayload() });
check("third request delta with new base_hash", deltaOf(out)?.base_hash === "h2");

// 4) ★ 核心：这笔 delta 死了（429 耗尽，onResponse 不触发，无任何响应事件）
//    下一笔请求（pi auto-retry 第 2 轮 / 用户重发）必须回全量
grow("user", "d");
out = emit("before_provider_request", { payload: nextPayload() });
check("★ 429-dead delta self-heals to full", !isDelta(out));
emit("after_provider_response", {
  status: 200,
  headers: { "x-delta-cache": "ready", "x-delta-token": "tok2", "x-delta-base-hash": "h3" },
});

// 5) 恢复 delta，用新 token
grow("assistant", "e");
out = emit("before_provider_request", { payload: nextPayload() });
check("delta resumes with new token", deltaOf(out)?.token === "tok2");
emit("after_provider_response", { status: 200, headers: { "x-delta-cache": "advance", "x-delta-base-hash": "h4" } });

// 6) miss 头：resetState（覆盖 HTTP 层内收到 miss 的场景）
emit("after_provider_response", { status: 429, headers: { "x-delta-cache": "miss" } });
grow("user", "f");
out = emit("before_provider_request", { payload: nextPayload() });
check("miss resets to full", !isDelta(out));
emit("after_provider_response", {
  status: 200,
  headers: { "x-delta-cache": "ready", "x-delta-token": "tok3", "x-delta-base-hash": "h5" },
});

// 7) ★ 非 router 2xx（无 x-delta-cache 头）：先确认 delta 已武装，再被停用
grow("assistant", "x");
out = emit("before_provider_request", { payload: nextPayload() });
check("delta armed before provider switch", deltaOf(out)?.token === "tok3");
emit("after_provider_response", { status: 200, headers: { "content-type": "text/event-stream" } }); // 别家：无 delta 头
grow("user", "y");
out = emit("before_provider_request", { payload: nextPayload() });
check("★ non-router 2xx disables delta", !isDelta(out));

// 8) instructions 变化（前缀不满足）：自然回全量
emit("after_provider_response", {
  status: 200,
  headers: { "x-delta-cache": "ready", "x-delta-token": "tok4", "x-delta-base-hash": "h6" },
});
grow("user", "new topic");
out = emit("before_provider_request", { payload: nextPayload("sys-v2") });
check("instructions change falls back to full", !isDelta(out));

// 9) session_start 重置
emit("after_provider_response", {
  status: 200,
  headers: { "x-delta-cache": "ready", "x-delta-token": "tok5", "x-delta-base-hash": "h7" },
});
grow("assistant", "z");
emit("session_start", { reason: "new" });
out = emit("before_provider_request", { payload: nextPayload() });
check("session_start resets to full", !isDelta(out));

console.log(`\n${passed} checks passed${process.exitCode ? " (with failures)" : ""}`);
