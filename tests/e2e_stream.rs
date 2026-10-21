//! 全链路集成测试：真实二进制（backend + front-proxy）+ 进程内 mock 上游。
//!
//! 背景（2026-10-21 两起部署事故）：
//! 1. 透传守卫冲刷时当前 chunk 被重复发送 → 长流 SSE 字节损坏，单组件测试未暴露；
//! 2. 后端开启 gzip 后 front-proxy 剥掉 content-encoding 头 → 浏览器拿到 gzip body
//!    按明文解析，dashboard 全挂。
//!
//! 因此本文件的验收口径是“客户端可见结果”，不信任任何单组件测试：
//! - SSE 流经 full chain（守卫 → 路由 → front-proxy）后与上游字节**逐字节恒等**，
//!   且每个 data 行可解析、事件无重复；
//! - 压缩语义经 front-proxy 后正确（SSE 永不压缩；JSON 压缩带头且可解压）。
//!
//! 拓扑：mock upstream(进程内 axum, 随机端口) ← backend 真实二进制(随机端口)
//!       ← front-proxy 真实二进制(随机端口) ← 测试内 reqwest 客户端。

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use futures_util::stream::iter;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_llm-provider-router");

/// mock 上游的 SSE 响应：4 个 chunk，故意在行中间截断（2026-10-21 事故场景）。
const MOCK_CHUNKS: [&str; 4] = [
    // chunk1: 两个完整元事件行 + 事件行截断
    "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\nevent: response.in_progress\ndata: {\"type\":\"response.in_prog",
    // chunk2: 接上行中间 + 完整行
    "ress\",\"sequence_number\":1}\n\n",
    // chunk3: 内容事件（触发守卫冲刷）+ 下一个内容行截断（UTF-8 中文跨 chunk）
    "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"你好，仓库\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"sequ",
    // chunk4: 接上行中间 + 终态事件 + DONE
    "ence_number\":3,\"delta\":\"管理系统!\"}\n\ndata: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\ndata: [DONE]\n\n",
];

fn mock_full_body() -> String {
    MOCK_CHUNKS.concat()
}

/// 占一个空闲端口并立即释放（存在极小竞态，测试可接受）。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// mock 上游：POST /responses 按 MOCK_CHUNKS 切片回放 SSE。
async fn spawn_mock_upstream() -> SocketAddr {
    async fn sse() -> Response {
        let chunks: Vec<Bytes> = MOCK_CHUNKS.iter().map(|c| Bytes::from(*c)).collect();
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(iter(
                chunks
                    .into_iter()
                    .map(Ok::<Bytes, std::convert::Infallible>),
            )))
            .unwrap()
    }
    let app = Router::new()
        .route("/responses", post(sse))
        .route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// 写最小可用的 config/ 目录：一个 provider(mock) + 一个物理模型 + 一个逻辑模型。
