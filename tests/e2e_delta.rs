//! Delta 上下文增量传输的全链路 e2e：真实二进制 + mock 上游 + front-proxy。
//!
//! 覆盖：`x-delta-*` 请求/响应头穿透 front-proxy（黑名单透传）、全量注册 ready 头、
//! delta 重组后上游收到完整 payload、坏 token 429 降级、开关 API 热切换与重协商。
//! 协议语义见 `src/features/responses/delta.rs` 模块文档。
//!
//! 注：mock 上游是独立 python 子进程（不用进程内 axum task）——mock 与测试 runtime
//! 完全隔离，避免 tokio 调度互相干扰；SSE 流式响应的**上游分发是惰性的**
//! （stream_responses_route 把上游请求放在 body stream 里，只有消费 body 才触发），
//! 所以每个流式请求都用 `.bytes().await` 读到流尾再断言 mock.seen()；
//! payload 重组正确性用 mock 侧记录断言，SSE 字节完整性回归归 `e2e_stream.rs`。

use serde_json::{json, Value};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_llm-provider-router");

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// mock 上游：独立 python 子进程。POST /responses 记录 payload 到 seen.jsonl 并回
/// 最小 SSE；GET /healthz 供就绪探测。绑定 :0 后把端口写入 port 文件供测试读取。
const MOCK_PY: &str = r#"
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

SSE = 'event: response.completed\ndata: {"type":"response.completed","sequence_number":0,"response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}\n\ndata: [DONE]\n\n'
SEEN = "__SEEN_PATH__"
PORT = "__PORT_PATH__"

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/healthz":
            self.send_response(200)
            self.send_header("content-length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
        else:
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = self.rfile.read(n)
        with open(SEEN, "ab") as f:
            f.write(body + b"\n")
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(SSE)))
        self.end_headers()
        self.wfile.write(SSE.encode())

    def log_message(self, *a):
        pass

srv = HTTPServer(("127.0.0.1", 0), H)
with open(PORT, "w") as f:
    f.write(str(srv.server_port))
srv.serve_forever()
"#;

struct MockUpstream {
    _child: ChildGuard,
    addr: String,
    seen_path: std::path::PathBuf,
}

impl MockUpstream {
    /// 上游收到的 payload（按到达顺序）。
    fn seen(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.seen_path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("mock seen jsonl"))
            .collect()
    }
}

