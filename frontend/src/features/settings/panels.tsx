import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../../api';
import type { CustomModelAlias, KeyConfig, ModelAliasConfig, ProviderConfig, WeightConfig } from '../../types';

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

export function ProvidersPanel({ config, onChange, onSaved, onError }: { config: ProviderConfig | null; onChange: (value: ProviderConfig) => void; onSaved: (value: ProviderConfig) => void; onError: (value: string) => void }) {
  if (!config) return <section className="card"><h2>Provider URLs</h2><p className="muted">Loading providers...</p></section>;
  const current = config;
  async function save() {
    try { onSaved(await api.saveProviders(Object.fromEntries(current.providers.map((item) => [item.name, item.base_url])))); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }
  return <section className="card"><div className="section-title"><h2>Provider URLs</h2><span className="muted">{current.config_path}</span></div><table><thead><tr><th>Provider</th><th>Base URL</th><th>Default</th></tr></thead><tbody>{current.providers.map((item, index) => <tr key={item.name}><td>{item.name}</td><td><input value={item.base_url} onChange={(event) => { const providers = [...current.providers]; providers[index] = { ...item, base_url: event.target.value }; onChange({ ...current, providers }); }} /></td><td>{item.default_base_url}</td></tr>)}</tbody></table><div className="toolbar"><button onClick={() => void save()}>Save Providers</button></div></section>;
}

export function WeightsPanel({ config, onChange, onSaved, onError }: { config: WeightConfig | null; onChange: (value: WeightConfig) => void; onSaved: (value: WeightConfig) => void; onError: (value: string) => void }) {
  const [selectedPool, setSelectedPool] = useState('__global__');

  useEffect(() => {
    if (config && (!config.supports_pool_weights || (selectedPool !== '__global__' && !config.pools.includes(selectedPool)))) setSelectedPool('__global__');
  }, [config, selectedPool]);

  const rows = useMemo(() => {
    if (!config) return [];
    if (selectedPool === '__global__') {
      const byName = new Map<string, WeightConfig['aliases'][string]['keys'][number]>();
      for (const alias of Object.values(config.aliases)) {
        for (const key of alias.keys) {
          if (!byName.has(key.name)) byName.set(key.name, key);
        }
      }
      const rows = [...byName.values()].sort((left, right) => left.name.localeCompare(right.name));
      if (rows.length) return rows;
      const visibleNames = new Set(Object.values(config.aliases).flatMap((alias) => alias.keys.map((key) => key.name)));
      return Object.entries(config.global_weights)
        .filter(([name]) => visibleNames.has(name))
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([name, weight]) => ({ name, provider: '', billing_type: '', default_weight: weight, global_weight: weight, weight, enabled: weight > 0, probability: 0 }));
    }
    return [...(config.aliases[selectedPool]?.keys ?? [])].sort((left, right) => left.name.localeCompare(right.name));
  }, [config, selectedPool]);

  const getWeight = useCallback((name: string, defaultWeight: number) => {
    if (!config) return defaultWeight;
    if (selectedPool === '__global__') return config.global_weights[name] ?? config.weights[name] ?? defaultWeight;
    return config.pool_weights[selectedPool]?.[name] ?? config.global_weights[name] ?? defaultWeight;
  }, [config, selectedPool]);

  const total = useMemo(() => rows.reduce((sum, row) => sum + Math.max(0, getWeight(row.name, row.default_weight)), 0), [rows, getWeight]);

  if (!config) return <section className="card"><h2>Key Weights</h2><p className="muted">Loading weights...</p></section>;
  const current = config;
  const poolWeights = current.pool_weights[selectedPool] ?? {};

  function setWeight(name: string, value: number) {
    const normalized = Math.max(0, Number(value) || 0);
    if (selectedPool === '__global__') {
      const global_weights = { ...current.global_weights, [name]: normalized };
      onChange({ ...current, weights: global_weights, global_weights });
      return;
    }
    onChange({
      ...current,
      pool_weights: {
        ...current.pool_weights,
        [selectedPool]: { ...poolWeights, [name]: normalized },
      },
    });
  }

  async function save() {
    const payload = Object.fromEntries(rows.map((row) => [row.name, getWeight(row.name, row.default_weight)]));
    try { onSaved(await api.saveWeights(payload, selectedPool === '__global__' ? undefined : selectedPool)); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }

  async function applyGlobalToPool() {
    if (selectedPool === '__global__') return;
    const payload = Object.fromEntries(rows.map((row) => [row.name, current.global_weights[row.name] ?? row.default_weight]));
    try { onSaved(await api.saveWeights(payload, selectedPool)); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }

  return <section className="card"><div className="section-title"><h2>Key Weights</h2><span className="muted">{current.config_path}</span></div><p className="muted">Global weights apply to every pool. Pool-specific weights override global values; weight 0 disables that key for routing.</p>{!current.supports_pool_weights && <p className="muted small-text">Pool-specific controls will appear after the router backend is restarted with the latest build.</p>}<section className="toolbar weight-filter"><div className="field"><label>Scope</label><select value={selectedPool} onChange={(event) => setSelectedPool(event.target.value)}><option value="__global__">Global pool weights</option>{current.supports_pool_weights && current.pools.map((pool) => <option key={pool} value={pool}>{pool}</option>)}</select></div>{selectedPool !== '__global__' && current.supports_pool_weights && <button className="secondary" onClick={() => void applyGlobalToPool()}>Apply Global to Pool</button>}</section><table><thead><tr><th>Key</th><th>Enabled</th><th>Weight</th>{selectedPool !== '__global__' && <th>Global</th>}<th>Source</th><th>Probability</th></tr></thead><tbody>{rows.map((row) => { const weight = getWeight(row.name, row.default_weight); const poolOverride = selectedPool !== '__global__' ? poolWeights[row.name] : undefined; const source = selectedPool === '__global__' ? 'Global' : poolOverride === undefined ? 'Global' : 'Pool override'; return <tr key={row.name} className={weight <= 0 ? 'disabled-row' : ''}><td>{row.name}<div className="muted small-text">{[row.provider, row.billing_type === 'payg' ? 'PAYG' : row.billing_type ? 'Subscription' : ''].filter(Boolean).join(' · ')}</div></td><td><input type="checkbox" checked={weight > 0} onChange={(event) => { const fallback = Math.max(1, selectedPool === '__global__' ? row.default_weight : current.global_weights[row.name] ?? row.default_weight); setWeight(row.name, event.target.checked ? fallback : 0); }} /></td><td><input className="weight-input" type="number" min="0" step="1" value={weight} onChange={(event) => setWeight(row.name, Number(event.target.value) || 0)} /></td>{selectedPool !== '__global__' && <td>{current.global_weights[row.name] ?? row.default_weight}</td>}<td>{source}</td><td>{total > 0 && weight > 0 ? `${((Math.max(0, weight) / total) * 100).toFixed(1)}%` : '0.0%'}</td></tr>; })}</tbody></table>{!rows.length && <p className="muted">No keys assigned to this scope.</p>}<div className="toolbar"><button onClick={() => void save()}>Save Weights</button></div></section>;
}

export function KeysPanel({ config, onSaved, onError }: { config: KeyConfig | null; onSaved: (value: KeyConfig) => void; onError: (value: string) => void }) {
  const [values, setValues] = useState<Record<string, string>>({});
  const [deleteNames, setDeleteNames] = useState<string[]>([]);
  const [add, setAdd] = useState({ name: '', value: '', weight: 1, aliases: [] as string[] });

  useEffect(() => {
    if (config && add.aliases.length === 0) setAdd((current) => ({ ...current, aliases: config.auto_aliases }));
  }, [config]);

  if (!config) return <section className="card"><h2>API Keys</h2><p className="muted">Loading keys...</p></section>;
  const current = config;

  async function save() {
    try { onSaved(await api.saveKeys(values, deleteNames)); setValues({}); setDeleteNames([]); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }
  async function addKey() {
    try { onSaved(await api.addKey(add)); setAdd({ name: '', value: '', weight: 1, aliases: current.auto_aliases }); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }

  const grouped = current.keys.reduce<Record<string, KeyConfig['keys']>>((groups, item) => { (groups[item.provider] ??= []).push(item); return groups; }, {});
  return <section className="card"><div className="section-title"><h2>API Keys</h2><span className="muted">{current.config_path}</span></div><p className="muted">Values are saved encrypted. Existing key values are never displayed.</p><div className="add-key-panel"><h3>Add Ark Key</h3><div className="add-key-grid"><div className="field"><label>Key Name</label><input value={add.name} onChange={(event) => setAdd({ ...add, name: event.target.value })} placeholder="shell" /></div><div className="field"><label>API Key</label><input type="password" value={add.value} onChange={(event) => setAdd({ ...add, value: event.target.value })} placeholder="Stored encrypted; never displayed" /></div><div className="field"><label>Weight</label><input type="number" min="0" step="1" value={add.weight} onChange={(event) => setAdd({ ...add, weight: Number(event.target.value) || 0 })} /></div></div><div className="pool-list">{current.auto_aliases.map((alias) => <label key={alias}><input type="checkbox" checked={add.aliases.includes(alias)} onChange={(event) => setAdd({ ...add, aliases: event.target.checked ? [...add.aliases, alias] : add.aliases.filter((item) => item !== alias) })} />{alias}</label>)}</div><div className="toolbar"><button onClick={() => void addKey()}>Add Key</button></div></div>{Object.entries(grouped).sort(([a], [b]) => a.localeCompare(b)).map(([provider, items]) => <div className="provider-group" key={provider}><h3>{provider}</h3><div className="table-wrap"><table className="api-key-table"><thead><tr><th>Key</th><th>Billing</th><th>Env Var</th><th>Status</th><th>New Value</th><th>Delete Encrypted</th></tr></thead><tbody>{items.map((item) => <tr key={item.name}><td>{item.name}</td><td>{item.billing_type === 'payg' ? 'Pay-as-you-go' : 'Subscription'}</td><td>{item.env_var}</td><td><span className={`status ${item.configured ? 'ok' : 'warn'}`}>{item.configured ? item.source : 'missing'}</span></td><td><input className="key-input" type="password" value={values[item.name] ?? ''} onChange={(event) => setValues({ ...values, [item.name]: event.target.value })} placeholder="Leave blank to keep current value" /></td><td><input type="checkbox" checked={deleteNames.includes(item.name)} onChange={(event) => setDeleteNames(event.target.checked ? [...deleteNames, item.name] : deleteNames.filter((name) => name !== item.name))} /></td></tr>)}</tbody></table></div></div>)}<div className="toolbar"><button onClick={() => void save()}>Save API Keys</button></div></section>;
}
