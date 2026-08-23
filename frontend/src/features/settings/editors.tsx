import { useState } from 'react';
import { api } from '../../api';
import { isKnownTarget } from '../../lib/format';
import type { TargetCandidateGroup, V2LogicalModel, V2ProviderStatus, V2Status } from '../../types';

type KeyDraft = { name: string; env_var: string; weight: number; billing_type: string; enabled: boolean };

type ProviderDraft = {
  name: string;
  base_url: string;
  keys: Record<string, { env_var: string; weight: number; billing_type: string; enabled: boolean }>;
};

export function ProviderEditor({ providerName, provider, isNew = false, onCancel, onSaved, onError }: {
  providerName: string;
  provider: V2ProviderStatus;
  isNew?: boolean;
  onCancel: () => void;
  onSaved: (value: V2Status) => void;
  onError: (value: string) => void;
}) {
  const [name, setName] = useState(providerName);
  const [baseUrl, setBaseUrl] = useState(provider.base_url);
  const [keys, setKeys] = useState<KeyDraft[]>(() =>
    Object.entries(provider.keys).map(([k, v]) => ({ name: k, env_var: v.env_var, weight: v.weight, billing_type: v.billing_type, enabled: v.enabled })),
  );
  function updateKey(index: number, patch: Partial<KeyDraft>) {
    const next = [...keys];
    next[index] = { ...next[index], ...patch };
    setKeys(next);
  }
  function addKey() {
    setKeys([...keys, { name: '', env_var: '', weight: 1, billing_type: 'subscription', enabled: true }]);
  }
  function removeKey(index: number) {
    setKeys(keys.filter((_, i) => i !== index));
  }
  async function save() {
    if (!name.trim()) { onError('Provider name must not be empty'); return; }
    if (!baseUrl.trim()) { onError('Base URL must not be empty'); return; }
    const keyMap: ProviderDraft['keys'] = {};
    for (const k of keys) {
      if (!k.name.trim()) continue;
      keyMap[k.name.trim()] = {
        env_var: k.env_var.trim(),
        weight: Math.max(0, Number(k.weight) || 0),
        billing_type: k.billing_type || 'subscription',
        enabled: k.enabled,
      };
    }
    try {
      if (isNew) {
        onSaved(await api.createV2Provider({ name: name.trim(), base_url: baseUrl.trim(), keys: keyMap }));
      } else {
        onSaved(await api.updateV2Provider(providerName, { name: name.trim(), base_url: baseUrl.trim(), keys: keyMap }));
      }
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }
  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <h3>{isNew ? 'Add Provider' : `Edit Provider: ${providerName}`}</h3>
    <div className="field"><label>Name</label><input value={name} onChange={(event) => setName(event.target.value)} /></div>
    <div className="field"><label>Base URL</label><input value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} /></div>
    <h4>Keys</h4>
    <div className="table-wrap"><table><thead><tr><th>Key</th><th>Env Var</th><th>Weight</th><th>Billing</th><th>Enabled</th><th></th></tr></thead><tbody>
      {keys.map((k, i) => <tr key={i}><td><input value={k.name} onChange={(event) => updateKey(i, { name: event.target.value })} /></td><td><input className="env-input" value={k.env_var} onChange={(event) => updateKey(i, { env_var: event.target.value })} /></td><td><input className="weight-input" type="number" min="0" step="1" value={k.weight} onChange={(event) => updateKey(i, { weight: Number(event.target.value) || 0 })} /></td><td><select value={k.billing_type} onChange={(event) => updateKey(i, { billing_type: event.target.value })}><option value="subscription">subscription</option><option value="payg">payg</option></select></td><td><input type="checkbox" checked={k.enabled} onChange={(event) => updateKey(i, { enabled: event.target.checked })} /></td><td><button className="secondary" onClick={() => removeKey(i)}>Delete</button></td></tr>)}
    </tbody></table></div>
    <button className="secondary" onClick={addKey}>Add Key</button>
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Cancel</button><button onClick={() => void save()}>Save</button></div>
  </div></div>;
}

