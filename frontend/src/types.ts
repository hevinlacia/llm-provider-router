export type Bucket = {
  requests: number;
  errors: number;
  prompt_tokens: number;
  cached_tokens: number;
  prompt_uncached_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  cache_hit_rate: number;
};


export type CostBucket = {
  input_uncached_cost: number;
  input_cached_cost: number;
  output_cost: number;
  total_cost: number;
};

export type UsageSnapshot = {
  started_at: number;
  uptime_seconds: number;
  range: { period: string; start: number | null; end: number | null };
  total: Bucket;
  by_model: Record<string, Bucket>;
  by_model_cost: Record<string, CostBucket>;
  total_cost: CostBucket;
  by_key: Record<string, Bucket>;
  by_status: Record<string, Bucket>;
  by_day: Record<string, Bucket>;
  by_month: Record<string, Bucket>;
  db_path: string;
};

export type StateResponse = {
  ok: boolean;
  frozen: Record<string, { seconds_remaining: number; reason: string }>;
  bindings: number;
  usage: UsageSnapshot;
};

export type WeightKey = {
  name: string;
  provider: string;
  billing_type: string;
  default_weight: number;
  global_weight: number;
  pool_weight?: number | null;
  weight: number;
  enabled: boolean;
  probability: number;
};

export type WeightAlias = {
  model: string;
  base_url: string;
  effective_base_url: string;
  provider: string;
  keys: WeightKey[];
};

export type WeightConfig = {
  ok: boolean;
  weights: Record<string, number>;
  global_weights: Record<string, number>;
  pool_weights: Record<string, Record<string, number>>;
  pools: string[];
  supports_pool_weights: boolean;
  aliases: Record<string, WeightAlias>;
  config_path: string;
};

export type ProviderConfig = {
  ok: boolean;
  providers: Array<{ name: string; base_url: string; default_base_url: string }>;
  config_path: string;
};

export type CustomModelAlias = {
  alias: string;
  upstream_model: string;
  provider: string;
  max_retry_seconds: number;
  retry_delay_seconds: number;
};

export type ModelAliasConfig = {
  ok: boolean;
  custom_aliases: CustomModelAlias[];
  config_path: string;
};


export type TokenPriceConfig = {
  ok: boolean;
  models: Array<{
    model: string;
    input_uncached_per_million: number;
    input_cached_per_million: number;
    output_per_million: number;
  }>;
  config_path: string;
};

export type ModelEquivalencesConfig = {
  ok: boolean;
  groups: Array<{ id: string; display_name: string; models: string[] }>;
  config_path: string;
};

export type KeyConfig = {
  ok: boolean;
  keys: Array<{
    name: string;
    provider: string;
    billing_type: string;
    env_var: string;
    configured: boolean;
    encrypted_configured: boolean;
    env_configured: boolean;
    source: string;
  }>;
  auto_aliases: string[];
  config_path: string;
  custom_key_config_path: string;
};

export type FilterState = {
  period: string;
  start: string;
  end: string;
};

// ---- v2 分层架构状态（GET /api/config/v2）----

export type V2KeyStatus = {
  env_var: string;
  weight: number;
  billing_type: string;
  enabled: boolean;
  frozen: boolean;
  frozen_reason?: string | null;
};

export type V2ProviderStatus = {
  base_url: string;
  key_total: number;
  key_enabled: number;
  key_frozen: number;
  available: boolean;
  keys: Record<string, V2KeyStatus>;
  /** 供应商详情（上游探测）模型列表 */
  models?: string[];
};

export type V2PhysicalModel = {
  id: string;
  provider: string;
  upstream_model: string;
  family?: string | null;
  params: Record<string, unknown>;
};

export type V2LogicalModel = {
  params: Record<string, unknown>;
  strategy: string;
  targets: Array<{ model: string; weight?: number | null }>;
};

export type V2Status = {
  v2_enabled: boolean;
  providers?: Record<string, V2ProviderStatus>;
  models?: V2PhysicalModel[];
  logical_models?: Record<string, V2LogicalModel>;
  /** 虚拟模型：虚拟名 → { 供应商 → 实际上游模型名 } */
  virtual_models?: Record<string, Record<string, string>>;
};

export type ProviderModelsResponse = {
  ok: boolean;
  provider: string;
  cached?: boolean;
  models?: string[];
  fetched_at?: number | null;
  error?: string;
};

/** Model Pool target 候选分组：物理模型 / 模型池 / 虚拟模型 */
export type TargetCandidateGroup = {
  group: string;
  items: string[];
};
