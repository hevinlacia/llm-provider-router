use crate::config::expand_path;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
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

    pub fn sync_to_known(&mut self, known: &HashSet<String>) {
        self.defaults.retain(|k, _| known.contains(k));
        self.memory.retain(|k, _| known.contains(k));
        for model in known {
            self.defaults
                .entry(model.clone())
                .or_insert_with(TokenPrice::default);
            self.memory
                .entry(model.clone())
                .or_insert_with(TokenPrice::default);
        }
        if self.is_memory() {
            return;
        }
        // Prune file to exactly `known` (keep existing prices for known, fill missing with default)
        let mut file_data: HashMap<String, TokenPrice> = if self.path.exists() {
            fs::read_to_string(&self.path)
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        let before = file_data.len();
        file_data.retain(|k, _| known.contains(k));
        for model in known {
            file_data
                .entry(model.clone())
                .or_insert_with(TokenPrice::default);
        }
        if file_data.len() != before || file_data.len() != known.len() {
            let _ = self.write(&file_data);
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

    fn is_memory(&self) -> bool {
        self.path.to_string_lossy() == ":memory:"
    }
}
