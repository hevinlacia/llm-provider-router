//! 翻译路径全链路集成测试：真实二进制（backend + front-proxy）+ 进程内 mock chat 上游。
//!
//! 背景（REQ-323，2026-11-19）：翻译分支（responses_passthrough=false，opencode-go
//! 走 /v1/responses → chat completions 翻译）存在两处缺陷，透传路径的 e2e_stream.rs
//! 覆盖不到：
//! 1. 逐 chunk 解析 SSE：TCP 分片切的半行被静默丢弃（丢 content/tool_calls/
//!    finish_reason → 收尾降级 incomplete/interrupted），汉字切半产生 U+FFFD；
//! 2. responses_to_chat 不合并 assistant 回合：连续 assistant 序列 + 并行工具调用
//!    拆成多条 content:null 单调用消息（部分上游只认回合边界会裁掉意图叙述）。
//!
//! 验收口径（AGENTS.md：不信任单组件测试）：客户端经 front-proxy 可见的 SSE 必须完整，
//! 且 router 发往上游的 chat 载荷必须是合法回合形状。mock 上游按固定字节切片回放
//! SSE（行中间 + UTF-8 序列中间截断），并记录收到的 chat 请求体供断言。

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::stream::iter;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_llm-provider-router");

/// mock 上游的 chat SSE：叙述（含中文）+ 工具调用 + finish_reason=tool_calls + usage。
/// 发送时按 [`CHUNK_BYTES`] 字节切片，模拟真实 TCP 分片（必然切在行中间与汉字中间）。
const CHAT_SSE: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":"先看目录："},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":"现在建文件："},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"bash","arguments":"{\"command\":\"ls /tmp\"}"}}]},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":null}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// 分片粒度：37 字节必然把 SSE 行与汉字（3 字节 UTF-8）切在中间。
const CHUNK_BYTES: usize = 37;

/// pi 多步会话的真实历史形状：叙述与工具调用分项、并行调用（同回合 2 个 fc）。
fn pi_shaped_request() -> serde_json::Value {
    let mut input = vec![serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": "在 /tmp 下建两个文件"}]
    })];
    for (i, (narrative, calls)) in [
        ("先看目录：", vec![("ls_a", "{}")]),
        ("现在建文件：", vec![("touch_b", "{}"), ("touch_c", "{}")]),
    ]
    .into_iter()
    .enumerate()
    {
        if !narrative.is_empty() {
            input.push(serde_json::json!({
                "type": "message", "role": "assistant", "status": "completed",
                "id": format!("msg_{i}"),
                "content": [{"type": "output_text", "text": narrative, "annotations": []}]
            }));
        }
        for (j, (name, args)) in calls.iter().enumerate() {
            input.push(serde_json::json!({
                "type": "function_call",
                "call_id": format!("call_{i}_{j}"), "name": name, "arguments": args,
                "id": format!("fc_{i}_{j}")
            }));
        }
        for (j, _) in calls.iter().enumerate() {
            input.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": format!("call_{i}_{j}"), "output": "ok"
            }));
        }
    }
    input.push(serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": "总结一下"}]
    }));
    serde_json::json!({
        "model": "e2e-auto",
        "input": input,
        "stream": true,
        "tools": [
            {"type":"function","name":"ls_a","parameters":{"type":"object"}},
            {"type":"function","name":"touch_b","parameters":{"type":"object"}},
            {"type":"function","name":"touch_c","parameters":{"type":"object"}}
        ]
    })
}

fn chunked_sse_response(body: &[u8]) -> Response {
    let chunks: Vec<Bytes> = body
        .chunks(CHUNK_BYTES)
        .map(|c| Bytes::copy_from_slice(c))
        .collect();
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

/// mock chat 上游：按 CHUNK_BYTES 切片回放 CHAT_SSE，并记录收到的 chat 请求体。
async fn spawn_chat_mock_upstream() -> (SocketAddr, Arc<Mutex<Vec<serde_json::Value>>>) {
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let received_for_handler = received.clone();
    let app = Router::new()
        .route(
            "/chat/completions",
            post(move |Json(payload): Json<serde_json::Value>| {
                let received = received_for_handler.clone();
                async move {
                    received.lock().unwrap().push(payload);
                    chunked_sse_response(CHAT_SSE.as_bytes())
                }
            }),
        )
        .route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, received)
}

