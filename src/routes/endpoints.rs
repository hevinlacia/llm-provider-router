//! 客户端访问端点候选表（`GET/PUT /api/router/endpoints`）。
//!
//! 背景：每台机器访问 router 的最优路径是**机器属性**而非全局属性——同一套代码在
//! LAN 机直连内网、跨网机走 Tailscale、外部机走公网兜底。本表由 router 统一管理
//! 候选清单与全局默认优先级（数组顺序即优先级），各机器的解析器
//! `~/Developer/.agents/scripts/router_url.py` 拉取后按本机覆盖优先级探活选路，
//! 胜出地址写本机缓存供 pi extension / systemd 消费（与 Agent Panel 的
//! `/api/panel/endpoints` + `panel_url.py` 同一套机制）。
//!
//! - 数据文件：`<state_dir>/router-endpoints.json`（state 目录，与 state.sqlite3 同级，
//!   平台配置不进 git；文件缺失时 GET 回落内置默认表，PUT 即恢复）。
//! - `url` 为空 = 该候选停用（保留占位，如公网未启用）。
//! - 内置默认表：内网域名 → tailscale 域名 → 公网 → subnet 中转（最低），见
//!   `default_endpoints_table` 的逐条说明。
//!
//! 注意 tailscale 候选带**子路径前缀** `/llm-router`（Caddy `uri strip_prefix` 后转
//! 内网后端），解析器拿到 base 后照常拼 `/v1`、`/api/router/*`、`/health`。
//!
//! 候选表两条硬约束（2026-10-28 修正）：
//! 1. **不写服务容器地址**（`192.168.6.245:8789`）：客户端一律走 edge 容器入口，由 edge
//!    统一转发——直连服务容器绕过入口层（子路径/压缩/守卫/flush 策略都在 edge）。
//! 2. **LAN 用域名而非内网 IP**：内网由 immortalwrt 提供 `.lan` 域名；且从 tailnet 机器
//!    访问 `192.168.6.x` 实际走 subnet 中转（≠ 真 LAN），必须单独作为最低优先级候选。

use std::path::{Path, PathBuf};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::config::expand_path;

/// 单条候选：`name` 是解析器/本机覆盖引用的稳定标识，`url` 空 = 停用。
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct RouterEndpointEntry {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) note: String,
}

/// 候选表：数组顺序 = 全局默认优先级。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct RouterEndpointsTable {
    #[serde(default)]
    pub(crate) endpoints: Vec<RouterEndpointEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
}

/// 内置默认表：四通道，数组顺序即优先级（2026-10-28 按网络实际形态定）：
///
/// 1. `lan`     内网域名（immortalwrt `.lan` → edge 容器）——真 LAN 路径，不走 subnet 中转
/// 2. `tailscale` Tailscale 域名（HTTPS + Caddy 子路径）
/// 3. `public`  公网兜底（当前停用占位：证书续期未修好）
/// 4. `subnet`  Tailscale subnet 中转兜底（经 immortalwrt 广播 192.168.6.0/24 访问 edge
///    内网 IP）——**最低优先级**：它看起来像 LAN，实际绕了 subnet 路由，只在真 LAN 域名
///    解析不到且 tailscale 域名不可用时才该命中
pub(crate) fn default_endpoints_table() -> RouterEndpointsTable {
    RouterEndpointsTable {
        endpoints: vec![
            RouterEndpointEntry {
                name: "lan".into(),
                url: "http://edge.lan/llm-router".into(),
                note: "内网域名 80 端口路径式（immortalwrt .lan → edge 容器，真 LAN 不走 subnet）".into(),
            },
            RouterEndpointEntry {
                name: "tailscale".into(),
                url: "https://edge.tail260d93.ts.net/llm-router".into(),
                note: "Tailscale 域名（HTTPS，Caddy strip_prefix 子路径）".into(),
            },
            RouterEndpointEntry {
                name: "public".into(),
                url: String::new(),
                note: "公网兜底（未启用：需 TLS + 鉴权，启用时填 URL）".into(),
            },
            RouterEndpointEntry {
                name: "subnet".into(),
                url: "http://192.168.6.249/llm-router".into(),
                note: "subnet 中转兜底（经 Tailscale 访问 edge 内网 IP 80 路径式，最低优先级）".into(),
            },
        ],
        updated_at: None,
    }
}

