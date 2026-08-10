import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from './api';
import type { Bucket, CostBucket, CustomModelAlias, FilterState, KeyConfig, ModelAliasConfig, ProviderConfig, StateResponse, TokenPriceConfig, UsageSnapshot, V2LogicalModel, V2ProviderStatus, V2Status, WeightConfig } from './types';
import './styles.css';

const number = new Intl.NumberFormat();
const money = new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD', maximumFractionDigits: 6 });

function formatMoney(value: number | undefined): string {
  return money.format((value ?? 0) || 0);
}

function formatPercent(value: number | undefined): string {
  return `${(((value ?? 0) || 0) * 100).toFixed(1)}%`;
}

function card(label: string, value: string | number) {
  const display = typeof value === 'number' ? number.format(value) : value;
  return <div className="card"><div className="label">{label}</div><div className="value">{display}</div></div>;
}

function UsageTable({ data, tokenFirst = false }: { data?: Record<string, Bucket>; tokenFirst?: boolean }) {
  const rows = Object.entries(data ?? {}).sort((left, right) => tokenFirst ? (right[1].total_tokens ?? 0) - (left[1].total_tokens ?? 0) : left[0].localeCompare(right[0]));
  if (!rows.length) return <p className="muted">No data yet.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Name</th><th>Requests</th><th>Errors</th><th>Input Uncached</th><th>Input Cached</th><th>Cache Hit</th><th>Output</th><th>Total</th></tr></thead><tbody>{rows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{number.format(item.requests)}</td><td>{number.format(item.errors)}</td><td>{number.format(item.prompt_uncached_tokens ?? Math.max(0, item.prompt_tokens - item.cached_tokens))}</td><td>{number.format(item.cached_tokens)}</td><td>{formatPercent(item.cache_hit_rate)}</td><td>{number.format(item.completion_tokens)}</td><td>{number.format(item.total_tokens)}</td></tr>)}</tbody></table></div>;
}

function TokenTable({ data }: { data?: Record<string, Bucket> }) {
  const rows = Object.entries(data ?? {}).sort((left, right) => (right[1].total_tokens ?? 0) - (left[1].total_tokens ?? 0));
  if (!rows.length) return <p className="muted">No token usage today.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Key</th><th>Input Uncached</th><th>Input Cached</th><th>Cache Hit</th><th>Output</th><th>Total Tokens</th><th>Requests</th></tr></thead><tbody>{rows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{number.format(item.prompt_uncached_tokens ?? Math.max(0, item.prompt_tokens - item.cached_tokens))}</td><td>{number.format(item.cached_tokens)}</td><td>{formatPercent(item.cache_hit_rate)}</td><td>{number.format(item.completion_tokens)}</td><td>{number.format(item.total_tokens)}</td><td>{number.format(item.requests)}</td></tr>)}</tbody></table></div>;
}


function CostTable({ data }: { data?: Record<string, CostBucket> }) {
  const rows = Object.entries(data ?? {}).sort((left, right) => (right[1].total_cost ?? 0) - (left[1].total_cost ?? 0));
  if (!rows.length) return <p className="muted">No model cost yet.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Model</th><th>Input Uncached</th><th>Input Cached</th><th>Output</th><th>Total Cost</th></tr></thead><tbody>{rows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{formatMoney(item.input_uncached_cost)}</td><td>{formatMoney(item.input_cached_cost)}</td><td>{formatMoney(item.output_cost)}</td><td>{formatMoney(item.total_cost)}</td></tr>)}</tbody></table></div>;
}

