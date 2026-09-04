// 用量 / 成本表格组件与 KPI 卡片（跨页面复用的小 UI）。

import type { Bucket, CostBucket } from '../types';
import { formatMoney, formatPercent, number, formatTokens } from '../lib/format';
import { useTokenUnit } from '../lib/tokenUnit';

export function UsageTable({ data, tokenFirst = false }: { data?: Record<string, Bucket>; tokenFirst?: boolean }) {
  const { unit } = useTokenUnit();
  const rows = Object.entries(data ?? {}).sort((left, right) => tokenFirst ? (right[1].total_tokens ?? 0) - (left[1].total_tokens ?? 0) : left[0].localeCompare(right[0]));
  if (!rows.length) return <p className="muted">No data yet.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Name</th><th>Requests</th><th>Errors</th><th>Input Uncached</th><th>Input Cached</th><th>Cache Hit</th><th>Output</th><th>Total</th></tr></thead><tbody>{rows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{number.format(item.requests)}</td><td>{number.format(item.errors)}</td><td>{formatTokens(item.prompt_uncached_tokens ?? Math.max(0, item.prompt_tokens - item.cached_tokens), unit)}</td><td>{formatTokens(item.cached_tokens, unit)}</td><td>{formatPercent(item.cache_hit_rate)}</td><td>{formatTokens(item.completion_tokens, unit)}</td><td>{formatTokens(item.total_tokens, unit)}</td></tr>)}</tbody></table></div>;
}

export function TokenTable({ data, providers }: { data?: Record<string, Bucket>; providers?: Record<string, string> }) {
  const { unit } = useTokenUnit();
  const rows = Object.entries(data ?? {}).sort((left, right) => (right[1].total_tokens ?? 0) - (left[1].total_tokens ?? 0));
  if (!rows.length) return <p className="muted">No token usage today.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Key</th><th>Input Uncached</th><th>Input Cached</th><th>Cache Hit</th><th>Output</th><th>Total Tokens</th><th>Requests</th></tr></thead><tbody>{rows.map(([name, item]) => { const provider = providers?.[name]; const slash = name.indexOf('/'); const displayName = slash > 0 ? name.slice(slash + 1) : name; const displayProvider = provider ?? (slash > 0 ? name.slice(0, slash) : undefined); return <tr key={name}><td>{displayName}{displayProvider ? <div className="muted small-text">{displayProvider}</div> : null}</td><td>{formatTokens(item.prompt_uncached_tokens ?? Math.max(0, item.prompt_tokens - item.cached_tokens), unit)}</td><td>{formatTokens(item.cached_tokens, unit)}</td><td>{formatPercent(item.cache_hit_rate)}</td><td>{formatTokens(item.completion_tokens, unit)}</td><td>{formatTokens(item.total_tokens, unit)}</td><td>{number.format(item.requests)}</td></tr>; })}</tbody></table></div>;
}

export function CostTable({ data }: { data?: Record<string, CostBucket> }) {
  const rows = Object.entries(data ?? {}).sort((left, right) => (right[1].total_cost ?? 0) - (left[1].total_cost ?? 0));
  if (!rows.length) return <p className="muted">No model cost yet.</p>;
  return <div className="table-wrap"><table><thead><tr><th>Model</th><th>Input Uncached</th><th>Input Cached</th><th>Output</th><th>Total Cost</th></tr></thead><tbody>{rows.map(([name, item]) => <tr key={name}><td>{name}</td><td>{formatMoney(item.input_uncached_cost)}</td><td>{formatMoney(item.input_cached_cost)}</td><td>{formatMoney(item.output_cost)}</td><td>{formatMoney(item.total_cost)}</td></tr>)}</tbody></table></div>;
}

export function KpiCard({ label, value, sub, accent }: { label: string; value: string; sub?: string; accent?: string }) {
  return <div className={`kpi ${accent ?? ''}`}><div className="kpi-label">{label}</div><div className="kpi-value">{value}</div>{sub ? <div className="kpi-sub">{sub}</div> : null}</div>;
}

export function BarRow({ label, hint, value, max, moneyValue, percent }: { label: string; hint?: string; value: number; max: number; moneyValue?: number; percent?: string }) {
  const { unit } = useTokenUnit();
  const w = max > 0 ? Math.max(2, (value / max) * 100) : 0;
  return <div className="bar-row"><div className="bar-head"><span className="bar-label">{label}{hint ? <em>{hint}</em> : null}</span><span className="bar-metric">{moneyValue !== undefined ? formatMoney(moneyValue) : formatTokens(value, unit)}{percent ? <i>{percent}</i> : null}</span></div><div className="bar-track"><div className="bar-fill" style={{ width: `${w}%` }} /></div><div className="bar-foot"><span>{formatTokens(value, unit)} tokens</span><span>{w.toFixed(1)}%</span></div></div>;
}