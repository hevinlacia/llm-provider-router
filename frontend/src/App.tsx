import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from './api';
import type { Bucket, CostBucket, CustomModelAlias, FilterState, KeyConfig, ModelAliasConfig, ModelEquivalencesConfig, ProviderConfig, ProviderModelsResponse, StateResponse, TargetCandidateGroup, TokenPriceConfig, UsageSnapshot, V2LogicalModel, V2ProviderStatus, V2Status, WeightConfig } from './types';
import './styles.css';

const number = new Intl.NumberFormat();
const money = new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD', maximumFractionDigits: 6 });

// ---- Model Pool target 候选分组（与后端 validate_targets 语义对齐）----
const TARGET_GROUP_PHYSICAL = 'Physical models (provider/upstream)';
const TARGET_GROUP_POOL = 'Model pools';
const TARGET_GROUP_VIRTUAL = 'Virtual models';

function buildTargetCandidates(config: V2Status, excludePool: string | null): TargetCandidateGroup[] {
  const registered = (config.models ?? []).map((m) => m.id);
  const seen = new Set(registered);
  const upstreamOnly: string[] = [];
  for (const [provider, p] of Object.entries(config.providers ?? {})) {
    for (const upstream of p.models ?? []) {
      const id = `${provider}/${upstream}`;
      if (!seen.has(id)) {
        seen.add(id);
        upstreamOnly.push(id);
      }
    }
  }
  upstreamOnly.sort();
  return [
    { group: TARGET_GROUP_PHYSICAL, items: [...registered, ...upstreamOnly] },
    {
      group: TARGET_GROUP_POOL,
      items: Object.keys(config.logical_models ?? {}).filter((pool) => pool !== excludePool),
    },
    { group: TARGET_GROUP_VIRTUAL, items: Object.keys(config.virtual_models ?? {}) },
  ];
}

function isKnownTarget(model: string, groups: TargetCandidateGroup[]): boolean {
  const physical = groups.find((g) => g.group === TARGET_GROUP_PHYSICAL)?.items ?? [];
  const pools = groups.find((g) => g.group === TARGET_GROUP_POOL)?.items ?? [];
  const virtuals = groups.find((g) => g.group === TARGET_GROUP_VIRTUAL)?.items ?? [];
  if (physical.includes(model) || pools.includes(model) || virtuals.includes(model)) return true;
  const slash = model.indexOf('/');
  if (slash > 0 && virtuals.includes(model.slice(slash + 1))) return true;
  return false;
}

function formatMoney(value: number | undefined): string {
  return money.format((value ?? 0) || 0);
}

function formatPercent(value: number | undefined): string {
  return `${(((value ?? 0) || 0) * 100).toFixed(1)}%`;
}

