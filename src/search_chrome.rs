//! Chrome(CDP) 搜索 provider：驱动本机专用 headless Chrome 渲染搜索引擎结果页并解析，
//! 不依赖任何第三方搜索 API key。
//!
//! 链路：`GET {cdp_url}/json/version` 取 browser websocket → `Target.createTarget/attach` →
//! 注入 webdriver 隐藏 + 真实 Chrome UA（headless 特征会被搜索引擎风控拦截）→
//! `Page.navigate`（引擎搜索 URL）→ 轮询结果节点出现 → `Runtime.evaluate` 取 outerHTML →
//! scraper 解析 → 统一 `UnifiedSearchResult`。
//!
//! 引擎顺序默认 google → bing。实测依据（2026-10，本机 Chrome 154/155，中国区网络）：
//! - google 未登录时 href 是 `/goto` 加密链接，需用 `cite` 展示 URL 还原，
//!   中文环境下部分 cite 被时间戳替换（如"1年前"），这类结果丢弃；
//!   但结果质量高（同一查询精准命中 docs.rs/tokio.rs 文档页）。
//! - bing 不拦截 headless（锚点 `li.b_algo` 多年稳定），但 cn.bing.com 对部分
//!   查询返回泛化结果（实测 "rust tokio select macro example" 只返回 rust 教程页，
//!   mkt/ensearch 参数均无法纠正），故作为兜底而非默认。
//!
//! Chrome 实例由 `llm-provider-router-chrome.service`（systemd user unit）托管，
//! 独立 user-data-dir + 显式 `--remote-debugging-port`（M136+ 默认 profile 有 CDP
//! lockdown，且 per-session 审批弹窗不适合无人值守服务）。本模块只连接不拉起；
//! 连接失败时由调用方（`SearchPool::search`）降级到 API key 池供应商。

use anyhow::{anyhow, bail, Context};
use futures_util::{SinkExt, StreamExt};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream};

use crate::search::UnifiedSearchResult;

