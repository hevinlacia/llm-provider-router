use crate::config::expand_path;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeyWeightsConfigData {
    #[serde(default)]
    pub global: HashMap<String, i64>,
    #[serde(default)]
    pub pools: HashMap<String, HashMap<String, i64>>,
}

impl KeyWeightsConfigData {
    pub fn effective_for_pool(&self, pool: &str) -> HashMap<String, i64> {
        let mut weights = self.global.clone();
        if let Some(pool_weights) = self.pools.get(pool) {
            weights.extend(pool_weights.clone());
        }
        weights
    }
}

pub struct KeyWeightConfig {
    pub path: PathBuf,
    defaults: HashMap<String, i64>,
    memory: KeyWeightsConfigData,
}

impl KeyWeightConfig {
    pub fn new(path: &str, defaults: HashMap<String, i64>) -> Self {
        Self {
            path: expand_path(path),
            memory: KeyWeightsConfigData {
                global: defaults.clone(),
                pools: HashMap::new(),
            },
            defaults,
        }
    }

    pub fn get_config(&mut self) -> KeyWeightsConfigData {
        let mut config = KeyWeightsConfigData {
            global: self.defaults.clone(),
            pools: HashMap::new(),
        };
        if self.is_memory() {
            config.global.extend(self.memory.global.clone());
            config.pools.extend(self.memory.pools.clone());
            return config;
        }
        if !self.path.exists() {
            let _ = self.write(&config);
            return config;
        }
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return config;
        };
        let Some(data) = parse_key_weights_config(&raw) else {
            return config;
        };
        for (name, weight) in data.global {
            if self.defaults.contains_key(&name) && weight >= 0 {
                config.global.insert(name, weight);
            }
        }
        for (pool, weights) in data.pools {
            let normalized = weights
                .into_iter()
                .filter(|(name, weight)| self.defaults.contains_key(name) && *weight >= 0)
                .collect::<HashMap<_, _>>();
            if !normalized.is_empty() {
                config.pools.insert(pool, normalized);
            }
        }
        config
    }

    pub fn effective_for_pool(&mut self, pool: &str) -> HashMap<String, i64> {
        self.get_config().effective_for_pool(pool)
    }

    pub fn set_global(
        &mut self,
        weights: HashMap<String, i64>,
    ) -> anyhow::Result<KeyWeightsConfigData> {
        let mut next = self.get_config();
        next.global = self.defaults.clone();
        for (name, weight) in weights {
            if self.defaults.contains_key(&name) {
                next.global.insert(name, weight);
            }
        }
        if self.is_memory() {
            self.memory = next.clone();
            return Ok(next);
        }
        self.write(&next)?;
        Ok(next)
    }

    pub fn set_pool(
        &mut self,
        pool: &str,
        weights: HashMap<String, i64>,
    ) -> anyhow::Result<KeyWeightsConfigData> {
        let mut next = self.get_config();
        let normalized = weights
            .into_iter()
            .filter(|(name, _)| self.defaults.contains_key(name))
            .collect::<HashMap<_, _>>();
        next.pools.insert(pool.to_string(), normalized);
        if self.is_memory() {
            self.memory = next.clone();
            return Ok(next);
        }
        self.write(&next)?;
        Ok(next)
    }

    pub fn add_defaults(&mut self, defaults: HashMap<String, i64>) {
        for (name, weight) in defaults {
            self.defaults.entry(name.clone()).or_insert(weight);
            self.memory.global.entry(name).or_insert(weight);
        }
    }

    fn write(&self, config: &KeyWeightsConfigData) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let global: BTreeMap<_, _> = config.global.iter().collect();
        let pools: BTreeMap<_, _> = config
            .pools
            .iter()
            .map(|(pool, weights)| {
                let sorted_weights: BTreeMap<_, _> = weights.iter().collect();
                (pool, sorted_weights)
            })
            .collect();
        let payload = serde_json::json!({ "global": global, "pools": pools });
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&payload)?),
        )?;
        Ok(())
    }

    fn is_memory(&self) -> bool {
        self.path.to_string_lossy() == ":memory:"
    }
}

fn parse_key_weights_config(raw: &str) -> Option<KeyWeightsConfigData> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let object = value.as_object()?;
    let mut config = KeyWeightsConfigData::default();
    if object.contains_key("global") || object.contains_key("pools") {
        if let Some(global) = object.get("global").and_then(serde_json::Value::as_object) {
            config.global = parse_weight_object(global);
        }
        if let Some(pools) = object.get("pools").and_then(serde_json::Value::as_object) {
            for (pool, weights) in pools {
                if let Some(weights) = weights.as_object() {
                    config
                        .pools
                        .insert(pool.clone(), parse_weight_object(weights));
                }
            }
        }
        return Some(config);
    }
    config.global = parse_weight_object(object);
    Some(config)
}

