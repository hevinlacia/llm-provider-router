import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../../api';
import { formatFetchedAt } from '../../lib/format';
import type { ProviderModelsResponse, V2Status, V2UnsupportedEntry } from '../../types';

export function ProviderModelsModal({ providerName, onCancel, onError }: {
  providerName: string;
  onCancel: () => void;
  onError: (value: string) => void;
}) {
  const [data, setData] = useState<ProviderModelsResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  const load = useCallback(async (refresh: boolean) => {
    if (refresh) setRefreshing(true); else setLoading(true);
    try {
      setData(await api.providerModels(providerName, refresh));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, [providerName, onError]);

  useEffect(() => { void load(false); }, [load]);

  const models = data?.models ?? [];
  const fetchedAt = formatFetchedAt(data?.fetched_at);
  const isError = Boolean(data && !data.ok);

  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <div className="section-title"><h3>Provider Models: {providerName}</h3><div className="title-actions"><span className="muted small-text">{data?.cached ? 'cached · ' : ''}{fetchedAt ? `fetched ${fetchedAt}` : ''}</span><button className="secondary compact-button" disabled={refreshing} onClick={() => void load(true)}>{refreshing ? 'Refreshing...' : 'Refresh'}</button></div></div>
    {loading && <p className="muted">Loading models...</p>}
    {isError && <p className="error">Failed to fetch: {data?.error}</p>}
    {!loading && !isError && models.length === 0 && <p className="muted">No models fetched yet. Click Refresh to pull the model list from the provider.</p>}
    {!loading && !isError && models.length > 0 && <>
      <p className="muted small-text">{models.length} models supported by this provider.</p>
      <div className="model-chip-list">{models.map((m) => <span className="model-chip" key={m}>{m}</span>)}</div>
    </>}
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Close</button></div>
  </div></div>;
}

export function ProviderVirtualModelsModal({ providerName, virtualModels, onCancel, onSaved, onError }: {
  providerName: string;
  virtualModels: Record<string, Record<string, string>>;
  onCancel: () => void;
  onSaved: (value: V2Status) => void;
  onError: (value: string) => void;
}) {
  const [name, setName] = useState('');
  const [upstream, setUpstream] = useState('');
  const [models, setModels] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);

  const existing = useMemo(() => Object.entries(virtualModels)
    .filter(([, m]) => m[providerName])
    .map(([name, m]) => ({ name, upstream: m[providerName] }))
    .sort((a, b) => a.name.localeCompare(b.name)), [virtualModels, providerName]);

  useEffect(() => {
    let cancelled = false;
    api.providerModels(providerName).then((res) => {
      if (!cancelled && res.models) setModels(res.models);
    }).catch(() => { /* datalist 为空也允许手动输入 */ });
    return () => { cancelled = true; };
  }, [providerName]);

  async function save() {
    if (!name.trim()) { onError('Virtual model name must not be empty'); return; }
    if (!upstream.trim()) { onError('Upstream model must not be empty'); return; }
    setSaving(true);
    try {
      const next = await api.upsertVirtualModel(name.trim(), providerName, upstream.trim());
      onSaved(next);
      setName(''); setUpstream('');
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  async function remove(item: { name: string }) {
    if (!confirm(`Delete virtual model "${item.name}" mapping for ${providerName}?`)) return;
    try {
      onSaved(await api.deleteVirtualModel(item.name, providerName));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  const datalistId = `vm-upstream-${providerName}`;
  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <h3>Virtual Models: {providerName}</h3>
    <p className="muted small-text">虚拟模型是供应商无关的抽象名，映射到该供应商的实际模型名。模型池 target 可直接填虚拟模型名，路由时自动展开到所有配置了该虚拟名的供应商。</p>
    <h4>Existing</h4>
    {existing.length === 0 && <p className="muted">No virtual models for this provider yet.</p>}
    {existing.length > 0 && <div className="table-wrap"><table><thead><tr><th>Virtual Model</th><th>Upstream Model</th><th></th></tr></thead><tbody>{existing.map((item) => <tr key={item.name}><td className="strong-cell">{item.name}</td><td className="muted small-text">{item.upstream}</td><td><button className="secondary compact-button" onClick={() => void remove(item)}>Delete</button></td></tr>)}</tbody></table></div>}
    <h4>Add New</h4>
    <div className="field"><label>Virtual Model Name</label><input value={name} onChange={(event) => setName(event.target.value)} placeholder="e.g. deepseek-v4-flash" /></div>
    <div className="field"><label>Upstream Model ({providerName})</label><input list={datalistId} value={upstream} onChange={(event) => setUpstream(event.target.value)} placeholder="Search or type actual model name" />
      <datalist id={datalistId}>{models.map((m) => <option key={m} value={m} />)}</datalist>
      <div className="muted small-text">{models.length > 0 ? `${models.length} models available (cached from provider). Type to filter or enter manually.` : 'No cached model list. Click provider Details > Refresh first, or type manually.'}</div>
    </div>
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Close</button><button disabled={saving} onClick={() => void save()}>{saving ? 'Saving...' : 'Add Virtual Model'}</button></div>
  </div></div>;
}

/** key × 模型"不支持"学习记录：阶梯退避状态 + 手动刷新（重置退避） */
export function UnsupportedKeysModal({ providerName, onCancel, onError }: {
  /** 传入时仅显示该 provider 的记录；不传显示全部 */
  providerName?: string;
  onCancel: () => void;
  onError: (value: string) => void;
}) {
  const [entries, setEntries] = useState<V2UnsupportedEntry[] | null>(null);
  const [refreshing, setRefreshing] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const data = await api.unsupportedKeys();
      const all = data.unsupported ?? [];
      setEntries(providerName ? all.filter((e) => e.provider === providerName) : all);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }, [providerName, onError]);

  useEffect(() => { void load(); }, [load]);

  async function refresh(filter: { provider?: string; key?: string; model?: string }, tag: string) {
    setRefreshing(tag);
    try {
      await api.refreshUnsupportedKeys(filter);
      await load();
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setRefreshing(null);
    }
  }

  function formatRetryIn(entry: V2UnsupportedEntry): string {
    if (entry.permanent) return '永久失效（不自动重试）';
    const s = entry.retry_in_seconds ?? 0;
    if (s >= 3600) return `退避中，${Math.floor(s / 3600)}h${Math.round((s % 3600) / 60)}m 后重试`;
    if (s >= 60) return `退避中，${Math.ceil(s / 60)}m 后重试`;
    return `退避中，${s}s 后重试`;
  }

  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <h3>Unsupported Models {providerName ? `— ${providerName}` : ''}</h3>
    <p className="muted">同一供应商不同 key 的订阅套餐支持的模型不同。上游明确拒绝（404 not support）时自动记录并按 1m → 5m → 30m → 2h → 8h 阶梯退避；累计达 1 天即永久失效。成功跑通会自动清除。Refresh 重置退避立即恢复参与。</p>
    <div className="table-wrap"><table className="settings-table"><thead><tr><th>Provider</th><th>Key</th><th>Model</th><th>Attempts</th><th>Status</th><th>Last Error</th><th></th></tr></thead><tbody>
      {(entries ?? []).map((e) => <tr key={`${e.provider}-${e.key}-${e.model}`}>
        <td className="muted small-text">{e.provider}</td>
        <td className="strong-cell">{e.key}</td>
        <td className="muted small-text">{e.model}</td>
        <td>{e.attempt}</td>
        <td><span className={`status ${e.permanent ? 'warn' : ''}`}>{formatRetryIn(e)}</span></td>
        <td className="muted small-text" title={e.last_error}>{e.last_error.length > 80 ? `${e.last_error.slice(0, 80)}…` : e.last_error}</td>
        <td><button className="secondary compact-button" disabled={refreshing !== null} onClick={() => void refresh({ provider: e.provider, key: e.key, model: e.model }, `${e.provider}-${e.key}-${e.model}`)}>{refreshing === `${e.provider}-${e.key}-${e.model}` ? '...' : 'Refresh'}</button></td>
      </tr>)}
      {entries !== null && entries.length === 0 && <tr><td colSpan={7} className="muted">No unsupported records — all keys are healthy for their models.</td></tr>}
    </tbody></table></div>
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Close</button><button className="secondary" disabled={refreshing !== null || !(entries ?? []).length} onClick={() => void refresh(providerName ? { provider: providerName } : {}, '__all__')}>{refreshing === '__all__' ? 'Refreshing...' : `Refresh All${providerName ? ` (${providerName})` : ''}`}</button></div>
  </div></div>;
}