fn spawn_mock_upstream(root: &Path) -> MockUpstream {
    let seen_path = root.join("mock-seen.jsonl");
    let port_path = root.join("mock-port");
    let script = root.join("mock_server.py");
    let script_src = MOCK_PY
        .replace("__SEEN_PATH__", &seen_path.to_string_lossy())
        .replace("__PORT_PATH__", &port_path.to_string_lossy());
    std::fs::write(&script, script_src).unwrap();
    let child = Command::new("python3")
        .arg(&script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python mock upstream");
    let deadline = Instant::now() + Duration::from_secs(10);
    let port = loop {
        if let Ok(text) = std::fs::read_to_string(&port_path) {
            if let Ok(p) = text.trim().parse::<u16>() {
                break p;
            }
        }
        assert!(
            Instant::now() < deadline,
            "mock upstream did not report port in time"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    MockUpstream {
        _child: ChildGuard(child),
        addr: format!("http://127.0.0.1:{port}"),
        seen_path,
    }
}

fn write_config_tree(root: &Path, mock_base: &str) {
    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("providers-v2.json"),
        format!(
            r#"{{"providers":{{"mock":{{"base_url":"","responses_base_url":"{mock_base}","keys":{{"mock-key":{{"env_var":"E2E_DELTA_MOCK_KEY","weight":1,"billing_type":"payg","enabled":true,"persist":true}}}}}}}}}}"#
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
        r#"{"logical_models":{"delta-e2e-auto":{"params":{},"route":{"strategy":"priority","targets":[{"model":"mock/glm-test","weight":null,"enabled":true,"keys":null}]},"display_name":null}}}"#,
    )
    .unwrap();
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

struct TestEnv {
    _tmp: TempDir,
    _backend: ChildGuard,
    _proxy: ChildGuard,
    mock: MockUpstream,
    proxy_url: String,
    client: reqwest::Client,
}

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
}

async fn setup_env() -> TestEnv {
    let tmp = TempDir::new().unwrap();
    let mock = spawn_mock_upstream(tmp.path());
    write_config_tree(tmp.path(), &mock.addr);
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    let backend_port = free_port();
    let proxy_port = free_port();
    let active_file = state_dir.join("active-backend.json");
    std::fs::write(&active_file, format!(r#"{{"port":{backend_port}}}"#)).unwrap();
    let log_dir = tmp.path().to_path_buf();

    let backend = Command::new(BIN)
        .arg("backend")
        .current_dir(tmp.path())
        .env("LLM_PROVIDER_ROUTER_HOST", "127.0.0.1")
        .env("LLM_PROVIDER_ROUTER_PORT", backend_port.to_string())
        .env("LLM_PROVIDER_ROUTER_USAGE_DB_PATH", ":memory:")
        .env(
            "LLM_PROVIDER_ROUTER_STATE_DB_PATH",
            state_dir.join("state.db"),
        )
        .env("LLM_PROVIDER_ROUTER_REQUEST_TIMEOUT_SECONDS", "30")
        .env("E2E_DELTA_MOCK_KEY", "dummy-key")
        .stdout(Stdio::from(
            std::fs::File::create(log_dir.join("backend-out.log")).unwrap(),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(log_dir.join("backend-err.log")).unwrap(),
        ))
        .spawn()
        .unwrap();

    let proxy = Command::new(BIN)
        .arg("front-proxy")
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
        .stdout(Stdio::from(
            std::fs::File::create(log_dir.join("proxy-out.log")).unwrap(),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(log_dir.join("proxy-err.log")).unwrap(),
        ))
        .spawn()
        .unwrap();

    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    wait_healthy(&format!("{proxy_url}/health"), "front-proxy", &log_dir).await;
    TestEnv {
        _tmp: tmp,
        _backend: ChildGuard(backend),
        _proxy: ChildGuard(proxy),
        mock,
        proxy_url,
        client: test_client(),
    }
}

/// 全量 payload 的构造（step1 与 step6b 重协商共用）。
fn full_payload() -> Value {
    json!({
        "model": "delta-e2e-auto",
        "stream": true,
        "instructions": "sys",
        "input": [{"role": "user", "content": "hi"}]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn delta_full_then_advance_then_miss_through_proxy() {
    let env = setup_env().await;
    let url = format!("{}/v1/responses", env.proxy_url);

    // 1) 全量 + x-delta-capable：响应头 ready/token/base_hash 穿透 front-proxy
    let resp = env
        .client
        .post(&url)
        .header("x-delta-capable", "1")
        .json(&full_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "full request must succeed");
    assert_eq!(
        resp.headers()["x-delta-cache"],
        "ready",
        "ready header must pass through proxy"
    );
    let token = resp.headers()["x-delta-token"]
        .to_str()
        .unwrap()
        .to_string();
    let hash1 = resp.headers()["x-delta-base-hash"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(!token.is_empty() && !hash1.is_empty());
    // 消费 SSE 流到流尾：惰性分发的上游请求只有 body 被轮询才会真正发出。
    let _ = resp.bytes().await.unwrap();
    let seen = env.mock.seen();
    assert_eq!(seen.len(), 1, "upstream saw the full request");
    assert_eq!(seen[0]["input"].as_array().unwrap().len(), 1);

    // 2) delta：上游收到重组后的完整 payload，响应 advance + 新 hash
    let delta = json!({
        "model": "delta-e2e-auto",
        "stream": true,
        "x_delta": {
            "v": 1,
            "token": token,
            "base_hash": hash1,
            "append": [
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "q2"}
            ]
        }
    });
    let resp = env.client.post(&url).json(&delta).send().await.unwrap();
    assert_eq!(resp.status(), 200, "delta request must succeed");
    assert_eq!(resp.headers()["x-delta-cache"], "advance");
    let hash2 = resp.headers()["x-delta-base-hash"]
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(hash1, hash2, "advance must rotate base hash");
    let _ = resp.bytes().await.unwrap();
    let seen = env.mock.seen();
    assert_eq!(seen.len(), 2, "upstream saw the delta request");
    assert_eq!(
        seen[1]["input"].as_array().unwrap().len(),
        3,
        "upstream must receive prefix + append"
    );
    assert_eq!(
        seen[1]["instructions"],
        json!("sys"),
        "instructions restored"
    );
    assert!(
        seen[1].get("x_delta").is_none(),
        "x_delta stripped before forwarding"
    );

    // 3) 坏 token：429 + miss 头（穿 proxy），不转发上游
    let bad = json!({
        "model": "delta-e2e-auto",
        "stream": true,
        "x_delta": {"v": 1, "token": "nope", "base_hash": "x", "append": []}
    });
    let resp = env.client.post(&url).json(&bad).send().await.unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["x-delta-cache"], "miss");
    // miss 是同步 JSON 错误响应（非惰性流），不触发上游；读掉 body 无害且统一。
    let _ = resp.bytes().await.unwrap();
    assert_eq!(env.mock.seen().len(), 2, "miss must not hit upstream");

    // 4) 开关 API：快照含 enabled + 计数器
    let stats: Value = env
        .client
        .get(format!("{}/api/config/delta-cache", env.proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["enabled"], json!(true));
    assert!(stats["stats"]["delta_hits"].as_u64().unwrap() >= 1);

    // 5) 热关闭：delta 一律 miss，全量请求不再下发 ready
    env.client
        .put(format!("{}/api/config/delta-cache", env.proxy_url))
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    let resp = env
        .client
        .post(&url)
        .json(&json!({
            "model": "delta-e2e-auto",
            "stream": true,
            "x_delta": {"v": 1, "token": token, "base_hash": hash2, "append": []}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429, "disabled: delta must miss");
    assert_eq!(resp.headers()["x-delta-cache"], "miss");
    let _ = resp.bytes().await.unwrap();
    let resp = env
        .client
        .post(&url)
        .header("x-delta-capable", "1")
        .json(&full_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("x-delta-cache").is_none(),
        "disabled: no ready header"
    );
    let _ = resp.bytes().await.unwrap();

    // 6) 重新开启：全量重新协商回到 ready
    env.client
        .put(format!("{}/api/config/delta-cache", env.proxy_url))
        .json(&json!({"enabled": true}))
        .send()
        .await
        .unwrap();
    let resp = env
        .client
        .post(&url)
        .header("x-delta-capable", "1")
        .json(&full_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["x-delta-cache"],
        "ready",
        "re-enable: full re-negotiates"
    );
    let _ = resp.bytes().await.unwrap();
}