/// 候选表文件路径：与 state.sqlite3 同目录（`state_db_path` 的父目录）。
pub(crate) fn endpoints_path_for(state_db_path: &str) -> PathBuf {
    let db = expand_path(state_db_path);
    match db.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("router-endpoints.json"),
        _ => PathBuf::from("router-endpoints.json"),
    }
}

fn endpoints_path(state: &AppState) -> PathBuf {
    endpoints_path_for(&state.settings.state_db_path)
}

/// 单条候选校验：name 非空且唯一（调用方保证）、url 空（停用）或 http(s)。
pub(crate) fn validate_endpoint(entry: &RouterEndpointEntry) -> Result<(), String> {
    let name = entry.name.trim();
    if name.is_empty() {
        return Err("endpoint.name 不能为空".to_string());
    }
    if name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "endpoint.name `{name}` 不合法（须 ^[A-Za-z0-9_-]{{1,64}}$）"
        ));
    }
    let url = entry.url.trim();
    if url.is_empty() {
        return Ok(()); // 停用占位
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!(
            "endpoint `{name}` 的 url 必须以 http:// 或 https:// 开头（`{url}`）"
        ));
    }
    Ok(())
}

/// 整表校验：逐条校验 + name 唯一。
pub(crate) fn validate_table(table: &RouterEndpointsTable) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for entry in &table.endpoints {
        validate_endpoint(entry)?;
        let name = entry.name.trim().to_string();
        if !seen.insert(name.clone()) {
            return Err(format!("endpoint.name `{name}` 重复"));
        }
    }
    Ok(())
}

/// 读表：文件缺失/损坏 → 内置默认（fail-open，解析器永远拿得到候选）。
pub(crate) fn load_table(path: &Path) -> (RouterEndpointsTable, &'static str) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (default_endpoints_table(), "default");
    };
    match serde_json::from_str::<RouterEndpointsTable>(&raw) {
        Ok(table) if validate_table(&table).is_ok() && !table.endpoints.is_empty() => {
            (table, "file")
        }
        _ => (default_endpoints_table(), "default"),
    }
}

/// 写表：原子替换（tmp + rename），调用方须先 validate_table。
pub(crate) fn save_table(path: &Path, table: &RouterEndpointsTable) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(table).unwrap_or_else(|_| "{}".to_string())
    );
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

fn rfc3339_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Debug, Deserialize)]
pub(crate) struct RouterEndpointsUpdate {
    pub(crate) endpoints: Vec<RouterEndpointEntry>,
}

/// GET /api/router/endpoints：当前候选表 + 来源（file/default）+ 解析器契约说明。
pub(crate) async fn api_router_endpoints(State(state): State<AppState>) -> Json<Value> {
    let path = endpoints_path(&state);
    let (table, source) = load_table(&path);
    Json(json!({
        "ok": true,
        "endpoints": table.endpoints,
        "updatedAt": table.updated_at,
        "source": source,
        "path": path.to_string_lossy(),
        "usage": {
            "order": "数组顺序 = 全局默认优先级；url 空 = 停用占位",
            "resolver": "~/Developer/.agents/scripts/router_url.py（本机可覆盖优先级/钉死地址）",
            "baseSemantics": "候选 url 即 base（含可选子路径前缀）；客户端自行拼 /v1、/api/router/*、/health",
            "cache": "~/.cache/llm-router/router-url.json（解析器探活回填，pi extension 读它）",
        },
    }))
}