function formatCompact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 1 : 2)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(n >= 10_000 ? 0 : 1)}k`;
  return number.format(n);
}

function providerOf(keyName: string, providers: Record<string, string>): string {
  if (providers[keyName]) return providers[keyName];
  const slash = keyName.indexOf('/');
  if (slash > 0) return keyName.slice(0, slash);
  return 'unknown';
}

function costPer1k(bucket: Bucket, cost: CostBucket | undefined): number {
  const c = cost?.total_cost ?? 0;
  const t = bucket.total_tokens ?? 0;
  if (!t || !c) return 0;
  return (c / t) * 1000;
}

// ---------- small UI helpers ----------

function UsageTable({ data, tokenFirst = false }: { data?: Record<string, Bucket>; tokenFirst?: boolean }) {
  const rows = Object.entries(data ?? {}).sort((left, right) => tokenFirst ? (right[1].total_tokens ?? 0) - (left[1].total_tokens ?? 0) : left[0].localeCompare(right[0]));
  if (!rows.length) return <p className="muted">No data yet.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Name</th><th>Requests</th><th>Errors</th><th>Input Uncached</th><th>Input Cached</th><th>Cache Hit</th><th>Output</th><th>Total</th></tr></thead><tbody>{rows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{number.format(item.requests)}</td><td>{number.format(item.errors)}</td><td>{number.format(item.prompt_uncached_tokens ?? Math.max(0, item.prompt_tokens - item.cached_tokens))}</td><td>{number.format(item.cached_tokens)}</td><td>{formatPercent(item.cache_hit_rate)}</td><td>{number.format(item.completion_tokens)}</td><td>{number.format(item.total_tokens)}</td></tr>)}</tbody></table></div>;
}

function TokenTable({ data, providers }: { data?: Record<string, Bucket>; providers?: Record<string, string> }) {
  const rows = Object.entries(data ?? {}).sort((left, right) => (right[1].total_tokens ?? 0) - (left[1].total_tokens ?? 0));
  if (!rows.length) return <p className="muted">No token usage today.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Key</th><th>Input Uncached</th><th>Input Cached</th><th>Cache Hit</th><th>Output</th><th>Total Tokens</th><th>Requests</th></tr></thead><tbody>{rows.map(([name, item]) => { const provider = providers?.[name]; const slash = name.indexOf('/'); const displayName = slash > 0 ? name.slice(slash + 1) : name; const displayProvider = provider ?? (slash > 0 ? name.slice(0, slash) : undefined); return <tr key={name}><td>{displayName}{displayProvider ? <div className="muted small-text">{displayProvider}</div> : null}</td><td>{number.format(item.prompt_uncached_tokens ?? Math.max(0, item.prompt_tokens - item.cached_tokens))}</td><td>{number.format(item.cached_tokens)}</td><td>{formatPercent(item.cache_hit_rate)}</td><td>{number.format(item.completion_tokens)}</td><td>{number.format(item.total_tokens)}</td><td>{number.format(item.requests)}</td></tr>; })}</tbody></table></div>;
}

function CostTable({ data }: { data?: Record<string, CostBucket> }) {
  const rows = Object.entries(data ?? {}).sort((left, right) => (right[1].total_cost ?? 0) - (left[1].total_cost ?? 0));
  if (!rows.length) return <p className="muted">No model cost yet.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Model</th><th>Input Uncached</th><th>Input Cached</th><th>Output</th><th>Total Cost</th></tr></thead><tbody>{rows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{formatMoney(item.input_uncached_cost)}</td><td>{formatMoney(item.input_cached_cost)}</td><td>{formatMoney(item.output_cost)}</td><td>{formatMoney(item.total_cost)}</td></tr>)}</tbody></table></div>;
}

function KpiCard({ label, value, sub, accent }: { label: string; value: string; sub?: string; accent?: string }) {
  return <div className={`kpi ${accent ?? ''}`}><div className="kpi-label">{label}</div><div className="kpi-value">{value}</div>{sub ? <div className="kpi-sub">{sub}</div> : null}</div>;
}

function BarRow({ label, hint, value, max, moneyValue, percent }: { label: string; hint?: string; value: number; max: number; moneyValue?: number; percent?: string }) {
  const w = max > 0 ? Math.max(2, (value / max) * 100) : 0;
  return <div className="bar-row"><div className="bar-head"><span className="bar-label">{label}{hint ? <em>{hint}</em> : null}</span><span className="bar-metric">{moneyValue !== undefined ? formatMoney(moneyValue) : number.format(value)}{percent ? <i>{percent}</i> : null}</span></div><div className="bar-track"><div className="bar-fill" style={{ width: `${w}%` }} /></div><div className="bar-foot"><span>{formatCompact(value)} tokens</span><span>{w.toFixed(1)}%</span></div></div>;
}

function HomePage() {
  const [filters, setFilters] = useState<FilterState>({ period: 'month', start: '', end: '' });
  const [state, setState] = useState<StateResponse | null>(null);
  const [today, setToday] = useState<UsageSnapshot | null>(null);
  const [keyConfig, setKeyConfig] = useState<KeyConfig | null>(null);
  const [tokenPrices, setTokenPrices] = useState<TokenPriceConfig | null>(null);
  const [error, setError] = useState('');

  const keyProviders = useMemo(() => {
    const map: Record<string, string> = {};
    for (const key of keyConfig?.keys ?? []) map[key.name] = key.provider;
    return map;
  }, [keyConfig]);

  const priceMap = useMemo(() => {
    const m = new Map<string, TokenPriceConfig['models'][number]>();
    for (const p of tokenPrices?.models ?? []) m.set(p.model, p);
    return m;
  }, [tokenPrices]);

  const loadData = useCallback(async () => {
    try {
      const [stateData, todayData, keyData, priceData] = await Promise.all([
        api.state(filters),
        api.usage({ period: 'today' }),
        api.keys(),
        api.tokenPrices(),
      ]);
      setState(stateData);
      setToday(todayData);
      setKeyConfig(keyData);
      setTokenPrices(priceData);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [filters]);

  useEffect(() => {
    void loadData();
    const timer = window.setInterval(() => void loadData(), 8000);
    return () => window.clearInterval(timer);
  }, [loadData]);

  const usage = state?.usage;
  const total = usage?.total;
  const totalCost = usage?.total_cost;

  // Supplier aggregation from by_key
  const byProvider = useMemo(() => {
    const agg: Record<string, Bucket> = {};
    for (const [keyName, bucket] of Object.entries(usage?.by_key ?? {})) {
      const prov = providerOf(keyName, keyProviders);
      const cur = agg[prov] ?? { requests: 0, errors: 0, prompt_tokens: 0, cached_tokens: 0, prompt_uncached_tokens: 0, completion_tokens: 0, total_tokens: 0, cache_hit_rate: 0 };
      cur.requests += bucket.requests;
      cur.errors += bucket.errors;
      cur.prompt_tokens += bucket.prompt_tokens;
      cur.cached_tokens += bucket.cached_tokens;
      cur.prompt_uncached_tokens += bucket.prompt_uncached_tokens ?? Math.max(0, bucket.prompt_tokens - bucket.cached_tokens);
      cur.completion_tokens += bucket.completion_tokens;
      cur.total_tokens += bucket.total_tokens;
      agg[prov] = cur;
    }
    for (const b of Object.values(agg)) {
      b.cache_hit_rate = b.prompt_tokens > 0 ? Math.round((b.cached_tokens / b.prompt_tokens) * 10000) / 10000 : 0;
    }
    return agg;
  }, [usage?.by_key, keyProviders]);

  const providerRows = useMemo(() => Object.entries(byProvider).sort((a, b) => b[1].total_tokens - a[1].total_tokens), [byProvider]);
  const maxProviderTokens = providerRows[0]?.[1].total_tokens ?? 1;
  const maxProviderForBar = maxProviderTokens || 1;

  const modelRows = useMemo(() => Object.entries(usage?.by_model ?? {}).sort((a, b) => b[1].total_tokens - a[1].total_tokens), [usage?.by_model]);
  const maxModelTokens = modelRows[0]?.[1].total_tokens ?? 1;
  const costRows = useMemo(() => Object.entries(usage?.by_model_cost ?? {}).sort((a, b) => b[1].total_cost - a[1].total_cost), [usage?.by_model_cost]);
  const maxModelCost = costRows[0]?.[1].total_cost ?? 1;

  const dayEntries = useMemo(() => Object.entries(usage?.by_day ?? {}).sort(([a], [b]) => a.localeCompare(b)).slice(-30), [usage?.by_day]);
  const maxDayTokens = Math.max(1, ...dayEntries.map(([, b]) => b.total_tokens));

  const avgCostPer1k = useMemo(() => {
    if (!total || !totalCost) return 0;
    if (!total.total_tokens) return 0;
    return (totalCost.total_cost / total.total_tokens) * 1000;
  }, [total, totalCost]);

  const topModelName = modelRows[0]?.[0] ?? '-';
  const topModelTokens = modelRows[0]?.[1].total_tokens ?? 0;
  const topProviderName = providerRows[0]?.[0] ?? '-';

  const cheapest = useMemo(() => {
    const items = modelRows.map(([name, bucket]) => {
      const c = usage?.by_model_cost?.[name];
      return { name, per1k: costPer1k(bucket, c), tokens: bucket.total_tokens, cost: c?.total_cost ?? 0 };
    }).filter((x) => x.cost > 0 && x.tokens > 2000).sort((a, b) => a.per1k - b.per1k);
    return items;
  }, [modelRows, usage?.by_model_cost]);

  const cheapestName = cheapest[0]?.name ?? '-';
  const expensiveName = cheapest[cheapest.length - 1]?.name ?? '-';

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
  const periodLabel = filters.period === 'month' ? 'This month' : filters.period === 'today' ? 'Today' : filters.period === 'day' ? 'Last 24h' : filters.period === 'all' ? 'All time' : filters.period;

  return <section className="page">
    <div className="hero">
      <div className="hero-grid" aria-hidden />
      <div className="hero-copy">
        <span className="eyebrow">Token Cost · 决策看板</span>
        <h1>当月开销一览</h1>
        <p>按 <b>供应商 / Key / 模型</b> 拆解消耗与成本，定位用量大户，辅助下月选型与采购。</p>
        <div className="hero-meta">
          <span className="pill">{periodLabel} · {number.format(total?.total_tokens ?? 0)} tokens</span>
          <span className="pill ghost">{number.format(total?.requests ?? 0)} req · {formatPercent(total?.cache_hit_rate)} cache hit</span>
          <span className="pill ghost">{state ? `${state.bindings} bindings · uptime ${number.format(usage?.uptime_seconds ?? 0)}s` : 'loading…'}</span>
        </div>
      </div>
      <div className="hero-orb"><strong>{formatMoney(totalCost?.total_cost)}</strong><span>EST. COST</span><em>{total?.total_tokens ? `${formatMoney(avgCostPer1k)} / 1k tokens` : '—'}</em></div>
    </div>

    {error && <div className="error" style={{ marginTop: 14 }}>{error}</div>}

    <div className="toolbar-wrap">
      <div className="toolbar">
        <div className="field"><label>Range</label><select value={filters.period} onChange={(event) => setFilters({ ...filters, period: event.target.value })}><option value="month">This Month</option><option value="all">All</option><option value="today">Today</option><option value="day">Last 24h</option></select></div>
        <div className="field"><label>Start</label><input type="date" value={filters.start} onChange={(event) => setFilters({ ...filters, start: event.target.value })} /></div>
        <div className="field"><label>End</label><input type="date" value={filters.end} onChange={(event) => setFilters({ ...filters, end: event.target.value })} /></div>
        <button className="secondary" onClick={() => setFilters({ period: 'month', start: '', end: '' })}>This Month</button>
        <button className="secondary" onClick={() => void loadData()}>Refresh</button>
        <button className="ghost" onClick={() => void resetUsage()}>Reset Usage</button>
      </div>
    </div>

    <section className="kpi-grid">
      <KpiCard label="Total Tokens" value={number.format(total?.total_tokens ?? 0)} sub={`${number.format(total?.prompt_tokens ?? 0)} prompt · ${number.format(total?.completion_tokens ?? 0)} output`} accent="kpi-primary" />
      <KpiCard label="Total Cost" value={formatMoney(totalCost?.total_cost)} sub={`${formatMoney(totalCost?.input_uncached_cost)} uncached · ${formatMoney(totalCost?.output_cost)} output`} />
      <KpiCard label="Avg Cost / 1k" value={total?.total_tokens ? formatMoney(avgCostPer1k) : '—'} sub={totalCost ? `input ${formatMoney((totalCost.input_uncached_cost / Math.max(1, total?.prompt_tokens ?? 1)) * 1000)} /1k` : 'no cost yet'} />
      <KpiCard label="Top Model (tokens)" value={topModelName} sub={topModelTokens ? `${formatCompact(topModelTokens)} tokens · ${total?.total_tokens ? ((topModelTokens / total.total_tokens) * 100).toFixed(1) : '0'}%` : '—'} />
      <KpiCard label="Top Supplier (tokens)" value={topProviderName} sub={providerRows[0] ? `${formatCompact(providerRows[0][1].total_tokens)} tokens · ${((providerRows[0][1].total_tokens / Math.max(1, total?.total_tokens ?? 1)) * 100).toFixed(1)}%` : '—'} />
      <KpiCard label="Cache Hit" value={formatPercent(total?.cache_hit_rate)} sub={`${number.format(total?.cached_tokens ?? 0)} cached · ${number.format((total?.prompt_tokens ?? 0) - (total?.cached_tokens ?? 0))} uncached`} />
    </section>

    <section className="insights">
      <div className="insight"><span className="insight-kicker">性价比</span><strong>最省钱模型 <b>{cheapestName}</b></strong><em>{cheapest[0] ? `${formatMoney(cheapest[0].per1k)} / 1k · ${formatCompact(cheapest[0].tokens)} tokens` : '样本不足（需 >2k tokens 有成本）'}</em></div>
      <div className="insight warn"><span className="insight-kicker">关注</span><strong>最贵模型 <b>{expensiveName}</b></strong><em>{cheapest[cheapest.length - 1] ? `${formatMoney(cheapest[cheapest.length - 1].per1k)} / 1k · 少用或切更便宜供应商` : '—'}</em></div>
      <div className="insight"><span className="insight-kicker">供应商差异</span><strong>{providerRows.length} 个供应商在用</strong><em>{providerRows.length > 1 ? `头部 ${topProviderName} 占 ${((providerRows[0][1].total_tokens / Math.max(1, total?.total_tokens ?? 1)) * 100).toFixed(1)}%，可对比 ${providerRows[1]?.[0] ?? ''} 的单价与稳定性` : '单一供应商 · 建议引入备用以比价'}</em></div>
    </section>

    <div className="panel-grid">
      <section className="card panel">
        <div className="section-title"><h2>供应商 · Tokens</h2><span className="muted">{providerRows.length} suppliers · 当月</span></div>
        <p className="muted small">按 Key 归属聚合 · 可判断“买哪家更划算”</p>
        <div className="bar-list">
          {providerRows.length ? providerRows.map(([name, bucket]) => {
            const share = total?.total_tokens ? `${((bucket.total_tokens / total.total_tokens) * 100).toFixed(1)}%` : undefined;
            const estCost = totalCost && total?.total_tokens ? (bucket.total_tokens / total.total_tokens) * totalCost.total_cost : undefined;
            return <BarRow key={name} label={name} hint={`${number.format(bucket.requests)} req · ${formatPercent(bucket.cache_hit_rate)} hit`} value={bucket.total_tokens} max={maxProviderForBar} moneyValue={estCost} percent={share} />;
          }) : <p className="muted">No supplier data.</p>}
        </div>
        {providerRows.length > 1 && <div className="callout">头部供应商与次头部差值 <b>{formatCompact(Math.max(0, providerRows[0][1].total_tokens - (providerRows[1]?.[1].total_tokens ?? 0)))} tokens</b> · 若单价相近，优先稳、缓存命中高的那家。</div>}
      </section>

      <section className="card panel">
        <div className="section-title"><h2>模型 · Tokens &amp; Cost</h2><span className="muted">{modelRows.length} models · 当月</span></div>
        <p className="muted small">按模型看“哪个最吃 token / 最烧钱” · 下月决策优先看性价比</p>
        <div className="model-leader">
          {modelRows.slice(0, 6).map(([name, bucket]) => {
            const cost = usage?.by_model_cost?.[name];
            const per1k = costPer1k(bucket, cost);
            const price = priceMap.get(name);
            return <div key={name} className="model-row">
              <div className="model-head"><strong>{name}</strong><span>{formatMoney(cost?.total_cost)} · {per1k ? `${formatMoney(per1k)}/1k` : '—'}</span></div>
              <div className="dual-bar">
                <div className="dual-track" title={`${number.format(bucket.total_tokens)} tokens`}><div className="dual-fill tokens" style={{ width: `${Math.max(4, (bucket.total_tokens / maxModelTokens) * 100)}%` }} /></div>
                <div className="dual-track cost" title={`${formatMoney(cost?.total_cost)}`}><div className="dual-fill cost" style={{ width: `${Math.max(4, ((cost?.total_cost ?? 0) / maxModelCost) * 100)}%` }} /></div>
              </div>
              <div className="model-foot"><span>{formatCompact(bucket.total_tokens)} tokens · {bucket.requests} req</span><span className="muted">{price ? `$${price.input_uncached_per_million}/$${price.input_cached_per_million}/$${price.output_per_million} per 1M` : 'custom price'}</span></div>
            </div>;
          })}
          {!modelRows.length && <p className="muted">No model data.</p>}
        </div>
      </section>
    </div>

    <section className="card panel">
      <div className="section-title"><h2>当月每日趋势</h2><span className="muted">{dayEntries.length} days · tokens / cost</span></div>
      {dayEntries.length ? <div className="trend">
        {dayEntries.map(([day, bucket]) => {
          const h = Math.max(6, (bucket.total_tokens / maxDayTokens) * 96);
          const cost = (() => {
            const share = bucket.total_tokens / Math.max(1, total?.total_tokens ?? 1);
            return totalCost ? share * totalCost.total_cost : 0;
          })();
          return <div key={day} className="trend-col" title={`${day}: ${number.format(bucket.total_tokens)} tokens · ${formatMoney(cost)} · ${bucket.requests} req`}>
            <div className="trend-bar" style={{ height: h }} />
            <span className="trend-day">{day.slice(5)}</span>
          </div>;
        })}
      </div> : <p className="muted">No daily data.</p>}
      <div className="trend-legend"><span>柱高 = 当日 tokens</span><span>hover 查看 cost · requests</span></div>
    </section>

    <div className="panel-grid">
      <section className="card"><div className="section-title"><h2>Today by Key</h2><span className="muted">{today ? `${number.format(today.total.total_tokens)} tokens today` : ''}</span></div><TokenTable data={today?.by_key} providers={keyProviders} /></section>
      <section className="card"><div className="section-title"><h2>Cost by Model</h2><span className="muted">{totalCost ? `${formatMoney(totalCost.total_cost)} total` : ''}</span></div><CostTable data={usage?.by_model_cost} /></section>
    </div>

    <div className="panel-grid">
      <section className="card"><h2>Usage by Model</h2><UsageTable data={usage?.by_model} /></section>
      <section className="card"><h2>Usage by Key</h2><UsageTable data={usage?.by_key} tokenFirst /></section>
    </div>

    <div className="panel-grid">
      <section className="card"><h2>Daily</h2><UsageTable data={usage?.by_day} /></section>
      <section className="card"><h2>Monthly</h2><UsageTable data={usage?.by_month} /></section>
    </div>

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

function ProviderEditor({ providerName, provider, isNew = false, onCancel, onSaved, onError }: {
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

function LogicalModelEditor({ name, logical, candidates, isNew = false, onCancel, onSaved, onError }: {
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

function formatFetchedAt(ts?: number | null): string {
  if (!ts) return '';
  return new Date(ts * 1000).toLocaleString();
}

function ProviderModelsModal({ providerName, onCancel, onError }: {
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

function ProviderVirtualModelsModal({ providerName, virtualModels, onCancel, onSaved, onError }: {
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

function V2Panel({ config, onSaved, onError }: { config: V2Status | null; onSaved: (value: V2Status) => void; onError: (value: string) => void }) {
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
      <div className="table-wrap"><table className="settings-table providers-table"><thead><tr><th>Provider</th><th>Base URL</th><th>Keys</th><th>Status</th><th></th></tr></thead><tbody>{providers.map(([name, p]) => <tr key={name}><td className="strong-cell">{name}</td><td className="muted small-text url-cell">{p.base_url}</td><td>{p.key_enabled}/{p.key_total}{p.key_frozen > 0 ? ` (${p.key_frozen} frozen)` : ''}</td><td><span className={`status ${p.available ? 'ok' : 'warn'}`}>{p.available ? 'available' : 'unavailable'}</span></td><td><div className="row-actions"><button className="secondary compact-button" onClick={() => setViewingModels(name)}>Details</button><button className="secondary compact-button" onClick={() => setViewingVirtual(name)}>Virtual</button><button className="secondary compact-button" onClick={() => setEditing(name)}>Edit</button></div></td></tr>)}</tbody></table></div>
    </section>
    <section className="card settings-section logical-models-section">
      <div className="section-title settings-title"><div><h2>Model Pools</h2><p className="muted">逻辑模型池：虚拟模型名与有序/加权路由目标。target 可填虚拟模型名、物理模型 id（provider/upstream）或另一个模型池。</p></div><div className="title-actions"><span className="muted">{logical.length} model pools</span><button className="secondary compact-button" onClick={() => setAddingPool(true)}>Add Pool</button></div></div>
      <div className="table-wrap"><table className="settings-table logical-models-table"><thead><tr><th>Model Pool</th><th>Strategy</th><th>Targets</th><th></th></tr></thead><tbody>{logical.map(([name, lm]) => <tr key={name}><td className="strong-cell">{name}</td><td><span className="status">{lm.strategy}</span></td><td className="muted small-text target-cell">{lm.targets.map((t) => <span className="target-pill" key={`${name}-${t.model}-${t.weight ?? 'default'}`}>{t.model}{t.weight != null ? <span className="target-weight">w={t.weight}</span> : null}</span>)}</td><td><div className="row-actions"><button className="secondary compact-button" onClick={() => setEditingLogical(name)}>Edit</button><button className="secondary compact-button" onClick={() => { if (confirm(`Delete model pool "${name}"? References from other pools will be removed.`)) void deletePool(name); }}>Delete</button></div></td></tr>)}</tbody></table></div>
    </section>
    {adding && <ProviderEditor isNew providerName="" provider={{ base_url: '', key_total: 0, key_enabled: 0, key_frozen: 0, available: false, keys: {} }} onCancel={() => setAdding(false)} onSaved={(next) => { setAdding(false); onSaved(next); }} onError={onError} />}
    {editingProvider && <ProviderEditor providerName={editing!} provider={editingProvider} onCancel={() => setEditing(null)} onSaved={(next) => { setEditing(null); onSaved(next); }} onError={onError} />}
    {editingLogical && config.logical_models?.[editingLogical] && <LogicalModelEditor name={editingLogical} logical={config.logical_models[editingLogical]} candidates={buildTargetCandidates(config, editingLogical)} onCancel={() => setEditingLogical(null)} onSaved={(next) => { setEditingLogical(null); onSaved(next); }} onError={onError} />}
    {addingPool && <LogicalModelEditor isNew name="" logical={{ strategy: 'priority', targets: [], params: {} }} candidates={buildTargetCandidates(config, null)} onCancel={() => setAddingPool(false)} onSaved={(next) => { setAddingPool(false); onSaved(next); }} onError={onError} />}
    {viewingModels && <ProviderModelsModal providerName={viewingModels} onCancel={() => setViewingModels(null)} onError={onError} />}
    {viewingVirtual && <ProviderVirtualModelsModal providerName={viewingVirtual} virtualModels={config.virtual_models ?? {}} onCancel={() => setViewingVirtual(null)} onSaved={onSaved} onError={onError} />}
  </>;
}

function SettingsPage() {
  const [tokenPrices, setTokenPrices] = useState<TokenPriceConfig | null>(null);
  const [v2, setV2] = useState<V2Status | null>(null);
  const [equivalences, setEquivalences] = useState<ModelEquivalencesConfig | null>(null);
  const [status, setStatus] = useState('');
  const [error, setError] = useState('');

  const loadSettings = useCallback(async () => {
    try {
      const [tokenPriceData, v2Data, equivData] = await Promise.all([api.tokenPrices(), api.v2Status(), api.equivalences()]);
      setTokenPrices(tokenPriceData);
      setV2(v2Data);
      setEquivalences(equivData);
      setStatus('Settings loaded.');
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  useEffect(() => { void loadSettings(); }, [loadSettings]);

  return <section className="page active"><header><div><h1>Settings</h1><div className="muted">Manage providers, keys, token prices, and logical model routing.</div>{status && <div className="ok">{status}</div>}{error && <div className="error">{error}</div>}</div><button className="secondary" onClick={() => void loadSettings()}>Refresh</button></header>
    <V2Panel config={v2} onSaved={setV2} onError={setError} />
    <TokenPricesPanel config={tokenPrices} v2={v2} equivalences={equivalences} onChange={setTokenPrices} onSaved={(next) => { setTokenPrices(next); setStatus('Token prices saved.'); }} onError={setError} />
    <ModelEquivalencesPanel config={equivalences} onSaved={(next) => { setEquivalences(next); setStatus('Equivalences saved.'); }} onError={setError} />
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

function TokenPricesPanel({ config, v2, equivalences, onChange, onSaved, onError }: { config: TokenPriceConfig | null; v2: V2Status | null; equivalences: ModelEquivalencesConfig | null; onChange: (value: TokenPriceConfig) => void; onSaved: (value: TokenPriceConfig) => void; onError: (value: string) => void }) {
  const providers = useMemo(() => Object.keys(v2?.providers ?? {}).sort(), [v2]);
  const [selectedProvider, setSelectedProvider] = useState<string>('__all__');
  const [pendingEquiv, setPendingEquiv] = useState<string | null>(null);

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

  const filtered = useMemo(() => {
    if (!config) return [];
    const items = config.models;
    if (selectedProvider === '__all__') return [...items].sort((a, b) => a.model.localeCompare(b.model));
    return [...items].filter((item) => item.model.startsWith(`${selectedProvider}/`)).sort((a, b) => a.model.localeCompare(b.model));
  }, [config, selectedProvider]);

  const indexByModel = useMemo(() => {
    const map = new Map<string, number>();
    (config?.models ?? []).forEach((item, idx) => map.set(item.model, idx));
    return map;
  }, [config]);

  if (!config) return <section className="card"><h2>Token Prices</h2><p className="muted">Loading token prices...</p></section>;
  const current = config;

  function update(model: string, patch: Partial<TokenPriceConfig['models'][number]>) {
    const idx = indexByModel.get(model);
    if (idx === undefined) return;
    const models = [...current.models];
    models[idx] = { ...models[idx], ...patch };
    onChange({ ...current, models });
  }

  async function save() {
    try { onSaved(await api.saveTokenPrices(current.models)); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }

  async function applyEquivalent(model: string) {
    setPendingEquiv(model);
    try {
      const res = await api.applyPriceToEquivalents(model, false);
      onSaved(res.token_prices);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setPendingEquiv(null);
    }
  }

  const counts = useMemo(() => {
    const map: Record<string, number> = {};
    for (const p of providers) map[p] = (config?.models ?? []).filter((m) => m.model.startsWith(`${p}/`)).length;
    return map;
  }, [config, providers]);

  return <section className="card"><div className="section-title"><h2>Token Prices</h2><span className="muted">{current.config_path}</span></div><p className="muted">仅展示模型池引用的供应商真实模型（<code>provider/model</code>）。右侧切换供应商，下方列出该供应商的模型；每行可一键把价格同步给等价关系表中同组的其它供应商模型。</p>
    <div className="toolbar" style={{ justifyContent: 'space-between', alignItems: 'flex-end' }}>
      <div className="field"><label>Supplier</label><select value={selectedProvider} onChange={(e) => setSelectedProvider(e.target.value)}><option value="__all__">All suppliers ({config.models.length})</option>{providers.map((p) => <option key={p} value={p}>{p} ({counts[p] ?? 0})</option>)}</select></div>
      <button onClick={() => void save()}>Save Token Prices</button>
    </div>
    <div className="table-wrap"><table><thead><tr><th>Model</th><th>Equiv. Group</th><th>Input Uncached / 1M</th><th>Input Cached / 1M</th><th>Output / 1M</th><th></th></tr></thead><tbody>{filtered.map((item) => {
      const group = modelToGroup.get(item.model);
      return <tr key={item.model}><td className="strong-cell">{item.model}</td><td><span className="status">{group ?? '—'}</span></td><td><input className="price-input" type="number" min="0" step="0.000001" value={item.input_uncached_per_million} onChange={(e) => update(item.model, { input_uncached_per_million: Number(e.target.value) || 0 })} /></td><td><input className="price-input" type="number" min="0" step="0.000001" value={item.input_cached_per_million} onChange={(e) => update(item.model, { input_cached_per_million: Number(e.target.value) || 0 })} /></td><td><input className="price-input" type="number" min="0" step="0.000001" value={item.output_per_million} onChange={(e) => update(item.model, { output_per_million: Number(e.target.value) || 0 })} /></td><td><button className="secondary compact-button" disabled={!group || pendingEquiv === item.model} onClick={() => void applyEquivalent(item.model)} title={group ? `Apply this price to all models in group \`${group}\`` : 'Not in any equivalence group'}>{pendingEquiv === item.model ? 'Applying…' : 'Apply to equivalents'}</button></td></tr>;
    })}{!filtered.length && <tr><td colSpan={6} className="muted">{selectedProvider === '__all__' ? 'No real models found (check logical-models / models.json).' : `No real models for supplier \`${selectedProvider}\`.`}</td></tr>}</tbody></table></div></section>;
}

