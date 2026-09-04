// 用量分析：按供应商/模型看 token 使用曲线（多线 SVG，无额外图表依赖）。

import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../../api';
import { formatCompact, formatMoney, number, providerOf, formatTokens } from '../../lib/format';
import { TokenUnitSelect, useTokenUnit } from '../../lib/tokenUnit';
import type {
  Bucket,
  FilterState,
  KeyConfig,
  UsageSeriesBucket,
  UsageSeriesGroupBy,
  UsageSeriesMetric,
  UsageSeriesResponse,
} from '../../types';

type BreakdownRow = {
  key: string;
  bucketByName: Record<string, Bucket>;
  total: number;
  requests: number;
  errors: number;
};

const PALETTE = ['#22d3ee', '#a855f7', '#22c55e', '#f59e0b', '#06b6d4', '#ec4899', '#84cc16', '#f97316', '#38bdf8', '#e879f9', '#64748b', '#94a3b8'];

function colorFor(idx: number): string {
  return PALETTE[idx % PALETTE.length];
}

function metricOf(bucket: Bucket, metric: UsageSeriesMetric): number {
  switch (metric) {
    case 'prompt_tokens':
      return bucket.prompt_tokens;
    case 'completion_tokens':
      return bucket.completion_tokens;
    case 'requests':
      return bucket.requests;
    case 'errors':
      return bucket.errors;
    default:
      return bucket.total_tokens;
  }
}

function Empty() {
  return <p className="muted">No data for this range.</p>;
}

function TokenTrend({
  buckets,
  rows,
  metric,
  height = 280,
}: {
  buckets: string[];
  rows: BreakdownRow[];
  metric: UsageSeriesMetric;
  height?: number;
}) {
  const { unit } = useTokenUnit();
  const maxY = useMemo(() => {
    let m = 1;
    for (const r of rows) for (const b of buckets) m = Math.max(m, metricOf(r.bucketByName[b] ?? (r.bucketByName[buckets[0]] as Bucket | undefined) ?? emptyBucket(), metric));
    return m;
  }, [buckets, rows, metric]);

  if (!buckets.length || !rows.length) return <Empty />;

  const pad = { top: 12, right: 16, bottom: 28, left: 76 };
  const w = 1040;
  const h = height;
  const innerW = w - pad.left - pad.right;
  const innerH = h - pad.top - pad.bottom;
  const x = (i: number) => pad.left + (innerW * i) / Math.max(1, buckets.length - 1);
  const y = (v: number) => pad.top + innerH - (innerH * v) / Math.max(1, maxY);

  // grid + labels
  const yTicks = 4;

  return (
    <div className="trend-shell">
      <svg viewBox={`0 0 ${w} ${h}`} className="trend-svg" role="img" aria-label="Token usage curve">
        {/* grid */}
        {Array.from({ length: yTicks + 1 }).map((_, i) => {
          const v = Math.round((maxY * (i / yTicks)) / 1) * 1;
          const yy = y(v);
          return (
            <g key={i}>
              <line x1={pad.left} x2={w - pad.right} y1={yy} y2={yy} className="trend-grid" />
              <text x={pad.left - 8} y={yy} dy="0.35em" className="trend-y-label" textAnchor="end">
                {unit === 'raw' ? formatCompact(v) : formatTokens(v, unit)}
              </text>
            </g>
          );
        })}
        {/* x labels */}
        {buckets.map((b, i) => {
          const dense = buckets.length > 14;
          const show = dense ? i % Math.ceil(buckets.length / 9) === 0 || i === buckets.length - 1 : true;
          return show ? (
            <text key={b} x={x(i)} y={h - 6} textAnchor="middle" className="trend-x-label">
              {b.length > 10 ? b.slice(5) : b}
            </text>
          ) : null;
        })}
        {/* lines */}
        {rows.map((row, idx) => {
          const pts = buckets.map((b, i) => {
            const bucket = row.bucketByName[b];
            const v = bucket ? metricOf(bucket, metric) : 0;
            return `${x(i)},${y(v)}`;
          });
          const d = `M ${pts.join(' L ')}`;
          return (
            <g key={row.key}>
              <path d={d} fill="none" stroke={colorFor(idx)} strokeWidth={2.1} strokeLinejoin="round" strokeLinecap="round" opacity={0.95} />
              {buckets.map((b, i) => {
                const bucket = row.bucketByName[b];
                const v = bucket ? metricOf(bucket, metric) : 0;
                return <circle key={`${row.key}-${b}`} cx={x(i)} cy={y(v)} r={2.4} fill={colorFor(idx)} opacity={0.92} />;
              })}
            </g>
          );
        })}
      </svg>

      <div className="trend-legend-dots">
        {rows.map((r, i) => (
          <span key={r.key} className="legend-dot">
            <i style={{ background: colorFor(i) }} />
            {r.key}
          </span>
        ))}
      </div>
    </div>
  );
}

