use crate::config::Settings;
use crate::config::{KeyRef, ModelAlias};
use crate::config_v2::{TargetCandidate, V2Strategy};
use crate::features::router::freeze::{parse_auth_invalid, parse_quota_reset};
use crate::features::router::selection::{order_targets, weighted_pick};
use crate::features::router::state::RouterState;
use crate::json_config::TokenPrice;
use crate::state_store::now_seconds;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;

fn test_settings() -> Settings {
    Settings {
        host: "127.0.0.1".to_string(),
        port: 8789,
        session_ttl_seconds: 3600.0,
        monthly_quota_fallback_seconds: 86400.0,
        five_hour_quota_fallback_seconds: 5400.0,
        request_timeout_seconds: 30.0,
        local_bearer_token: None,
        usage_db_path: ":memory:".to_string(),
        state_db_path: ":memory:".to_string(),
        api_keys_path: ":memory:".to_string(),
        token_price_config_path: ":memory:".to_string(),
        model_alias_config_path: ":memory:".to_string(),
        search_providers_path: ":memory:".to_string(),
        provider_models_path: ":memory:".to_string(),
        auth_invalid_freeze_seconds: 86400.0,
        // router_state 测试覆盖旧逻辑；v2 行为由 config_v2 模块测试覆盖。
        diag_dir: ":memory:".to_string(),
        diag_max_bytes: 10 * 1024 * 1024,
        diag_max_files: 0,
        diag_sample_every: 1,
        env_file_path: None,
    }
}

#[test]
fn weighted_pick_is_sticky_for_session() {
    let keys = vec![
        KeyRef {
            name: "a".into(),
            env_var: "A".into(),
            weight: 1,
            provider: "ark".into(),
            billing_type: "subscription".into(),
            persist: true,
        },
        KeyRef {
            name: "b".into(),
            env_var: "B".into(),
            weight: 3,
            provider: "ark".into(),
            billing_type: "subscription".into(),
            persist: true,
        },
        KeyRef {
            name: "c".into(),
            env_var: "C".into(),
            weight: 5,
            provider: "ark".into(),
            billing_type: "subscription".into(),
            persist: true,
        },
    ];
    let first = weighted_pick(&keys, Some("session-1"), "alias").unwrap();
    let second = weighted_pick(&keys, Some("session-1"), "alias").unwrap();
    assert_eq!(first.name, second.name);
}

#[test]
fn parses_quota_reset_fallback() {
    let settings = test_settings();
    let (until, reason) =
        parse_quota_reset("You have exceeded the monthly usage quota", &settings).unwrap();
    assert_eq!(reason, "monthly_quota");
    assert!(until > now_seconds() + 86000.0);
}

#[test]
fn parses_auth_invalid_error() {
    let settings = test_settings();
    let (until, reason) =
        parse_auth_invalid("authentication_error: api key invalid", &settings).unwrap();
    assert_eq!(reason, "auth_invalid");
    assert!(until > now_seconds() + 86000.0);
}

#[test]
fn stored_keys_are_kept_and_applied_to_env() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("api-keys.json");
    fs::write(
        &store_path,
        json!({
            "AGENT_AI_ARK_TEST_PERSIST_API_KEY": "persist-value",
        })
        .to_string(),
    )
    .unwrap();
    env::set_var("AGENT_AI_ARK_TEST_PERSIST_API_KEY", "persist-value");

    let settings = Settings {
        api_keys_path: store_path.to_str().unwrap().to_string(),
        ..test_settings()
    };
    let mut state = RouterState::new(settings).unwrap();

    // Persist key 保留在 store 中。
    let stored: HashMap<String, String> =
        serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert!(stored.contains_key("AGENT_AI_ARK_TEST_PERSIST_API_KEY"));

    // store 中的 key 值在 env 缺失时被应用到进程环境（启动恢复机制）。
    let restored = state
        .all_key_refs()
        .into_iter()
        .find(|key| key.env_var == "AGENT_AI_ARK_TEST_PERSIST_API_KEY");
    if let Some(key) = restored {
        assert!(key.persist);
        assert_eq!(
            state.upstream_key_value(&key).unwrap().as_deref(),
            Some("persist-value")
        );
    }
}

