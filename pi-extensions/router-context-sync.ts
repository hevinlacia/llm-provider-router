/**
 * router-context-sync — B 全量托管（Pi 侧）
 *
 * 目标：Pi 完全不维护 models 列表/窗口，全部由 llm-provider-router 定。
 * Pi 的 ~/.pi/agent/models.json 只需保留 provider 声明（baseUrl/api/apiKey），
 * 模型列表与 contextWindow/maxTokens 由本扩展在启动时 fetch capabilities
 * 并通过 pi.registerProvider 全量注册；切换模型与动态路由的窗口差异通过
 * 定时 + 响应头双通道持续热更新，无需改 Pi 内核。
 *
 * 安装：cp 到 ~/.pi/agent/extensions/router-context-sync.ts  然后 /reload 或重启 pi
 * 环境：LLM_PROVIDER_ROUTER_CAPABILITIES_URL / MODELS_URL / SYNC_INTERVAL / BEARER_TOKEN
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const CAP_URL =
  process.env.LLM_PROVIDER_ROUTER_CAPABILITIES_URL ||
  "http://127.0.0.1:8789/api/router/capabilities";
const MODELS_URL =
  process.env.LLM_PROVIDER_ROUTER_MODELS_URL ||
  "http://127.0.0.1:8789/v1/models";
const POLL_MS = Number(process.env.LLM_PROVIDER_ROUTER_SYNC_INTERVAL_MS || 5 * 60 * 1000);
const PROVIDER_ID = "llm-provider-router";
// 罗盘兜底：即使 Pi 的 models.json 已空，扩展自带可用的 provider 配置
const FALLBACK_PROVIDER = {
  baseUrl: process.env.LLM_PROVIDER_ROUTER_BASE_URL || "http://127.0.0.1:8789/v1",
  apiKey: process.env.LLM_PROVIDER_ROUTER_API_KEY || "local-dev",
  api: "openai-completions" as const,
  authHeader: true as const,
};

type CapModel = {
  id: string;
  name?: string;
  display_name?: string | null;
  reasoning?: boolean | null;
  input?: string[] | null;
  thinking_level_map?: Record<string, string | null> | null;
  thinking_format?: string | null;
  effective?: { contextWindow?: number | null; maxTokens?: number | null };
  // 兼容旧 /v1/models enriched 的平铺字段
  context_window?: number | null;
  contextWindow?: number | null;
  max_output_tokens?: number | null;
  maxTokens?: number | null;
};

type CapResponse = {
  ok?: boolean;
  v2_enabled?: boolean;
  models?: CapModel[];
  data?: CapModel[]; // /v1/models 形态
};

let currentModels: Record<string, CapModel> = {};
let timer: ReturnType<typeof setInterval> | undefined;

function bearerHeaders(): Record<string, string> {
  const token =
    process.env.LLM_PROVIDER_ROUTER_BEARER_TOKEN ||
    process.env.LLM_PROVIDER_ROUTER_API_KEY ||
    "";
  if (!token) return {};
  return { Authorization: `Bearer ${token}` };
}

function toPiModel(m: CapModel) {
  const cw = m.effective?.contextWindow ?? m.effective?.contextWindow ?? m.context_window ?? m.contextWindow ?? undefined;
  const mt = m.effective?.maxTokens ?? m.max_output_tokens ?? m.maxTokens ?? undefined;
  const name = m.display_name || m.name || m.id;
  // Pi 的 Model 必备字段；cost 保留 0，真实计费走 Router usage
  const model: Record<string, unknown> = {
    id: m.id,
    name,
    reasoning: m.reasoning ?? false,
    input: m.input ?? ["text"],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: cw ?? 128000,
    maxTokens: mt ?? 16384,
  };
  if (m.thinking_level_map) model.thinkingLevelMap = m.thinking_level_map;
  if (m.thinking_format) {
    model.compat = { thinkingFormat: m.thinking_format as string };
  }
  return model;
}

function registerFromCaps(pi: ExtensionAPI, caps: CapModel[]) {
  // 去重、排序，滑动最小已在 Router capabilities effective 做过
  const byId = new Map<string, CapModel>();
  for (const m of caps) byId.set(m.id, m);
  const models = [...byId.values()].sort((a, b) => a.id.localeCompare(b.id)).map(toPiModel);

  // B 全量托管：用扩展的 models 完全替换该 provider 的模型列表
  // 需带上 baseUrl/api/apiKey 以在 Pi 的 models.json 为空时仍可用（幂等）
  try {
    pi.registerProvider(PROVIDER_ID, {
      baseUrl: FALLBACK_PROVIDER.baseUrl,
      api: FALLBACK_PROVIDER.api,
      apiKey: FALLBACK_PROVIDER.apiKey,
      authHeader: FALLBACK_PROVIDER.authHeader,
      models: models as never,
    } as never);
    // 记录当前快照，供响应头精细校正时局部 patch
    currentModels = Object.fromEntries(byId.entries());
  } catch {
    // provider 尚未就绪，下次 poll 重试
  }
}

async function fetchCaps(): Promise<CapModel[] | null> {
  // 优先结构化 capabilities，其次 /v1/models enriched
  try {
    const res = await fetch(CAP_URL, { headers: bearerHeaders() });
    if (res.ok) {
      const data = (await res.json()) as CapResponse;
      if (data.models?.length) return data.models;
    }
  } catch { /* fallback */ }
  try {
    const res = await fetch(MODELS_URL, { headers: bearerHeaders() });
    if (res.ok) {
      const data = (await res.json()) as { data?: CapModel[] };
      if (data.data?.length) return data.data;
    }
  } catch { /* ignore */ }
  return null;
}

