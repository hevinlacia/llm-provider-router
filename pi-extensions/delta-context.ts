/**
 * delta-context — pi ↔ llm-provider-router 上下文增量传输（协商式握手）
 *
 * 动机：外网使用时 pi 经公网（上行 ~4Mbps，与 agent panel 共享）连家里 router，
 * Responses 请求每笔全量重发整个上下文（长会话几百 KB~几 MB），上传是瓶颈。
 * 本扩展让 pi 只传新增 input items，router 缓存前缀并重组完整 payload 转发上游。
 *
 * 协议（与 router 的 features/responses/delta.rs 成对实现）：
 * 1. 首笔全量：请求头带 X-Delta-Capable → router 注册缓存后响应头下发
 *    X-Delta-Cache: ready + X-Delta-Token + X-Delta-Base-Hash；
 * 2. 后续增量：payload 替换为 delta 包（去 input/instructions，加 x_delta:
 *    {token, base_hash, append}）→ router 校验重组后响应 X-Delta-Cache: advance
 *    + 新 base_hash（缓存已前进，即使上游失败同轮重试仍走增量）；
 * 3. 任何不一致退化为全量，绝不静默发错上下文。
 *
 * ⚠️ 429 自愈（pendingDelta 悬空检测，2026-10-27 事故修复）：pi-ai 的 HTTP 重试
 * 闭包不含 onPayload，且 4xx 抛异常时 onResponse 不会执行——「收到 miss 头回全量」
 * 在真实链路上从未生效过（切槽 15 次 miss 0 次重注册的根因）。因此不能依赖 miss
 * 头：改为发出 delta 包时置 pendingDelta=true，收到 advance/miss 才清除；下一笔
 * 请求发现残留 → 判定上笔已死（429 耗尽/网络断/切槽缓存清零）→ resetState 回
 * 全量。pi 的 auto-retry 每轮重新执行 onPayload，所以第 2 轮重试即自愈。
 *
 * 非 router 防御：2xx 响应但无 x-delta-cache 头（切到其他供应商，或 router 端
 * 未注册）→ resetState 停回全量，避免 delta 包发往不认识的服务器丢上下文。
 *
 * 并发模型：subagent 是独立 pi 进程（模块级状态天然隔离）；同进程 /new /resume
 * /fork 为串行切换且必触发 session_start，此处重置状态即可。
 *
 * 安装：cp 到 ~/.pi/agent/extensions/delta-context.ts 然后 /reload 或重启 pi。
 * 开关：router 侧 GET/PUT /api/config/delta-cache（dashboard 可控）；禁用后
 * ready 头不再下发，本扩展自动停在全量模式，无需动 pi 侧配置。
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type DeltaMode = "full" | "delta";

type DeltaState = {
  mode: DeltaMode;
  token: string;
  baseHash: string;
  lastInstructions: unknown;
  /** 上次成功发送（或已注册）的完整 input 数组；delta 前缀判断基准。 */
  lastInput: unknown[] | null;
  /** 已发出但未被 advance/miss 确认的 delta 包：残留说明上笔已死（429 耗尽等），
   *  下一笔请求必须回全量（router 缓存可能已不存在，如切槽/重启/淘汰）。 */
  pendingDelta: boolean;
};

const state: DeltaState = {
  mode: "full",
  token: "",
  baseHash: "",
  lastInstructions: null,
  lastInput: null,
  pendingDelta: false,
};

function resetState() {
  state.mode = "full";
  state.token = "";
  state.baseHash = "";
  state.lastInstructions = null;
  state.lastInput = null;
  state.pendingDelta = false;
}

function headerValue(headers: Record<string, string | undefined>, name: string): string | undefined {
  return headers[name] ?? headers[name.toUpperCase()] ?? headers[name.replace(/(^|-)([a-z])/g, (_, p, c) => p + c.toUpperCase())];
}

/** 紧凑递归深比较（同构 JSON；pi 程序化构建 payload，键序稳定，仍按无序比较兜底）。 */
function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b || a === null || b === null) return false;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
    return a.every((item, i) => deepEqual(item, b[i]));
  }
  if (typeof a === "object" && typeof b === "object") {
    const oa = a as Record<string, unknown>;
    const ob = b as Record<string, unknown>;
    const ka = Object.keys(oa);
    const kb = Object.keys(ob);
    if (ka.length !== kb.length) return false;
    return ka.every((k) => k in ob && deepEqual(oa[k], ob[k]));
  }
  return false;
}