fn write_config_tree(root: &Path, mock_base: &str) {
    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("providers-v2.json"),
        format!(
            r#"{{"providers":{{"mock":{{"base_url":"","responses_base_url":"{mock_base}","keys":{{"mock-key":{{"env_var":"E2E_MOCK_KEY","weight":1,"billing_type":"payg","enabled":true,"persist":true}}}}}}}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        cfg.join("models.json"),
        r#"{"families":{},"models":{"mock/glm-test":{"provider":"mock","upstream_model":"glm-test","family":null,"params":{},"context_window":1000000,"max_output_tokens":128000,"supports_image":null,"thinking_level_map":null,"thinking_format":null}}}"#,
    )
    .unwrap();
    std::fs::write(
        cfg.join("logical-models.json"),
        r#"{"logical_models":{"e2e-auto":{"params":{},"route":{"strategy":"priority","targets":[{"model":"mock/glm-test","weight":null,"enabled":true,"keys":null}]},"display_name":null}}}"#,
    )
    .unwrap();
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TestEnv {
    _tmp: TempDir,
    _backend: ChildGuard,
    _proxy: ChildGuard,
    proxy_url: String,
    client: reqwest::Client,
}

async fn wait_healthy(url: &str, label: &str, log_dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let client = reqwest::Client::new();
    loop {
        if let Ok(resp) = client.get(url).timeout(Duration::from_secs(1)).send().await {
            if resp.status().is_success() {
                return;
            }
        }
        if Instant::now() >= deadline {
            let mut dump = String::new();
            for name in [
                "backend-err.log",
                "backend-out.log",
                "proxy-err.log",
                "proxy-out.log",
            ] {
                if let Ok(content) = std::fs::read_to_string(log_dir.join(name)) {
                    dump.push_str(&format!("--- {name} ---\n{content}\n"));
                }
            }
            panic!("{label} 未在超时内就绪: {url}\n子进程日志:\n{dump}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn setup_env() -> TestEnv {
    let mock_addr = spawn_mock_upstream().await;
    let mock_base = format!("http://{mock_addr}");
    let tmp = tempfile::tempdir().unwrap();
    write_config_tree(tmp.path(), &mock_base);

    let backend_port = free_port();
    let proxy_port = free_port();
    let active_file = tmp.path().join("active-backend.json");
    std::fs::write(
        &active_file,
        format!(r#"{{"slot":"blue","base_url":"http://127.0.0.1:{backend_port}","updated_at":0}}"#),
    )
    .unwrap();

    let log = |name: &str| std::fs::File::create(tmp.path().join(format!("{name}.log"))).unwrap();
    let mut backend = Command::new(BIN)
        .args(["backend"])
        .current_dir(tmp.path())
        .env("LLM_PROVIDER_ROUTER_HOST", "127.0.0.1")
        .env("LLM_PROVIDER_ROUTER_PORT", backend_port.to_string())
        .env(
            "LLM_PROVIDER_ROUTER_USAGE_DB_PATH",
            tmp.path().join("usage.sqlite3"),
        )
        .env("LLM_PROVIDER_ROUTER_REQUEST_TIMEOUT_SECONDS", "30")
        .env("E2E_MOCK_KEY", "dummy-key")
        .stdout(Stdio::from(log("backend-out")))
        .stderr(Stdio::from(log("backend-err")))
        .spawn()
        .map(ChildGuard)
        .unwrap();

    let mut proxy = Command::new(BIN)
        .args(["front-proxy"])
        .current_dir(tmp.path())
        .env("LLM_PROVIDER_ROUTER_PROXY_HOST", "127.0.0.1")
        .env("LLM_PROVIDER_ROUTER_PROXY_PORT", proxy_port.to_string())
        .env(
            "LLM_PROVIDER_ROUTER_BLUE_URL",
            format!("http://127.0.0.1:{backend_port}"),
        )
        .env("LLM_PROVIDER_ROUTER_GREEN_URL", "http://127.0.0.1:1")
        .env("LLM_PROVIDER_ROUTER_ACTIVE_BACKEND_FILE", &active_file)
        .env("LLM_PROVIDER_ROUTER_SLOT_AUTO_HEAL", "0")
        .stdout(Stdio::from(log("proxy-out")))
        .stderr(Stdio::from(log("proxy-err")))
        .spawn()
        .map(ChildGuard)
        .unwrap();

    let backend_url = format!("http://127.0.0.1:{backend_port}");
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    wait_healthy(&format!("{backend_url}/health"), "backend", tmp.path()).await;
    wait_healthy(&format!("{proxy_url}/health"), "front-proxy", tmp.path()).await;

    TestEnv {
        _tmp: tmp,
        _backend: backend,
        _proxy: proxy,
        proxy_url,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap(),
    }
}

/// 回归（2026-10-21 事故一）：SSE 流经 full chain 后与上游字节逐字节恒等，
/// 每个 data 行可解析、事件无重复、终态事件存在。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_sse_byte_integrity_through_full_chain() {
    let env = setup_env().await;
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.proxy_url))
        .json(&serde_json::json!({"model": "e2e-auto", "stream": true, "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "status={}", resp.status());

    let body = resp.bytes().await.unwrap();
    // 1) 字节级恒等：路由与代理不得增删改任何字节
    assert_eq!(
        body.as_ref(),
        mock_full_body().as_bytes(),
        "客户端可见流与上游字节不一致"
    );

    // 2) 逐行解析 + 事件无重复
    let text = String::from_utf8(body.to_vec()).unwrap();
    let mut types = Vec::new();
    for line in text.lines().filter(|l| l.starts_with("data: ")) {
        let payload = &line["data: ".len()..];
        if payload == "[DONE]" {
            types.push("[DONE]".to_string());
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(payload)
            .unwrap_or_else(|e| panic!("损坏的 SSE JSON 行: {e}: {payload}"));
        types.push(v["type"].as_str().unwrap_or("?").to_string());
    }
    assert_eq!(types.iter().filter(|t| *t == "response.created").count(), 1);
    assert_eq!(types.last().map(String::as_str), Some("[DONE]"));
    for expected in [
        "response.created",
        "response.in_progress",
        "response.output_text.delta",
        "response.completed",
    ] {
        let n = types.iter().filter(|t| *t == expected).count();
        let want = if expected == "response.output_text.delta" {
            2
        } else {
            1
        };
        assert_eq!(n, want, "事件 {expected} 出现次数不符: {n} != {want}");
    }
}

/// 回归（2026-10-21 事故一）：SSE 永不被压缩——即使客户端带 Accept-Encoding: gzip。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_sse_never_compressed() {
    let env = setup_env().await;
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.proxy_url))
        .header("Accept-Encoding", "gzip")
        .json(&serde_json::json!({"model": "e2e-auto", "stream": true, "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert!(
        resp.headers().get("content-encoding").is_none(),
        "SSE 响应不得带 content-encoding"
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), mock_full_body().as_bytes());
}

/// 回归（2026-10-21 事故二）：router 自身 JSON 经压缩 + front-proxy 转发后，
/// content-encoding 头必须原样到达，且 body 可解压。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_json_compressed_and_forwarded_through_proxy() {
    let env = setup_env().await;
    let resp = env
        .client
        .get(format!("{}/api/state", env.proxy_url))
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
        Some("gzip"),
        "JSON 响应应带 gzip 压缩头（front-proxy 不得剥掉）"
    );
    let bytes = resp.bytes().await.unwrap();
    let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut decoder, &mut raw).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).expect("解压后应为合法 JSON");
    assert!(v.get("ok").is_some() || v.is_object());
}

/// 压缩语义反例：不带 Accept-Encoding 时不得压缩。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_json_uncompressed_without_accept_encoding() {
    let env = setup_env().await;
    let resp = env
        .client
        .get(format!("{}/api/state", env.proxy_url))
        .send()
        .await
        .unwrap();
    assert!(resp.headers().get("content-encoding").is_none());
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v.is_object());
}
