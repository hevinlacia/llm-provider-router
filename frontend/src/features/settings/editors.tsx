import { useCallback, useEffect, useState } from 'react';
import { api } from '../../api';
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
  // —— 密钥配置状态（/api/config/keys snapshot：environment / vault / missing，不回显明文）——
  const [keySource, setKeySource] = useState<Record<string, string>>({});
  const [keyConfigEditing, setKeyConfigEditing] = useState<{ index: number; name: string; mode: 'env' | 'plain' } | null>(null);
  const loadKeySource = useCallback(async () => {
    try {
      const snap = await api.keys();
      const map: Record<string, string> = {};
      for (const k of snap.keys ?? []) map[k.name] = k.source;
      setKeySource(map);
    } catch { /* snapshot 不可用时状态列退化 */ }
  }, []);
  useEffect(() => { if (!isNew) void loadKeySource(); }, [isNew, loadKeySource]);
  async function savePlainValue(keyName: string, value: string) {
    try {
      await api.saveKeys(value ? { [keyName]: value } : {}, value ? [] : [keyName]);
      await loadKeySource();
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }
  function sourceOf(keyName: string): string {
    return keySource[keyName] ?? 'missing';
  }
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
      onError('At least one of Chat Completions API / Responses API / Anthropic API must not be empty');
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
    <div className="field"><label>Chat Completions API</label><input value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} placeholder="https://api.example.com/v1" /></div>
    <div className="field"><label>Responses API</label><input value={responsesBaseUrl} onChange={(event) => setResponsesBaseUrl(event.target.value)} placeholder="https://api.example.com/v1（留空则翻译）" /></div>
    <div className="field"><label>Anthropic API</label><input value={anthropicBaseUrl} onChange={(event) => setAnthropicBaseUrl(event.target.value)} placeholder="https://api.anthropic.com" /></div>
    <h4>Keys — 通过「密钥配置」列选择环境变量或明文密钥（明文保存后立即生效；git 副本经 bin/vault.sh 加密）</h4>
    <div className="table-wrap"><table><colgroup><col style={{ width: '16%' }} /><col style={{ width: '34%' }} /><col style={{ width: '10%' }} /><col style={{ width: '14%' }} /><col style={{ width: '11%' }} /><col style={{ width: '15%' }} /></colgroup><thead><tr><th>Key</th><th>密钥配置</th><th>Weight</th><th>Billing</th><th>Enabled</th><th></th></tr></thead><tbody>
      {keys.map((k, i) => {
        const src = sourceOf(k.name);
        return <tr key={i}><td><input value={k.name} onChange={(event) => updateKey(i, { name: event.target.value })} /></td><td><span style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}><button type="button" className={`secondary compact-button ${src === 'environment' ? 'key-src-active' : ''}`} title={k.env_var ? `当前环境变量：${k.env_var}` : '绑定环境变量名，值从进程环境读取'} onClick={() => setKeyConfigEditing({ index: i, name: k.name, mode: 'env' })}>环境变量</button><button type="button" className={`secondary compact-button ${src === 'vault' ? 'key-src-active' : ''}`} title={src === 'vault' ? '已设置明文密钥（立即生效）' : '直接粘贴明文密钥值，存本机 vault'} onClick={() => setKeyConfigEditing({ index: i, name: k.name, mode: 'plain' })}>明文密钥</button>{src === 'missing' && <span className="muted small-text">未配置</span>}</span></td><td><input className="weight-input" type="number" min="0" step="1" value={k.weight} onChange={(event) => updateKey(i, { weight: Number(event.target.value) || 0 })} /></td><td><select value={k.billing_type} onChange={(event) => updateKey(i, { billing_type: event.target.value })}><option value="subscription">subscription</option><option value="payg">payg</option></select></td><td><input type="checkbox" checked={k.enabled} onChange={(event) => updateKey(i, { enabled: event.target.checked })} /></td><td><button className="secondary" onClick={() => removeKey(i)}>Delete</button></td></tr>;
      })}
    </tbody></table></div>
    <div className="toolbar" style={{ justifyContent: 'space-between' }}><button className="secondary" onClick={addKey}>Add Key</button><span style={{ display: 'flex', gap: 10 }}><button className="secondary" onClick={onCancel}>Cancel</button><button onClick={() => void save()}>Save</button></span></div>
    {keyConfigEditing && <KeyConfigModal
      keyName={keyConfigEditing.name}
      mode={keyConfigEditing.mode}
      currentEnvVar={keys[keyConfigEditing.index]?.env_var ?? ''}
      source={sourceOf(keyConfigEditing.name)}
      known={keySource[keyConfigEditing.name] !== undefined}
      onClose={() => setKeyConfigEditing(null)}
      onSaveEnvVar={(value) => updateKey(keyConfigEditing.index, { env_var: value })}
      onSavePlain={(value) => { void savePlainValue(keyConfigEditing.name, value); }}
      onClearPlain={() => { void savePlainValue(keyConfigEditing.name, ''); }}
    />}
  </div></div>;
}