function emptyBucket(): Bucket {
  return { requests: 0, errors: 0, prompt_tokens: 0, cached_tokens: 0, prompt_uncached_tokens: 0, completion_tokens: 0, total_tokens: 0, cache_hit_rate: 0 };
}

export function AnalyticsPage() {
  const { unit } = useTokenUnit();
  const [filters, setFilters] = useState<FilterState>({ period: 'month', start: '', end: '' });
  const [bucket, setBucket] = useState<UsageSeriesBucket>('day');
  const [groupBy, setGroupBy] = useState<UsageSeriesGroupBy>('model');
  const [metric, setMetric] = useState<UsageSeriesMetric>('total_tokens');
  const [top, setTop] = useState(6);
  const [providers, setProviders] = useState<Record<string, string>>({});
  const [providerList, setProviderList] = useState<string[]>([]);
  const [view, setView] = useState<'all' | 'single'>('all');
  const [provider, setProvider] = useState('');
  const [data, setData] = useState<UsageSeriesResponse | null>(null);
  const [error, setError] = useState('');
  const [queryKey, setQueryKey] = useState('');

  // 单供应商模式下只允许按 Key / Model 分线（一个供应商内再按供应商分组无意义）
  const effectiveGroupBy: UsageSeriesGroupBy =
    view === 'single' && groupBy === 'provider' ? 'key' : groupBy;

  const load = useCallback(async () => {
    try {
      const keyCfg = await api.keys().catch(() => null as unknown as KeyConfig);
      if (keyCfg) {
        const m: Record<string, string> = {};
        const set = new Set<string>();
        for (const k of keyCfg.keys ?? []) {
          m[k.name] = k.provider;
          if (k.provider) set.add(k.provider);
        }
        setProviders(m);
        setProviderList(Array.from(set).sort());
      }
      // 单供应商视角下未选择供应商时暂不拉取 series，等待选择
      if (view === 'single' && !provider) {
        setData(null);
        setError('');
        return;
      }
      const series = await api.usageSeries({
        period: filters.period,
        start: filters.start || undefined,
        end: filters.end || undefined,
        bucket,
        group_by: effectiveGroupBy,
        top,
        provider: view === 'single' ? provider : undefined,
      });
      setData(series);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [filters, bucket, effectiveGroupBy, top, view, provider]);

  useEffect(() => {
    void load();
  }, [load]);

  // 展示用的 group 名：provider 分组时把 raw key 映射为 provider 名
  const rawBuckets = data?.buckets ?? [];
  const rawSeries = data?.series ?? {};

  const displayBuckets = rawBuckets;

  const rows: BreakdownRow[] = useMemo(() => {
    if (!data) return [];

    // provider 分组：把原始 key_name 折到 provider
    if (effectiveGroupBy === 'provider') {
      const byProvider: Record<string, Record<string, Bucket>> = {};
      for (const [rawKey, buckets] of Object.entries(rawSeries)) {
        const prov = providerOf(rawKey, providers);
        const cur = (byProvider[prov] ??= {});
        for (const [b, bucket] of Object.entries(buckets)) {
          const dst = cur[b] ?? (cur[b] = emptyBucket());
          dst.requests += bucket.requests;
          dst.errors += bucket.errors;
          dst.prompt_tokens += bucket.prompt_tokens;
          dst.cached_tokens += bucket.cached_tokens;
          dst.prompt_uncached_tokens = Math.max(0, dst.prompt_tokens - dst.cached_tokens);
          dst.completion_tokens += bucket.completion_tokens;
          dst.total_tokens += bucket.total_tokens;
        }
      }
      // 重新 top 聚合（provider 已折叠）
      const entries = Object.entries(byProvider).map(([k, m]) => {
        const total = Object.values(m).reduce((s, b) => s + b.total_tokens, 0);
        const requests = Object.values(m).reduce((s, b) => s + b.requests, 0);
        const errors = Object.values(m).reduce((s, b) => s + b.errors, 0);
        return { key: k, bucketByName: m, total, requests, errors } as BreakdownRow;
      });
      entries.sort((a, b) => b.total - a.total);
      // provider 也遵循 top
      if (entries.length > top) {
        const keep = entries.slice(0, top);
        const rest = entries.slice(top);
        const other: Record<string, Bucket> = {};
        for (const r of rest) for (const [b, bk] of Object.entries(r.bucketByName)) {
          const dst = other[b] ?? (other[b] = emptyBucket());
          dst.requests += bk.requests; dst.errors += bk.errors;
          dst.prompt_tokens += bk.prompt_tokens; dst.cached_tokens += bk.cached_tokens;
          dst.prompt_uncached_tokens = Math.max(0, dst.prompt_tokens - dst.cached_tokens);
          dst.completion_tokens += bk.completion_tokens; dst.total_tokens += bk.total_tokens;
        }
        const otherTotal = Object.values(other).reduce((s, b) => s + b.total_tokens, 0);
        if (otherTotal) keep.push({ key: 'other', bucketByName: other, total: otherTotal, requests: Object.values(other).reduce((s, b) => s + b.requests, 0), errors: Object.values(other).reduce((s, b) => s + b.errors, 0) });
        keep.sort((a, b) => b.total - a.total);
        return keep;
      }
      return entries;
    }

    // model / key：后端已做 top + other
    const entries: BreakdownRow[] = Object.entries(rawSeries).map(([k, m]) => {
      const total = Object.values(m).reduce((s, b) => s + b.total_tokens, 0);
      const requests = Object.values(m).reduce((s, b) => s + b.requests, 0);
      const errors = Object.values(m).reduce((s, b) => s + b.errors, 0);
      return { key: k, bucketByName: m, total, requests, errors };
    });
    entries.sort((a, b) => b.total - a.total);
    return entries;
  }, [data, rawSeries, providers, effectiveGroupBy, top]);

  const totalTokens = data?.total.total_tokens ?? 0;
  const totalCost = data?.total_cost.total_cost ?? 0;

  // 单桶 drill-down：选中某个桶看当桶的 breakdown
  const [activeBucket, setActiveBucket] = useState<string | null>(null);
  useEffect(() => {
    if (displayBuckets.length && !activeBucket) setActiveBucket(displayBuckets[displayBuckets.length - 1] ?? null);
  }, [displayBuckets, activeBucket]);

  const bucketRows = useMemo(() => {
    if (!activeBucket) return [] as Array<{ key: string; bucket: Bucket }>;
    const arr: Array<{ key: string; bucket: Bucket }> = [];
    for (const r of rows) arr.push({ key: r.key, bucket: r.bucketByName[activeBucket] ?? emptyBucket() });
    arr.sort((a, b) => b.bucket.total_tokens - a.bucket.total_tokens);
    return arr;
  }, [rows, activeBucket]);

  // URL → filters（刷新/分享可还原）
  useEffect(() => {
    const q = new URLSearchParams(window.location.search);
    const p = q.get('period');
    const s = q.get('start');
    const e = q.get('end');
    const b = q.get('bucket') as UsageSeriesBucket | null;
    const g = q.get('group_by') as UsageSeriesGroupBy | null;
    const v = q.get('view') as 'all' | 'single' | null;
    const pr = q.get('provider');
    if (p || s || e || b || g) {
      setFilters((prev) => ({ period: p ?? prev.period, start: s ?? prev.start, end: e ?? prev.end }));
      if (b === 'hour' || b === 'day' || b === 'month') setBucket(b);
      if (g === 'model' || g === 'provider' || g === 'key') setGroupBy(g);
    }
    if (v === 'single' || v === 'all') setView(v);
    if (pr) setProvider(pr);
  }, []);

  const syncUrl = useCallback(() => {
    const q = new URLSearchParams();
    q.set('period', filters.period);
    if (filters.start) q.set('start', filters.start);
    if (filters.end) q.set('end', filters.end);
    q.set('bucket', bucket);
    q.set('group_by', effectiveGroupBy);
    if (view === 'single') {
      q.set('view', 'single');
      if (provider) q.set('provider', provider);
    }
    const qs = `?${q.toString()}`;
    if (qs !== queryKey) {
      window.history.replaceState({}, '', `/analytics${qs}`);
      setQueryKey(qs);
    }
  }, [filters, bucket, effectiveGroupBy, view, provider, queryKey]);

  useEffect(() => {
    syncUrl();
  }, [syncUrl]);

  const groupLabel = effectiveGroupBy === 'provider' ? '供应商' : effectiveGroupBy === 'key' ? 'Key' : '模型';
  const heroTitle = view === 'single' ? `${provider || '供应商'} · 明细看板` : '供应商 / 模型 Token 曲线';

  return (
    <section className="page">
      <div className="hero">
        <div className="hero-grid" aria-hidden />
        <div className="hero-copy">
          <span className="eyebrow">Usage · 用量分析</span>
          <h1>{heroTitle}</h1>
          <p>
            {view === 'single' ? (
              <>只看 <b>{provider || '…'}</b> 旗下消耗：按 <b>{groupLabel}</b> 分线，逐 <b>{bucket}</b> 看 token 走势；表格可按桶 drill-down。</>
            ) : (
              <>站在所有供应商角度，按 <b>{groupLabel}</b> 分线，逐 <b>{bucket}</b> 看 token 走势；表格可按桶 drill-down。</>
            )}
          </p>
          <div className="hero-meta">
            <span className="pill">{formatTokens(totalTokens, unit)} tokens · {number.format(rows.reduce((s, r) => s + r.requests, 0))} req</span>
            <span className="pill ghost">{totalCost ? `${formatMoney(totalCost)} EST` : 'cost pending'}</span>
            <span className="pill ghost">{displayBuckets.length} buckets</span>
          </div>
        </div>
        <div className="hero-orb">
          <strong>{effectiveGroupBy === 'provider' ? `${rows.length} 供应商` : effectiveGroupBy === 'key' ? `${rows.length} keys` : `${rows.length} models`}</strong>
          <span>BREAKDOWN</span>
          <em>top {top} + other</em>
        </div>
      </div>

      {error && <div className="error" style={{ marginTop: 14 }}>{error}</div>}

      <div className="toolbar-wrap">
        <div className="toolbar">
          <div className="field">
            <label>视角</label>
            <div className="segmented">
              <button className={view === 'all' ? 'active' : ''} onClick={() => setView('all')}>所有供应商</button>
              <button className={view === 'single' ? 'active' : ''} onClick={() => { setView('single'); if (groupBy === 'provider') setGroupBy('key'); }}>单个供应商</button>
            </div>
          </div>
          {view === 'single' && (
            <div className="field">
              <label>供应商</label>
              <select value={provider} onChange={(e) => setProvider(e.target.value)}>
                <option value="">选择供应商</option>
                {providerList.map((p) => (
                  <option key={p} value={p}>{p}</option>
                ))}
              </select>
            </div>
          )}
          <div className="field">
            <label>Range</label>
            <select value={filters.period} onChange={(e) => setFilters({ ...filters, period: e.target.value })}>
              <option value="day">Last 24h</option>
              <option value="month">This Month</option>
              <option value="all">All</option>
              <option value="today">Today</option>
            </select>
          </div>
          <div className="field">
            <label>Start</label>
            <input type="date" value={filters.start} onChange={(e) => setFilters({ ...filters, start: e.target.value })} />
          </div>
          <div className="field">
            <label>End</label>
            <input type="date" value={filters.end} onChange={(e) => setFilters({ ...filters, end: e.target.value })} />
          </div>
          <div className="field">
            <label>Bucket</label>
            <select value={bucket} onChange={(e) => setBucket(e.target.value as UsageSeriesBucket)}>
              <option value="hour">Hour</option>
              <option value="day">Day</option>
              <option value="month">Month</option>
            </select>
          </div>
          <div className="field">
            <label>Group by</label>
            <select value={effectiveGroupBy} onChange={(e) => setGroupBy(e.target.value as UsageSeriesGroupBy)}>
              <option value="model">Model</option>
              {view === 'all' && <option value="provider">Supplier</option>}
              <option value="key">Key</option>
            </select>
          </div>
          <div className="field">
            <label>Metric</label>
            <select value={metric} onChange={(e) => setMetric(e.target.value as UsageSeriesMetric)}>
              <option value="total_tokens">Total tokens</option>
              <option value="prompt_tokens">Prompt</option>
              <option value="completion_tokens">Completion</option>
              <option value="requests">Requests</option>
            </select>
          </div>
          <div className="field">
            <label>Token Unit</label>
            <TokenUnitSelect />
          </div>
          <div className="field">
            <label>Top</label>
            <select value={String(top)} onChange={(e) => setTop(Number(e.target.value))}>
              <option value="4">Top 4</option>
              <option value="6">Top 6</option>
              <option value="10">Top 10</option>
              <option value="0">All</option>
            </select>
          </div>
          <button className="secondary" onClick={() => setFilters({ period: 'month', start: '', end: '' })}>
            This Month
          </button>
          <button className="secondary" onClick={() => void load()}>
            Refresh
          </button>
        </div>
      </div>

      <section className="card panel">
        <div className="section-title">
          <h2>Token 曲线 — {groupLabel}</h2>
          <span className="muted">{metric} · {displayBuckets.length} buckets</span>
        </div>
        {view === 'single' && !provider ? (
          <p className="muted">请先在上方选择供应商，查看其 Key / 模型消耗明细。</p>
        ) : rows.length ? (
          <TokenTrend buckets={displayBuckets} rows={rows} metric={metric} />
        ) : (
          <Empty />
        )}
        <div className="callout">
          小技巧：<b>所有供应商</b>视角切 <b>Group by = Supplier</b> 看哪家占用多、切 <b>Model</b> 看单模型突峰；
          <b>单个供应商</b>视角选中某家后按 <b>Key / Model</b> 拆它自己的消耗。
          支持 URL 参数：<code>?period=month&bucket=day&group_by=provider</code> 可直接分享当前视角。
        </div>
      </section>

      <section className="card panel">
        <div className="section-title">
          <h2>排行榜 · {activeBucket ? `Bucket ${activeBucket}` : '总览'}</h2>
          <div className="title-actions">
            <span className="muted">{rows.length} series</span>
            <div className="field" style={{ minWidth: 160 }}>
              <label style={{ fontSize: 10 }}>Bucket</label>
              <select value={activeBucket ?? ''} onChange={(e) => setActiveBucket(e.target.value || null)}>
                <option value="">(total)</option>
                {displayBuckets.map((b) => (
                  <option key={b} value={b}>
                    {b}
                  </option>
                ))}
              </select>
            </div>
          </div>
        </div>

        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>{effectiveGroupBy === 'provider' ? 'Supplier' : effectiveGroupBy === 'key' ? 'Key' : 'Model'}</th>
                <th>Total</th>
                <th>Prompt</th>
                <th>Cached</th>
                <th>Completion</th>
                <th>Requests</th>
                <th>Errors</th>
              </tr>
            </thead>
            <tbody>
              {(activeBucket ? bucketRows : rows.slice(0, 30).map((r) => {
                // 汇总 bucket
                const agg = emptyBucket();
                for (const b of Object.values(r.bucketByName)) {
                  agg.requests += b.requests; agg.errors += b.errors;
                  agg.prompt_tokens += b.prompt_tokens; agg.cached_tokens += b.cached_tokens;
                  agg.prompt_uncached_tokens = Math.max(0, agg.prompt_tokens - agg.cached_tokens);
                  agg.completion_tokens += b.completion_tokens; agg.total_tokens += b.total_tokens;
                }
                return { key: r.key, bucket: agg };
              })).map((item) => (
                <tr key={item.key}>
                  <td className="strong-cell">{item.key}</td>
                  <td>{formatTokens(item.bucket.total_tokens, unit)}</td>
                  <td>{formatTokens(item.bucket.prompt_tokens, unit)}</td>
                  <td>{formatTokens(item.bucket.cached_tokens, unit)}</td>
                  <td>{formatTokens(item.bucket.completion_tokens, unit)}</td>
                  <td>{number.format(item.bucket.requests)}</td>
                  <td>{number.format(item.bucket.errors)}</td>
                </tr>
              ))}
              {!rows.length && (
                <tr>
                  <td colSpan={7} className="muted">
                    No series.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </section>
    </section>
  );
}
