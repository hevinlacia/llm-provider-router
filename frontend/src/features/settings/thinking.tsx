import { useEffect, useMemo, useState } from 'react';
import { api } from '../../api';
import type { ModelEquivalencesConfig, ThinkingMapsConfig, V2Status } from '../../types';

const STANDARD_LEVELS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh'] as const;

export function ThinkingLevelMapPanel({ config, v2, equivalences, onChange, onSaved, onError }: {
  config: ThinkingMapsConfig | null;
  v2: V2Status | null;
  equivalences: ModelEquivalencesConfig | null;
  onChange: (value: ThinkingMapsConfig) => void;
  onSaved: (value: ThinkingMapsConfig) => void;
  onError: (value: string) => void;
}) {
  const providers = useMemo(() => Object.keys(v2?.providers ?? {}).sort(), [v2]);
  const [selectedProvider, setSelectedProvider] = useState<string>('__all__');
  const [pendingModel, setPendingModel] = useState<string | null>(null);
  const [onlyMissing, setOnlyMissing] = useState(false);

  useEffect(() => {
    if (providers.length && selectedProvider !== '__all__' && !providers.includes(selectedProvider)) {
      setSelectedProvider('__all__');
    }
  }, [providers, selectedProvider]);

  const modelToGroup = useMemo(() => {
    const m = new Map<string, string>();
    for (const g of equivalences?.groups ?? []) for (const model of g.models) m.set(model, g.id);
    return m;
  }, [equivalences]);

  const counts = useMemo(() => {
    const map: Record<string, number> = {};
    for (const p of providers) map[p] = (config?.maps ?? []).filter((m) => m.model.startsWith(`${p}/`)).length;
    return map;
  }, [config, providers]);

  const filtered = useMemo(() => {
    if (!config) return [];
    const items = config.maps;
    if (selectedProvider === '__all__') return [...items].sort((a, b) => a.model.localeCompare(b.model));
    return [...items].filter((item) => item.model.startsWith(`${selectedProvider}/`)).sort((a, b) => a.model.localeCompare(b.model));
  }, [config, selectedProvider]);

  const indexByModel = useMemo(() => {
    const map = new Map<string, number>();
    (config?.maps ?? []).forEach((item, idx) => map.set(item.model, idx));
    return map;
  }, [config]);

  if (!config) return <section className="card"><h2>Thinking Level Maps</h2><p className="muted">Loading thinking maps...</p></section>;
  const current = config;

  function updateMap(model: string, field: 'thinking_level_map' | 'thinking_format', value: unknown) {
    const idx = indexByModel.get(model);
    if (idx === undefined) return;
    const maps = [...current.maps];
    const prev = maps[idx];
    if (field === 'thinking_format') {
      const fmt = (value as string | null)?.trim() || null;
      maps[idx] = { ...prev, thinking_format: fmt || null };
    } else {
      // value is partial Record<string, string|null> for one level
      const { level, wire } = value as { level: string; wire: string };
      const nextMap = { ...(prev.thinking_level_map ?? {}) } as Record<string, string | null>;
      const trimmed = wire.trim();
      if (trimmed === '') {
        // 空字符串 = 删除该档位（回退透传/逻辑池回退）
        delete nextMap[level];
      } else if (trimmed === '__null__') {
        nextMap[level] = null;
      } else {
        nextMap[level] = trimmed;
      }
      const hasAny = Object.keys(nextMap).length > 0;
      maps[idx] = { ...prev, thinking_level_map: hasAny ? nextMap : null };
    }
    onChange({ ...current, maps });
  }

  function thinkingWireFor(item: ThinkingMapsConfig['maps'][number], level: string): string {
    const v = item.thinking_level_map?.[level];
    if (v === null) return '__null__';
    if (v === undefined) return '';
    return String(v);
  }

  async function save() {
    try {
      const payload = current.maps.map((m) => ({
        model: m.model,
        thinking_level_map: m.thinking_level_map,
        thinking_format: m.thinking_format,
      }));
      onSaved(await api.saveThinkingMaps(payload));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  async function applyEquivalent(model: string) {
    setPendingModel(model);
    try {
      const res = await api.applyThinkingToEquivalents(model, onlyMissing);
      onSaved(res.thinking_maps);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setPendingModel(null);
    }
  }

  const fallbackInfo = current.logical_fallback && Object.keys(current.logical_fallback).length > 0
    ? <p className="muted small-text">逻辑池回退：{Object.entries(current.logical_fallback).map(([k, v]) => `${k}: ${JSON.stringify(v.thinking_level_map)} / ${v.thinking_format ?? 'null'}`).join(' · ')}</p>
    : null;

  return <section className="card settings-section thinking-section"><div className="section-title settings-title thinking-title"><div><h2>Thinking Level Maps</h2><p className="muted">按供应商过滤物理模型思考强度映射；每张卡对应一个模型。</p></div><span className="muted small-text config-path">{current.config_path}</span></div>
    <div className="thinking-help-grid">
      <p className="muted">标准档位 <code>off / minimal / low / medium / high / xhigh</code> → 上游 wire 值（如 DeepSeek <code>xhigh → max</code>）。</p>
      <p className="muted">留空=透传/回退逻辑池；<code>null</code>=上游不支持并删除字段；可一键同步给同等价组模型。</p>
    </div>
    {fallbackInfo}
    <div className="toolbar thinking-toolbar">
      <div className="field thinking-filter"><label>Supplier</label><select value={selectedProvider} onChange={(e) => setSelectedProvider(e.target.value)}><option value="__all__">All suppliers ({config.maps.length})</option>{providers.map((p) => <option key={p} value={p}>{p} ({counts[p] ?? 0})</option>)}</select></div>
      <div className="thinking-actions">
        <span className="muted small-text">Showing {filtered.length} models</span>
        <label className="muted small-text thinking-checkbox"><input type="checkbox" checked={onlyMissing} onChange={(e) => setOnlyMissing(e.target.checked)} /> Only missing</label>
        <button onClick={() => void save()}>Save Thinking Maps</button>
      </div>
    </div>
    <div className="thinking-map-list">{filtered.map((item) => {
      const group = modelToGroup.get(item.model);
      return <article className="thinking-model-card" key={item.model}>
        <div className="thinking-model-head">
          <div className="thinking-model-meta"><strong>{item.model}</strong><span className="status">{group ?? 'No group'}</span></div>
          <div className="thinking-model-controls">
            <label className="thinking-format"><span>Format</span><select value={item.thinking_format ?? ''} onChange={(e) => updateMap(item.model, 'thinking_format', e.target.value || null)}><option value="">— fallback</option><option value="reasoning_effort">reasoning_effort</option></select></label>
            <button className="secondary compact-button" disabled={!group || pendingModel === item.model} onClick={() => void applyEquivalent(item.model)} title={group ? `Apply this map to all models in group \`${group}\` (${onlyMissing ? 'only missing' : 'overwrite'})` : 'Not in any equivalence group'}>{pendingModel === item.model ? 'Applying…' : 'Apply to equivalents'}</button>
          </div>
        </div>
        <div className="thinking-level-grid">{STANDARD_LEVELS.map((lv) => {
          const v = thinkingWireFor(item, lv);
          const isNull = v === '__null__';
          return <label className={`thinking-level-field ${isNull ? 'is-null' : ''}`} key={lv}><span>{lv}</span><div className="thinking-level-input"><input value={isNull ? '' : v} placeholder={isNull ? 'null' : '— fallback'} title={isNull ? 'null (不支持)' : undefined} onChange={(e) => updateMap(item.model, 'thinking_level_map', { level: lv, wire: e.target.value === '' && isNull ? '__null__' : e.target.value })} /><button className="secondary null-button" title="Set null (unsupported)" type="button" onClick={() => updateMap(item.model, 'thinking_level_map', { level: lv, wire: '__null__' })}>∅</button></div></label>;
        })}</div>
      </article>;
    })}{!filtered.length && <div className="thinking-empty muted">{selectedProvider === '__all__' ? 'No physical models (check models.json).' : `No models for supplier \`${selectedProvider}\`.`}</div>}</div>
    <p className="muted small-text thinking-tip">Tip: 点 ∅ 将该档位设为 <code>null</code>（显式不支持，请求时删除字段）；清空输入框则删除该档位映射（回退逻辑池/透传）。Format 留空=回退逻辑池。</p>
  </section>;
}