/// 密钥配置二次弹窗：环境变量模式（随主弹窗 Save 生效）/ 明文密钥模式（立即生效）。
function KeyConfigModal({ keyName, mode, currentEnvVar, source, known, onClose, onSaveEnvVar, onSavePlain, onClearPlain }: {
  keyName: string;
  mode: 'env' | 'plain';
  currentEnvVar: string;
  source: string;
  known: boolean;
  onClose: () => void;
  onSaveEnvVar: (value: string) => void;
  onSavePlain: (value: string) => void;
  onClearPlain: () => void;
}) {
  const [envVar, setEnvVar] = useState(currentEnvVar);
  const [value, setValue] = useState('');
  const [show, setShow] = useState(false);
  const isEnv = mode === 'env';
  return <div className="modal-overlay high" onClick={onClose}><div className="modal modal-sm" onClick={(event) => event.stopPropagation()}>
    <h3>密钥配置：{keyName}</h3>
    {isEnv ? <>
      <div className="field"><label>环境变量名</label><input value={envVar} autoFocus placeholder="e.g. AGENT_AI_ARK_HEVIN_API_KEY" onChange={(event) => setEnvVar(event.target.value)} /></div>
      <p className="muted small-text">密钥值从进程环境变量读取（systemd env 文件或启动环境注入）。确定后需点击主弹窗 Save 保存供应商配置才生效。</p>
      <div className="toolbar"><button type="button" className="secondary" onClick={onClose}>取消</button><button type="button" onClick={() => { onSaveEnvVar(envVar.trim()); onClose(); }}>确定</button></div>
    </> : <>
      {source === 'vault' && <p className="muted small-text">当前已设置明文密钥（值不回显）。重新粘贴将覆盖；清除后 key 变为 missing。</p>}
      {!known && <p className="muted small-text">该 key 尚未保存到供应商配置，请先在主弹窗点击 Save，再回来设置明文值。</p>}
      <div className="field"><label>明文密钥</label><span style={{ display: 'flex', gap: 6, alignItems: 'center' }}><input style={{ flex: 1 }} type={show ? 'text' : 'password'} value={value} placeholder="paste key value" autoFocus disabled={!known} onChange={(event) => setValue(event.target.value)} onKeyDown={(event) => { if (event.key === 'Enter' && value) { onSavePlain(value); onClose(); } }} /><button type="button" className="secondary" onClick={() => setShow(!show)}>{show ? '隐藏' : '显示'}</button></span></div>
      <p className="muted small-text">保存后立即生效；明文存本机 config/api-keys.json，git 副本经 bin/vault.sh 加密。</p>
      <div className="toolbar">
        {source === 'vault' && <button type="button" className="secondary" onClick={() => { onClearPlain(); onClose(); }}>清除明文</button>}
        <button type="button" className="secondary" onClick={onClose}>取消</button>
        <button type="button" disabled={!known || !value} onClick={() => { onSavePlain(value); onClose(); }}>保存明文</button>
      </div>
    </>}
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
  const [targets, setTargets] = useState<Array<{ model: string; weight: string; keys: string }>>(() =>
    logical.targets.map((t) => ({ model: t.model, weight: t.weight != null ? String(t.weight) : '', keys: (t.keys ?? []).join(', ') })),
  );
  function updateTarget(index: number, patch: Partial<{ model: string; weight: string; keys: string }>) {
    const next = [...targets];
    next[index] = { ...next[index], ...patch };
    setTargets(next);
  }
  function addTarget() { setTargets([...targets, { model: '', weight: '', keys: '' }]); }
  function removeTarget(index: number) { setTargets(targets.filter((_, i) => i !== index)); }
  async function save() {
    const cleaned = targets.filter((t) => t.model.trim());
    if (!cleaned.length) { onError('At least one target is required'); return; }
    if (!poolName.trim()) { onError('Model pool name must not be empty'); return; }
    const parsed = cleaned.map((t) => ({
      model: t.model.trim(),
      weight: t.weight.trim() === '' ? null : Math.max(0, Number(t.weight) || 0),
      keys: t.keys.split(',').map((s) => s.trim()).filter(Boolean),
    }));
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
    <h4>Targets — physical model (provider/upstream), model pool, or virtual model. Keys: optional comma-separated key-name allowlist (empty = all enabled keys of the provider).</h4>
    <datalist id={datalistId}>{candidates.filter((g) => g.items.length > 0).map((group) => <optgroup key={group.group} label={group.group}>{group.items.map((candidate) => <option key={candidate} value={candidate} />)}</optgroup>)}</datalist>
    <div className="table-wrap"><table><thead><tr><th>Target</th><th>Weight</th><th>Keys allowlist</th><th></th></tr></thead><tbody>
      {targets.map((t, i) => <tr key={i}><td><input list={datalistId} value={t.model} placeholder="e.g. openai-relay/grok-4.6 (physical), a pool name, or a virtual model" onChange={(event) => updateTarget(i, { model: event.target.value })} /></td><td><input className="weight-input" type="number" min="0" value={t.weight} placeholder="optional" onChange={(event) => updateTarget(i, { weight: event.target.value })} /></td><td><input value={t.keys} placeholder="optional, e.g. hevin, hevin2" onChange={(event) => updateTarget(i, { keys: event.target.value })} /></td><td><button className="secondary" onClick={() => removeTarget(i)}>Delete</button></td></tr>)}
    </tbody></table></div>
    <button className="secondary" onClick={addTarget}>Add Target</button>
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Cancel</button><button onClick={() => void save()}>{isNew ? 'Create' : 'Save'}</button></div>
  </div></div>;
}
