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

  return <section className="card"><div className="section-title"><h2>Thinking Level Maps</h2><span className="muted">{current.config_path}</span></div>
    <p className="muted">按供应商过滤的物理模型思考强度映射：标准档位 <code>off / minimal / low / medium / high / xhigh</code> → 上游 wire 值（如 DeepSeek <code>xhigh → max</code>）。留空=该档位透传/回退逻辑池，<code>null</code>=该档位上游不支持（删除字段）。每行可一键把映射同步给等价关系表中同组的其它供应商模型。</p>
    {fallbackInfo}
    <div className="toolbar" style={{ justifyContent: 'space-between', alignItems: 'flex-end', flexWrap: 'wrap', gap: 12 }}>
      <div className="field"><label>Supplier</label><select value={selectedProvider} onChange={(e) => setSelectedProvider(e.target.value)}><option value="__all__">All suppliers ({config.maps.length})</option>{providers.map((p) => <option key={p} value={p}>{p} ({counts[p] ?? 0})</option>)}</select></div>
      <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
        <label className="muted small-text" style={{ display: 'flex', gap: 6, alignItems: 'center' }}><input type="checkbox" checked={onlyMissing} onChange={(e) => setOnlyMissing(e.target.checked)} /> Only missing</label>
        <button onClick={() => void save()}>Save Thinking Maps</button>
      </div>
    </div>
    <div className="table-wrap"><table><thead><tr><th>Model</th><th>Group</th>{STANDARD_LEVELS.map((lv) => <th key={lv}>{lv}</th>)}<th>Format</th><th></th></tr></thead><tbody>{filtered.map((item) => {
      const group = modelToGroup.get(item.model);
      return <tr key={item.model}><td className="strong-cell">{item.model}</td><td><span className="status">{group ?? '—'}</span></td>
        {STANDARD_LEVELS.map((lv) => {
          const v = thinkingWireFor(item, lv);
          return <td key={lv}><input style={{ width: 88 }} value={v === '__null__' ? '' : v} placeholder={v === '__null__' ? 'null' : '—'} title={v === '__null__' ? 'null (不支持)' : undefined} onChange={(e) => updateMap(item.model, 'thinking_level_map', { level: lv, wire: e.target.value === '' && v === '__null__' ? '__null__' : e.target.value })} />
            <button className="secondary" style={{ marginLeft: 4, padding: '2px 6px', fontSize: 11 }} title="Set null (unsupported)" onClick={() => updateMap(item.model, 'thinking_level_map', { level: lv, wire: '__null__' })}>∅</button></td>;
        })}
        <td><select value={item.thinking_format ?? ''} onChange={(e) => updateMap(item.model, 'thinking_format', e.target.value || null)}><option value="">—</option><option value="reasoning_effort">reasoning_effort</option></select></td>
        <td><button className="secondary compact-button" disabled={!group || pendingModel === item.model} onClick={() => void applyEquivalent(item.model)} title={group ? `Apply this map to all models in group \`${group}\` (${onlyMissing ? 'only missing' : 'overwrite'})` : 'Not in any equivalence group'}>{pendingModel === item.model ? 'Applying…' : 'Apply to equivalents'}</button></td></tr>;
    })}{!filtered.length && <tr><td colSpan={STANDARD_LEVELS.length + 4} className="muted">{selectedProvider === '__all__' ? 'No physical models (check models.json).' : `No models for supplier \`${selectedProvider}\`.`}</td></tr>}</tbody></table></div>
    <p className="muted small-text" style={{ marginTop: 8 }}>Tip: 点 ∅ 将该档位设为 <code>null</code>（显式不支持，请求时删除字段）；清空输入框则删除该档位映射（回退逻辑池/透传）。Format 留空=回退逻辑池。</p>
  </section>;
}
