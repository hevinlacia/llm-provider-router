//! 搜索 key 池 + 统一 `/v1/search` 契约。
//!
//! 对外：`POST /v1/search`，用 router 本地 bearer token 认证（对外只暴露一个 key）。
//! 对内：按 key 池（providers → keys）加权选择供应商与 key，把统一请求翻译成
//!       各搜索供应商的 API 请求，再把响应归一化为统一结构。
//!
//! 配置：`config/search-providers.json`（路径可用 `LLM_PROVIDER_ROUTER_SEARCH_PROVIDERS_PATH` 覆盖）
//! ```jsonc
//! {
//!   "providers": {
//!     "tavily": {
//!       "base_url": "https://api.tavily.com",           // 可选，缺省用官方默认
//!       "keys": {
//!         "hevin": { "env_var": "AGENT_SEARCH_TAVILY_HEVIN_API_KEY", "weight": 5, "enabled": true }
//!       }
//!     },
//!     "exa":   { "keys": { "hevin": { "env_var": "AGENT_SEARCH_EXA_HEVIN_API_KEY", "weight": 5 } } },
//!     "brave": { "keys": { "hevin": { "env_var": "AGENT_SEARCH_BRAVE_HEVIN_API_KEY", "weight": 5 } } }
//!   }
//! }
//! ```
//!
//! 统一请求体：
//! ```json
//! {
//!   "query": "Spring Boot 4 requirements",
//!   "max_results": 5,
//!   "provider": "auto",             // auto | tavily | exa | brave
//!   "search_depth": "basic",        // tavily: basic | advanced
//!   "topic": "general",             // tavily: general | news | finance
//!   "time_range": "week",           // tavily 可选
//!   "include_answer": false,        // tavily 可选
//!   "include_domains": ["docs.spring.io"],
//!   "exclude_domains": ["reddit.com"]
//! }
//! ```
//!
//! 统一响应体：
//! ```json
//! {
//!   "provider": "tavily",
//!   "query": "...",
//!   "results": [
//!     { "title": "...", "url": "...", "snippet": "...", "published_date": "...", "score": 0.9 }
//!   ],
//!   "answer": "..."      // include_answer=true 且供应商支持时出现
//! }
//! ```

use crate::config::expand_path;
use anyhow::{anyhow, Context};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS_CAP: usize = 10;

// ---------------------------------------------------------------------------
// 配置数据结构
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchKeyEntry {
    pub env_var: String,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_weight() -> i64 {
    1
}
fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SearchProviderConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub keys: HashMap<String, SearchKeyEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SearchProvidersFile {
    #[serde(default)]
    pub providers: HashMap<String, SearchProviderConfig>,
}

/// 供应商类型：名字（tavily/exa/brave）决定请求翻译与响应归一化方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchProviderKind {
    Tavily,
    Exa,
    Brave,
}

impl SearchProviderKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "tavily" => Some(Self::Tavily),
            "exa" => Some(Self::Exa),
            "brave" => Some(Self::Brave),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Tavily => "tavily",
            Self::Exa => "exa",
            Self::Brave => "brave",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Tavily => "https://api.tavily.com",
            Self::Exa => "https://api.exa.ai",
            Self::Brave => "https://api.search.brave.com",
        }
    }
}

// ---------------------------------------------------------------------------
// 统一请求 / 响应
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnifiedSearchRequest {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub search_depth: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub time_range: Option<String>,
    #[serde(default)]
    pub include_answer: Option<bool>,
    #[serde(default)]
    pub include_domains: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_domains: Option<Vec<String>>,
}

