// 通用格式化 / 目标候选分组工具（无 React 依赖，供各页面复用）。

import type { Bucket, CostBucket, TargetCandidateGroup, V2Status } from '../types';

const number = new Intl.NumberFormat();
const money = new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD', maximumFractionDigits: 6 });

export { number, money };

export function formatMoney(value: number | undefined): string {
  return money.format((value ?? 0) || 0);
}

export function formatPercent(value: number | undefined): string {
  return `${(((value ?? 0) || 0) * 100).toFixed(1)}%`;
}

export function formatCompact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 1 : 2)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(n >= 10_000 ? 0 : 1)}k`;
  return number.format(n);
}

export function formatFetchedAt(ts?: number | null): string {
  if (!ts) return '';
  return new Date(ts * 1000).toLocaleString();
}

export function formatWindow(n?: number | null): string {
  if (n == null) return '—';
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

export function providerOf(keyName: string, providers: Record<string, string>): string {
  if (providers[keyName]) return providers[keyName];
  const slash = keyName.indexOf('/');
  if (slash > 0) return keyName.slice(0, slash);
  return 'unknown';
}

export function costPer1k(bucket: Bucket, cost: CostBucket | undefined): number {
  const c = cost?.total_cost ?? 0;
  const t = bucket.total_tokens ?? 0;
  if (!t || !c) return 0;
  return (c / t) * 1000;
}

// ---- Model Pool target 候选分组（与后端 validate_targets 语义对齐）----
export const TARGET_GROUP_PHYSICAL = 'Physical models (provider/upstream)';
export const TARGET_GROUP_POOL = 'Model pools';
export const TARGET_GROUP_VIRTUAL = 'Virtual models';

export function buildTargetCandidates(config: V2Status, excludePool: string | null): TargetCandidateGroup[] {
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

export function isKnownTarget(model: string, groups: TargetCandidateGroup[]): boolean {
  const physical = groups.find((g) => g.group === TARGET_GROUP_PHYSICAL)?.items ?? [];
  const pools = groups.find((g) => g.group === TARGET_GROUP_POOL)?.items ?? [];
  const virtuals = groups.find((g) => g.group === TARGET_GROUP_VIRTUAL)?.items ?? [];
  if (physical.includes(model) || pools.includes(model) || virtuals.includes(model)) return true;
  const slash = model.indexOf('/');
  if (slash > 0 && virtuals.includes(model.slice(slash + 1))) return true;
  return false;
}