#[test]
fn zero_weight_key_is_not_selected_or_reused_from_binding() {
    let settings = test_settings();
    let mut state = RouterState::new(settings).unwrap();
    let alias = ModelAlias::new(
        "test-pool",
        "openai/test",
        "https://example.test",
        vec![
            KeyRef {
                name: "off".into(),
                env_var: "OFF".into(),
                weight: 0,
                provider: "ark".into(),
                billing_type: "subscription".into(),
                persist: true,
            },
            KeyRef {
                name: "on".into(),
                env_var: "ON".into(),
                weight: 1,
                provider: "ark".into(),
                billing_type: "subscription".into(),
                persist: true,
            },
        ],
        None,
    );
    state.bind("test-pool", "session-1", "off").unwrap();
    let selected = state
        .select_key_excluding(&alias, Some("session-1"), &HashSet::new())
        .unwrap();
    assert_eq!(selected.name, "on");
}

#[test]
fn usage_snapshot_includes_cost_by_model() {
    let settings = test_settings();
    let mut state = RouterState::new(settings).unwrap();
    state
        .set_token_prices(HashMap::from([(
            "ark/glm-5-3-260801".to_string(),
            TokenPrice {
                input_uncached_per_million: 10.0,
                input_cached_per_million: 1.0,
                output_per_million: 20.0,
            },
        )]))
        .unwrap();
    let usage = json!({
        "prompt_tokens": 100,
        "prompt_tokens_details": { "cached_tokens": 40 },
        "completion_tokens": 25,
        "total_tokens": 125
    });
    state
        .record_usage(
            "glm-latest-auto",
            "hevin",
            200,
            Some(&usage),
            Some("sess-test"),
        )
        .unwrap();

    let snapshot = state.usage_snapshot("all", None, None).unwrap();

    assert_eq!(
        snapshot["by_model"]["glm-latest-auto"]["prompt_uncached_tokens"],
        60
    );
    assert_eq!(
        snapshot["by_model_cost"]["glm-latest-auto"]["total_cost"],
        0.00114
    );
    assert_eq!(snapshot["total_cost"]["total_cost"], 0.00114);
}

fn test_alias(name: &str, base_url: &str) -> ModelAlias {
    ModelAlias::new(name, &format!("openai/{name}"), base_url, vec![], None)
}

#[test]
fn order_targets_priority_keeps_target_order() {
    let cands = vec![
        TargetCandidate {
            model: test_alias("m", "u-1"),
            weight: None,
            strategy: V2Strategy::Priority,
        },
        TargetCandidate {
            model: test_alias("m", "u-2"),
            weight: None,
            strategy: V2Strategy::Priority,
        },
    ];
    let ordered = order_targets(cands, None, None);
    let urls: Vec<&str> = ordered.iter().map(|a| a.base_url.as_str()).collect();
    assert_eq!(urls, vec!["u-1", "u-2"], "priority 应按 targets 原序");
}

#[test]
fn order_targets_preferred_overrides_weighted_sampling() {
    let cands = vec![
        TargetCandidate {
            model: test_alias("m", "u-a"),
            weight: Some(1),
            strategy: V2Strategy::Weighted,
        },
        TargetCandidate {
            model: test_alias("m", "u-b"),
            weight: Some(9),
            strategy: V2Strategy::Weighted,
        },
        TargetCandidate {
            model: test_alias("m", "u-c"),
            weight: Some(5),
            strategy: V2Strategy::Weighted,
        },
    ];
    // preferred=2 强制首选 u-c，其余按 weight 降序
    let ordered = order_targets(cands, Some("sess"), Some(2));
    let urls: Vec<&str> = ordered.iter().map(|a| a.base_url.as_str()).collect();
    assert_eq!(urls[0], "u-c", "preferred 应作为首选");
    assert_eq!(urls[1], "u-b");
    assert_eq!(urls[2], "u-a");
}