function HomePage() {
  const [filters, setFilters] = useState<FilterState>({ period: 'all', start: '', end: '' });
  const [state, setState] = useState<StateResponse | null>(null);
  const [today, setToday] = useState<UsageSnapshot | null>(null);
  const [error, setError] = useState('');

  const loadData = useCallback(async () => {
    try {
      const [stateData, todayData] = await Promise.all([api.state(filters), api.usage({ period: 'today' })]);
      setState(stateData);
      setToday(todayData);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [filters]);

  useEffect(() => {
    void loadData();
    const timer = window.setInterval(() => void loadData(), 5000);
    return () => window.clearInterval(timer);
  }, [loadData]);

  const usage = state?.usage;
  const total = usage?.total;

  async function resetUsage() {
    if (!confirm('Reset all recorded usage metrics?')) return;
    await api.resetUsage();
    await loadData();
  }

  async function clearFrozen() {
    await api.clearFrozen();
    await loadData();
  }

  const frozenRows = Object.entries(state?.frozen ?? {});

  return <section className="page active"><header><div><h1>Dashboard</h1><div className="muted">{usage ? `Uptime ${number.format(usage.uptime_seconds)}s · ${state?.bindings ?? 0} active bindings · period ${usage.range.period}` : 'Loading usage metrics...'}</div>{error && <div className="error">{error}</div>}</div><div className="header-actions"><button className="secondary" onClick={() => void loadData()}>Refresh</button><button onClick={() => void resetUsage()}>Reset Usage</button></div></header>
    <section className="toolbar"><div className="field"><label>Range</label><select value={filters.period} onChange={(event) => setFilters({ ...filters, period: event.target.value })}><option value="all">All</option><option value="today">Today</option><option value="day">Last 24h</option><option value="month">This Month</option></select></div><div className="field"><label>Start</label><input type="date" value={filters.start} onChange={(event) => setFilters({ ...filters, start: event.target.value })} /></div><div className="field"><label>End</label><input type="date" value={filters.end} onChange={(event) => setFilters({ ...filters, end: event.target.value })} /></div><button className="secondary" onClick={() => setFilters({ period: 'all', start: '', end: '' })}>Clear Range</button></section>
    <section className="grid">{card('Requests', total?.requests ?? 0)}{card('Errors', total?.errors ?? 0)}{card('Input Uncached', total?.prompt_uncached_tokens ?? Math.max(0, (total?.prompt_tokens ?? 0) - (total?.cached_tokens ?? 0)))}{card('Input Cached', total?.cached_tokens ?? 0)}{card('Cache Hit', formatPercent(total?.cache_hit_rate))}{card('Output Tokens', total?.completion_tokens ?? 0)}{card('Total Tokens', total?.total_tokens ?? 0)}{card('Total Cost', formatMoney(usage?.total_cost?.total_cost))}</section>
    <section className="card"><div className="section-title"><h2>Today by Key</h2><span className="muted">{today ? `${number.format(today.total.total_tokens)} total tokens today` : ''}</span></div><TokenTable data={today?.by_key} /></section>
    <section className="card"><h2>Daily Requests</h2><UsageTable data={usage?.by_day} /></section>
    <section className="card"><h2>Monthly Requests</h2><UsageTable data={usage?.by_month} /></section>
    <section className="card"><div className="section-title"><h2>Cost by Model</h2><span className="muted">{usage?.total_cost ? `${formatMoney(usage.total_cost.total_cost)} total` : ''}</span></div><CostTable data={usage?.by_model_cost} /></section>
    <section className="card"><h2>Usage by Model</h2><UsageTable data={usage?.by_model} /></section>
    <section className="card"><h2>Usage by Key</h2><UsageTable data={usage?.by_key} tokenFirst /></section>
    <section className="card"><h2>Usage by Status</h2><UsageTable data={usage?.by_status} /></section>
    <section className="card"><div className="section-title"><h2>Frozen Keys</h2><button className="secondary" onClick={() => void clearFrozen()}>Clear Frozen Keys</button></div>{frozenRows.length ? <table><thead><tr><th>Key</th><th>Remaining</th><th>Reason</th></tr></thead><tbody>{frozenRows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{number.format(item.seconds_remaining)}s</td><td>{item.reason}</td></tr>)}</tbody></table> : <p className="muted">No frozen keys.</p>}</section>
  </section>;
}

type KeyDraft = { name: string; env_var: string; weight: number; billing_type: string; enabled: boolean };

type ProviderDraft = {
  name: string;
  base_url: string;
  keys: Record<string, { env_var: string; weight: number; billing_type: string; enabled: boolean }>;
};

function ProviderEditor({ providerName, provider, onCancel, onSaved, onError }: {
  providerName: string;
  provider: V2ProviderStatus;
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
      onSaved(await api.updateV2Provider(providerName, { name: name.trim(), base_url: baseUrl.trim(), keys: keyMap }));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }
  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <h3>Edit Provider: {providerName}</h3>
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

function LogicalModelEditor({ name, logical, candidates, onCancel, onSaved, onError }: {
  name: string;
  logical: V2LogicalModel;
  candidates: string[];
  onCancel: () => void;
  onSaved: (value: V2Status) => void;
  onError: (value: string) => void;
}) {
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
    const parsed = cleaned.map((t) => ({ model: t.model.trim(), weight: t.weight.trim() === '' ? null : Math.max(0, Number(t.weight) || 0) }));
    try {
      onSaved(await api.updateV2LogicalModel(name, { strategy, targets: parsed }));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }
  const datalistId = `lm-targets-${name}`;
  return <div className="modal-overlay" onClick={onCancel}><div className="modal" onClick={(event) => event.stopPropagation()}>
    <h3>Edit Logical Model: {name}</h3>
    <div className="field"><label>Strategy</label>
      <select value={strategy} onChange={(event) => setStrategy(event.target.value)}>
        <option value="priority">priority</option>
        <option value="weighted">weighted</option>
        <option value="usage-aware">usage-aware</option>
      </select>
    </div>
    <h4>Targets (physical models or logical models)</h4>
    <datalist id={datalistId}>{candidates.map((candidate) => <option key={candidate} value={candidate} />)}</datalist>
    <div className="table-wrap"><table><thead><tr><th>Target</th><th>Weight</th><th></th></tr></thead><tbody>
      {targets.map((t, i) => <tr key={i}><td><input list={datalistId} value={t.model} placeholder="ark/deepseek-v4-flash-260801 or another logical model" onChange={(event) => updateTarget(i, { model: event.target.value })} /></td><td><input className="weight-input" type="number" min="0" value={t.weight} placeholder="optional" onChange={(event) => updateTarget(i, { weight: event.target.value })} /></td><td><button className="secondary" onClick={() => removeTarget(i)}>Delete</button></td></tr>)}
    </tbody></table></div>
    <button className="secondary" onClick={addTarget}>Add Target</button>
    <div className="toolbar"><button className="secondary" onClick={onCancel}>Cancel</button><button onClick={() => void save()}>Save</button></div>
  </div></div>;
}

function V2Panel({ config, onSaved, onError }: { config: V2Status | null; onSaved: (value: V2Status) => void; onError: (value: string) => void }) {
  const [editing, setEditing] = useState<string | null>(null);
  const [editingLogical, setEditingLogical] = useState<string | null>(null);
  if (!config) return <section className="card"><h2>Providers & Logical Models</h2><p className="muted">Loading routing settings...</p></section>;
  if (!config.v2_enabled) return <section className="card"><div className="section-title"><h2>Providers & Logical Models</h2><span className="muted">disabled</span></div><p className="muted">Layered routing is disabled (set LLM_PROVIDER_ROUTER_V2=1 to enable).</p></section>;
  const providers = Object.entries(config.providers ?? {}).sort(([a], [b]) => a.localeCompare(b));
  const logical = Object.entries(config.logical_models ?? {}).sort(([a], [b]) => a.localeCompare(b));
  const editingProvider = editing ? (config.providers?.[editing] ?? null) : null;
  return <>
    <section className="card settings-section providers-section">
      <div className="section-title settings-title"><div><h2>Providers</h2><p className="muted">Upstream provider endpoints, key availability, and provider health.</p></div><span className="muted">{providers.length} providers · {config.models?.length ?? 0} physical models</span></div>
      <div className="table-wrap"><table className="settings-table providers-table"><thead><tr><th>Provider</th><th>Base URL</th><th>Keys</th><th>Status</th><th></th></tr></thead><tbody>{providers.map(([name, p]) => <tr key={name}><td className="strong-cell">{name}</td><td className="muted small-text url-cell">{p.base_url}</td><td>{p.key_enabled}/{p.key_total}{p.key_frozen > 0 ? ` (${p.key_frozen} frozen)` : ''}</td><td><span className={`status ${p.available ? 'ok' : 'warn'}`}>{p.available ? 'available' : 'unavailable'}</span></td><td><button className="secondary compact-button" onClick={() => setEditing(name)}>Edit</button></td></tr>)}</tbody></table></div>
    </section>
    <section className="card settings-section logical-models-section">
      <div className="section-title settings-title"><div><h2>Logical Models</h2><p className="muted">Virtual model names and ordered target routing.</p></div><span className="muted">{logical.length} logical models</span></div>
      <div className="table-wrap"><table className="settings-table logical-models-table"><thead><tr><th>Logical Model</th><th>Strategy</th><th>Targets</th><th></th></tr></thead><tbody>{logical.map(([name, lm]) => <tr key={name}><td className="strong-cell">{name}</td><td><span className="status">{lm.strategy}</span></td><td className="muted small-text target-cell">{lm.targets.map((t) => <span className="target-pill" key={`${name}-${t.model}-${t.weight ?? 'default'}`}>{t.model}{t.weight != null ? <span className="target-weight">w={t.weight}</span> : null}</span>)}</td><td><button className="secondary compact-button" onClick={() => setEditingLogical(name)}>Edit</button></td></tr>)}</tbody></table></div>
    </section>
    {editingProvider && <ProviderEditor providerName={editing!} provider={editingProvider} onCancel={() => setEditing(null)} onSaved={(next) => { setEditing(null); onSaved(next); }} onError={onError} />}
    {editingLogical && config.logical_models?.[editingLogical] && <LogicalModelEditor name={editingLogical} logical={config.logical_models[editingLogical]} candidates={[(config.models ?? []).map((m) => m.id), Object.keys(config.logical_models ?? {}).filter((x) => x !== editingLogical)].flat()} onCancel={() => setEditingLogical(null)} onSaved={(next) => { setEditingLogical(null); onSaved(next); }} onError={onError} />}
  </>;
}

function SettingsPage() {
  const [tokenPrices, setTokenPrices] = useState<TokenPriceConfig | null>(null);
  const [v2, setV2] = useState<V2Status | null>(null);
  const [status, setStatus] = useState('');
  const [error, setError] = useState('');

  const loadSettings = useCallback(async () => {
    try {
      const [tokenPriceData, v2Data] = await Promise.all([api.tokenPrices(), api.v2Status()]);
      setTokenPrices(tokenPriceData);
      setV2(v2Data);
      setStatus('Settings loaded.');
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  useEffect(() => { void loadSettings(); }, [loadSettings]);

  return <section className="page active"><header><div><h1>Settings</h1><div className="muted">Manage providers, keys, token prices, and logical model routing.</div>{status && <div className="ok">{status}</div>}{error && <div className="error">{error}</div>}</div><button className="secondary" onClick={() => void loadSettings()}>Refresh</button></header>
    <V2Panel config={v2} onSaved={setV2} onError={setError} />
    <TokenPricesPanel config={tokenPrices} onChange={setTokenPrices} onSaved={(next) => { setTokenPrices(next); setStatus('Token prices saved.'); }} onError={setError} />
  </section>;
}

function ModelAliasesPanel({ config, onChange, onSaved, onError }: { config: ModelAliasConfig | null; onChange: (value: ModelAliasConfig) => void; onSaved: (value: ModelAliasConfig) => void; onError: (value: string) => void }) {
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

function ProvidersPanel({ config, onChange, onSaved, onError }: { config: ProviderConfig | null; onChange: (value: ProviderConfig) => void; onSaved: (value: ProviderConfig) => void; onError: (value: string) => void }) {
  if (!config) return <section className="card"><h2>Provider URLs</h2><p className="muted">Loading providers...</p></section>;
  const current = config;
  async function save() {
    try { onSaved(await api.saveProviders(Object.fromEntries(current.providers.map((item) => [item.name, item.base_url])))); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }
  return <section className="card"><div className="section-title"><h2>Provider URLs</h2><span className="muted">{current.config_path}</span></div><table><thead><tr><th>Provider</th><th>Base URL</th><th>Default</th></tr></thead><tbody>{current.providers.map((item, index) => <tr key={item.name}><td>{item.name}</td><td><input value={item.base_url} onChange={(event) => { const providers = [...current.providers]; providers[index] = { ...item, base_url: event.target.value }; onChange({ ...current, providers }); }} /></td><td>{item.default_base_url}</td></tr>)}</tbody></table><div className="toolbar"><button onClick={() => void save()}>Save Providers</button></div></section>;
}

function WeightsPanel({ config, onChange, onSaved, onError }: { config: WeightConfig | null; onChange: (value: WeightConfig) => void; onSaved: (value: WeightConfig) => void; onError: (value: string) => void }) {
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
      // Fallback for backends that do not expose per-alias keys yet:
      // only show keys that belong to a visible (auto) pool, never keys of
      // real upstream models (single provider/key, e.g. oai-hevin).
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

function TokenPricesPanel({ config, onChange, onSaved, onError }: { config: TokenPriceConfig | null; onChange: (value: TokenPriceConfig) => void; onSaved: (value: TokenPriceConfig) => void; onError: (value: string) => void }) {
  if (!config) return <section className="card"><h2>Token Prices</h2><p className="muted">Loading token prices...</p></section>;
  const current = config;
  async function save() {
    try { onSaved(await api.saveTokenPrices(current.models)); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }
  function update(index: number, patch: Partial<TokenPriceConfig['models'][number]>) {
    const models = [...current.models];
    models[index] = { ...models[index], ...patch };
    onChange({ ...current, models });
  }
  return <section className="card"><div className="section-title"><h2>Token Prices</h2><span className="muted">{current.config_path}</span></div><p className="muted">Prices are USD per 1M tokens. Input is split into uncached and cache-hit tokens.</p><div className="table-wrap"><table><thead><tr><th>Model</th><th>Input Uncached / 1M</th><th>Input Cached / 1M</th><th>Output / 1M</th></tr></thead><tbody>{current.models.map((item, index) => <tr key={item.model}><td>{item.model}</td><td><input className="price-input" type="number" min="0" step="0.000001" value={item.input_uncached_per_million} onChange={(event) => update(index, { input_uncached_per_million: Number(event.target.value) || 0 })} /></td><td><input className="price-input" type="number" min="0" step="0.000001" value={item.input_cached_per_million} onChange={(event) => update(index, { input_cached_per_million: Number(event.target.value) || 0 })} /></td><td><input className="price-input" type="number" min="0" step="0.000001" value={item.output_per_million} onChange={(event) => update(index, { output_per_million: Number(event.target.value) || 0 })} /></td></tr>)}</tbody></table></div><div className="toolbar"><button onClick={() => void save()}>Save Token Prices</button></div></section>;
}

function KeysPanel({ config, onSaved, onError }: { config: KeyConfig | null; onSaved: (value: KeyConfig) => void; onError: (value: string) => void }) {
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

export default function App() {
  const [page, setPage] = useState<'home' | 'settings'>('home');
  return <div className="shell"><aside><div className="brand">LLM Provider Router</div><nav><button className={`nav-button ${page === 'home' ? 'active' : ''}`} onClick={() => setPage('home')}>Home</button><button className={`nav-button ${page === 'settings' ? 'active' : ''}`} onClick={() => setPage('settings')}>Settings</button></nav></aside><main>{page === 'home' ? <HomePage /> : <SettingsPage />}</main></div>;
}
