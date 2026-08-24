import type { FilterState, KeyConfig, ModelAliasConfig, PhysicalModelPatch, PhysicalModelsConfig, ProbeResult, ProviderConfig, ProviderModelsResponse, RouterCapabilities, StateResponse, ThinkingMapsConfig, TokenPriceConfig, UsageSeriesBucket, UsageSeriesGroupBy, UsageSeriesResponse, UsageSnapshot, V2Status, WeightConfig } from './types';

function queryFromFilters(filters: FilterState): string {
  const params = new URLSearchParams();
  if (filters.period) params.set('period', filters.period);
  if (filters.start) params.set('start', filters.start);
  if (filters.end) params.set('end', filters.end);
  const query = params.toString();
  return query ? `?${query}` : '';
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(url, init);
  if (!response.ok) {
    const text = await response.text();
    throw new Error(text || `${response.status} ${response.statusText}`);
  }
  return response.json() as Promise<T>;
}

function normalizeWeightConfig(raw: Partial<WeightConfig>): WeightConfig {
  const aliases = (raw.aliases ?? {}) as WeightConfig['aliases'];
  const weights = raw.weights ?? raw.global_weights ?? {};
  const pools = raw.pools ?? Object.entries(aliases)
    .filter(([, alias]) => Array.isArray(alias.keys) && alias.keys.length > 0)
    .map(([name]) => name)
    .sort();
  return {
    ok: raw.ok ?? true,
    weights,
    global_weights: raw.global_weights ?? weights,
    pool_weights: raw.pool_weights ?? {},
    pools,
    supports_pool_weights: Boolean(raw.global_weights && raw.pool_weights && raw.pools),
    aliases,
    config_path: raw.config_path ?? '',
  };
}