function ModelEquivalencesPanel({ config, onSaved, onError }: { config: ModelEquivalencesConfig | null; onSaved: (value: ModelEquivalencesConfig) => void; onError: (value: string) => void }) {
  const [draft, setDraft] = useState<ModelEquivalencesConfig>(config ?? { ok: true, groups: [], config_path: '' });
  useEffect(() => { if (config) setDraft(config); }, [config]);
  if (!config) return <section className="card"><h2>Model Equivalences</h2><p className="muted">Loading equivalences...</p></section>;
  function updateGroup(idx: number, patch: Partial<ModelEquivalencesConfig['groups'][number]>) {
    const groups = [...draft.groups];
    groups[idx] = { ...groups[idx], ...patch };
    setDraft({ ...draft, groups });
  }
  function addGroup() {
    setDraft({ ...draft, groups: [...draft.groups, { id: '', display_name: '', models: [] }] });
  }
  function removeGroup(idx: number) {
    setDraft({ ...draft, groups: draft.groups.filter((_, i) => i !== idx) });
  }
  async function save() {
    const groups = draft.groups.map((g) => ({ ...g, id: g.id.trim(), display_name: g.display_name.trim(), models: g.models.map((m) => m.trim()).filter(Boolean) }));
    if (groups.some((g) => !g.id || !g.display_name)) { onError('Each group needs id and display_name'); return; }
    const ids = groups.map((g) => g.id);
    if (new Set(ids).size !== ids.length) { onError('Duplicate group id'); return; }
    try { onSaved(await api.saveEquivalences(groups)); } catch (err) { onError(err instanceof Error ? err.message : String(err)); }
  }
  return <section className="card"><div className="section-title"><h2>Model Equivalences</h2><span className="muted">{draft.config_path}</span></div><p className="muted">等价关系表：同一组的多个 <code>provider/model</code> 视为同一模型，Token Prices 可一键把价格同步给组内其它供应商版本。后续你可在设置页直接增删改。</p>
    {draft.groups.map((g, idx) => <div key={idx} className="route-row" style={{ marginBottom: 12 }}><div style={{ display: 'grid', gridTemplateColumns: '180px 1fr auto', gap: 12, alignItems: 'end' }}><div className="field"><label>Group ID</label><input value={g.id} onChange={(e) => updateGroup(idx, { id: e.target.value })} placeholder="e.g. deepseek-v4-flash" /></div><div className="field"><label>Display Name</label><input value={g.display_name} onChange={(e) => updateGroup(idx, { display_name: e.target.value })} placeholder="e.g. DeepSeek V4 Flash" /></div><button className="secondary compact-button" onClick={() => removeGroup(idx)}>Delete Group</button></div><div className="field" style={{ marginTop: 10 }}><label>Models (provider/model, comma or newline separated)</label><textarea rows={2} style={{ width: '100%', padding: '8px 10px', borderRadius: 10, border: '1px solid #334155', background: '#111827', color: '#e5e7eb' }} value={g.models.join(', ')} onChange={(e) => updateGroup(idx, { models: e.target.value.split(/[\n,]+/).map((s) => s.trim()).filter(Boolean) })} placeholder="ark/deepseek-v4-flash-260801, deepseek-official/deepseek-v4-flash" /></div></div>)}
    <div className="toolbar"><button className="secondary" onClick={addGroup}>Add Group</button><button onClick={() => void save()}>Save Equivalences</button></div></section>;
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

function pathToPage(pathname: string): 'home' | 'settings' {
  if (pathname === '/settings' || pathname.startsWith('/settings/')) return 'settings';
  return 'home';
}

export default function App() {
  const [page, setPage] = useState<'home' | 'settings'>(() => pathToPage(window.location.pathname));
  const navigate = useCallback((next: 'home' | 'settings') => {
    const path = next === 'settings' ? '/settings' : '/';
    window.history.pushState({}, '', path);
    setPage(next);
  }, []);
  useEffect(() => {
    const onPop = () => setPage(pathToPage(window.location.pathname));
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  return <div className="shell"><aside><div className="brand">LLM Provider Router</div><nav><button className={`nav-button ${page === 'home' ? 'active' : ''}`} onClick={() => navigate('home')}>Dashboard</button><button className={`nav-button ${page === 'settings' ? 'active' : ''}`} onClick={() => navigate('settings')}>Settings</button></nav><div className="side-card"><span>Cost Board</span><strong>当月决策看板</strong><em>by supplier · key · model</em></div></aside><main>{page === 'home' ? <HomePage /> : <SettingsPage />}</main></div>;
}