/// 真实 Chrome UA：`HeadlessChrome/x.y` UA 会被 bing/google 风控直接拦截或降级。
const DESKTOP_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36";
/// 抹掉 `navigator.webdriver` 自动化特征（google 据此返回"启用 JS"空壳页）。
const WEBDRIVER_HIDE: &str = "Object.defineProperty(navigator,'webdriver',{get:()=>undefined})";
/// 结果节点就绪后的短暂等待：让 snippet 渲染完整。
const SNIPPET_SETTLE: Duration = Duration::from_millis(600);
const RENDER_POLL_INTERVAL: Duration = Duration::from_millis(400);
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// 顶层 `chrome` 配置节（`search-providers.json`），无 key、无需 env。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChromeSearchConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cdp_url")]
    pub cdp_url: String,
    /// 引擎优先级；空/未配置时 [google, bing]（本机实测 google 结果质量更优，bing 兑底）。
    #[serde(default)]
    pub engines: Vec<String>,
    /// 单次搜索（含所有引擎）总预算。
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for ChromeSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cdp_url: default_cdp_url(),
            engines: Vec::new(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

impl ChromeSearchConfig {
    pub fn engines_or_default(&self) -> Vec<String> {
        if self.engines.is_empty() {
            vec!["google".into(), "bing".into()]
        } else {
            self.engines.clone()
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_cdp_url() -> String {
    "http://127.0.0.1:9223".into()
}
fn default_timeout_ms() -> u64 {
    20_000
}

// ---------------------------------------------------------------------------
// 引擎定义与解析
// ---------------------------------------------------------------------------

struct Engine {
    name: &'static str,
    /// `{q}` 查询、`{n}` 结果数占位。
    url: &'static str,
    /// 结果节点就绪判定（轮询 `querySelectorAll` 计数 > 0）。
    ready_selector: &'static str,
    parser: fn(&str) -> Vec<UnifiedSearchResult>,
}

const ENGINES: &[Engine] = &[
    Engine {
        name: "bing",
        url: "https://www.bing.com/search?q={q}&count={n}",
        ready_selector: "li.b_algo",
        parser: parse_bing,
    },
    Engine {
        name: "google",
        url: "https://www.google.com/search?q={q}&num={n}",
        ready_selector: "a.zReHs, div.yuRUbf a",
        parser: parse_google,
    },
];

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn uri_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// bing：`li.b_algo` 块内闭包解析（标题 `h2 a`，href 明文；snippet 在 caption/lineclamp 段落）。
fn parse_bing(html: &str) -> Vec<UnifiedSearchResult> {
    let doc = Html::parse_document(html);
    let sel_item = Selector::parse("li.b_algo").expect("valid css");
    let sel_link = Selector::parse("h2 a[href]").expect("valid css");
    let sel_any_link = Selector::parse("a[href]").expect("valid css");
    let sel_snippet =
        Selector::parse(".b_caption p, p.b_lineclamp2, p.b_lineclamp3, p.b_lineclamp4")
            .expect("valid css");

    let mut out = Vec::new();
    for item in doc.select(&sel_item) {
        let a = item
            .select(&sel_link)
            .next()
            .or_else(|| item.select(&sel_any_link).next());
        let Some(a) = a else { continue };
        let Some(url) = a.value().attr("href") else {
            continue;
        };
        if !url.starts_with("http") {
            continue;
        }
        let title = collapse_ws(&a.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .select(&sel_snippet)
            .next()
            .map(|s| collapse_ws(&s.text().collect::<String>()))
            .filter(|s| !s.is_empty());
        out.push(UnifiedSearchResult {
            title,
            url: url.to_string(),
            snippet,
            published_date: None,
            score: None,
        });
    }
    out
}

/// google：标题链接 `a.zReHs / div.yuRUbf a`（含 h3 才是结果，排除 AI 卡片等）。
///
/// 明文 href 直接用；`/goto` 加密链接时用 `cite` 展示 URL 还原（实测每结果恰好 2 个
/// 相同 cite，步长 2；某些版本 1:1）。中文环境部分 cite 是时间戳（"1年前"）→ 还原失败
/// 丢弃。还原出的 URL 含省略号（cite 被截断展示）同样丢弃——无效链接比缺字段更糟。
///
/// snippet 用全局 `div.VwiC3b` 与结果按文档顺序对齐（实测每结果恰好一个；AI 卡片
/// 两者都不产生，不影响对齐）。若个别结果缺 snippet，其后结果 snippet 可能错位——
/// title/url 不受影响，可接受（google 仅为兜底引擎）。
fn parse_google(html: &str) -> Vec<UnifiedSearchResult> {
    let doc = Html::parse_document(html);
    let sel_link = Selector::parse("a.zReHs, div.yuRUbf a").expect("valid css");
    let sel_h3 = Selector::parse("h3").expect("valid css");
    let sel_snippet = Selector::parse("div.VwiC3b").expect("valid css");
    let sel_cite = Selector::parse("cite").expect("valid css");

    let links: Vec<ElementRef<'_>> = doc
        .select(&sel_link)
        .filter(|a| a.select(&sel_h3).next().is_some())
        .collect();
    let snippets: Vec<ElementRef<'_>> = doc.select(&sel_snippet).collect();
    let cites: Vec<String> = doc
        .select(&sel_cite)
        .map(|c| collapse_ws(&c.text().collect::<String>()))
        .collect();
    let cite_stride = if cites.len() == links.len() {
        1
    } else if cites.len() == 2 * links.len() {
        2
    } else {
        0
    };

    let mut out = Vec::new();
    for (i, a) in links.iter().enumerate() {
        let title = a
            .select(&sel_h3)
            .next()
            .map(|h| collapse_ws(&h.text().collect::<String>()))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let raw_url = a.value().attr("href").unwrap_or("");
        let url = if raw_url.starts_with("http") {
            Some(raw_url.to_string())
        } else if raw_url.starts_with("/goto") && cite_stride > 0 {
            cites.get(i * cite_stride).and_then(|c| un_google_cite(c))
        } else {
            None
        };
        let Some(url) = url else { continue };
        let snippet = snippets
            .get(i)
            .map(|s| collapse_ws(&s.text().collect::<String>()))
            .filter(|s| !s.is_empty());
        out.push(UnifiedSearchResult {
            title,
            url,
            snippet,
            published_date: None,
            score: None,
        });
    }
    out
}

/// `'https://docs.rs › axum › struct.Json.html'` → `'https://docs.rs/axum/struct.Json.html'`。
/// 非以 http 开头（时间戳等）或被截断（含省略号）时返回 None。
fn un_google_cite(cite: &str) -> Option<String> {
    let mut parts = cite.split('›').map(str::trim).filter(|p| !p.is_empty());
    let first = parts.next()?;
    if !first.starts_with("http") {
        return None;
    }
    let mut url = first.trim_end_matches('/').to_string();
    for p in parts {
        url.push('/');
        url.push_str(p);
    }
    if url.contains('…') || url.ends_with("...") {
        return None;
    }
    Some(url)
}

// ---------------------------------------------------------------------------
// CDP 薄客户端
// ---------------------------------------------------------------------------

struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn connect(ws_url: &str) -> anyhow::Result<Self> {
        // tungstenite 默认 max_message_size = 64MiB，足够容纳 ~2MB 的 outerHTML 结果。
        let (ws, _) = connect_async(ws_url)
            .await
            .with_context(|| format!("cdp websocket connect failed: {ws_url}"))?;
        Ok(Self { ws, next_id: 0 })
    }

    /// 发送命令并等待同 id 响应（忽略事件消息）。
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session {
            msg["sessionId"] = json!(sid);
        }
        let method_owned = method.to_string();
        let inner = async {
            self.ws
                .send(Message::Text(msg.to_string().into()))
                .await
                .context("cdp websocket send failed")?;
            loop {
                let Some(Ok(m)) = self.ws.next().await else {
                    bail!("cdp websocket closed while awaiting {method_owned}");
                };
                let Message::Text(text) = m else { continue };
                let v: Value =
                    serde_json::from_str(text.as_str()).context("cdp message is not valid json")?;
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(err) = v.get("error") {
                        bail!("cdp {method_owned} error: {err}");
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        };
        match tokio::time::timeout(CALL_TIMEOUT, inner).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("cdp {method} timed out after {CALL_TIMEOUT:?}")),
        }
    }

    /// 打开 tab → attach → 反检测注入 → 导航 → 轮询结果节点 → 取渲染 HTML → 关 tab。
    async fn fetch_engine_html(
        &mut self,
        engine_url: &str,
        ready_selector: &str,
        deadline: Instant,
    ) -> anyhow::Result<String> {
        let created = self
            .call("Target.createTarget", json!({ "url": "about:blank" }), None)
            .await?;
        let tid = created
            .get("targetId")
            .and_then(Value::as_str)
            .context("createTarget: no targetId")?
            .to_string();
        let result = self
            .render_and_extract(&tid, engine_url, ready_selector, deadline)
            .await;
        // 无论成败都关 tab，避免临时 tab 堆积。
        let _ = self
            .call("Target.closeTarget", json!({ "targetId": tid }), None)
            .await;
        result
    }

    async fn render_and_extract(
        &mut self,
        tid: &str,
        engine_url: &str,
        ready_selector: &str,
        deadline: Instant,
    ) -> anyhow::Result<String> {
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": tid, "flatten": true }),
                None,
            )
            .await?;
        let sid = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .context("attachToTarget: no sessionId")?
            .to_string();

        self.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": WEBDRIVER_HIDE }),
            Some(&sid),
        )
        .await?;
        self.call(
            "Network.setUserAgentOverride",
            json!({ "userAgent": DESKTOP_UA }),
            Some(&sid),
        )
        .await?;
        self.call("Page.navigate", json!({ "url": engine_url }), Some(&sid))
            .await?;

        loop {
            if Instant::now() >= deadline {
                bail!("render timeout waiting for '{ready_selector}'");
            }
            let ready = self
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": format!("document.querySelectorAll('{ready_selector}').length"),
                        "returnByValue": true,
                    }),
                    Some(&sid),
                )
                .await?;
            if ready
                .pointer("/result/value")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
            {
                break;
            }
            tokio::time::sleep(RENDER_POLL_INTERVAL).await;
        }
        tokio::time::sleep(SNIPPET_SETTLE).await;

        let ev = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": "document.documentElement.outerHTML",
                    "returnByValue": true,
                }),
                Some(&sid),
            )
            .await?;
        let html = ev
            .pointer("/result/value")
            .and_then(Value::as_str)
            .context("outerHTML missing from Runtime.evaluate result")?;
        Ok(html.to_string())
    }
}

