import { useState } from 'react';
import { api } from '../../api';
import { isKnownTarget } from '../../lib/format';
import type { TargetCandidateGroup, V2LogicalModel, V2ProviderStatus, V2Status } from '../../types';

type KeyDraft = { name: string; env_var: string; weight: number; billing_type: string; enabled: boolean };

type ProviderDraft = {
  name: string;
  base_url: string;
  responses_base_url?: string | null;
  anthropic_base_url?: string | null;
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
  const [responsesBaseUrl, setResponsesBaseUrl] = useState(provider.responses_base_url ?? '');
  const [anthropicBaseUrl, setAnthropicBaseUrl] = useState(provider.anthropic_base_url ?? '');
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
    if (!baseUrl.trim() && !responsesBaseUrl.trim() && !anthropicBaseUrl.trim()) {
      onError('At least one of Base URL / Responses API Base URL / Anthropic Base URL must not be empty');
      return;
    }
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
        onSaved(await api.createV2Provider({ name: name.trim(), base_url: baseUrl.trim(), responses_base_url: responsesBaseUrl.trim() || null, anthropic_base_url: anthropicBaseUrl.trim() || null, keys: keyMap }));
      } else {
        onSaved(await api.updateV2Provider(providerName, { name: name.trim(), base_url: baseUrl.trim(), responses_base_url: responsesBaseUrl.trim() || null, anthropic_base_url: anthropicBaseUrl.trim() || null, keys: keyMap }));
      }
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }
  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <h3>{isNew ? 'Add Provider' : `Edit Provider: ${providerName}`}</h3>
    <div className="field"><label>Name</label><input value={name} onChange={(event) => setName(event.target.value)} /></div>
    <div className="field"><label>Base URL (Chat Completions API)</label><input value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} placeholder="https://api.example.com/v1" /></div>
    <div className="field"><label>Responses API Base URL <span className="muted small-text">（可选）供应商原生支持 OpenAI Responses API 时填写；填写后 /v1/responses 请求将透传到 {`{responses_base_url}/responses`}，未填写则由 Router 翻译成 chat completions 走上方地址</span></label><input value={responsesBaseUrl} onChange={(event) => setResponsesBaseUrl(event.target.value)} placeholder="https://api.example.com/v1（留空则翻译）" /></div>
    <div className="field"><label>Anthropic Base URL <span className="muted small-text">（可选）供应商同时提供 Anthropic 兼容 API 时填写；能力探测将优先走 Anthropic /v1/models 获取精确 context_window</span></label><input value={anthropicBaseUrl} onChange={(event) => setAnthropicBaseUrl(event.target.value)} placeholder="https://api.anthropic.com（留空则用 OpenAI 兼容探测）" /></div>
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
    try {
      if (isNew) {
        onSaved(await api.createV2LogicalModel({ name: poolName.trim(), strategy, targets: parsed }));
      } else {
        onSaved(await api.updateV2LogicalModel(poolName.trim(), { strategy, targets: parsed }));
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
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Cancel</button><button onClick={() => void save()}>{isNew ? 'Create' : 'Save'}</button></div>
  </div></div>;
}