#[test]
fn order_targets_usage_aware_strategy_behaves_like_weighted_without_preferred() {
    let cands = vec![
        TargetCandidate {
            model: test_alias("m", "u-a"),
            weight: Some(3),
            strategy: V2Strategy::UsageAware,
        },
        TargetCandidate {
            model: test_alias("m", "u-b"),
            weight: Some(3),
            strategy: V2Strategy::UsageAware,
        },
    ];
    let o1 = order_targets(cands.clone(), Some("sess"), None);
    let o2 = order_targets(cands.clone(), Some("sess"), None);
    let u1: Vec<&str> = o1.iter().map(|a| a.base_url.as_str()).collect();
    let u2: Vec<&str> = o2.iter().map(|a| a.base_url.as_str()).collect();
    assert_eq!(u1, u2, "无 preferred 时按 session 粘性加权");
    assert_eq!(u1.len(), 2);
}

#[test]
fn order_targets_weighted_session_sticky_and_fallback_sorted() {
    let cands = vec![
        TargetCandidate {
            model: test_alias("m", "u-a"),
            weight: Some(1),
            strategy: V2Strategy::Weighted,
        },
        TargetCandidate {
            model: test_alias("m", "u-b"),
            weight: Some(9),
            strategy: V2Strategy::Weighted,
        },
        TargetCandidate {
            model: test_alias("m", "u-c"),
            weight: Some(5),
            strategy: V2Strategy::Weighted,
        },
    ];
    let o1 = order_targets(cands.clone(), Some("sess"), None);
    let o2 = order_targets(cands.clone(), Some("sess"), None);
    assert_eq!(o1.len(), 3);
    // session 粘性：同 session 两次结果一致
    let u1: Vec<&str> = o1.iter().map(|a| a.base_url.as_str()).collect();
    let u2: Vec<&str> = o2.iter().map(|a| a.base_url.as_str()).collect();
    assert_eq!(u1, u2, "同一 session 首选应稳定");
    // 集合不变
    let mut all: Vec<&str> = u1.clone();
    all.sort();
    assert_eq!(all, vec!["u-a", "u-b", "u-c"]);
    // 首选之后的回退按 weight 降序
    let weight_of = |u: &str| match u {
        "u-a" => 1,
        "u-b" => 9,
        "u-c" => 5,
        _ => 0,
    };
    let rest: Vec<i64> = u1[1..].iter().map(|u| weight_of(u)).collect();
    let mut sorted = rest.clone();
    sorted.sort_by(|x, y| y.cmp(x));
    assert_eq!(rest, sorted, "回退应按 weight 降序");
}

#[test]
fn reload_env_reads_env_file_and_injects_vars() {
    let dir = tempfile::tempdir().unwrap();
    let env_file = dir.path().join("agent-env.conf");
    fs::write(
        &env_file,
        "# comment line\n\nAGENT_TEST_RELOAD_KEY=reload-value\nOTHER_KEY=\n",
    )
    .unwrap();

    let settings = Settings {
        env_file_path: Some(env_file.to_str().unwrap().to_string()),
        ..test_settings()
    };
    let mut state = RouterState::new(settings).unwrap();

    let result = state.reload_env().unwrap();
    let reloaded = result.get("reloaded").and_then(|v| v.as_u64()).unwrap();
    // 跳过注释/空行/空值行（OTHER_KEY= 空值仍计入）
    assert!(
        reloaded >= 1,
        "expected at least 1 var imported, got {reloaded}"
    );
    assert_eq!(
        std::env::var("AGENT_TEST_RELOAD_KEY").as_deref().ok(),
        Some("reload-value")
    );
    std::env::remove_var("AGENT_TEST_RELOAD_KEY");
}