fn parse_weight_object(
    object: &serde_json::Map<String, serde_json::Value>,
) -> HashMap<String, i64> {
    object
        .iter()
        .filter_map(|(name, value)| value.as_i64().map(|weight| (name.clone(), weight)))
        .collect()
}

pub struct ProviderConfig {
    pub path: PathBuf,
    defaults: HashMap<String, String>,
    memory: HashMap<String, String>,
}

impl ProviderConfig {
    pub fn new(path: &str, defaults: HashMap<String, String>) -> Self {
        Self {
            path: expand_path(path),
            memory: defaults.clone(),
            defaults,
        }
    }

    pub fn get(&mut self) -> HashMap<String, String> {
        let mut base_urls = self.defaults.clone();
        if self.is_memory() {
            base_urls.extend(self.memory.clone());
            return base_urls;
        }
        if !self.path.exists() {
            let _ = self.write(&base_urls);
            return base_urls;
        }
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return base_urls;
        };
        let Ok(data) = serde_json::from_str::<HashMap<String, String>>(&raw) else {
            return base_urls;
        };
        for (name, base_url) in data {
            if base_urls.contains_key(&name) && !base_url.is_empty() {
                base_urls.insert(name, base_url);
            }
        }
        base_urls
    }

    pub fn set(
        &mut self,
        base_urls: HashMap<String, String>,
    ) -> anyhow::Result<HashMap<String, String>> {
        let mut next = self.defaults.clone();
        next.extend(base_urls);
        if self.is_memory() {
            self.memory = next.clone();
            return Ok(next);
        }
        self.write(&next)?;
        Ok(next)
    }

    fn write(&self, base_urls: &HashMap<String, String>) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let sorted: BTreeMap<_, _> = base_urls.iter().collect();
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&sorted)?),
        )?;
        Ok(())
    }

    fn is_memory(&self) -> bool {
        self.path.to_string_lossy() == ":memory:"
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CustomKeyPools {
    #[serde(default)]
    pub keys: HashMap<String, CustomKeyEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomKeyEntry {
    pub env_var: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_billing_type")]
    pub billing_type: String,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default)]
    pub aliases: Vec<String>,
}

fn default_provider() -> String {
    "ark".to_string()
}
fn default_billing_type() -> String {
    "subscription".to_string()
}
fn default_weight() -> i64 {
    1
}

pub struct CustomKeyPoolConfig {
    pub path: PathBuf,
    memory: CustomKeyPools,
}

impl CustomKeyPoolConfig {
    pub fn new(path: &str) -> Self {
        Self {
            path: expand_path(path),
            memory: CustomKeyPools::default(),
        }
    }

