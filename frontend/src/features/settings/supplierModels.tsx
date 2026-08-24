import { useEffect, useMemo, useState } from 'react';
import { api } from '../../api';
import { formatWindow } from '../../lib/format';
import type { PhysicalModelPatch, ProbeResult, TokenPrice, TokenPriceConfig, V2PhysicalModel, V2Status } from '../../types';

const STANDARD_LEVELS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh'] as const;

/** 推荐方案：一个物理模型的完整配置组合（能力参数 + token 价格），动态收集（保存时自动入库，按内容去重）。 */
type Preset = {
  id: string;
  source: string;
  updated_at: number;
  context_window?: number | null;
  max_output_tokens?: number | null;
  supports_image?: boolean | null;
  thinking_level_map?: Record<string, string | null> | null;
  thinking_format?: string | null;
  input_uncached_per_million?: number | null;
  input_cached_per_million?: number | null;
  output_per_million?: number | null;
};

const PRESETS_KEY = 'lpr-supplier-model-presets';

function loadPresets(): Preset[] {
  try {
    const raw = localStorage.getItem(PRESETS_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as Preset[];
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function savePresets(presets: Preset[]) {
  try {
    localStorage.setItem(PRESETS_KEY, JSON.stringify(presets.slice(0, 50)));
  } catch {
    /* storage full / unavailable: ignore */
  }
}

function presetId(p: Preset): string {
  return JSON.stringify([
    p.context_window ?? null,
    p.max_output_tokens ?? null,
    p.supports_image ?? null,
    p.thinking_level_map ?? null,
    p.thinking_format ?? null,
    p.input_uncached_per_million ?? null,
    p.input_cached_per_million ?? null,
    p.output_per_million ?? null,
  ]);
}

function collectPreset(source: string, draft: Draft): Preset {
  return {
    id: '',
    source,
    updated_at: Date.now(),
    context_window: draft.context_window,
    max_output_tokens: draft.max_output_tokens,
    supports_image: draft.supports_image,
    thinking_level_map: draft.thinking_level_map,
    thinking_format: draft.thinking_format,
    input_uncached_per_million: draft.input_uncached_per_million,
    input_cached_per_million: draft.input_cached_per_million,
    output_per_million: draft.output_per_million,
  };
}

type Draft = {
  context_window?: number | null;
  max_output_tokens?: number | null;
  supports_image?: boolean | null;
  thinking_level_map: Record<string, string | null> | null;
  thinking_format: string | null;
  input_uncached_per_million?: number | null;
  input_cached_per_million?: number | null;
  output_per_million?: number | null;
};

function priceFor(model: string, prices: TokenPrice[] | undefined): TokenPrice | undefined {
  return prices?.find((p) => p.model === model);
}

function modelToDraft(model: V2PhysicalModel | undefined, prices: TokenPrice[] | undefined): Draft {
  const price = priceFor(model?.id ?? '', prices);
  return {
    context_window: model?.context_window ?? null,
    max_output_tokens: model?.max_output_tokens ?? null,
    supports_image: model?.supports_image ?? null,
    thinking_level_map: (model?.thinking_level_map as Record<string, string | null> | null | undefined) ?? null,
    thinking_format: (model?.thinking_format as string | null | undefined) ?? null,
    input_uncached_per_million: price?.input_uncached_per_million ?? null,
    input_cached_per_million: price?.input_cached_per_million ?? null,
    output_per_million: price?.output_per_million ?? null,
  };
}

function draftToPatch(model: string, draft: Draft): PhysicalModelPatch {
  return {
    model,
    context_window: draft.context_window ?? null,
    max_output_tokens: draft.max_output_tokens ?? null,
    supports_image: draft.supports_image ?? null,
    thinking_level_map: draft.thinking_level_map,
    thinking_format: draft.thinking_format,
  };
}

function thinkingSummary(map: Record<string, string | null> | null): string {
  if (!map) return '—';
  const entries = Object.entries(map);
  if (!entries.length) return '—';
  return entries.map(([lv, wire]) => `${lv}→${wire ?? '∅'}`).join(' ');
}

function priceSummary(price: TokenPrice | undefined): string {
  if (!price) return '—';
  const parts: string[] = [];
  if (price.input_uncached_per_million > 0) parts.push(`in ${price.input_uncached_per_million}`);
  if (price.input_cached_per_million > 0) parts.push(`cache ${price.input_cached_per_million}`);
  if (price.output_per_million > 0) parts.push(`out ${price.output_per_million}`);
  return parts.length ? parts.join(' · ') : '0';
}

export function SupplierModelConfigPanel({ v2, tokenPrices, onSaved, onError }: {
  v2: V2Status | null;
  tokenPrices: TokenPriceConfig | null;
  onSaved: (value: V2Status) => void;
  onError: (value: string) => void;
}) {
  const providers = useMemo(() => Object.keys(v2?.providers ?? {}).sort(), [v2]);
  const [selectedProvider, setSelectedProvider] = useState<string>('__all__');
  const [editing, setEditing] = useState<V2PhysicalModel | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [probingKey, setProbingKey] = useState<string | null>(null);
  const [probeResult, setProbeResult] = useState<ProbeResult | null>(null);

  useEffect(() => {
    if (providers.length && selectedProvider !== '__all__' && !providers.includes(selectedProvider)) {
      setSelectedProvider('__all__');
    }
  }, [providers, selectedProvider]);

  if (!v2) return <section className="card settings-section"><h2>Supplier Model Config</h2><p className="muted">Loading supplier model configuration...</p></section>;
  if (!v2.v2_enabled) return <section className="card settings-section"><h2>Supplier Model Config</h2><p className="muted">Layered routing disabled (set LLM_PROVIDER_ROUTER_V2=1 to enable).</p></section>;

  const prices = tokenPrices?.models ?? [];
  const detailModels = selectedProvider === '__all__'
    ? new Set<string>()
    : new Set(v2.providers?.[selectedProvider]?.models ?? []);
  const physical = (v2.models ?? []).filter((m) => {
    if (selectedProvider === '__all__') return true;
    return m.provider === selectedProvider;
  });

  // 模型列表 = 该供应商 detail 列表 + 已注册物理模型（按 upstream 名去重）
  const seen = new Set<string>();
  const rows: Array<{ key: string; upstream: string; model?: V2PhysicalModel; fromDetail: boolean }> = [];
  for (const p of physical) {
    if (seen.has(p.upstream_model)) continue;
    seen.add(p.upstream_model);
    rows.push({ key: p.id, upstream: p.upstream_model, model: p, fromDetail: detailModels.has(p.upstream_model) });
  }
  for (const upstream of detailModels) {
    if (seen.has(upstream)) continue;
    seen.add(upstream);
    rows.push({ key: upstream, upstream, model: undefined, fromDetail: true });
  }
  rows.sort((a, b) => a.upstream.localeCompare(b.upstream));

  async function refresh() {
    if (selectedProvider === '__all__') return;
    setRefreshing(true);
    try {
      await api.providerModels(selectedProvider, true);
      onSaved(await api.v2Status());
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setRefreshing(false);
    }
  }

  async function patchModel(model: V2PhysicalModel, draft: Draft) {
    try {
      // 能力参数 → models.json；token 价格 → token-prices.json（同一模型一次保存）
      const saved = await api.savePhysicalModels([draftToPatch(model.id, draft)]);
      await api.saveTokenPrices([{
        model: model.id,
        input_uncached_per_million: draft.input_uncached_per_million ?? 0,
        input_cached_per_million: draft.input_cached_per_million ?? 0,
        output_per_million: draft.output_per_million ?? 0,
      }]);
      onSaved(saved);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  async function probe(row: { key: string; upstream: string; provider: string; model?: V2PhysicalModel }) {
    // 探测会向上游发 3 次真实请求（上下文 128K 输入可能产生少量费用），需确认
    if (!window.confirm(`Probe \`${row.upstream}\` 的能力边界？\n\n将向 ${row.provider} 发送 3 次探测请求：\n- 上下文：128K tokens 输入（若窗口更大会产生输入费用）\n- 最大输出：max_tokens=200K\n- 图片：1×1 图片\n\n每次探测固定 1 个请求，不做重试。`)) return;
    setProbingKey(row.key);
    try {
      const res = await api.probePhysicalModel(row.provider, row.upstream);
      setProbeResult(res);
      setEditing(row.model ?? { id: `${row.provider}/${row.upstream}`, provider: row.provider, upstream_model: row.upstream, params: {}, context_window: null, max_output_tokens: null, supports_image: null });
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setProbingKey(null);
    }
  }

  return <>
    <section className="card settings-section thinking-section">
      <div className="section-title settings-title thinking-title"><div><h2>Supplier Model Config</h2><p className="muted">按供应商配置物理模型能力参数（上下文 / 输出 / 图片 / 思考档位映射 / Token 价格）。逻辑模型自动取池内最低参数，不再单独配置。</p></div><span className="muted small-text config-path">config/models.json</span></div>
      <div className="toolbar thinking-toolbar">
        <div className="field thinking-filter"><label>Supplier</label>
          <select value={selectedProvider} onChange={(e) => setSelectedProvider(e.target.value)}>
            <option value="__all__">All suppliers ({physical.length} physical)</option>
            {providers.map((p) => <option key={p} value={p}>{p} ({physical.filter((m) => m.provider === p).length} / {v2.providers?.[p]?.models?.length ?? 0} detail)</option>)}
          </select>
        </div>
        <div className="thinking-actions">
          <span className="muted small-text">Showing {rows.length} models</span>
          <button className="secondary compact-button" disabled={selectedProvider === '__all__' || refreshing} onClick={() => void refresh()}>{refreshing ? 'Refreshing…' : 'Refresh detail models'}</button>
        </div>
      </div>
      <div className="table-wrap"><table className="settings-table supplier-models-table">
        <thead><tr><th>Model</th><th>Context</th><th>Output</th><th>Image</th><th>Thinking map</th><th>Token Price</th><th></th></tr></thead>
        <tbody>{rows.map((row) => {
          const m = row.model;
          const map = (m?.thinking_level_map as Record<string, string | null> | null | undefined) ?? null;
          const price = priceFor(row.key, prices);
          return <tr key={row.key} className={!m ? 'supplier-unregistered-row' : ''}>
            <td className="strong-cell">{row.upstream}{!m && <span className="status warn" style={{ marginLeft: 8 }}>unregistered</span>}</td>
            <td className="muted small-text">{m?.context_window ? formatWindow(m.context_window) : '—'}</td>
            <td className="muted small-text">{m?.max_output_tokens ? formatWindow(m.max_output_tokens) : '—'}</td>
            <td>{m?.supports_image ? <span className="status ok">image</span> : m ? <span className="muted small-text">text</span> : <span className="muted small-text">—</span>}</td>
            <td className="muted small-text thinking-summary-cell">{thinkingSummary(map)}</td>
            <td className="muted small-text thinking-summary-cell">{priceSummary(price)}</td>
            <td><div className="row-actions"><button className="secondary compact-button" disabled={probingKey === row.key} onClick={() => void probe({ key: row.key, upstream: row.upstream, provider: m?.provider ?? (selectedProvider === '__all__' ? '' : selectedProvider), model: m })} title="探测上下文/输出/图片能力边界（向上游发固定 3 个请求）">{probingKey === row.key ? 'Probing…' : 'Probe'}</button><button className="secondary compact-button" onClick={() => setEditing(m ?? { id: `${selectedProvider}/${row.upstream}`, provider: selectedProvider, upstream_model: row.upstream, params: {}, context_window: null, max_output_tokens: null, supports_image: null })}>{m ? 'Configure' : 'Register & Configure'}</button></div></td>
          </tr>;
        })}{!rows.length && <tr><td colSpan={7} className="muted">{selectedProvider === '__all__' ? 'No physical models configured yet.' : `No models for supplier \`${selectedProvider}\` yet. Click Refresh to pull from upstream, or register a model via Model Pools.`}</td></tr>}</tbody>
      </table></div>
      <p className="muted small-text thinking-tip">Tip: 上下文/输出在 models.json 上声明后，Context Negotiation 会把逻辑模型（含模型池）的对外能力按「池内最低参数」聚合。图片支持参与输入模态协商。Token 价格随配置弹窗一并保存。</p>
    </section>
    {editing && <ModelConfigModal model={editing} prices={prices} probeResult={probeResult} onCancel={() => setEditing(null)} onSaved={async (draft) => { await patchModel(editing, draft); setEditing(null); }} onError={onError} />}
  </>;
}

function ModelConfigModal({ model, prices, probeResult, onCancel, onSaved, onError }: {
  model: V2PhysicalModel;
  prices: TokenPrice[];
  probeResult: ProbeResult | null;
  onCancel: () => void;
  onSaved: (draft: Draft) => void | Promise<void>;
  onError: (value: string) => void;
}) {
  const [draft, setDraft] = useState<Draft>(() => modelToDraft(model, prices));
  const [presets, setPresets] = useState<Preset[]>(() => loadPresets());
  const [saving, setSaving] = useState(false);

  // 探测结果填充：仅当探测到精确值时覆盖 draft（“≥ 阈值”不写，避免误导）
  useEffect(() => {
    if (!probeResult) return;
    setDraft((prev) => ({
      ...prev,
      context_window: probeResult.context_window ?? prev.context_window,
      max_output_tokens: probeResult.max_output_tokens ?? prev.max_output_tokens,
      supports_image: probeResult.supports_image ?? prev.supports_image,
    }));
  }, [probeResult]);
  function update(level: string, raw: string) {
    const nextMap = { ...(draft.thinking_level_map ?? {}) } as Record<string, string | null>;
    const trimmed = raw.trim();
    if (trimmed === '') delete nextMap[level];
    else if (trimmed === '__null__') nextMap[level] = null;
    else nextMap[level] = trimmed;
    setDraft({ ...draft, thinking_level_map: Object.keys(nextMap).length ? nextMap : null });
  }

  function setLevelNull(level: string) {
    setDraft({ ...draft, thinking_level_map: { ...(draft.thinking_level_map ?? {}), [level]: null } });
  }

  function applyPreset(preset: Preset) {
    setDraft({
      context_window: preset.context_window ?? null,
      max_output_tokens: preset.max_output_tokens ?? null,
      supports_image: preset.supports_image ?? null,
      thinking_level_map: preset.thinking_level_map ?? null,
      thinking_format: preset.thinking_format ?? null,
      input_uncached_per_million: preset.input_uncached_per_million ?? null,
      input_cached_per_million: preset.input_cached_per_million ?? null,
      output_per_million: preset.output_per_million ?? null,
    });
  }

  async function save() {
    setSaving(true);
    try {
      // 动态收集推荐方案：保存成功即把该组合入库（按内容去重）
      const nextPresets = [...presets];
      const cand = collectPreset(model.upstream_model, draft);
      const id = presetId(cand);
      const existing = nextPresets.find((p) => presetId(p) === id);
      if (existing) {
        existing.source = model.upstream_model;
        existing.updated_at = Date.now();
      } else {
        nextPresets.unshift(cand);
      }
      savePresets(nextPresets);
      setPresets(nextPresets);
      await onSaved(draft);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <div className="section-title"><h3>Configure: {model.upstream_model}</h3><span className="muted small-text">{model.id}</span></div>
    {presets.length > 0 && <div className="preset-block">
      <h4>推荐方案 <span className="muted small-text">（历史保存组合，点击一键应用后微调）</span></h4>
      <div className="preset-list">{presets.map((p) => (
        <button key={p.id || presetId(p)} className="preset-chip" title={`from ${p.source}`} onClick={() => applyPreset(p)}>
          <span className="preset-chip-title">{p.source}</span>
          <span className="preset-chip-detail">{p.context_window ? `${formatWindow(p.context_window)} ctx` : ''}{p.supports_image ? ' · img' : ''} · {thinkingSummary(p.thinking_level_map ?? null)}</span>
        </button>
      ))}</div>
    </div>}
    <h4>能力参数</h4>
    {probeResult && probeResult.notes.length > 0 && <div className="probe-notes"><div className="muted small-text">🔍 探测结果：</div>{probeResult.notes.map((n) => <div key={n} className="muted small-text">· {n}</div>)}</div>}
    <div className="modal-config-grid">
      <div className="field"><label>Context Window (tokens)</label><input type="number" min="0" step="1000" value={draft.context_window ?? ''} placeholder="— (infer)" onChange={(e) => setDraft({ ...draft, context_window: e.target.value === '' ? null : Math.max(0, Number(e.target.value)) || 0 })} /></div>
      <div className="field"><label>Max Output (tokens)</label><input type="number" min="0" step="1000" value={draft.max_output_tokens ?? ''} placeholder="— (infer)" onChange={(e) => setDraft({ ...draft, max_output_tokens: e.target.value === '' ? null : Math.max(0, Number(e.target.value)) || 0 })} /></div>
      <div className="field"><label>Supports Image</label>
        <select value={draft.supports_image == null ? '' : draft.supports_image ? 'true' : 'false'} onChange={(e) => setDraft({ ...draft, supports_image: e.target.value === '' ? null : e.target.value === 'true' })}>
          <option value="">— (unset)</option>
          <option value="true">yes</option>
          <option value="false">no</option>
        </select>
      </div>
      <div className="field"><label>Thinking Format</label>
        <select value={draft.thinking_format ?? ''} onChange={(e) => setDraft({ ...draft, thinking_format: e.target.value || null })}>
          <option value="">— (none)</option>
          <option value="reasoning_effort">reasoning_effort</option>
        </select>
      </div>
    </div>
    <h4>Token 价格 <span className="muted small-text">/ 1M tokens</span></h4>
    <div className="modal-config-grid">
      <div className="field"><label>Input Uncached / 1M</label><input type="number" min="0" step="0.000001" value={draft.input_uncached_per_million ?? ''} placeholder="0" onChange={(e) => setDraft({ ...draft, input_uncached_per_million: e.target.value === '' ? null : Math.max(0, Number(e.target.value)) || 0 })} /></div>
      <div className="field"><label>Input Cached / 1M</label><input type="number" min="0" step="0.000001" value={draft.input_cached_per_million ?? ''} placeholder="0" onChange={(e) => setDraft({ ...draft, input_cached_per_million: e.target.value === '' ? null : Math.max(0, Number(e.target.value)) || 0 })} /></div>
      <div className="field"><label>Output / 1M</label><input type="number" min="0" step="0.000001" value={draft.output_per_million ?? ''} placeholder="0" onChange={(e) => setDraft({ ...draft, output_per_million: e.target.value === '' ? null : Math.max(0, Number(e.target.value)) || 0 })} /></div>
    </div>
    <h4>思考档位映射 <span className="muted small-text">标准档位 → 上游 wire 值（留空=透传，∅=不支持）</span></h4>
    <div className="thinking-level-grid">{STANDARD_LEVELS.map((lv) => {
      const v = draft.thinking_level_map?.[lv];
      const isNull = v === null;
      return <label className={`thinking-level-field ${isNull ? 'is-null' : ''}`} key={lv}><span>{lv}</span><div className="thinking-level-input"><input value={isNull ? '' : (v ?? '')} placeholder={isNull ? 'null' : '—'} title={isNull ? 'null (不支持)' : undefined} onChange={(e) => update(lv, e.target.value === '' && isNull ? '__null__' : e.target.value)} /><button className="secondary null-button" type="button" title="Set null (unsupported)" onClick={() => setLevelNull(lv)}>∅</button></div></label>;
    })}</div>
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Cancel</button><button disabled={saving} onClick={() => void save()}>{saving ? 'Saving…' : 'Save'}</button></div>
  </div></div>;
}