async function sync(pi: ExtensionAPI) {
  const caps = await fetchCaps();
  if (!caps?.length) return;
  // 仅当 effective 窗口变化或有新模型时才重注册，避免无谓抖动
  let changed = Object.keys(currentModels).length !== caps.length;
  if (!changed) {
    for (const m of caps) {
      const prev = currentModels[m.id];
      const cw = m.effective?.contextWindow ?? m.context_window ?? m.contextWindow;
      const mt = m.effective?.maxTokens ?? m.max_output_tokens ?? m.maxTokens;
      const pcw = prev?.effective?.contextWindow ?? prev?.context_window ?? (prev as unknown as { contextWindow?: number })?.contextWindow;
      const pmt = prev?.effective?.maxTokens ?? prev?.max_output_tokens ?? (prev as unknown as { maxTokens?: number })?.maxTokens;
      if (pcw !== cw || pmt !== mt || prev?.reasoning !== m.reasoning) {
        changed = true;
        break;
      }
    }
  }
  if (changed) registerFromCaps(pi, caps);
}

export default async function (pi: ExtensionAPI) {
  // async factory 阻塞到首次拉取完成，首个 session 的模型列表即为准
  await sync(pi);

  timer = setInterval(() => void sync(pi), POLL_MS);
  if (timer && typeof (timer as unknown as { unref?: () => void }).unref === "function") {
    (timer as unknown as { unref: () => void }).unref!();
  }

  pi.on("model_select", () => void sync(pi));

  // 活链路精细校正：非流式响应的精确命中窗口在响应头即刻可得
  // B 托管下通过重注册 models 的单条目实现（modelOverrides 在扩展侧不会生效，故用全量重注册的 patch）
  pi.on("after_provider_response", async (event) => {
    const h = event.headers as Record<string, string | undefined>;
    const cwRaw = h["x-llm-router-context-window"] ?? h["X-LLM-Router-Context-Window"];
    const moRaw = h["x-llm-router-max-output"] ?? h["X-LLM-Router-Max-Output"];
    const aliasRaw = h["x-llm-router-model"] ?? h["X-LLM-Router-Model"];
    if (!cwRaw && !moRaw) return;
    const cw = cwRaw ? Number(cwRaw) : undefined;
    const mo = moRaw ? Number(moRaw) : undefined;
    if ((cw != null && !Number.isFinite(cw)) || (mo != null && !Number.isFinite(mo))) return;
    const alias = aliasRaw?.trim();
    if (!alias || !currentModels[alias]) return;
    const prev = currentModels[alias];
    const prevCw = prev.effective?.contextWindow ?? prev.context_window ?? (prev as unknown as { contextWindow?: number })?.contextWindow;
    const prevMo = prev.effective?.maxTokens ?? prev.max_output_tokens ?? (prev as unknown as { maxTokens?: number })?.maxTokens;
    // 已是滑动最小（capabilities effective 已是 min），此处仅在 Router 返回更小精确值时下修，避免抖动拉大
    const nextCw = cw != null && prevCw != null ? Math.min(prevCw, cw) : (cw ?? prevCw);
    const nextMo = mo != null && prevMo != null ? Math.min(prevMo, mo) : (mo ?? prevMo);
    if (nextCw === prevCw && nextMo === prevMo) return;
    // patch 单条并重注册全量
    currentModels[alias] = {
      ...prev,
      effective: { contextWindow: nextCw as number, maxTokens: nextMo as number },
    };
    registerFromCaps(pi, Object.values(currentModels));
  });

  pi.on("session_shutdown", async () => {
    if (timer) clearInterval(timer);
    timer = undefined;
  });
}
