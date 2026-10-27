//! 客户端流量统计的全链路 e2e：真实二进制（仅 backend，无需上游/mock）。
//!
//! 覆盖：/v1 请求上行字节记账（Content-Length 路径）、session 归属（prompt_cache_key
//! → extensions → 中间件）、/v1 响应下行逐 chunk 计数、/api/* 与 /v1 的路径隔离、
//! /api/sessions/active 的 traffic 快照合并。速率窗口数学归 src/traffic.rs 单测。
//!
//! 注：未知模型返回 404 错误响应同样计入流量（真实 wire 字节），正好用作无上游
//! 依赖的下行流量源；错误响应同样带 session extensions（handler 统一注入）。

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

fn write_config_tree(root: &Path) {
    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    // provider 指向必不通的地址：本测试不发上游，未知模型在路由层就被 404。
    std::fs::write(
        cfg.join("providers-v2.json"),
        r#"{"providers":{"mock":{"base_url":"","responses_base_url":"http://127.0.0.1:1","keys":{"mock-key":{"env_var":"E2E_TRAFFIC_MOCK_KEY","weight":1,"billing_type":"payg","enabled":true,"persist":true}}}}}"#,
    )
    .unwrap();
    std::fs::write(
        cfg.join("models.json"),
        r#"{"families":{},"models":{"mock/glm-test":{"provider":"mock","upstream_model":"glm-test","family":null,"params":{},"context_window":1000000,"max_output_tokens":128000,"supports_image":null,"thinking_level_map":null,"thinking_format":null}}}"#,
    )
    .unwrap();
    std::fs::write(
        cfg.join("logical-models.json"),
        r#"{"logical_models":{"traffic-e2e-auto":{"params":{},"route":{"strategy":"priority","targets":[{"model":"mock/glm-test","weight":null,"enabled":true,"keys":null}]},"display_name":null}}}"#,
    )
    .unwrap();
}

async fn wait_healthy(url: &str, log_dir: &Path) {
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
            for name in ["backend-err.log", "backend-out.log"] {
                if let Ok(content) = std::fs::read_to_string(log_dir.join(name)) {
                    dump.push_str(&format!("--- {name} ---\n{content}\n"));
                }
            }
            panic!("backend 未在超时内就绪: {url}\n子进程日志:\n{dump}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

struct TestEnv {
    _tmp: TempDir,
    _backend: ChildGuard,
    base_url: String,
    client: reqwest::Client,
}

async fn setup_env() -> TestEnv {
    let tmp = TempDir::new().unwrap();
    write_config_tree(tmp.path());
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    let backend_port = free_port();
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
        .env("E2E_TRAFFIC_MOCK_KEY", "dummy-key")
        .stdout(Stdio::from(
            std::fs::File::create(log_dir.join("backend-out.log")).unwrap(),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(log_dir.join("backend-err.log")).unwrap(),
        ))
        .spawn()
        .unwrap();

    let base_url = format!("http://127.0.0.1:{backend_port}");
    wait_healthy(&format!("{base_url}/health"), &log_dir).await;
    TestEnv {
        _tmp: tmp,
        _backend: ChildGuard(backend),
        base_url,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap(),
    }
}

#[tokio::test]
async fn traffic_up_down_and_session_attribution() {
    let env = setup_env().await;

    // 1) 带 session 的 /v1/responses（未知模型 → 404 错误响应）：
    //    上行按 Content-Length 记账，下行错误体同样按 session 归属计数。
    let up_body = json!({
        "model": "traffic-e2e-auto",
        "input": "hello",
        "prompt_cache_key": "traffic-sess-1",
        "store": false
    });
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.base_url))
        .json(&up_body)
        .send()
        .await
        .unwrap();
    let up_wire = up_body.to_string().len() as u64;
    assert!(
        resp.status().is_client_error(),
        "unknown model should 4xx, got {}",
        resp.status()
    );
    let down_first = resp.bytes().await.unwrap().len() as u64;

    // 2) 无 session 的 /v1 请求（GET /v1/models，200）：只进全局下行。
    let resp = env
        .client
        .get(format!("{}/v1/models", env.base_url))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let models_down = resp.bytes().await.unwrap().len() as u64;
    assert!(models_down > 0);

    // 3) /api/* 管理面不计流量：即便响应体更大，也不得出现在全局下行里。
    //    （先记当前快照基线，请求 /api 后下行不得增长）
    let traffic_before = sessions_traffic(&env).await;
    let before_down = traffic_before["global"]["total_down"].as_u64().unwrap();
    let resp = env
        .client
        .get(format!("{}/api/sessions/active", env.base_url))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let _ = resp.bytes().await.unwrap();
    let traffic_after = sessions_traffic(&env).await;
    let after_down = traffic_after["global"]["total_down"].as_u64().unwrap();
    assert_eq!(
        before_down, after_down,
        "/api/* 响应不应计入客户端流量统计"
    );

    // 4) 断言：上行（session + 全局）、下行（session 归属 + 全局）
    let sess = &traffic_after["sessions"]["traffic-sess-1"];
    assert_eq!(
        sess["total_up"].as_u64().unwrap(),
        up_wire,
        "上行 = Content-Length（wire 字节）"
    );
    assert!(
        sess["total_down"].as_u64().unwrap() >= down_first,
        "session 下行应包含错误响应体"
    );
    let g = &traffic_after["global"];
    assert!(g["total_up"].as_u64().unwrap() >= up_wire);
    assert!(
        g["total_down"].as_u64().unwrap() >= down_first + models_down,
        "全局下行 = session 归属 + 无归属请求之和"
    );
    // 速率字段齐全（1s/5s/60s 三档）
    for dir in ["up", "down"] {
        for w in ["1s", "5s", "60s"] {
            assert!(
                g["rates"][dir][w].is_f64(),
                "global rates.{dir}.{w} 缺失"
            );
            assert!(
                sess["rates"][dir][w].is_f64(),
                "session rates.{dir}.{w} 缺失"
            );
        }
    }
}

async fn sessions_traffic(env: &TestEnv) -> Value {
    let resp = env
        .client
        .get(format!("{}/api/sessions/active", env.base_url))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    body["traffic"].clone()
}
