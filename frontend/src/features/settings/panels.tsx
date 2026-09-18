import { useMemo, useState } from 'react';
import { api } from '../../api';
import type { CustomModelAlias, KeyConfig, ModelAliasConfig } from '../../types';

export function ModelAliasesPanel({ config, onChange, onSaved, onError }: { config: ModelAliasConfig | null; onChange: (value: ModelAliasConfig) => void; onSaved: (value: ModelAliasConfig) => void; onError: (value: string) => void }) {
  if (!config) return <section className="card"><h2>Custom Model Aliases</h2><p className="muted">Loading model aliases...</p></section>;
  const current = config;

  function updateAlias(index: number, alias: CustomModelAlias) {
    const custom_aliases = [...current.custom_aliases];
    custom_aliases[index] = alias;
    onChange({ ...current, custom_aliases });
  }

  function removeAlias(index: number) {
    const custom_aliases = current.custom_aliases.filter((_, i) => i !== index);
    onChange({ ...current, custom_aliases });
  }

  function addAlias() {
    onChange({
      ...current,
      custom_aliases: [...current.custom_aliases, {
        alias: '',
        upstream_model: '',
        provider: 'ark',
        max_retry_seconds: 300,
        retry_delay_seconds: 5.0,
      }],
    });
  }

  async function save() {
    try { onSaved(await api.saveModelAliases(current.custom_aliases)); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }

  return <section className="card"><div className="section-title"><h2>Custom Model Aliases</h2><span className="muted">{current.config_path}</span></div>
    <p className="muted">Add and configure new model names for each provider. Alias names become available as targets in model routes.</p>
    {current.custom_aliases.length > 0 && <div className="table-wrap"><table>
      <thead><tr><th>Alias Name</th><th>Upstream Model</th><th>Provider</th><th>Max Retry (s)</th><th>Delay (s)</th><th>Actions</th></tr></thead>
      <tbody>{current.custom_aliases.map((alias, index) => <tr key={alias.alias || index}>
        <td><input value={alias.alias} onChange={(e) => updateAlias(index, { ...alias, alias: e.target.value })} placeholder="e.g. my-model-auto" /></td>
        <td><input value={alias.upstream_model} onChange={(e) => updateAlias(index, { ...alias, upstream_model: e.target.value })} placeholder="e.g. openai/deepseek-v4" /></td>
        <td><select value={alias.provider} onChange={(e) => updateAlias(index, { ...alias, provider: e.target.value })}>
          <option value="ark">Ark</option>
          <option value="deepseek-official">DeepSeek Official</option>
          <option value="openai-relay">OpenAI Relay</option>
        </select></td>
        <td><input className="number-input" type="number" min="0" value={alias.max_retry_seconds} onChange={(e) => updateAlias(index, { ...alias, max_retry_seconds: Number(e.target.value) || 0 })} /></td>
        <td><input className="number-input" type="number" min="0" step="0.1" value={alias.retry_delay_seconds} onChange={(e) => updateAlias(index, { ...alias, retry_delay_seconds: Number(e.target.value) || 0 })} /></td>
        <td><button className="secondary" onClick={() => removeAlias(index)}>Remove</button></td>
      </tr>)}</tbody>
    </table></div>}
    {!current.custom_aliases.length && <p className="muted">No custom model aliases defined yet.</p>}
    <div className="toolbar"><button className="secondary" onClick={addAlias}>Add New Alias</button><button onClick={() => void save()}>Save Aliases</button></div>
  </section>;
}

export function KeysPanel({ config, onSaved, onError }: { config: KeyConfig | null; onSaved: (value: KeyConfig) => void; onError: (value: string) => void }) {
  const [values, setValues] = useState<Record<string, string>>({});
  const [deleteNames, setDeleteNames] = useState<string[]>([]);
  if (!config) return <section className="card"><h2>API Keys</h2><p className="muted">Loading keys...</p></section>;
  const current = config;

  async function save() {
    try { onSaved(await api.saveKeys(values, deleteNames)); setValues({}); setDeleteNames([]); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }

  const grouped = current.keys.reduce<Record<string, KeyConfig['keys']>>((groups, item) => { (groups[item.provider] ??= []).push(item); return groups; }, {});
  return <section className="card"><div className="section-title"><h2>API Keys</h2><span className="muted">{current.config_path}</span></div><p className="muted">Values are saved encrypted. Existing key values are never displayed.</p>{Object.entries(grouped).sort(([a], [b]) => a.localeCompare(b)).map(([provider, items]) => <div className="provider-group" key={provider}><h3>{provider}</h3><div className="table-wrap"><table className="api-key-table"><thead><tr><th>Key</th><th>Billing</th><th>Env Var</th><th>Status</th><th>New Value</th><th>Delete Encrypted</th></tr></thead><tbody>{items.map((item) => <tr key={item.name}><td>{item.name}</td><td>{item.billing_type === 'payg' ? 'Pay-as-you-go' : 'Subscription'}</td><td>{item.env_var}</td><td><span className={`status ${item.configured ? 'ok' : 'warn'}`}>{item.configured ? item.source : 'missing'}</span></td><td><input className="key-input" type="password" value={values[item.name] ?? ''} onChange={(event) => setValues({ ...values, [item.name]: event.target.value })} placeholder="Leave blank to keep current value" /></td><td><input type="checkbox" checked={deleteNames.includes(item.name)} onChange={(event) => setDeleteNames(event.target.checked ? [...deleteNames, item.name] : deleteNames.filter((name) => name !== item.name))} /></td></tr>)}</tbody></table></div></div>)}<div className="toolbar"><button onClick={() => void save()}>Save API Keys</button></div></section>;
}