export const api = {
  async modelAliases() {
    return request<ModelAliasConfig>('/api/config/model-aliases');
  },
  saveModelAliases(custom_aliases: ModelAliasConfig['custom_aliases']) {
    return request<ModelAliasConfig>('/api/config/model-aliases', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ custom_aliases }),
    });
  },
  state(filters: FilterState) {
    return request<StateResponse>(`/api/state${queryFromFilters(filters)}`);
  },
  usage(filters: Partial<FilterState> = {}) {
    return request<UsageSnapshot>(`/api/usage${queryFromFilters({ period: filters.period ?? 'all', start: filters.start ?? '', end: filters.end ?? '' })}`);
  },
  usageSeries(params: {
    period?: string;
    start?: string;
    end?: string;
    bucket?: UsageSeriesBucket;
    group_by?: UsageSeriesGroupBy;
    top?: number;
  }) {
    const q = new URLSearchParams();
    if (params.period) q.set('period', params.period);
    if (params.start) q.set('start', params.start);
    if (params.end) q.set('end', params.end);
    if (params.bucket) q.set('bucket', params.bucket);
    if (params.group_by) q.set('group_by', params.group_by);
    if (params.top != null) q.set('top', String(params.top));
    const qs = q.toString();
    return request<UsageSeriesResponse>(`/api/usage/series${qs ? `?${qs}` : ''}`);
  },
  resetUsage() {
    return request<{ ok: boolean; usage: UsageSnapshot }>('/api/usage/reset', { method: 'POST' });
  },
  clearFrozen() {
    return request<StateResponse>('/api/frozen/clear', { method: 'POST' });
  },
  async weights() {
    return normalizeWeightConfig(await request<Partial<WeightConfig>>('/api/config/weights'));
  },
  saveWeights(weights: Record<string, number>, pool?: string) {
    return request<WeightConfig>('/api/config/weights', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ weights, pool: pool ?? '__global__' }),
    });
  },
  providers() {
    return request<ProviderConfig>('/api/config/providers');
  },
  saveProviders(providers: Record<string, string>) {
    return request<ProviderConfig>('/api/config/providers', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ providers }),
    });
  },
  async tokenPrices() {
    try {
      return await request<TokenPriceConfig>('/api/config/token-prices');
    } catch {
      return { ok: false, models: [], config_path: 'restart router backend to enable token price settings' };
    }
  },
  saveTokenPrices(models: TokenPriceConfig['models']) {
    return request<TokenPriceConfig>('/api/config/token-prices', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ models }),
    });
  },
  thinkingMaps() {
    return request<ThinkingMapsConfig>('/api/config/thinking-maps');
  },
  saveThinkingMaps(maps: Array<{ model: string; thinking_level_map: Record<string, string | null> | null; thinking_format: string | null }>) {
    return request<ThinkingMapsConfig>('/api/config/thinking-maps', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ maps }),
    });
  },
  savePhysicalModels(models: PhysicalModelPatch[]) {
    return request<PhysicalModelsConfig>('/api/config/v2/physical-models', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ models }),
    });
  },
  probePhysicalModel(provider: string, upstream: string) {
    return request<ProbeResult>('/api/config/v2/physical-models/probe', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ provider, upstream }),
    });
  },
  applyThinkingToEquivalents(model: string, onlyMissing = false) {
    return request<{ ok: boolean; applied_to: string[]; thinking_maps: ThinkingMapsConfig }>('/api/config/thinking-maps/apply-equivalents', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model, only_missing: onlyMissing }),
    });
  },
  keys() {
    return request<KeyConfig>('/api/config/keys');
  },
  v2Status() {
    return request<V2Status>('/api/config/v2');
  },
  routerCapabilities() {
    return request<RouterCapabilities>('/api/router/capabilities');
  },
  models() {
    return request<{ object: string; data: Array<{ id: string; object: string; owned_by: string; context_window?: number; max_output_tokens?: number }> }>('/v1/models');
  },
  providerModels(name: string, refresh = false) {
    const query = refresh ? '?refresh=1' : '';
    return request<ProviderModelsResponse>(`/api/config/v2/providers/${encodeURIComponent(name)}/models${query}`);
  },
  updateV2Provider(oldName: string, provider: { name: string; base_url: string; keys: Record<string, { env_var: string; weight: number; billing_type: string; enabled: boolean }> }) {
    return request<V2Status>('/api/config/v2/providers', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ old_name: oldName, provider }),
    });
  },
  createV2Provider(provider: { name: string; base_url: string; keys: Record<string, { env_var: string; weight: number; billing_type: string; enabled: boolean }> }) {
    return request<V2Status>('/api/config/v2/providers', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ provider }),
    });
  },
  updateV2LogicalModel(name: string, body: { strategy: string; params?: Record<string, unknown>; targets: Array<{ model: string; weight?: number | null }> }) {
    return request<V2Status>('/api/config/v2/logical-models', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, ...body }),
    });
  },
  createV2LogicalModel(body: { name: string; strategy: string; params?: Record<string, unknown>; targets: Array<{ model: string; weight?: number | null }> }) {
    return request<V2Status>('/api/config/v2/logical-models', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  },
  deleteV2LogicalModel(name: string) {
    return request<V2Status>('/api/config/v2/logical-models', {
      method: 'DELETE',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name }),
    });
  },
  upsertVirtualModel(name: string, provider: string, upstreamModel: string) {
    return request<V2Status>('/api/config/v2/virtual-models', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, provider, upstream_model: upstreamModel }),
    });
  },
  deleteVirtualModel(name: string, provider: string) {
    return request<V2Status>('/api/config/v2/virtual-models', {
      method: 'DELETE',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, provider }),
    });
  },
  saveKeys(keys: Record<string, string>, deleteNames: string[]) {
    return request<KeyConfig>('/api/config/keys', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ keys, delete: deleteNames }),
    });
  },
  addKey(payload: { name: string; value: string; weight: number; aliases: string[] }) {
    return request<KeyConfig>('/api/config/keys', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
  },
};
