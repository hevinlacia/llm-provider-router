use crate::app::AppState;
use crate::config::Settings;
use crate::config::{KeyRef, ModelAlias};
use crate::config_v2::{TargetCandidate, V2Strategy};
use crate::error_rules::{builtin_config, classify, ErrorClass};
use crate::features::router::failure::{
    circuit_breaker_on_exhaustion, handle_response_failure, note_success_differential,
    FailureAction,
};
use crate::features::router::freeze::{key_state_id, parse_quota_reset};
use crate::features::router::selection::{
    drop_exhausted_candidates, key_exhausted, order_targets, priority_key_pick, weighted_pick,
};
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
        inject_prompt_cache_key: true,
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
        error_rules_path: ":memory:/nonexistent/error-rules.json".to_string(),
        auth_invalid_freeze_seconds: 86400.0,
        subscription_invalid_freeze_seconds: 86400.0,
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
            daily_token_quota: None,
        },
        KeyRef {
            name: "b".into(),
            env_var: "B".into(),
            weight: 3,
            provider: "ark".into(),
            billing_type: "subscription".into(),
            persist: true,
            daily_token_quota: None,
        },
        KeyRef {
            name: "c".into(),
            env_var: "C".into(),
            weight: 5,
            provider: "ark".into(),
            billing_type: "subscription".into(),
            persist: true,
            daily_token_quota: None,
        },
    ];
    let first = weighted_pick(&keys, Some("session-1"), "alias").unwrap();
    let second = weighted_pick(&keys, Some("session-1"), "alias").unwrap();
    assert_eq!(first.name, second.name);
}

#[test]
fn parses_quota_reset_fallback() {
    let (until, reason) =
        parse_quota_reset("You have exceeded the monthly usage quota", 86400.0, 5400.0).unwrap();
    assert_eq!(reason, "monthly_quota");
    assert!(until > now_seconds() + 86000.0);
}

#[test]
fn auth_status_codes_classify_invalid() {
    // 401/402/403 按状态码直接归为失效，不再依赖报错文案。
    let config = builtin_config();
    for status in [401u16, 402, 403] {
        assert_eq!(
            classify(&config, "ark", status, "whatever body").class,
            ErrorClass::Invalid,
            "status {status} 应归为失效"
        );
    }
}

// ---------------------------------------------------------------------------
// 订阅失效（key 级永久故障）识别与整把冻结（error-rules 分类版）
//
// ark coding plan 账号订阅过期时以 HTTP 400 返回订阅失效文案，400 不在
// retry_on_status。修复前既不冻结 key 也不换下一把，直接以
// "all 1 upstream keys failed" 终态失败；分类引擎后由 ark 模板识别两代文案
// （CodingPlan / CodingPlanEnterprise）归为 invalid，整把冻结并换下一把。
// ---------------------------------------------------------------------------

const ARK_SUBSCRIPTION_EXPIRED_BODY: &str = r#"{"error":{"code":"SubscriptionNotValid","message":"Your account (2102661813) does not have a valid CodingPlan subscription, or your subscription has expired. Please visit https://console.volcengine.com/ark/ to review your subscription status."}}"#;

const ARK_ENTERPRISE_SUBSCRIPTION_BODY: &str = r#"{"error":{"message":"Your account (2102405658, 77769405) lacks a valid CodingPlanEnterprise subscription, the plan has expired, or no seat is allocated. Visit https://console.volcengine.com/ark/ to check your status or renew."}}"#;

#[test]
fn ark_subscription_bodies_classify_invalid() {
    let config = builtin_config();
    assert_eq!(
        classify(
            &config,
            "ark",
            400,
            &ARK_SUBSCRIPTION_EXPIRED_BODY.to_lowercase()
        )
        .class,
        ErrorClass::Invalid
    );
    assert_eq!(
        classify(
            &config,
            "ark",
            400,
            &ARK_ENTERPRISE_SUBSCRIPTION_BODY.to_lowercase()
        )
        .class,
        ErrorClass::Invalid
    );
    // 普通请求级 400 不命中
    assert_eq!(
        classify(
            &config,
            "ark",
            400,
            &json!({"error": {"message": "Invalid parameter: messages is empty"}})
                .to_string()
                .to_lowercase()
        )
        .class,
        ErrorClass::Transient
    );
}

