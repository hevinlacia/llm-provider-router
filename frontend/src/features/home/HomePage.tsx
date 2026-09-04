import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../../api';
import { BarRow, CostTable, KpiCard, TokenTable, UsageTable } from '../../components/tables';
import { costPer1k, formatMoney, formatPercent, number, providerOf, formatTokens } from '../../lib/format';
import { TokenUnitSelect, useTokenUnit } from '../../lib/tokenUnit';
import type { Bucket, FilterState, KeyConfig, StateResponse, TokenPriceConfig, UsageSnapshot } from '../../types';

export function HomePage() {
  const { unit } = useTokenUnit();
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
          <span className="pill">{periodLabel} · {formatTokens(total?.total_tokens, unit)} tokens</span>
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
        <div className="field"><label>Token Unit</label><TokenUnitSelect /></div>
        <div className="field"><label>Start</label><input type="date" value={filters.start} onChange={(event) => setFilters({ ...filters, start: event.target.value })} /></div>
        <div className="field"><label>End</label><input type="date" value={filters.end} onChange={(event) => setFilters({ ...filters, end: event.target.value })} /></div>
        <button className="secondary" onClick={() => setFilters({ period: 'month', start: '', end: '' })}>This Month</button>
        <button className="secondary" onClick={() => void loadData()}>Refresh</button>
        <button className="ghost" onClick={() => void resetUsage()}>Reset Usage</button>
      </div>
    </div>

    <section className="kpi-grid">
      <KpiCard label="Total Tokens" value={formatTokens(total?.total_tokens, unit)} sub={`${formatTokens(total?.prompt_tokens, unit)} prompt · ${formatTokens(total?.completion_tokens, unit)} output`} accent="kpi-primary" />
      <KpiCard label="Total Cost" value={formatMoney(totalCost?.total_cost)} sub={`${formatMoney(totalCost?.input_uncached_cost)} uncached · ${formatMoney(totalCost?.output_cost)} output`} />
      <KpiCard label="Avg Cost / 1k" value={total?.total_tokens ? formatMoney(avgCostPer1k) : '—'} sub={totalCost ? `input ${formatMoney((totalCost.input_uncached_cost / Math.max(1, total?.prompt_tokens ?? 1)) * 1000)} /1k` : 'no cost yet'} />
      <KpiCard label="Top Model (tokens)" value={topModelName} sub={topModelTokens ? `${formatTokens(topModelTokens, unit)} tokens · ${total?.total_tokens ? ((topModelTokens / total.total_tokens) * 100).toFixed(1) : '0'}%` : '—'} />
      <KpiCard label="Top Supplier (tokens)" value={topProviderName} sub={providerRows[0] ? `${formatTokens(providerRows[0][1].total_tokens, unit)} tokens · ${((providerRows[0][1].total_tokens / Math.max(1, total?.total_tokens ?? 1)) * 100).toFixed(1)}%` : '—'} />
      <KpiCard label="Cache Hit" value={formatPercent(total?.cache_hit_rate)} sub={`${formatTokens(total?.cached_tokens, unit)} cached · ${formatTokens((total?.prompt_tokens ?? 0) - (total?.cached_tokens ?? 0), unit)} uncached`} />
    </section>

    <section className="insights">
      <div className="insight"><span className="insight-kicker">性价比</span><strong>最省钱模型 <b>{cheapestName}</b></strong><em>{cheapest[0] ? `${formatMoney(cheapest[0].per1k)} / 1k · ${formatTokens(cheapest[0].tokens, unit)} tokens` : '样本不足（需 >2k tokens 有成本）'}</em></div>
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
        {providerRows.length > 1 && <div className="callout">头部供应商与次头部差值 <b>{formatTokens(Math.max(0, providerRows[0][1].total_tokens - (providerRows[1]?.[1].total_tokens ?? 0)), unit)} tokens</b> · 若单价相近，优先稳、缓存命中高的那家。</div>}
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
              <div className="model-foot"><span>{formatTokens(bucket.total_tokens, unit)} tokens · {bucket.requests} req</span><span className="muted">{price ? `$${price.input_uncached_per_million}/$${price.input_cached_per_million}/$${price.output_per_million} per 1M` : 'custom price'}</span></div>
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
          return <div key={day} className="trend-col" title={`${day}: ${formatTokens(bucket.total_tokens, unit)} tokens · ${formatMoney(cost)} · ${bucket.requests} req`}>
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
