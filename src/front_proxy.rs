use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

const DEFAULT_ACTIVE_BACKEND_FILE: &str = "~/.local/state/llm-provider-router/active-backend.json";

/// 请求体上限：`Bytes` extractor 全量缓冲，默认仅 2MB，内嵌 base64 图片的 chat 请求会直接 413。
/// 与后端 routes/mod.rs 的 BODY_LIMIT 保持一致，链路两侧都放开。
const BODY_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Backend {
    slot: String,
    base_url: String,
}

#[derive(Clone)]
struct ProxyState {
    client: reqwest::Client,
}

pub async fn serve() -> anyhow::Result<()> {
    let host = env_or("LLM_PROVIDER_ROUTER_PROXY_HOST", "127.0.0.1");
    let port: u16 = env_or("LLM_PROVIDER_ROUTER_PROXY_PORT", "8789").parse()?;
    let state = ProxyState {
        client: reqwest::Client::builder().build()?,
    };
    let app = Router::new()
        .route("/_proxy/health", get(proxy_health))
        .route("/_proxy/active/{slot}", post(set_active))
        .fallback(any(proxy))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn proxy_health() -> Response {
    let active = read_active_slot();
    let mut probes = Vec::new();
    for backend in ordered_backends() {
        probes.push(health_probe(&backend).await);
    }
    let ok = probes.iter().any(|item| {
        item.get("ok")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
            && item.get("slot").and_then(|value| value.as_str()) == Some(active.as_str())
    });
    Json(json!({
        "ok": ok,
        "active_slot": active,
        "active_backend_file": active_backend_path().to_string_lossy(),
        "backends": probes,
    }))
    .into_response()
}

async fn set_active(Path(slot): Path<String>) -> Response {
    if !configured_backends().contains_key(&slot) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": format!("unknown slot: {slot}") })),
        )
            .into_response();
    }
    match write_active_backend(&slot) {
        Ok(backend) => {
            Json(json!({ "ok": true, "active_slot": slot, "backend": backend.base_url }))
                .into_response()
        }
        Err(exc) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": exc.to_string() })),
        )
            .into_response(),
    }
}

async fn proxy(
    State(state): State<ProxyState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(uri.path());
    let mut errors = Vec::new();
    for backend in ordered_backends() {
        let target = format!(
            "{}{}",
            backend.base_url.trim_end_matches('/'),
            path_and_query
        );
        let request = state
            .client
            .request(method.clone(), target)
            .headers(clean_request_headers(&headers))
            .body(body.clone());
        let response = match request.send().await {
            Ok(response) => response,
            Err(exc) => {
                errors.push(json!({ "slot": backend.slot, "error": exc.to_string() }));
                continue;
            }
        };
        let status = response.status();
        let response_headers = clean_response_headers(response.headers());
        let stream = response.bytes_stream().map(|item| match item {
            Ok(bytes) => Ok::<Bytes, std::convert::Infallible>(bytes),
            Err(exc) => Ok(Bytes::from(format!("\n<!-- proxy stream error: {exc} -->"))),
        });
        let mut builder = Response::builder().status(status);
        for (name, value) in response_headers.iter() {
            builder = builder.header(name, value);
        }
        return builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "message": "failed to build proxy response" } })),
            )
                .into_response()
        });
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": { "message": "no llm-provider-router backend available", "details": errors }
        })),
    )
        .into_response()
}

fn configured_backends() -> HashMap<String, Backend> {
    HashMap::from([
        (
            "blue".to_string(),
            Backend {
                slot: "blue".to_string(),
                base_url: env_or("LLM_PROVIDER_ROUTER_BLUE_URL", "http://127.0.0.1:8790"),
            },
        ),
        (
            "green".to_string(),
            Backend {
                slot: "green".to_string(),
                base_url: env_or("LLM_PROVIDER_ROUTER_GREEN_URL", "http://127.0.0.1:8791"),
            },
        ),
    ])
}

fn read_active_slot() -> String {
    let path = active_backend_path();
    if let Ok(raw) = fs::read_to_string(path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(slot) = value.get("slot").and_then(|value| value.as_str()) {
                if configured_backends().contains_key(slot) {
                    return slot.to_string();
                }
            }
        }
    }
    env_or("LLM_PROVIDER_ROUTER_DEFAULT_SLOT", "blue")
}

fn ordered_backends() -> Vec<Backend> {
    let backends = configured_backends();
    let active = read_active_slot();
    let mut ordered = Vec::new();
    if let Some(backend) = backends.get(&active) {
        ordered.push(backend.clone());
    }
    for (slot, backend) in backends {
        if slot != active {
            ordered.push(backend);
        }
    }
    ordered
}

fn write_active_backend(slot: &str) -> anyhow::Result<Backend> {
    let backend = configured_backends()
        .get(slot)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown backend slot: {slot}"))?;
    let path = active_backend_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = json!({
        "slot": backend.slot,
        "base_url": backend.base_url,
        "updated_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    });
    let tmp_path = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(
        &tmp_path,
        format!("{}\n", serde_json::to_string_pretty(&payload)?),
    )?;
    fs::rename(tmp_path, &path)?;
    Ok(backend)
}

async fn health_probe(backend: &Backend) -> serde_json::Value {
    let url = format!("{}/health", backend.base_url.trim_end_matches('/'));
    match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => match client.get(url).send().await {
            Ok(response) => json!({
                "slot": backend.slot,
                "base_url": backend.base_url,
                "ok": response.status().is_success(),
                "status": response.status().as_u16(),
            }),
            Err(exc) => json!({
                "slot": backend.slot,
                "base_url": backend.base_url,
                "ok": false,
                "error": exc.to_string(),
            }),
        },
        Err(exc) => json!({
            "slot": backend.slot,
            "base_url": backend.base_url,
            "ok": false,
            "error": exc.to_string(),
        }),
    }
}

fn clean_request_headers(headers: &HeaderMap) -> HeaderMap {
    clean_headers(headers, &request_skip_headers())
}

fn clean_response_headers(headers: &HeaderMap) -> HeaderMap {
    clean_headers(headers, &response_skip_headers())
}

fn clean_headers(headers: &HeaderMap, skip: &HashSet<&'static str>) -> HeaderMap {
    let mut result = HeaderMap::new();
    for (name, value) in headers.iter() {
        if !skip.contains(name.as_str()) {
            result.insert(name.clone(), value.clone());
        }
    }
    result
}

fn request_skip_headers() -> HashSet<&'static str> {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
    ]
    .into_iter()
    .collect()
}

fn response_skip_headers() -> HashSet<&'static str> {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
        "content-encoding",
        "date",
        "server",
    ]
    .into_iter()
    .collect()
}

fn active_backend_path() -> PathBuf {
    expand_path(&env_or(
        "LLM_PROVIDER_ROUTER_ACTIVE_BACKEND_FILE",
        DEFAULT_ACTIVE_BACKEND_FILE,
    ))
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(value)
}
