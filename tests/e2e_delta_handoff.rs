//! Delta 缓存切槽 handoff 的全链路 e2e：两个真实 backend 模拟 blue/green。
//!
//! 覆盖：A 槽注册会话（ready）→ dump（/internal/delta-cache/dump）→ load 进 B 槽
//! （/internal/delta-cache/load）→ 用 A 槽发的 token+base_hash 直接向 B 槽发增量
//! 请求 → advance + 新 base_hash（这就是"切槽无感"的判定标准）。
//! 注意：增量请求在重组后走正常管线，本测试用未知模型拿到 404——重组发生在
//! 路由之前，delta 头仍会附在错误响应上，无需真实上游。
//!
//! 安全性附带验证：front-proxy 封锁 /internal/*（用例里起 proxy 后从代理侧访问
//! dump 必须 403）。

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
    std::fs::write(
        cfg.join("providers-v2.json"),
        r#"{"providers":{"mock":{"base_url":"","responses_base_url":"http://127.0.0.1:1","keys":{"mock-key":{"env_var":"E2E_HANDOFF_MOCK_KEY","weight":1,"billing_type":"payg","enabled":true,"persist":true}}}}}"#,
    )
    .unwrap();
    std::fs::write(
        cfg.join("models.json"),
        r#"{"families":{},"models":{"mock/glm-test":{"provider":"mock","upstream_model":"glm-test","family":null,"params":{},"context_window":1000000,"max_output_tokens":128000,"supports_image":null,"thinking_level_map":null,"thinking_format":null}}}"#,
    )
    .unwrap();
    std::fs::write(
        cfg.join("logical-models.json"),
        r#"{"logical_models":{"handoff-e2e-auto":{"params":{},"route":{"strategy":"priority","targets":[{"model":"mock/glm-test","weight":null,"enabled":true,"keys":null}]},"display_name":null}}}"#,
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
            for name in ["a-err.log", "a-out.log", "b-err.log", "b-out.log", "proxy-err.log"] {
                if let Ok(content) = std::fs::read_to_string(log_dir.join(name)) {
                    dump.push_str(&format!("--- {name} ---\n{content}\n"));
                }
            }
            panic!("{label} 未在超时内就绪: {url}\n子进程日志:\n{dump}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn spawn_backend(name: &str, port: u16, tmp: &Path) -> ChildGuard {
    let state_dir = tmp.join(format!("state-{name}"));
    std::fs::create_dir_all(&state_dir).unwrap();
    Command::new(BIN)
        .arg("backend")
        .current_dir(tmp)
        .env("LLM_PROVIDER_ROUTER_HOST", "127.0.0.1")
        .env("LLM_PROVIDER_ROUTER_PORT", port.to_string())
        .env("LLM_PROVIDER_ROUTER_USAGE_DB_PATH", ":memory:")
        .env("LLM_PROVIDER_ROUTER_STATE_DB_PATH", state_dir.join("state.db"))
        .env("LLM_PROVIDER_ROUTER_REQUEST_TIMEOUT_SECONDS", "30")
        .env("E2E_HANDOFF_MOCK_KEY", "dummy-key")
        .stdout(Stdio::from(
            std::fs::File::create(tmp.join(format!("{name}-out.log"))).unwrap(),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(tmp.join(format!("{name}-err.log"))).unwrap(),
        ))
        .spawn()
        .map(ChildGuard)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn delta_cache_handoff_between_slots() {
    let tmp = TempDir::new().unwrap();
    write_config_tree(tmp.path());
    let port_a = free_port();
    let port_b = free_port();
    let _a = spawn_backend("a", port_a, tmp.path());
    let _b = spawn_backend("b", port_b, tmp.path());
    let base_a = format!("http://127.0.0.1:{port_a}");
    let base_b = format!("http://127.0.0.1:{port_b}");
    wait_healthy(&format!("{base_a}/health"), "backend-a", tmp.path()).await;
    wait_healthy(&format!("{base_b}/health"), "backend-b", tmp.path()).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();

    // 1) A 槽：全量注册（首笔带 X-Delta-Capable；404 响应同样带 ready 头）
    let full = json!({
        "model": "handoff-e2e-auto",
        "instructions": "sys",
        "input": [{"role": "user", "content": "q1"}],
        "store": false
    });
    let resp = client
        .post(format!("{base_a}/v1/responses"))
        .header("x-delta-capable", "1")
        .json(&full)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-delta-cache"], "ready", "A: full registered");
    let token = resp.headers()["x-delta-token"].to_str().unwrap().to_string();
    let hash1 = resp.headers()["x-delta-base-hash"].to_str().unwrap().to_string();
    let _ = resp.bytes().await.unwrap();

    // 2) B 槽上同一 token 必然 miss（缓存是进程内存态）——事故基线
    let delta = json!({
        "model": "handoff-e2e-auto",
        "x_delta": {"v": 1, "token": token, "base_hash": hash1, "append": [{"role": "assistant", "content": "ok"}]}
    });
    let resp = client
        .post(format!("{base_b}/v1/responses"))
        .json(&delta)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-delta-cache"], "miss", "B: cache empty before handoff");
    let _ = resp.bytes().await.unwrap();

    // 3) handoff：dump A → load B
    let dumped: Value = client
        .get(format!("{base_a}/internal/delta-cache/dump"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dumped["count"], 1, "A dump has the session");
    let loaded: Value = client
        .post(format!("{base_b}/internal/delta-cache/load"))
        .json(&dumped)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(loaded["imported"], 1, "B imported the session");

    // 4) ★ 切槽无感判定：用 A 槽原 token+base_hash 向 B 发增量 → advance
    let delta2 = json!({
        "model": "handoff-e2e-auto",
        "x_delta": {"v": 1, "token": token, "base_hash": hash1, "append": [{"role": "assistant", "content": "ok"}]}
    });
    let resp = client
        .post(format!("{base_b}/v1/responses"))
        .json(&delta2)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()["x-delta-cache"],
        "advance",
        "handoff: delta survives the slot switch"
    );
    let hash2 = resp.headers()["x-delta-base-hash"].to_str().unwrap().to_string();
    assert_ne!(hash1, hash2, "advance rotates base hash");
    let _ = resp.bytes().await.unwrap();

    // 5) 损坏条目防护：篡改 base_hash 的 dump 被 load 拒收
    let mut corrupted = dumped.clone();
    corrupted["sessions"][0]["base_hash"] = json!("deadbeef");
    let loaded: Value = client
        .post(format!("{base_b}/internal/delta-cache/load"))
        .json(&corrupted)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(loaded["imported"], 0, "corrupted entry rejected");

    // 6) front-proxy 封锁 /internal/*（公网/代理链路不可达）
    let proxy_port = free_port();
    let active_file = tmp.path().join("active-backend.json");
    std::fs::write(&active_file, format!(r#"{{"port":{port_a}}}"#)).unwrap();
    let _proxy = Command::new(BIN)
        .arg("front-proxy")
        .current_dir(tmp.path())
        .env("LLM_PROVIDER_ROUTER_PROXY_HOST", "127.0.0.1")
        .env("LLM_PROVIDER_ROUTER_PROXY_PORT", proxy_port.to_string())
        .env("LLM_PROVIDER_ROUTER_BLUE_URL", &base_a)
        .env("LLM_PROVIDER_ROUTER_GREEN_URL", &base_b)
        .env("LLM_PROVIDER_ROUTER_ACTIVE_BACKEND_FILE", &active_file)
        .env("LLM_PROVIDER_ROUTER_SLOT_AUTO_HEAL", "0")
        .stdout(Stdio::from(std::fs::File::create(tmp.path().join("proxy-out.log")).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(tmp.path().join("proxy-err.log")).unwrap()))
        .spawn()
        .map(ChildGuard)
        .unwrap();
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    wait_healthy(&format!("{proxy_url}/health"), "front-proxy", tmp.path()).await;
    let resp = client
        .get(format!("{proxy_url}/internal/delta-cache/dump"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "proxy must block /internal/* (session content must not leave the host)"
    );
}
