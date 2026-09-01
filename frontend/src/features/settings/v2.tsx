import { useState } from 'react';
import { api } from '../../api';
import { buildTargetCandidates } from '../../lib/format';
import type { V2Status } from '../../types';
import { LogicalModelEditor, ProviderEditor } from './editors';
import { ProviderModelsModal, ProviderVirtualModelsModal } from './modals';

export function V2Panel({ config, onSaved, onError }: { config: V2Status | null; onSaved: (value: V2Status) => void; onError: (value: string) => void }) {
  const [editing, setEditing] = useState<string | null>(null);
  const [editingLogical, setEditingLogical] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [addingPool, setAddingPool] = useState(false);
  const [viewingModels, setViewingModels] = useState<string | null>(null);
  const [viewingVirtual, setViewingVirtual] = useState<string | null>(null);
  if (!config) return <section className="card"><h2>Providers & Logical Models</h2><p className="muted">Loading routing settings...</p></section>;
  if (!config.v2_enabled) return <section className="card"><div className="section-title"><h2>Providers & Logical Models</h2><span className="muted">disabled</span></div><p className="muted">Layered routing is disabled (set LLM_PROVIDER_ROUTER_V2=1 to enable).</p></section>;
  const providers = Object.entries(config.providers ?? {}).sort(([a], [b]) => a.localeCompare(b));
  const logical = Object.entries(config.logical_models ?? {}).sort(([a], [b]) => a.localeCompare(b));
  const editingProvider = editing ? (config.providers?.[editing] ?? null) : null;
  async function deletePool(name: string) {
    try {
      onSaved(await api.deleteV2LogicalModel(name));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }
  return <>
    <section className="card settings-section providers-section">
      <div className="section-title settings-title"><div><h2>Providers</h2><p className="muted">Upstream provider endpoints, key availability, and provider health.</p></div><div className="title-actions"><span className="muted">{providers.length} providers · {config.models?.length ?? 0} physical models</span><button className="secondary compact-button" onClick={() => setAdding(true)}>Add Provider</button></div></div>
      <div className="table-wrap"><table className="settings-table providers-table"><thead><tr><th>Provider</th><th>Chat Completions API</th><th>Keys</th><th>Status</th><th></th></tr></thead><tbody>{providers.map(([name, p]) => <tr key={name}><td className="strong-cell">{name}</td><td className="muted small-text url-cell">{p.base_url}</td><td>{p.key_enabled}/{p.key_total}{p.key_frozen > 0 ? ` (${p.key_frozen} frozen)` : ''}</td><td><span className={`status ${p.available ? 'ok' : 'warn'}`}>{p.available ? 'available' : 'unavailable'}</span></td><td><div className="row-actions"><button className="secondary compact-button" onClick={() => setViewingModels(name)}>Details</button><button className="secondary compact-button" onClick={() => setViewingVirtual(name)}>Virtual</button><button className="secondary compact-button" onClick={() => setEditing(name)}>Edit</button></div></td></tr>)}</tbody></table></div>
    </section>
    <section className="card settings-section logical-models-section">
      <div className="section-title settings-title"><div><h2>Model Pools</h2><p className="muted">逻辑模型池：虚拟模型名与有序/加权路由目标。target 可填虚拟模型名、物理模型 id（provider/upstream）或另一个模型池。</p></div><div className="title-actions"><span className="muted">{logical.length} model pools</span><button className="secondary compact-button" onClick={() => setAddingPool(true)}>Add Pool</button></div></div>
      <div className="table-wrap"><table className="settings-table logical-models-table"><thead><tr><th>Model Pool</th><th>Strategy</th><th>Targets</th><th></th></tr></thead><tbody>{logical.map(([name, lm]) => <tr key={name}><td className="strong-cell">{name}</td><td><span className="status">{lm.strategy}</span></td><td className="muted small-text target-cell">{lm.targets.map((t) => <span className="target-pill" key={`${name}-${t.model}-${t.weight ?? 'default'}`}>{t.model}{t.weight != null ? <span className="target-weight">w={t.weight}</span> : null}</span>)}</td><td><div className="row-actions"><button className="secondary compact-button" onClick={() => setEditingLogical(name)}>Edit</button><button className="secondary compact-button" onClick={() => { if (confirm(`Delete model pool "${name}"? References from other pools will be removed.`)) void deletePool(name); }}>Delete</button></div></td></tr>)}</tbody></table></div>
    </section>
    {adding && <ProviderEditor isNew providerName="" provider={{ base_url: '', responses_base_url: null, anthropic_base_url: null, key_total: 0, key_enabled: 0, key_frozen: 0, available: false, keys: {} }} onCancel={() => setAdding(false)} onSaved={(next) => { setAdding(false); onSaved(next); }} onError={onError} />}
    {editingProvider && <ProviderEditor providerName={editing!} provider={editingProvider} onCancel={() => setEditing(null)} onSaved={(next) => { setEditing(null); onSaved(next); }} onError={onError} />}
    {editingLogical && config.logical_models?.[editingLogical] && <LogicalModelEditor name={editingLogical} logical={config.logical_models[editingLogical]} candidates={buildTargetCandidates(config, editingLogical)} onCancel={() => setEditingLogical(null)} onSaved={(next) => { setEditingLogical(null); onSaved(next); }} onError={onError} />}
    {addingPool && <LogicalModelEditor isNew name="" logical={{ strategy: 'priority', targets: [], params: {} }} candidates={buildTargetCandidates(config, null)} onCancel={() => setAddingPool(false)} onSaved={(next) => { setAddingPool(false); onSaved(next); }} onError={onError} />}
    {viewingModels && <ProviderModelsModal providerName={viewingModels} onCancel={() => setViewingModels(null)} onError={onError} />}
    {viewingVirtual && <ProviderVirtualModelsModal providerName={viewingVirtual} virtualModels={config.virtual_models ?? {}} onCancel={() => setViewingVirtual(null)} onSaved={onSaved} onError={onError} />}
  </>;
}