/// 写最小可用 config/：provider 只有 base_url（无 responses_base_url）→ 强制翻译模式。
fn write_translate_config_tree(root: &Path, mock_base: &str) {
    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("api-keys.json"), r#"{"mock-key":"dummy-key"}"#).unwrap();
    // 只配 base_url：supports_responses()=false → passthrough_responses()=false → 翻译分支
    std::fs::write(
        cfg.join("providers-v2.json"),
        format!(
            r#"{{"providers":{{"mock":{{"base_url":"{mock_base}","keys":{{"mock-key":{{"weight":1,"billing_type":"payg","enabled":true}}}}}}}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        cfg.join("models.json"),
        r#"{"families":{},"models":{"mock/chat-test":{"provider":"mock","upstream_model":"chat-test","family":null,"params":{},"context_window":1000000,"max_output_tokens":128000,"supports_image":null,"thinking_level_map":null,"thinking_format":null}}}"#,
    )
    .unwrap();
    std::fs::write(
        cfg.join("logical-models.json"),
        r#"{"logical_models":{"e2e-auto":{"params":{},"route":{"strategy":"priority","targets":[{"model":"mock/chat-test","weight":null,"enabled":true,"keys":null}]},"display_name":null}}}"#,
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
    _serial: tokio::sync::MutexGuard<'static, ()>,
    proxy_url: String,
    client: reqwest::Client,
}

fn env_serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 占一个空闲端口并立即释放（存在极小竞态，配合串行锁可接受）。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local_addr")
        .port()
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

async fn setup_env(mock_addr: SocketAddr) -> TestEnv {
    let serial = env_serial_lock().lock().await;
    let mock_base = format!("http://{mock_addr}");
    let tmp = tempfile::tempdir().unwrap();
    write_translate_config_tree(tmp.path(), &mock_base);

    let backend_port = free_port();
    let proxy_port = free_port();
    let active_file = tmp.path().join("active-backend.json");
    std::fs::write(
        &active_file,
        format!(r#"{{"slot":"blue","base_url":"http://127.0.0.1:{backend_port}","updated_at":0}}"#),
    )
    .unwrap();

    let log = |name: &str| std::fs::File::create(tmp.path().join(format!("{name}.log"))).unwrap();
    let backend = Command::new(BIN)
        .args(["backend"])
        .current_dir(tmp.path())
        .env("LLM_PROVIDER_ROUTER_HOST", "127.0.0.1")
        .env("LLM_PROVIDER_ROUTER_PORT", backend_port.to_string())
        .env(
            "LLM_PROVIDER_ROUTER_USAGE_DB_PATH",
            tmp.path().join("usage.sqlite3"),
        )
        .env(
            "LLM_PROVIDER_ROUTER_STATE_DB_PATH",
            tmp.path().join("state.sqlite3"),
        )
        .env("LLM_PROVIDER_ROUTER_READ_TIMEOUT_SECONDS", "30")
        .stdout(Stdio::from(log("backend-out")))
        .stderr(Stdio::from(log("backend-err")))
        .spawn()
        .map(ChildGuard)
        .unwrap();

    let proxy = Command::new(BIN)
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
        _serial: serial,
        proxy_url,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap(),
    }
}

/// REQ-323 回归：翻译路径 + 37 字节分片上游 + pi 形状历史（叙述 + 并行调用）。
/// 客户端可见结果必须完整：叙述不丢、工具调用不丢、completed（而非 incomplete）、
/// 无 U+FFFD；发往上游的 chat 载荷必须是合法回合形状（无连续 assistant、并行调用合并）。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_translate_path_survives_chunked_upstream_and_merges_turns() {
    let (mock_addr, received) = spawn_chat_mock_upstream().await;
    let env = setup_env(mock_addr).await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.proxy_url))
        .header(header::CONTENT_TYPE, "application/json")
        .json(&pi_shaped_request())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "status={}", resp.status());
    let text = String::from_utf8(resp.bytes().await.unwrap().to_vec()).unwrap();

    // ---- 客户端可见结果 ----

    // 1) 终态必须是 completed（finish_reason 经分片后仍被解析到），不得降级 incomplete
    assert!(
        text.contains("event: response.completed"),
        "应正常 completed（分片不得丢 finish_reason）: {text}"
    );
    assert!(
        !text.contains("response.incomplete"),
        "分片不得把正常流降级为 incomplete: {text}"
    );

    // 2) 叙述文本完整（跨 chunk 的中文不得丢、不得变 U+FFFD）
    for narrative in ["先看目录：", "现在建文件："] {
        assert!(
            text.contains(narrative),
            "叙述片段 {narrative:?} 经分片后必须仍在客户端可见流中: {text}"
        );
    }
    assert!(
        !text.contains('\u{FFFD}'),
        "客户端可见流不得含 U+FFFD: {text}"
    );

    // 3) 工具调用完整：bash 调用与完整 arguments JSON 送达
    assert!(
        text.contains("\"name\":\"bash\""),
        "工具调用经分片后必须仍在客户端可见流中: {text}"
    );
    assert!(
        text.contains("ls /tmp"),
        "工具参数 JSON 经分片后必须完整: {text}"
    );

    // 4) 每个 data 行都是合法 JSON（无半行损坏）
    for line in text.lines().filter(|l| l.starts_with("data: ")) {
        let payload = line["data: ".len()..].trim();
        if payload == "[DONE]" {
            continue;
        }
        serde_json::from_str::<serde_json::Value>(payload)
            .unwrap_or_else(|e| panic!("损坏的 SSE JSON 行: {e}: {payload}"));
    }

    // ---- 发往上游的 chat 载荷形状 ----

    let payloads = received.lock().unwrap().clone();
    assert_eq!(payloads.len(), 1, "应恰好一次上游请求: {payloads:?}");
    let messages = payloads[0]["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    let consecutive_assistant = roles
        .windows(2)
        .filter(|w| w[0] == "assistant" && w[1] == "assistant")
        .count();
    assert_eq!(
        consecutive_assistant, 0,
        "发往上游的 chat 载荷不得有连续 assistant 消息（pi 形状必须合并回合）: roles={roles:?} messages={messages:#?}"
    );
    // 并行调用（touch_b + touch_c）必须合并在同一条 assistant 的 tool_calls 里
    let merged = messages
        .iter()
        .find(|m| {
            m["tool_calls"]
                .as_array()
                .map(|a| a.len() == 2)
                .unwrap_or(false)
        })
        .expect("并行工具调用必须合并到一条 assistant 消息的 tool_calls 数组");
    let ids: Vec<&str> = merged["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["call_1_0", "call_1_1"],
        "并行调用 id 不符: {merged:#?}"
    );
    // 合并后的 assistant 文本不得是 null（叙述必须保留）
    assert_eq!(
        merged["content"], "现在建文件：",
        "合并回合的叙述不得丢失: {merged:#?}"
    );
}