impl UnifiedSearchRequest {
    pub fn normalized_max_results(&self) -> usize {
        let value = self.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        value.clamp(1, MAX_RESULTS_CAP)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnifiedSearchResult {
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

fn unified_response(provider: &str, query: &str, results: Vec<UnifiedSearchResult>, answer: Option<String>) -> Value {
    let mut payload = json!({
        "provider": provider,
        "query": query,
        "results": results,
    });
    if let Some(answer) = answer {
        payload["answer"] = Value::String(answer);
    }
    payload
}

// ---------------------------------------------------------------------------
// SearchPool：配置加载 / 写回 / key 值解析 / key 池选择 / 统一搜索
// ---------------------------------------------------------------------------

/// 锁内同步解析出的搜索目标（provider / key / base_url），锁外执行网络请求。
pub struct ResolvedSearch {
    pub provider: String,
    pub key_value: String,
    pub base_url: String,
    pub kind: SearchProviderKind,
}

pub struct SearchPool {
    pub config_path: PathBuf,
    memory: SearchProvidersFile,
}

impl SearchPool {
    pub fn new(path: &str) -> Self {
        Self {
            config_path: expand_path(path),
            memory: SearchProvidersFile::default(),
        }
    }

    pub fn get(&mut self) -> SearchProvidersFile {
        if self.is_memory() {
            return self.memory.clone();
        }
        if !self.config_path.exists() {
            let empty = SearchProvidersFile::default();
            let _ = self.write(&empty);
            return empty;
        }
        let Ok(raw) = fs::read_to_string(&self.config_path) else {
            return self.memory.clone();
        };
        serde_json::from_str::<SearchProvidersFile>(&raw).unwrap_or_else(|_| self.memory.clone())
    }

    pub fn set(&mut self, file: SearchProvidersFile) -> anyhow::Result<SearchProvidersFile> {
        self.write(&file)?;
        Ok(file)
    }

    fn write(&mut self, file: &SearchProvidersFile) -> anyhow::Result<()> {
        if self.is_memory() {
            self.memory = file.clone();
            return Ok(());
        }
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut providers: BTreeMap<_, _> = BTreeMap::new();
        for (name, provider) in &file.providers {
            let mut keys: BTreeMap<_, _> = BTreeMap::new();
            for (key_name, key) in &provider.keys {
                keys.insert(key_name.clone(), key.clone());
            }
            providers.insert(
                name.clone(),
                serde_json::json!({
                    "base_url": provider.base_url,
                    "keys": keys,
                }),
            );
        }
        let payload = serde_json::json!({ "providers": providers });
        fs::write(
            &self.config_path,
            format!("{}\n", serde_json::to_string_pretty(&payload)?),
        )?;
        Ok(())
    }

    pub fn key_value(&self, env_var: &str) -> Option<String> {
        env::var(env_var).ok().filter(|v| !v.is_empty())
    }

    /// 某供应商下 enabled 且 env 已配置的 key（name, entry）。
    fn available_keys(&self, provider: &SearchProviderConfig) -> Vec<(String, SearchKeyEntry)> {
        provider
            .keys
            .iter()
            .filter(|(_, key)| key.enabled && key.weight > 0 && self.key_value(&key.env_var).is_some())
            .map(|(name, key)| (name.clone(), key.clone()))
            .collect()
    }

    /// 加权随机选一个 key（无 session 语义，纯随机）。
    fn weighted_pick_key(&self, keys: &[(String, SearchKeyEntry)]) -> Option<(String, SearchKeyEntry)> {
        let total: i64 = keys.iter().map(|(_, key)| key.weight.max(0)).sum();
        if total <= 0 {
            return None;
        }
        let mut target = rand::thread_rng().gen_range(0..total);
        for item in keys {
            target -= item.1.weight.max(0);
            if target < 0 {
                return Some(item.clone());
            }
        }
        keys.last().cloned()
    }

    /// 选供应商 + key，返回 (provider 名, key 名, key 值)。
    /// `requested` 为 None / "auto" 时在全部可用供应商间按"供应商可用 key 权重和"加权选择；
    /// 指定供应商时只在它内部选（其下无可用 key 则报错）。
    pub fn pick_provider_key(
        &mut self,
        requested: Option<&str>,
    ) -> anyhow::Result<(String, String, String)> {
        let file = self.get();

        let requested_kind = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("auto") => None,
            Some(s) => Some(SearchProviderKind::from_name(s)
                .ok_or_else(|| anyhow!("unknown search provider: {s}"))?),
        };

        // 候选 provider：name + kind + 可用 key 列表
        let mut candidates: Vec<(String, SearchProviderKind, Vec<(String, SearchKeyEntry)>)> = Vec::new();
        for (name, provider) in &file.providers {
            let kind = SearchProviderKind::from_name(name)
                .ok_or_else(|| anyhow!("config provider '{name}' is not a known search provider (tavily/exa/brave)"))?;
            if let Some(requested_kind) = requested_kind {
                if kind != requested_kind {
                    continue;
                }
            }
            let keys = self.available_keys(provider);
            if !keys.is_empty() {
                candidates.push((name.clone(), kind, keys));
            }
        }
        if candidates.is_empty() {
            let requested = requested_kind.map(|k| k.name()).unwrap_or("any");
            return Err(anyhow!(
                "no available search key for provider '{requested}' (set the key env var and ensure enabled)"
            ));
        }

        let (provider_name, keys) = if requested_kind.is_some() {
            // 指定供应商：内部加权
            let (name, _, keys) = candidates.into_iter().next().expect("non-empty");
            (name, keys)
        } else {
            // auto：按供应商总权重加权
            let provider_total: i64 = candidates
                .iter()
                .map(|(_, _, keys)| keys.iter().map(|(_, k)| k.weight.max(0)).sum::<i64>())
                .sum();
            let mut target = rand::thread_rng().gen_range(0..provider_total.max(1));
            let mut chosen: Option<&(String, SearchProviderKind, Vec<(String, SearchKeyEntry)>)> = None;
            for candidate in &candidates {
                let weight: i64 = candidate.2.iter().map(|(_, k)| k.weight.max(0)).sum();
                target -= weight;
                if target < 0 {
                    chosen = Some(candidate);
                    break;
                }
            }
            let chosen = chosen.unwrap_or_else(|| candidates.last().expect("non-empty"));
            let (name, _, keys) = chosen.clone();
            (name, keys)
        };

        let (key_name, key) = self
            .weighted_pick_key(&keys)
            .ok_or_else(|| anyhow!("provider {provider_name}: no weighted key candidate"))?;
        let key_value = self
            .key_value(&key.env_var)
            .ok_or_else(|| anyhow!("provider {provider_name} key {key_name}: env var {} not set", key.env_var))?;
        Ok((provider_name, key_name, key_value))
    }

    /// 锁内同步：选供应商 + key，返回解析结果（不含网络 IO）。
    /// `requested` 为 None / "auto" 时在全部可用供应商间按"供应商可用 key 权重和"加权选择；
    /// 指定供应商时只在它内部选（其下无可用 key 则报错）。
    pub fn resolve(&mut self, req: &UnifiedSearchRequest) -> anyhow::Result<ResolvedSearch> {
        let query = req.query.trim();
        if query.is_empty() {
            return Err(anyhow!("query must not be empty"));
        }
        let (provider_name, _key_name, key_value) =
            self.pick_provider_key(req.provider.as_deref())?;
        let kind = SearchProviderKind::from_name(&provider_name).expect("picked provider is known");

        let file = self.get();
        let base_url = file
            .providers
            .get(&provider_name)
            .and_then(|p| p.base_url.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| kind.default_base_url())
            .to_string();

        Ok(ResolvedSearch {
            provider: provider_name,
            key_value,
            base_url,
            kind,
        })
    }

    /// 锁外执行：按已解析目标调用对应供应商 API。
    pub async fn execute(
        resolved: &ResolvedSearch,
        client: &reqwest::Client,
        req: &UnifiedSearchRequest,
    ) -> anyhow::Result<Value> {
        match resolved.kind {
            SearchProviderKind::Tavily => {
                search_tavily(client, &resolved.base_url, &resolved.key_value, &resolved.provider, req).await
            }
            SearchProviderKind::Exa => {
                search_exa(client, &resolved.base_url, &resolved.key_value, &resolved.provider, req).await
            }
            SearchProviderKind::Brave => {
                search_brave(client, &resolved.base_url, &resolved.key_value, &resolved.provider, req).await
            }
        }
    }

    fn is_memory(&self) -> bool {
        self.config_path.to_string_lossy() == ":memory:"
    }
}

// ---------------------------------------------------------------------------
// 供应商适配
// ---------------------------------------------------------------------------

async fn read_upstream_error(response: reqwest::Response) -> String {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("error").and_then(|e| e.get("message")))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let cleaned = text.chars().filter(|c| !c.is_control()).collect::<String>();
            cleaned.chars().take(300).collect()
        });
    format!("search API error ({status}): {message}")
}

