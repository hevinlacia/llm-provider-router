import type { FilterState, KeyConfig, ModelAliasConfig, ProviderConfig, StateResponse, TokenPriceConfig, UsageSnapshot, V2Status, WeightConfig } from './types';

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
  keys() {
    return request<KeyConfig>('/api/config/keys');
  },
  v2Status() {
    return request<V2Status>('/api/config/v2');
  },
  updateV2Provider(oldName: string, provider: { name: string; base_url: string; keys: Record<string, { env_var: string; weight: number; billing_type: string; enabled: boolean }> }) {
    return request<V2Status>('/api/config/v2/providers', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ old_name: oldName, provider }),
    });
  },
  updateV2LogicalModel(name: string, body: { strategy: string; params?: Record<string, unknown>; targets: Array<{ model: string; weight?: number | null }> }) {
    return request<V2Status>('/api/config/v2/logical-models', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, ...body }),
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