async fn browser_ws_url(client: &reqwest::Client, cdp_url: &str) -> anyhow::Result<String> {
    let url = format!("{}/json/version", cdp_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .with_context(|| {
            format!(
                "chrome CDP unreachable at {cdp_url} (is llm-provider-router-chrome.service running?)"
            )
        })?;
    if !resp.status().is_success() {
        bail!("chrome CDP {url} returned {}", resp.status());
    }
    let payload: Value = resp
        .json()
        .await
        .context("chrome CDP version parse failed")?;
    payload
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("chrome CDP version: no webSocketDebuggerUrl")
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 按引擎顺序渲染 + 解析，第一个产出非空结果的引擎胜出。
/// 全部失败/空结果时返回错误（错误信息含每个引擎的失败原因，便于排障）。
pub async fn search(
    cfg: &ChromeSearchConfig,
    engines: &[String],
    client: &reqwest::Client,
    req: &crate::search::UnifiedSearchRequest,
) -> anyhow::Result<Value> {
    let query = req.query.trim();
    if query.is_empty() {
        bail!("query must not be empty");
    }
    let max_results = req.normalized_max_results();
    let ws_url = browser_ws_url(client, &cfg.cdp_url).await?;
    let mut cdp = Cdp::connect(&ws_url).await?;
    let deadline = Instant::now() + Duration::from_millis(cfg.timeout_ms.max(3_000));

    let mut failures: Vec<String> = Vec::new();
    for engine_name in engines {
        let Some(engine) = ENGINES.iter().find(|e| e.name == engine_name.as_str()) else {
            failures.push(format!("{engine_name}: unknown engine"));
            continue;
        };
        let url = engine
            .url
            .replace("{q}", &uri_encode(query))
            .replace("{n}", &max_results.to_string());
        match cdp
            .fetch_engine_html(&url, engine.ready_selector, deadline)
            .await
        {
            Ok(html) => {
                let results = (engine.parser)(&html);
                if !results.is_empty() {
                    return Ok(json!({
                        "provider": format!("chrome-{}", engine.name),
                        "query": req.query,
                        "results": results,
                    }));
                }
                failures.push(format!("{}: 0 parsed results", engine.name));
            }
            Err(e) => failures.push(format!("{}: {e:#}", engine.name)),
        }
    }
    Err(anyhow!(
        "chrome search exhausted {} (engines: {})",
        if failures.is_empty() {
            "without attempts"
        } else {
            "with failures"
        },
        if failures.is_empty() {
            "none".into()
        } else {
            failures.join("; ")
        }
    ))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_basic() {
        assert_eq!(uri_encode("rust axum"), "rust%20axum");
        assert_eq!(uri_encode("c++"), "c%2B%2B");
        assert_eq!(uri_encode("a.b_c-d~e"), "a.b_c-d~e");
    }

    #[test]
    fn un_google_cite_cases() {
        assert_eq!(
            un_google_cite("https://docs.rs › axum › struct.Json.html"),
            Some("https://docs.rs/axum/struct.Json.html".into())
        );
        // 时间戳 cite（中文环境）
        assert_eq!(un_google_cite("1年前"), None);
        // 被截断的 cite
        assert_eq!(
            un_google_cite("https://mojoauth.com › dev-guides › parse-..."),
            None
        );
        assert_eq!(
            un_google_cite("https://mojoauth.com › dev-guides › parse-…"),
            None
        );
    }

    #[test]
    fn parse_bing_fixture() {
        let html = include_str!("../fixtures/chrome-search-bing.html");
        let results = parse_bing(html);
        assert!(
            results.len() >= 8,
            "expected >=8 bing results, got {}",
            results.len()
        );
        assert!(results.iter().all(|r| r.url.starts_with("http")));
        assert!(results.iter().any(|r| r.snippet.is_some()));
        // fixture 对应查询 "tokio select macro example"，首条为 tokio.rs 官网
        assert!(
            results[0].url.contains("tokio.rs"),
            "first result: {}",
            results[0].url
        );
    }

    #[test]
    fn parse_google_fixture_cite_restore() {
        let html = include_str!("../fixtures/chrome-search-google.html");
        let results = parse_google(html);
        // 8 块中部分 cite 被截断/替换为时间戳，对应结果被正确丢弃，≥4 即符合预期
        assert!(
            results.len() >= 4,
            "expected >=4 google results, got {}",
            results.len()
        );
        // goto 加密链接必须全部被 cite 还原成明文 URL
        assert!(results.iter().all(|r| r.url.starts_with("http")));
        assert!(results.iter().all(|r| !r.url.starts_with("/goto")));
        assert!(results.iter().any(|r| r.url.contains("docs.rs")));
    }

    #[test]
    fn parse_google_skips_non_url_results() {
        // cite 数量与结果数不构成 1:1/2:1 时（stride=0），goto 结果应被整体丢弃而非误还原
        let html = r#"<html><body><div id="rso">
          <div><div class="yuRUbf"><a href="/goto?url=XYZ"><h3>Title A</h3></a></div></div>
        </div></body></html>"#;
        let results = parse_google(html);
        assert!(results.is_empty());
    }
}