    pub fn get(&mut self) -> CustomKeyPools {
        if self.is_memory() {
            return self.memory.clone();
        }
        if !self.path.exists() {
            let empty = CustomKeyPools::default();
            let _ = self.write(&empty);
            return empty;
        }
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return CustomKeyPools::default();
        };
        serde_json::from_str::<CustomKeyPools>(&raw).unwrap_or_default()
    }

    pub fn add_key(
        &mut self,
        name: String,
        entry: CustomKeyEntry,
    ) -> anyhow::Result<CustomKeyPools> {
        let mut config = self.get();
        config.keys.insert(name, entry);
        self.write(&config)?;
        Ok(config)
    }

    fn write(&mut self, config: &CustomKeyPools) -> anyhow::Result<()> {
        if self.is_memory() {
            self.memory = config.clone();
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let sorted: BTreeMap<_, _> = config.keys.iter().collect();
        let payload = serde_json::json!({ "keys": sorted });
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&payload)?),
        )?;
        Ok(())
    }

    fn is_memory(&self) -> bool {
        self.path.to_string_lossy() == ":memory:"
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenPrice {
    #[serde(default)]
    pub input_uncached_per_million: f64,
    #[serde(default)]
    pub input_cached_per_million: f64,
    #[serde(default)]
    pub output_per_million: f64,
}

impl TokenPrice {
    pub fn is_valid(&self) -> bool {
        self.input_uncached_per_million.is_finite()
            && self.input_cached_per_million.is_finite()
            && self.output_per_million.is_finite()
            && self.input_uncached_per_million >= 0.0
            && self.input_cached_per_million >= 0.0
            && self.output_per_million >= 0.0
    }

    pub fn cost_parts(
        &self,
        input_uncached_tokens: i64,
        input_cached_tokens: i64,
        output_tokens: i64,
    ) -> (f64, f64, f64) {
        let uncached =
            input_uncached_tokens.max(0) as f64 * self.input_uncached_per_million / 1_000_000.0;
        let cached =
            input_cached_tokens.max(0) as f64 * self.input_cached_per_million / 1_000_000.0;
        let output = output_tokens.max(0) as f64 * self.output_per_million / 1_000_000.0;
        (uncached, cached, output)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CustomModelAlias {
    pub alias: String,
    pub upstream_model: String,
    pub provider: String,
    #[serde(default = "default_retry_max_seconds")]
    pub max_retry_seconds: u64,
    #[serde(default = "default_retry_delay_seconds")]
    pub retry_delay_seconds: f64,
}

fn default_retry_max_seconds() -> u64 {
    300
}
fn default_retry_delay_seconds() -> f64 {
    5.0
}

pub struct ModelAliasConfig {
    pub path: PathBuf,
    memory: Vec<CustomModelAlias>,
}

impl ModelAliasConfig {
    pub fn new(path: &str) -> Self {
        Self {
            path: expand_path(path),
            memory: Vec::new(),
        }
    }

    pub fn get(&mut self) -> Vec<CustomModelAlias> {
        if self.is_memory() {
            return self.memory.clone();
        }
        if !self.path.exists() {
            return Vec::new();
        }
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<CustomModelAlias>>(&raw).unwrap_or_default()
    }

    pub fn set(&mut self, aliases: Vec<CustomModelAlias>) -> anyhow::Result<Vec<CustomModelAlias>> {
        if self.is_memory() {
            self.memory = aliases.clone();
            return Ok(aliases);
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&aliases)?),
        )?;
        Ok(aliases)
    }

    fn is_memory(&self) -> bool {
        self.path.to_string_lossy() == ":memory:"
    }
}

pub struct TokenPriceConfig {
    pub path: PathBuf,
    defaults: HashMap<String, TokenPrice>,
    memory: HashMap<String, TokenPrice>,
}

impl TokenPriceConfig {
    pub fn new(path: &str, defaults: HashMap<String, TokenPrice>) -> Self {
        Self {
            path: expand_path(path),
            memory: defaults.clone(),
            defaults,
        }
    }

    pub fn get(&mut self) -> HashMap<String, TokenPrice> {
        let mut prices = self.defaults.clone();
        if self.is_memory() {
            prices.extend(self.memory.clone());
            return prices;
        }
        if !self.path.exists() {
            let _ = self.write(&prices);
            return prices;
        }
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return prices;
        };
        let Ok(data) = serde_json::from_str::<HashMap<String, TokenPrice>>(&raw) else {
            return prices;
        };
        for (model, price) in data {
            if prices.contains_key(&model) && price.is_valid() {
                prices.insert(model, price);
            }
        }
        prices
    }

    pub fn set(
        &mut self,
        prices: HashMap<String, TokenPrice>,
        known_models: &HashSet<String>,
    ) -> anyhow::Result<HashMap<String, TokenPrice>> {
        let mut next = self.defaults.clone();
        for (model, price) in prices {
            if known_models.contains(&model) {
                next.insert(model, price);
            }
        }
        if self.is_memory() {
            self.memory = next.clone();
            return Ok(next);
        }
        self.write(&next)?;
        Ok(next)
    }

    pub fn add_defaults(&mut self, defaults: HashMap<String, TokenPrice>) {
        for (model, price) in defaults {
            self.defaults.entry(model.clone()).or_insert(price.clone());
            self.memory.entry(model).or_insert(price);
        }
    }

    fn write(&self, prices: &HashMap<String, TokenPrice>) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let sorted: BTreeMap<_, _> = prices.iter().collect();
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&sorted)?),
        )?;
        Ok(())
    }

    fn is_memory(&self) -> bool {
        self.path.to_string_lossy() == ":memory:"
    }
}

/// Stores LLM provider key VALUES (env_var -> secret) in a gitignored JSON file.
/// Encrypted backup lives in ~/Developer/vault; the vault regenerates
/// ~/.config/environment.d/agent-env.conf from this file to share keys with
/// other tools (opencode, pi) that consume env vars directly.
pub struct ApiKeysStore {
    pub path: PathBuf,
}

impl ApiKeysStore {
    pub fn new(path: &str) -> Self {
        Self {
            path: expand_path(path),
        }
    }

    pub fn exists(&self) -> bool {
        !self.is_memory() && self.path.exists()
    }

    pub fn load(&self) -> HashMap<String, String> {
        if self.is_memory() || !self.path.exists() {
            return HashMap::new();
        }
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return HashMap::new();
        };
        serde_json::from_str::<HashMap<String, String>>(&raw).unwrap_or_default()
    }

    pub fn write(&self, keys: &HashMap<String, String>) -> anyhow::Result<()> {
        if self.is_memory() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let sorted: BTreeMap<_, _> = keys.iter().collect();
        fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&sorted)?),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn upsert(&self, env_var: &str, value: &str) -> anyhow::Result<()> {
        let mut keys = self.load();
        keys.insert(env_var.to_string(), value.to_string());
        self.write(&keys)
    }

    fn is_memory(&self) -> bool {
        self.path.to_string_lossy() == ":memory:"
    }
}