fn sub_test_key(name: &str, env_var: &str) -> KeyRef {
    KeyRef {
        name: name.into(),
        env_var: env_var.into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        persist: true,
        daily_token_quota: None,
    }
}

#[test]
fn subscription_invalid_400_freezes_key_and_select_skips_it() {
    let app = AppState::new(test_settings()).unwrap();
    let expired = sub_test_key("expired", "SUB_TEST_KEY");
    let alias_with_expired = ModelAlias::new(
        "ark/test-model",
        "openai/test-model",
        "https://ark.example.com/api/coding/v3",
        vec![expired.clone(), sub_test_key("healthy", "SUB_TEST_HEALTHY")],
        None,
    );
    let headers = http::HeaderMap::new();

    // 400 + 订阅失效文案 → invalid → 冻结整把 key 并换下一把。
    let action = handle_response_failure(
        &app,
        &alias_with_expired,
        &expired,
        400,
        &headers,
        ARK_ENTERPRISE_SUBSCRIPTION_BODY,
        None,
        None,
        0,
    );
    assert!(
        matches!(action, FailureAction::NextKey(_)),
        "订阅失效 400 应换下一把 key，实际 {action:?}"
    );
    {
        let mut state = app.state.lock().unwrap();
        assert!(
            state.is_frozen(&key_state_id(&expired)).unwrap(),
            "订阅失效 400 必须冻结整把 key"
        );
    }

    // 冻结后的 key 不再被选中（排除出可用池）
    {
        let mut state = app.state.lock().unwrap();
        for _ in 0..5 {
            let picked = state
                .select_key_excluding(&alias_with_expired, None, &HashSet::new())
                .unwrap();
            assert_ne!(picked.name, "expired", "冻结的订阅失效 key 不应再被选中");
        }
    }

    // 普通 400（无订阅失效特征）→ transient → 不冻结，但应换下一把。
    let plain = sub_test_key("plain", "SUB_TEST_PLAIN");
    let action = handle_response_failure(
        &app,
        &alias_with_expired,
        &plain,
        400,
        &headers,
        &json!({"error": {"message": "Invalid parameter"}}).to_string(),
        None,
        None,
        0,
    );
    assert!(
        matches!(action, FailureAction::NextKeyTransient(_)),
        "普通 400 应按 transient 换 key，实际 {action:?}"
    );
    let mut state = app.state.lock().unwrap();
    assert!(
        !state.is_frozen(&key_state_id(&plain)).unwrap(),
        "普通请求级 400 不应冻结 key"
    );
}

#[test]
fn differential_success_freezes_ambiguous_keys() {
    let app = AppState::new(test_settings()).unwrap();
    let flaky = sub_test_key("flaky", "DIFF_TEST_KEY");
    note_success_differential(&app, std::slice::from_ref(&flaky));
    let mut state = app.state.lock().unwrap();
    assert!(
        state.is_frozen(&key_state_id(&flaky)).unwrap(),
        "同行成功的差分证据应冻结失败 key"
    );
}

#[test]
fn circuit_breaker_freezes_unfrozen_pool_keys() {
    let app = AppState::new(test_settings()).unwrap();
    let alias = ModelAlias::new(
        "ark/breaker",
        "openai/breaker",
        "https://ark.example.com/api/coding/v3",
        vec![
            sub_test_key("k1", "BREAKER_K1"),
            sub_test_key("k2", "BREAKER_K2"),
        ],
        None,
    );
    circuit_breaker_on_exhaustion(&app, &alias);
    let mut state = app.state.lock().unwrap();
    for key in &alias.keys {
        assert!(
            state.is_frozen(&key_state_id(key)).unwrap(),
            "全池 transient 耗尽后应短熔断整池 {}",
            key.name
        );
    }
}