/** Responses 形态请求才参与 delta（model + input[]）。 */
function isResponsesPayload(payload: unknown): payload is Record<string, unknown> {
  return (
    typeof payload === "object" &&
    payload !== null &&
    Array.isArray((payload as Record<string, unknown>).input) &&
    typeof (payload as Record<string, unknown>).model === "string"
  );
}

/** delta 发送条件：mode=delta 且 instructions 未变且新 input 严格前缀延伸上次发送值。 */
function deltaEligible(payload: Record<string, unknown>): unknown[] | null {
  if (state.mode !== "delta" || !state.lastInput) return null;
  if (!deepEqual(payload.instructions, state.lastInstructions)) return null;
  const input = payload.input as unknown[];
  if (input.length < state.lastInput.length) return null;
  for (let i = 0; i < state.lastInput.length; i++) {
    if (!deepEqual(input[i], state.lastInput[i])) return null;
  }
  return input;
}

export default async function (pi: ExtensionAPI) {
  // 声明增量能力：所有 provider 请求都带头，router 只在 /v1/responses 全量分支消费
  pi.on("before_provider_headers", (event) => {
    const headers = event.headers as unknown as Record<string, string>;
    headers["X-Delta-Capable"] = "1";
  });

  pi.on("before_provider_request", (event) => {
    const payload = event.payload as unknown;
    if (!isResponsesPayload(payload)) return undefined;
    // 悬空检测：上一笔 delta 包从未被 advance/miss 确认（429 耗尽、网络断、
    // 切槽缓存清零等）→ token 已不可信，resetState 回全量。pi 的 auto-retry
    // 每轮重新触发本 hook，因此重试第 2 轮即自愈，无需用户干预。
    if (state.pendingDelta) resetState();
    const appendTail = deltaEligible(payload);
    if (appendTail) {
      const append = (payload.input as unknown[]).slice(state.lastInput!.length);
      const deltaPkg: Record<string, unknown> = { ...payload };
      delete deltaPkg.input;
      delete deltaPkg.instructions;
      deltaPkg.x_delta = {
        v: 1,
        token: state.token,
        base_hash: state.baseHash,
        append,
      };
      // 缓存前进先于上游转发（router 重组成功即落盘），同轮重试前缀判断基准直接前进。
      // pendingDelta 置位：若这笔请求得不到 advance/miss 确认，下一笔请求回全量。
      state.lastInput = payload.input as unknown[];
      state.pendingDelta = true;
      return deltaPkg;
    }
    // 全量：router 在 handler 入口注册缓存（与上游成败无关），立即落地基准
    state.lastInstructions = payload.instructions;
    state.lastInput = payload.input as unknown[];
    return undefined;
  });

  pi.on("after_provider_response", (event) => {
    const h = event.headers as Record<string, string | undefined>;
    const cache = headerValue(h, "x-delta-cache");
    if (cache === "ready") {
      const token = headerValue(h, "x-delta-token");
      const hash = headerValue(h, "x-delta-base-hash");
      if (token && hash && state.lastInput) {
        state.mode = "delta";
        state.token = token;
        state.baseHash = hash;
        state.pendingDelta = false;
      }
    } else if (cache === "advance") {
      const hash = headerValue(h, "x-delta-base-hash");
      if (hash) state.baseHash = hash;
      state.pendingDelta = false;
    } else if (cache === "miss") {
      // router 明确拒绝：token/hash 失效，回全量重新协商（此分支主要覆盖
      // HTTP 层重试内自愈成功的场景；429 耗尽时本 hook 不触发，由上面的
      // pendingDelta 悬空检测兑底）。
      resetState();
    } else if (state.mode === "delta") {
      // 2xx 但无任何 delta 头：对面不是 router（用户切换了供应商），或 router
      // 端未启用/未注册。立即停用 delta，避免后续请求被改写成 delta 包导致
      // 上下文丢失。
      resetState();
    }
  });

  // /new /resume /fork /reload 均触发：新会话上下文与旧 token 无关，重置回全量
  pi.on("session_start", () => {
    resetState();
  });
}