export function LogicalModelEditor({ name, logical, candidates, isNew = false, onCancel, onSaved, onError }: {
  name: string;
  logical: V2LogicalModel;
  candidates: TargetCandidateGroup[];
  isNew?: boolean;
  onCancel: () => void;
  onSaved: (value: V2Status) => void;
  onError: (value: string) => void;
}) {
  const [poolName, setPoolName] = useState(name);
  const [strategy, setStrategy] = useState(logical.strategy);
  const [targets, setTargets] = useState<Array<{ model: string; weight: string }>>(() =>
    logical.targets.map((t) => ({ model: t.model, weight: t.weight != null ? String(t.weight) : '' })),
  );
  const [thinkingLevelMap, setThinkingLevelMap] = useState<Record<string, string | null> | null>(
    () => (logical.thinking_level_map as Record<string, string | null> | undefined) ?? null,
  );
  const [thinkingFormat, setThinkingFormat] = useState<string | null>(
    () => (logical.thinking_format as string | null | undefined) ?? null,
  );
  const [showThinking, setShowThinking] = useState(() => Boolean(logical.thinking_level_map || logical.thinking_format));
  function updateTarget(index: number, patch: Partial<{ model: string; weight: string }>) {
    const next = [...targets];
    next[index] = { ...next[index], ...patch };
    setTargets(next);
  }
  function addTarget() { setTargets([...targets, { model: '', weight: '' }]); }
  function removeTarget(index: number) { setTargets(targets.filter((_, i) => i !== index)); }
  async function save() {
    const cleaned = targets.filter((t) => t.model.trim());
    if (!cleaned.length) { onError('At least one target is required'); return; }
    if (!poolName.trim()) { onError('Model pool name must not be empty'); return; }
    const unknown = cleaned.filter((t) => !isKnownTarget(t.model.trim(), candidates));
    if (unknown.length) {
      onError(`Unknown target: ${unknown.map((t) => t.model.trim()).join(', ')}. Targets may only be: a physical model (provider/upstream), another model pool, or a registered virtual model.`);
      return;
    }
    const parsed = cleaned.map((t) => ({ model: t.model.trim(), weight: t.weight.trim() === '' ? null : Math.max(0, Number(t.weight) || 0) }));
    const thinkingPayload: { thinking_level_map?: Record<string, string | null> | null; thinking_format?: string | null } = {};
    if (showThinking) {
      thinkingPayload.thinking_level_map = thinkingLevelMap;
      thinkingPayload.thinking_format = thinkingFormat;
    }
    try {
      if (isNew) {
        onSaved(await api.createV2LogicalModel({ name: poolName.trim(), strategy, targets: parsed, ...thinkingPayload }));
      } else {
        onSaved(await api.updateV2LogicalModel(poolName.trim(), { strategy, targets: parsed, ...thinkingPayload }));
      }
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }
  const datalistId = `lm-targets-${poolName || 'new'}`;
  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <h3>{isNew ? 'Add Model Pool' : `Edit Model Pool: ${name}`}</h3>
    <div className="field"><label>Pool Name</label><input value={poolName} onChange={(event) => setPoolName(event.target.value)} disabled={!isNew} placeholder="e.g. low-model-auto" /></div>
    <div className="field"><label>Strategy</label>
      <select value={strategy} onChange={(event) => setStrategy(event.target.value)}>
        <option value="priority">priority</option>
        <option value="weighted">weighted</option>
        <option value="usage-aware">usage-aware</option>
      </select>
    </div>
    <h4>Targets — physical model (provider/upstream), model pool, or virtual model</h4>
    <datalist id={datalistId}>{candidates.filter((g) => g.items.length > 0).map((group) => <optgroup key={group.group} label={group.group}>{group.items.map((candidate) => <option key={candidate} value={candidate} />)}</optgroup>)}</datalist>
    <div className="table-wrap"><table><thead><tr><th>Target</th><th>Weight</th><th></th></tr></thead><tbody>
      {targets.map((t, i) => <tr key={i}><td><input list={datalistId} value={t.model} placeholder="e.g. openai-relay/grok-4.6 (physical), a pool name, or a virtual model" onChange={(event) => updateTarget(i, { model: event.target.value })} /></td><td><input className="weight-input" type="number" min="0" value={t.weight} placeholder="optional" onChange={(event) => updateTarget(i, { weight: event.target.value })} /></td><td><button className="secondary" onClick={() => removeTarget(i)}>Delete</button></td></tr>)}
    </tbody></table></div>
    <button className="secondary" onClick={addTarget}>Add Target</button>
    <div style={{ marginTop: 16, borderTop: '1px solid #1f2937', paddingTop: 12 }}>
      <label style={{ display: 'flex', gap: 8, alignItems: 'center', cursor: 'pointer' }}><input type="checkbox" checked={showThinking} onChange={(e) => setShowThinking(e.target.checked)} /><span>Thinking fallback（逻辑池回退映射，物理未配置时生效）</span></label>
      {showThinking && <div style={{ marginTop: 10 }}>
        <p className="muted small-text">标准档位 → 上游 wire 值；留空该行档位=不写入（透传），<code>null</code>=不支持。物理层已配置则优先物理。</p>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 8, marginTop: 8 }}>
          {(['off', 'minimal', 'low', 'medium', 'high', 'xhigh'] as const).map((lv) => {
            const v = thinkingLevelMap?.[lv];
            const display = v === null ? '' : v === undefined ? '' : String(v);
            const isNull = v === null;
            return <div key={lv} className="field"><label>{lv}{isNull ? ' (null)' : ''}</label><div style={{ display: 'flex', gap: 4 }}><input value={display} placeholder={isNull ? 'null' : '—'} title={isNull ? 'null (不支持)' : undefined} onChange={(e) => {
              const raw = e.target.value;
              if (raw === '' && isNull) return; // keep null until cleared via ∅ toggle
              setThinkingLevelMap((prev) => {
                const next = { ...(prev ?? {}) } as Record<string, string | null>;
                if (raw.trim() === '') delete next[lv];
                else next[lv] = raw.trim();
                return Object.keys(next).length ? next : null;
              });
            }} /><button className="secondary" style={{ padding: '4px 8px', fontSize: 11 }} title="Set null (unsupported)" onClick={() => setThinkingLevelMap((prev) => ({ ...(prev ?? {}), [lv]: null }))}>∅</button>{isNull && <button className="secondary" style={{ padding: '4px 8px', fontSize: 11 }} onClick={() => setThinkingLevelMap((prev) => { const n = { ...(prev ?? {}) }; delete n[lv]; return Object.keys(n).length ? n : null; })}>✕</button>}</div></div>;
          })}
        </div>
        <div className="field" style={{ marginTop: 10 }}><label>Format</label><select value={thinkingFormat ?? ''} onChange={(e) => setThinkingFormat(e.target.value || null)}><option value="">— (inherit)</option><option value="reasoning_effort">reasoning_effort</option></select></div>
      </div>}
    </div>
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Cancel</button><button onClick={() => void save()}>{isNew ? 'Create' : 'Save'}</button></div>
  </div></div>;
}