#[test]
fn binding_skips_key_already_tried_in_request() {
    // 回归：会话绑定存 provider/name（如 ark/a），而请求内 tried 以裸名（a）记账。
    // 修复前 binding 分支比较两种口径永不相等，粘住的刚失败 key 会被原样返回。
    let mut state = RouterState::new(test_settings()).unwrap();
    let alias = ModelAlias::new(
        "ark/binding-test",
        "openai/binding-test",
        "https://ark.example.com/api/coding/v3",
        vec![
            sub_test_key("a", "BINDING_A"),
            sub_test_key("b", "BINDING_B"),
        ],
        None,
    );
    state
        .bind("ark/binding-test", "sess-1", &key_state_id(&alias.keys[0]))
        .unwrap();
    let mut tried = HashSet::new();
    tried.insert("a".to_string());
    let picked = state
        .select_key_excluding(&alias, Some("sess-1"), &tried)
        .unwrap();
    assert_eq!(picked.name, "b", "粘住的 key 本请求已失败过时必须换下一把");
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
                daily_token_quota: None,
            },
            KeyRef {
                name: "on".into(),
                env_var: "ON".into(),
                weight: 1,
                provider: "ark".into(),
                billing_type: "subscription".into(),
                persist: true,
                daily_token_quota: None,
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
    // 价格模型必须是被引用的物理模型（set_token_prices 校验），
    // 而本机 config/ 是随 dashboard 漂移的线上数据 —— 取记账池 glm-latest-auto
    // 实际解析出的物理模型，避免测试与具体模型名耦合（测试意图是成本计算）。
    let routed = state
        .route_aliases("glm-latest-auto", None)
        .into_iter()
        .next()
        .expect("glm-latest-auto should resolve to at least one alias");
    let model_id = format!("{}/{}", routed.provider(), routed.upstream_model());
    state
        .set_token_prices(HashMap::from([(
            model_id,
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

// ---------------------------------------------------------------------------
// key × 模型“不支持”学习（阶梯退避 + 永久失效 + dashboard 刷新）
// ---------------------------------------------------------------------------

use crate::features::chat::select::is_unsupported_signal;

fn two_key_alias() -> ModelAlias {
    ModelAlias::new(
        "ark/test-model",
        "openai/test-model",
        "https://ark.example.com/api/coding/v3",
        vec![
            KeyRef {
                name: "good".into(),
                env_var: "TWO_KEY_A".into(),
                weight: 1,
                provider: "ark".into(),
                billing_type: "subscription".into(),
                persist: true,
                daily_token_quota: None,
            },
            KeyRef {
                name: "bad".into(),
                env_var: "TWO_KEY_B".into(),
                weight: 1,
                provider: "ark".into(),
                billing_type: "subscription".into(),
                persist: true,
                daily_token_quota: None,
            },
        ],
        None,
    )
}

#[test]
fn unsupported_signal_matches_only_404_not_support() {
    let body = json!({
        "error": { "message": "The requested model does not support the coding plan feature." }
    })
    .to_string();
    assert!(is_unsupported_signal(404, &body));
    // 非 404 / 无文案 / 普通错误不记
    assert!(!is_unsupported_signal(429, &body));
    assert!(!is_unsupported_signal(
        404,
        &json!({"error": {"message": "rate limited"}}).to_string()
    ));
    assert!(!is_unsupported_signal(404, "not json"));
}

#[test]
fn unsupported_ladder_escalates_then_permanent() {
    let mut state = RouterState::new(test_settings()).unwrap();
    let model = "test-model";
    let delays = [60.0, 300.0, 1800.0, 7200.0, 28800.0];
    for (i, expected) in delays.iter().enumerate() {
        state.mark_key_model_unsupported("ark", "bad", model, "does not support");
        let entry = state
            .unsupported_view()
            .iter()
            .find(|r| r["key"] == "bad")
            .unwrap()
            .clone();
        assert_eq!(entry["attempt"].as_u64().unwrap(), (i + 1) as u64);
        assert!(!entry["permanent"].as_bool().unwrap());
        let retry_in = entry["retry_in_seconds"].as_u64().unwrap();
        assert!(
            (retry_in as f64 - *expected).abs() < 5.0,
            "attempt {}: retry_in={retry_in}, expected ~{expected}",
            i + 1
        );
    }
    // 第 6 次失败：间隔达到 1 天 → 永久失效
    state.mark_key_model_unsupported("ark", "bad", model, "does not support");
    let entry = state
        .unsupported_view()
        .iter()
        .find(|r| r["key"] == "bad")
        .unwrap()
        .clone();
    assert_eq!(entry["attempt"].as_u64().unwrap(), 6);
    assert!(entry["permanent"].as_bool().unwrap());
    assert!(entry["retry_in_seconds"].is_null());
}

#[test]
fn unsupported_select_skips_blocked_key_and_success_clears() {
    let mut state = RouterState::new(test_settings()).unwrap();
    let alias = two_key_alias();
    // bad key 被标记为永久不支持 → select 永远只出 good
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support");
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support");
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support");
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support");
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support");
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support"); // permanent
    for _ in 0..10 {
        let key = state
            .select_key_excluding(&alias, None, &HashSet::new())
            .unwrap();
        assert_eq!(key.name, "good", "永久不支持的 key 不应再被选中");
    }
    // dashboard 刷新（重置退避）后恢复参与
    let removed = state
        .refresh_unsupported(Some("ark"), Some("bad"), Some("test-model"))
        .unwrap();
    assert_eq!(removed, 1);
    let names: HashSet<String> = (0..20)
        .filter_map(|_| {
            state
                .select_key_excluding(&alias, None, &HashSet::new())
                .ok()
        })
        .map(|k| k.name)
        .collect();
    assert!(names.contains("bad"), "刷新后 bad key 应重新参与");
    // 成功跑通 → 清除记录
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support");
    state.clear_key_model_unsupported("ark", "bad", "test-model");
    assert!(state.unsupported_view().iter().all(|r| r["key"] != "bad"));
}

#[test]
fn unsupported_all_blocked_still_probes() {
    let mut state = RouterState::new(test_settings()).unwrap();
    let alias = two_key_alias();
    // 两个 key 都在退避窗口内（非永久）→ select 放开过滤继续 probe
    state.mark_key_model_unsupported("ark", "good", "test-model", "does not support");
    state.mark_key_model_unsupported("ark", "bad", "test-model", "does not support");
    let key = state
        .select_key_excluding(&alias, None, &HashSet::new())
        .unwrap();
    assert!(
        key.name == "good" || key.name == "bad",
        "全阻塞时应放开过滤给出候选"
    );
}

// ---------------------------------------------------------------------------
// 空 env_var（dashboard 明文直配）key 的 vault-by-name 读写链路
// ---------------------------------------------------------------------------

#[test]
fn inline_key_value_reads_from_store_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let keys_path = dir.path().join("api-keys.json");
    fs::write(
        &keys_path,
        json!({ "vault-key": "vault-value" }).to_string(),
    )
    .unwrap();
    let settings = Settings {
        api_keys_path: keys_path.to_str().unwrap().to_string(),
        ..test_settings()
    };
    let mut state = RouterState::new(settings).unwrap();
    // vault-by-name：env_var 为空 → upstream_key_value fallback 到 store 按 key 名读取
    let inline = KeyRef {
        name: "vault-key".into(),
        env_var: "".into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        persist: true,
        daily_token_quota: None,
    };
    assert_eq!(
        state.upstream_key_value(&inline).unwrap().as_deref(),
        Some("vault-value")
    );
    // env 优先：env_var 非空且有值时优先取 env
    env::set_var("INLINE_KEY_ENV_TEST", "env-wins");
    let with_env = KeyRef {
        name: "env-key".into(),
        env_var: "INLINE_KEY_ENV_TEST".into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        persist: true,
        daily_token_quota: None,
    };
    assert_eq!(
        state.upstream_key_value(&with_env).unwrap().as_deref(),
        Some("env-wins")
    );
    // 两者皆无 → None
    let missing = KeyRef {
        name: "no-value-key".into(),
        env_var: "".into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        persist: true,
        daily_token_quota: None,
    };
    assert_eq!(state.upstream_key_value(&missing).unwrap(), None);
    env::remove_var("INLINE_KEY_ENV_TEST");
}

/// —— 负载均衡策略：日配额耗尽与 priority 降级 ——

fn quota_key(name: &str, weight: i64, quota: Option<u64>) -> KeyRef {
    KeyRef {
        name: name.into(),
        env_var: format!("{name}-env"),
        weight,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        persist: true,
        daily_token_quota: quota,
    }
}

#[test]
fn key_exhausted_only_when_quota_reached() {
    let key = quota_key("k", 1, Some(1_000_000));
    assert!(!key_exhausted(&key, 999_999), "未达配额不算耗尽");
    assert!(key_exhausted(&key, 1_000_000), "达到配额即耗尽");
    let unlimited = quota_key("k", 1, None);
    assert!(
        !key_exhausted(&unlimited, i64::MAX as u64 as i64),
        "无配额永不耗尽"
    );
}

#[test]
fn priority_key_pick_descends_weight_and_skips_exhausted() {
    let keys = vec![
        quota_key("big", 5, Some(100)),
        quota_key("mid", 3, Some(100)),
        quota_key("small", 1, None),
    ];
    // 无用量：最高 weight 优先
    let totals = HashMap::new();
    assert_eq!(priority_key_pick(&keys, &totals).unwrap().name, "big");
    // big 耗尽 -> mid
    let mut totals = HashMap::new();
    totals.insert("big".to_string(), 100i64);
    assert_eq!(priority_key_pick(&keys, &totals).unwrap().name, "mid");
    // big+mid 耗尽 -> small（无配额永不用尽）
    totals.insert("mid".to_string(), 200i64);
    assert_eq!(priority_key_pick(&keys, &totals).unwrap().name, "small");
    // 全耗尽（small 无配额所以不可能）—— 全部有配额且达限时返回 None
    let all_quota = vec![quota_key("a", 2, Some(10)), quota_key("b", 1, Some(10))];
    let mut totals = HashMap::new();
    totals.insert("a".to_string(), 10i64);
    totals.insert("b".to_string(), 99i64);
    assert!(
        priority_key_pick(&all_quota, &totals).is_none(),
        "全耗尽应返回 None"
    );
}

#[test]
fn drop_exhausted_candidates_skips_only_when_alternative_exists() {
    let mk = |quota: Option<u64>, url: &str| {
        let mut alias = test_alias("m", url);
        alias.keys = vec![quota_key("k", 1, quota)];
        TargetCandidate {
            model: alias,
            weight: None,
            strategy: V2Strategy::Priority,
        }
    };
    // 首选耗尽、次选可用 -> 跳过首选
    let mut totals = HashMap::new();
    totals.insert("k".to_string(), 50i64);
    let kept = drop_exhausted_candidates(vec![mk(Some(10), "u-1"), mk(None, "u-2")], &totals);
    let urls: Vec<&str> = kept.iter().map(|c| c.model.base_url.as_str()).collect();
    assert_eq!(urls, vec!["u-2"], "耗尽候选应被跳过");
    // 全部耗尽 -> 保留原列表（软偏好，freeze 兜底）
    let all_exhausted = vec![mk(Some(10), "u-1"), mk(Some(10), "u-2")];
    let kept = drop_exhausted_candidates(all_exhausted, &totals);
    assert_eq!(kept.len(), 2, "全耗尽应保留原列表");
}