fn as_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// Tavily: POST {base}/search, Authorization: Bearer <key>
async fn search_tavily(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    provider: &str,
    req: &UnifiedSearchRequest,
) -> anyhow::Result<Value> {
    let mut body = serde_json::Map::new();
    body.insert("query".into(), json!(req.query));
    body.insert("max_results".into(), json!(req.normalized_max_results()));
    body.insert("search_depth".into(), json!(req.search_depth.clone().unwrap_or_else(|| "basic".into())));
    body.insert("topic".into(), json!(req.topic.clone().unwrap_or_else(|| "general".into())));
    body.insert("include_answer".into(), json!(req.include_answer.unwrap_or(false)));
    body.insert("include_raw_content".into(), json!(false));
    if let Some(time_range) = req.time_range.clone() {
        body.insert("time_range".into(), json!(time_range));
    }
    if let Some(domains) = req.include_domains.clone() {
        body.insert("include_domains".into(), json!(domains));
    }
    if let Some(domains) = req.exclude_domains.clone() {
        body.insert("exclude_domains".into(), json!(domains));
    }

    let response = client
        .post(format!("{}/search", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("tavily request failed")?;
    if !response.status().is_success() {
        return Err(anyhow!(read_upstream_error(response).await));
    }
    let payload: Value = response.json().await.context("tavily response parse failed")?;

    let mut results = Vec::new();
    if let Some(items) = payload.get("results").and_then(Value::as_array) {
        for item in items {
            let title = as_string(&item["title"]).unwrap_or_else(|| "(untitled)".into());
            let url = as_string(&item["url"]).unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let snippet = as_string(&item["content"]).filter(|s| !s.is_empty());
            results.push(UnifiedSearchResult {
                title,
                url,
                snippet,
                published_date: None,
                score: item.get("score").and_then(Value::as_f64),
            });
        }
    }
    let answer = if req.include_answer.unwrap_or(false) {
        as_string(&payload["answer"]).filter(|s| !s.is_empty())
    } else {
        None
    };
    Ok(unified_response(provider, &req.query, results, answer))
}

/// Exa: POST {base}/search, x-api-key: <key>
async fn search_exa(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    provider: &str,
    req: &UnifiedSearchRequest,
) -> anyhow::Result<Value> {
    let mut body = serde_json::Map::new();
    body.insert("query".into(), json!(req.query));
    body.insert("numResults".into(), json!(req.normalized_max_results()));
    body.insert("contents".into(), json!({ "highlights": true, "summary": true }));
    if let Some(domains) = req.include_domains.clone() {
        body.insert("includeDomains".into(), json!(domains));
    }
    if let Some(domains) = req.exclude_domains.clone() {
        body.insert("excludeDomains".into(), json!(domains));
    }

    let response = client
        .post(format!("{}/search", base_url.trim_end_matches('/')))
        .header("x-api-key", api_key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("exa request failed")?;
    if !response.status().is_success() {
        return Err(anyhow!(read_upstream_error(response).await));
    }
    let payload: Value = response.json().await.context("exa response parse failed")?;

    let mut results = Vec::new();
    if let Some(items) = payload.get("results").and_then(Value::as_array) {
        for item in items {
            let title = as_string(&item["title"]).unwrap_or_else(|| "(untitled)".into());
            let url = as_string(&item["url"]).unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let snippet = as_string(&item["summary"])
                .or_else(|| {
                    item.get("highlights")
                        .and_then(Value::as_array)
                        .and_then(|h| h.first())
                        .and_then(as_string)
                })
                .filter(|s| !s.is_empty());
            results.push(UnifiedSearchResult {
                title,
                url,
                snippet,
                published_date: as_string(&item["publishedDate"]),
                score: item.get("score").and_then(Value::as_f64),
            });
        }
    }
    Ok(unified_response(provider, &req.query, results, None))
}

/// Brave: GET {base}/res/v1/web/search?q=..&count=.., X-Subscription-Token: <key>
async fn search_brave(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    provider: &str,
    req: &UnifiedSearchRequest,
) -> anyhow::Result<Value> {
    let mut query_params = vec![
        ("q".to_string(), req.query.clone()),
        ("count".to_string(), req.normalized_max_results().to_string()),
    ];
    if let Some(domains) = &req.include_domains {
        if !domains.is_empty() {
            let site = domains.iter().map(|d| format!("site:{}", d)).collect::<Vec<_>>().join(" OR ");
            query_params.push(("q".to_string(), format!("({})", site)));
        }
    }
    if let Some(domains) = &req.exclude_domains {
        for domain in domains {
            query_params.push(("q".to_string(), format!("-site:{}", domain)));
        }
    }

    let response = client
        .get(format!("{}/res/v1/web/search", base_url.trim_end_matches('/')))
        .header("X-Subscription-Token", api_key)
        .header("accept", "application/json")
        .query(&query_params)
        .send()
        .await
        .context("brave request failed")?;
    if !response.status().is_success() {
        return Err(anyhow!(read_upstream_error(response).await));
    }
    let payload: Value = response.json().await.context("brave response parse failed")?;

    let mut results = Vec::new();
    let web = payload.get("web").and_then(Value::as_object);
    if let Some(items) = web.and_then(|w| w.get("results")).and_then(Value::as_array) {
        for item in items {
            let title = as_string(&item["title"]).unwrap_or_else(|| "(untitled)".into());
            let url = as_string(&item["url"]).unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let snippet = as_string(&item["description"]).filter(|s| !s.is_empty());
            let published_date = as_string(&item["age"]).filter(|s| !s.is_empty());
            results.push(UnifiedSearchResult {
                title,
                url,
                snippet,
                published_date,
                score: None,
            });
        }
    }
    Ok(unified_response(provider, &req.query, results, None))
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_pool(providers_json: &str) -> SearchPool {
        let seq = {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        };
        let dir = std::env::temp_dir().join(format!("lpr-search-test-{}-{seq}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("search-providers.json");
        std::fs::write(&path, providers_json).unwrap();
        SearchPool::new(path.to_str().unwrap())
    }

    #[test]
    fn parses_config_with_defaults() {
        let mut pool = test_pool(
            r#"{
              "providers": {
                "tavily": {
                  "base_url": "https://api.tavily.com",
                  "keys": {
                    "hevin": { "env_var": "AGENT_SEARCH_TAVILY_HEVIN_API_KEY", "weight": 5, "enabled": true },
                    "other": { "env_var": "AGENT_SEARCH_TAVILY_OTHER_API_KEY" }
                  }
                },
                "exa": { "keys": { "hevin": { "env_var": "AGENT_SEARCH_EXA_HEVIN_API_KEY" } } }
              }
            }"#,
        );
        let file = pool.get();
        let tavily = &file.providers["tavily"];
        assert_eq!(tavily.keys["hevin"].weight, 5);
        assert_eq!(tavily.keys["hevin"].enabled, true);
        // 缺省 weight/enabled
        assert_eq!(tavily.keys["other"].weight, 1);
        assert_eq!(tavily.keys["other"].enabled, true);
        assert_eq!(file.providers["exa"].base_url, None);
    }

    #[test]
    fn provider_kind_from_name() {
        assert_eq!(SearchProviderKind::from_name("tavily"), Some(SearchProviderKind::Tavily));
        assert_eq!(SearchProviderKind::from_name("EXA"), Some(SearchProviderKind::Exa));
        assert_eq!(SearchProviderKind::from_name("brave"), Some(SearchProviderKind::Brave));
        assert_eq!(SearchProviderKind::from_name("google"), None);
        assert_eq!(SearchProviderKind::Tavily.default_base_url(), "https://api.tavily.com");
        assert_eq!(SearchProviderKind::Exa.default_base_url(), "https://api.exa.ai");
        assert_eq!(SearchProviderKind::Brave.default_base_url(), "https://api.search.brave.com");
    }

    #[test]
    fn normalized_max_results_clamped() {
        let req = UnifiedSearchRequest {
            query: "q".into(),
            max_results: Some(100),
            provider: None,
            search_depth: None,
            topic: None,
            time_range: None,
            include_answer: None,
            include_domains: None,
            exclude_domains: None,
        };
        assert_eq!(req.normalized_max_results(), MAX_RESULTS_CAP);
        let req2 = UnifiedSearchRequest { max_results: Some(0), ..req.clone() };
        assert_eq!(req2.normalized_max_results(), 1);
        let req3 = UnifiedSearchRequest { max_results: None, ..req };
        assert_eq!(req3.normalized_max_results(), DEFAULT_MAX_RESULTS);
    }

    #[test]
    fn pick_provider_key_requires_configured_env() {
        // 所有 env_var 都指向一个必然未设置的名字，选 key 应失败
        let mut pool = test_pool(
            r#"{
              "providers": {
                "tavily": { "keys": { "k": { "env_var": "LPR_NO_SUCH_ENV_FOR_SEARCH_TEST", "weight": 5 } } }
              }
            }"#,
        );
        let result = pool.pick_provider_key(Some("tavily"));
        assert!(result.is_err(), "无 env 值的 key 不应被选中");
        assert!(result.unwrap_err().to_string().contains("no available search key"));
    }

    #[test]
    fn pick_provider_key_unknown_provider_errors() {
        let mut pool = test_pool(r#"{"providers":{}}"#);
        let result = pool.pick_provider_key(Some("google"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown search provider"));
    }

    #[test]
    fn set_writes_config_back() {
        let seq = {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        };
        let dir = std::env::temp_dir().join(format!("lpr-search-test-write-{}-{seq}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("search-providers.json");
        let mut pool = SearchPool::new(path.to_str().unwrap());
        let mut file = SearchProvidersFile::default();
        let mut keys = HashMap::new();
        keys.insert(
            "hevin".to_string(),
            SearchKeyEntry { env_var: "AGENT_SEARCH_TAVILY_HEVIN_API_KEY".into(), weight: 5, enabled: true },
        );
        file.providers.insert("tavily".into(), SearchProviderConfig { base_url: None, keys });
        pool.set(file).unwrap();

        let reloaded = SearchPool::new(path.to_str().unwrap()).get();
        assert!(reloaded.providers.contains_key("tavily"));
        assert_eq!(reloaded.providers["tavily"].keys["hevin"].env_var, "AGENT_SEARCH_TAVILY_HEVIN_API_KEY");
    }
}