/// PUT /api/router/endpoints：整表替换（数组顺序即优先级）。校验失败 400 不落盘。
pub(crate) async fn api_router_endpoints_update(
    State(state): State<AppState>,
    Json(payload): Json<RouterEndpointsUpdate>,
) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let mut table = RouterEndpointsTable {
        endpoints: payload.endpoints,
        updated_at: Some(rfc3339_now()),
    };
    for entry in &mut table.endpoints {
        entry.name = entry.name.trim().to_string();
        entry.url = entry.url.trim().trim_end_matches('/').to_string();
    }
    if let Err(reason) = validate_table(&table) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": reason })),
        ));
    }
    let path = endpoints_path(&state);
    if let Err(e) = save_table(&path, &table) {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": format!("写候选表失败: {e}") })),
        ));
    }
    eprintln!(
        "[endpoints] 候选表已更新: path={} count={}",
        path.display(),
        table.endpoints.len()
    );
    Ok(Json(json!({
        "ok": true,
        "endpoints": table.endpoints,
        "updatedAt": table.updated_at,
        "path": path.to_string_lossy(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "router-endpoints-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    #[test]
    fn default_table_has_four_channel_priority_order() {
        let table = default_endpoints_table();
        let names: Vec<&str> = table.endpoints.iter().map(|e| e.name.as_str()).collect();
        // 优先级：内网域名 > tailscale 域名 > 公网 > subnet 中转（最低）
        assert_eq!(names, vec!["lan", "tailscale", "public", "subnet"]);
        assert_eq!(
            table.endpoints[0].url, "http://edge.lan/llm-router",
            "LAN 走内网 .lan 域名 80 端口路径式（真 LAN，不走 subnet）"
        );
        assert_eq!(
            table.endpoints[1].url,
            "https://edge.tail260d93.ts.net/llm-router"
        );
        assert!(table.endpoints[2].url.is_empty(), "public 默认停用占位");
        assert_eq!(
            table.endpoints[3].url, "http://192.168.6.249/llm-router",
            "subnet 兜底指向 edge 内网 IP（80 路径式）且排最后"
        );
        // 硬约束：候选表不得直连服务容器
        assert!(
            table
                .endpoints
                .iter()
                .all(|e| !e.url.contains("192.168.6.245")),
            "候选表不得直连服务容器"
        );
        assert!(validate_table(&table).is_ok());
    }

    #[test]
    fn load_falls_back_to_default_when_missing_or_corrupt() {
        let dir = tmp_dir();
        let path = dir.join("router-endpoints.json");
        let (table, source) = load_table(&path);
        assert_eq!(source, "default");
        assert_eq!(table.endpoints.len(), 4);

        std::fs::write(&path, "{not-json").unwrap();
        let (_, source) = load_table(&path);
        assert_eq!(source, "default", "损坏文件回落内置默认");

        // 非法内容（url 协议错）也回落
        std::fs::write(&path, r#"{"endpoints":[{"name":"lan","url":"ftp://x"}]}"#).unwrap();
        let (_, source) = load_table(&path);
        assert_eq!(source, "default");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tmp_dir();
        let path = dir.join("router-endpoints.json");
        let mut table = default_endpoints_table();
        table.endpoints[2].url = "https://llmrouter.example.com".into();
        table.updated_at = Some("2026-10-28T09:00:00Z".into());
        save_table(&path, &table).expect("save");
        let (loaded, source) = load_table(&path);
        assert_eq!(source, "file");
        assert_eq!(loaded.endpoints[2].url, "https://llmrouter.example.com");
        assert_eq!(loaded.updated_at.as_deref(), Some("2026-10-28T09:00:00Z"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_rejects_bad_name_url_and_duplicates() {
        let bad_name = RouterEndpointEntry {
            name: "bad name!".into(),
            url: "http://x".into(),
            note: String::new(),
        };
        assert!(validate_endpoint(&bad_name).is_err());
        let bad_url = RouterEndpointEntry {
            name: "lan".into(),
            url: "192.168.6.245:8789".into(),
            note: String::new(),
        };
        assert!(validate_endpoint(&bad_url).is_err(), "缺协议须拒绝");
        let disabled = RouterEndpointEntry {
            name: "public".into(),
            url: String::new(),
            note: String::new(),
        };
        assert!(validate_endpoint(&disabled).is_ok(), "空 url = 停用占位");
        let dup = RouterEndpointsTable {
            endpoints: vec![
                RouterEndpointEntry {
                    name: "lan".into(),
                    url: "http://a:1".into(),
                    note: String::new(),
                },
                RouterEndpointEntry {
                    name: "lan".into(),
                    url: "http://b:1".into(),
                    note: String::new(),
                },
            ],
            updated_at: None,
        };
        let err = validate_table(&dup).unwrap_err();
        assert!(err.contains("重复"), "{err}");
    }

    /// 候选表路径与 state.sqlite3 同目录（解析器靠它定位本机表副本）。
    #[test]
    fn endpoints_path_sits_next_to_state_db() {
        let p = endpoints_path_for("/var/lib/router/state.sqlite3");
        assert_eq!(p, PathBuf::from("/var/lib/router/router-endpoints.json"));
        let p = endpoints_path_for(":memory:");
        assert!(p.ends_with("router-endpoints.json"));
    }
}
