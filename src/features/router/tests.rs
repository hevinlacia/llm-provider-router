use crate::app::AppState;
use crate::config::Settings;
use crate::config::{KeyRef, ModelAlias};
use crate::config_v2::{TargetCandidate, V2FreezeScope, V2Strategy};
use crate::error_rules::{builtin_config, classify, ErrorClass};
use crate::features::chat::select::clear_unsupported_if_ok;
use crate::features::router::failure::{
    circuit_breaker_on_exhaustion, extract_stream_error_event, handle_connect_failure,
    handle_response_failure, note_stream_break, note_success_differential, recent_errors_json,
    FailureAction,
};
use crate::features::router::freeze::{key_state_id, model_scoped_state_id, parse_quota_reset};
use crate::features::router::selection::{
    drop_exhausted_candidates, key_exhausted, order_targets, priority_key_pick, weighted_pick,
};
use crate::features::router::state::config::apply_key_value_write;
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
        read_timeout_seconds: 30.0,
        local_bearer_token: None,
        usage_db_path: ":memory:".to_string(),
        state_db_path: ":memory:".to_string(),
        api_keys_path: ":memory:".to_string(),
        token_price_config_path: ":memory:".to_string(),
        model_alias_config_path: ":memory:".to_string(),
        search_providers_path: ":memory:".to_string(),
        provider_models_path: ":memory:".to_string(),
        error_rules_path: ":memory:/nonexistent/error-rules.json".to_string(),
        dns_config_path: "".into(),
        auth_invalid_freeze_seconds: 86400.0,
        subscription_invalid_freeze_seconds: 86400.0,
        // router_state 测试覆盖旧逻辑；v2 行为由 config_v2 模块测试覆盖。
        diag_dir: ":memory:".to_string(),
        diag_max_bytes: 10 * 1024 * 1024,
        diag_max_files: 0,
        diag_sample_every: 1,
        delta_cache_enabled: true,
        delta_cache_max_sessions: 256,
        delta_cache_max_total_bytes: 512 * 1024 * 1024,
        delta_cache_session_max_bytes: 16 * 1024 * 1024,
        delta_cache_ttl_seconds: 3 * 3600,
    }
}

#[test]
fn weighted_pick_is_sticky_for_session() {
    let keys = vec![
        KeyRef {
            name: "a".into(),
            weight: 1,
            provider: "ark".into(),
            billing_type: "subscription".into(),
            daily_token_quota: None,
        },
        KeyRef {
            name: "b".into(),
            weight: 3,
            provider: "ark".into(),
            billing_type: "subscription".into(),
            daily_token_quota: None,
        },
        KeyRef {
            name: "c".into(),
            weight: 5,
            provider: "ark".into(),
            billing_type: "subscription".into(),
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

fn sub_test_key(name: &str) -> KeyRef {
    KeyRef {
        name: name.into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        daily_token_quota: None,
    }
}

#[test]
fn subscription_invalid_400_freezes_key_and_select_skips_it() {
    let app = AppState::new(test_settings()).unwrap();
    let expired = sub_test_key("expired");
    let alias_with_expired = ModelAlias::new(
        "ark/test-model",
        "openai/test-model",
        "https://ark.example.com/api/coding/v3",
        vec![expired.clone(), sub_test_key("healthy")],
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
    let plain = sub_test_key("plain");
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
    let flaky = sub_test_key("flaky");
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
        vec![sub_test_key("k1"), sub_test_key("k2")],
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
        vec![sub_test_key("a"), sub_test_key("b")],
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
fn startup_no_longer_touches_vault_or_env() {
    // 2026-11-04 起 vault↔env 互通已删除：启动时不再把 vault 条目注入进程环境，
    // 也不做播种/清理；2026-11-05 起环境变量取值整条移除，vault 是唯一来源。
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("api-keys.json");
    fs::write(
        &store_path,
        json!({
            "AGENT_AI_ARK_TEST_PERSIST_API_KEY": "persist-value",
            "vault-inline": "inline-value",
        })
        .to_string(),
    )
    .unwrap();
    env::remove_var("AGENT_AI_ARK_TEST_PERSIST_API_KEY");

    let settings = Settings {
        api_keys_path: store_path.to_str().unwrap().to_string(),
        ..test_settings()
    };
    let mut state = RouterState::new(settings).unwrap();
    let _ = &mut state;

    // 启动后 env 未被注入（互通已删除）。
    assert!(env::var("AGENT_AI_ARK_TEST_PERSIST_API_KEY").is_err());

    // vault 文件原样保留（无播种/清理改动）。
    let stored: HashMap<String, String> =
        serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert!(stored.contains_key("AGENT_AI_ARK_TEST_PERSIST_API_KEY"));
    assert!(stored.contains_key("vault-inline"));
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
                weight: 0,
                provider: "ark".into(),
                billing_type: "subscription".into(),
                daily_token_quota: None,
            },
            KeyRef {
                name: "on".into(),
                weight: 1,
                provider: "ark".into(),
                billing_type: "subscription".into(),
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
                weight: 1,
                provider: "ark".into(),
                billing_type: "subscription".into(),
                daily_token_quota: None,
            },
            KeyRef {
                name: "bad".into(),
                weight: 1,
                provider: "ark".into(),
                billing_type: "subscription".into(),
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
// 失败隔离粒度（provider.freeze_scope：key 级 / key × 模型级）
// ---------------------------------------------------------------------------

/// freeze_scope=model 的供应商：一个模型被冻结只隔离该模型，
/// 同一把 key 的其它模型照常可选；key 级冻结仍然全模型生效。
#[test]
fn model_scoped_freeze_isolates_single_model() {
    let mut state = RouterState::new(test_settings()).unwrap();
    let keys = vec![KeyRef {
        name: "hevin".into(),
        weight: 1,
        provider: "opencode-go".into(),
        billing_type: "subscription".into(),
        daily_token_quota: None,
    }];
    let beam = ModelAlias::new(
        "opencode-go/beam-free",
        "openai/beam-free",
        "https://opencode.ai/inference/go/openai/v1",
        keys.clone(),
        None,
    );
    let glm = ModelAlias::new(
        "opencode-go/glm-5.3",
        "openai/glm-5.3",
        "https://opencode.ai/inference/go/openai/v1",
        keys.clone(),
        None,
    );
    let until = now_seconds() + 3600.0;
    // 只为 beam-free 冻结（invalid 类，model 粒度）
    state
        .freeze(
            &model_scoped_state_id(&keys[0], "beam-free"),
            until,
            "invalid:default/invalid#0",
        )
        .unwrap();
    assert!(
        state
            .select_key_excluding(&beam, None, &HashSet::new())
            .is_err(),
        "被隔离的模型不应再拿到 key"
    );
    assert_eq!(
        state
            .select_key_excluding(&glm, None, &HashSet::new())
            .unwrap()
            .name,
        "hevin",
        "同一 key 的其它模型必须继续可用（这正是 freeze_scope=model 的目的）"
    );
    // key 级冻结仍然对全部模型生效
    state
        .freeze(&key_state_id(&keys[0]), until, "invalid:default/invalid#0")
        .unwrap();
    assert!(state
        .select_key_excluding(&glm, None, &HashSet::new())
        .is_err());
}

/// 缺省作用域 = key：未配置 freeze_scope 的供应商保持历史行为。
#[test]
fn provider_freeze_scope_defaults_to_key() {
    let state = RouterState::new(test_settings()).unwrap();
    assert_eq!(
        state.provider_freeze_scope("opencode-go"),
        V2FreezeScope::Key
    );
    assert_eq!(
        state.provider_freeze_scope("no-such-provider"),
        V2FreezeScope::Key
    );
}

// ---------------------------------------------------------------------------
// 明文 api-keys.json 按 key 名取值的读写链路
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
    // key 值只按 key 名从明文 api-keys.json 读取。
    let inline = KeyRef {
        name: "vault-key".into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        daily_token_quota: None,
    };
    assert_eq!(
        state.upstream_key_value(&inline).unwrap().as_deref(),
        Some("vault-value")
    );
    // 缺失 → None
    let missing = KeyRef {
        name: "no-value-key".into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        daily_token_quota: None,
    };
    assert_eq!(state.upstream_key_value(&missing).unwrap(), None);
}

/// —— 负载均衡策略：日配额耗尽与 priority 降级 ——
fn quota_key(name: &str, weight: i64, quota: Option<u64>) -> KeyRef {
    KeyRef {
        name: name.into(),
        weight,
        provider: "ark".into(),
        billing_type: "subscription".into(),
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

#[test]
fn note_stream_break_records_usage_ring_buffer_and_journal() {
    let app = AppState::new(test_settings()).unwrap();
    let key = sub_test_key("break-key");
    let alias = ModelAlias::new(
        "ark/break-model",
        "openai/break-model",
        "https://ark.example.com/api/coding/v3",
        vec![key.clone()],
        None,
    );

    // 断流观测：ring buffer 必须有 598 entry、usage 必须出现 598 状态计数。
    note_stream_break(
        &app,
        &alias,
        &key,
        "error decoding response body",
        false,
        1234,
        Some("sess-break"),
    );

    let mut state = app.state.lock().unwrap();
    let errors = recent_errors_json(&state);
    assert_eq!(errors.len(), 1, "断流必须进入最近报错 ring buffer");
    let entry = &errors[0];
    assert_eq!(entry["status"].as_u64(), Some(598));
    assert_eq!(entry["provider"], "ark", "provider 应取 alias 前缀");
    assert_eq!(entry["key"], "break-key");
    // upstream_model() 会剥掉 openai/ 前缀（与 handle_response_failure 等 entry 同口径）。
    assert_eq!(entry["model"], "break-model");
    assert_eq!(entry["class"], "transient");
    let message = entry["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("[stream-break:transport]") && message.contains("1234 bytes"),
        "错误消息应含分类断流前缀与已收字节数，实际 {message}"
    );

    let snapshot = state.usage_snapshot("all", None, None).unwrap();
    assert!(
        snapshot["by_status"].get("598").is_some(),
        "usage 快照必须出现 598 状态计数，实际 {snapshot}"
    );
}

#[test]
fn note_stream_break_classifies_timeout_vs_transport() {
    // 2026-11-02 事故：600s 总死线殄断长流被 reqwest 统一包装成
    // "error decoding response body"，无法区分超时与真上游断流。
    // note_stream_break 必须按 is_timeout 落分类标签。
    let app = AppState::new(test_settings()).unwrap();
    let key = sub_test_key("break-key");
    let alias = ModelAlias::new(
        "ark/break-model",
        "openai/break-model",
        "https://ark.example.com/api/coding/v3",
        vec![key.clone()],
        None,
    );

    note_stream_break(
        &app,
        &alias,
        &key,
        "error decoding response body",
        true,
        3_436_970,
        Some("sess-break-timeout"),
    );

    let state = app.state.lock().unwrap();
    let errors = recent_errors_json(&state);
    assert_eq!(errors.len(), 1);
    let message = errors[0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("[stream-break:timeout]"),
        "超时类断流必须带 timeout 标签，实际 {message}"
    );
}

fn connect_test_key() -> KeyRef {
    KeyRef {
        name: "jt".into(),
        weight: 1,
        provider: "zp".into(),
        billing_type: "subscription".into(),
        daily_token_quota: None,
    }
}

fn connect_test_alias(key: &KeyRef) -> ModelAlias {
    // 双 key 池：connect 短熔断的 failover 语义只在多 key 池成立
    // （单 key 池冻结无收益，见 handle_connect_failure 的 2026-11-05 豁免）。
    let mut second = key.clone();
    second.name = "jt-backup".into();
    ModelAlias::new(
        "glm-auto",
        "openai/glm-x",
        "http://zp.test",
        vec![key.clone(), second],
        None,
    )
}

fn connect_test_single_key_alias(key: &KeyRef) -> ModelAlias {
    ModelAlias::new(
        "glm-auto",
        "openai/glm-x",
        "http://zp.test",
        vec![key.clone()],
        None,
    )
}

#[test]
fn connect_failure_freezes_key_short_ttl_with_backoff_and_reset() {
    let app = AppState::new(test_settings()).unwrap();
    let key = connect_test_key();
    let alias = connect_test_alias(&key);

    // 第一次连接失败：默认 base 30s 短熔断，reason=connect_error。
    match handle_connect_failure(&app, &alias, &key, "connection refused", Some("s1")) {
        FailureAction::NextKeyTransient(_) => {}
        other => panic!("期望 NextKeyTransient，实际 {other:?}"),
    }
    let frozen = {
        let mut state = app.state.lock().unwrap();
        state.snapshot().unwrap()["frozen"]["zp/jt"].clone()
    };
    assert_eq!(frozen["reason"], "connect_error");
    let first = frozen["seconds_remaining"].as_i64().unwrap();
    assert!(first > 0 && first <= 30, "首次冻结应≈30s，实际 {first}");

    // 立即再次失败：连败退避翻倍（≈60s），计数为 2。
    let _ = handle_connect_failure(&app, &alias, &key, "connection refused", Some("s1"));
    let second = {
        let mut state = app.state.lock().unwrap();
        let remaining = state.snapshot().unwrap()["frozen"]["zp/jt"]["seconds_remaining"]
            .as_i64()
            .unwrap();
        assert!(
            remaining > first && remaining <= 60,
            "连败第二次应≈60s，实际 {remaining}"
        );
        state.connect_streak_count("zp/jt")
    };
    assert_eq!(second, 2, "连败计数应为 2");

    // 成功跑通：连败计数清零；下次失败重新从 base 起步。
    // （已存在的未过期冻结窗口按设计不缩短，freeze 只延长不缩短。）
    clear_unsupported_if_ok(&app, &key, &alias, 200);
    {
        let state = app.state.lock().unwrap();
        assert_eq!(state.connect_streak_count("zp/jt"), 0, "成功后计数应清零");
    }
    let _ = handle_connect_failure(&app, &alias, &key, "connection refused", Some("s1"));
    {
        let state = app.state.lock().unwrap();
        assert_eq!(
            state.connect_streak_count("zp/jt"),
            1,
            "重置后的失败应从 1 重新计数"
        );
    }
}

#[test]
fn connect_failure_does_not_shorten_longer_freeze() {
    let app = AppState::new(test_settings()).unwrap();
    let key = connect_test_key();
    let alias = connect_test_alias(&key);
    {
        let mut state = app.state.lock().unwrap();
        state
            .freeze("zp/jt", now_seconds() + 10_000.0, "monthly_quota")
            .unwrap();
    }
    let _ = handle_connect_failure(&app, &alias, &key, "connection refused", None);
    let mut state = app.state.lock().unwrap();
    let frozen = state.snapshot().unwrap()["frozen"]["zp/jt"].clone();
    assert_eq!(frozen["reason"], "monthly_quota");
    assert!(
        frozen["seconds_remaining"].as_i64().unwrap() > 9_000,
        "长冻结不得被短熔断覆盖"
    );
}

#[test]
fn connect_failure_does_not_freeze_when_tunable_zero() {
    let dir = tempfile::tempdir().unwrap();
    let rules_path = dir.path().join("error-rules.json");
    fs::write(
        &rules_path,
        r#"{"tunables": {"connect_error_freeze_seconds": 0}}"#,
    )
    .unwrap();
    let mut settings = test_settings();
    settings.error_rules_path = rules_path.display().to_string();
    let app = AppState::new(settings).unwrap();
    let key = connect_test_key();
    let alias = connect_test_alias(&key);
    let _ = handle_connect_failure(&app, &alias, &key, "connection refused", None);
    let mut state = app.state.lock().unwrap();
    let snap = state.snapshot().unwrap();
    assert!(
        snap["frozen"].get("zp/jt").is_none(),
        "tunable=0 时不应冻结，实际 {}",
        snap["frozen"]
    );
}

#[test]
fn connect_streak_resets_after_gap_longer_than_freeze_window() {
    let mut state = RouterState::new(test_settings()).unwrap();
    let now = now_seconds();
    // 上一轮连败 3 次、ttl 240s，但已过去 2000s（> 240×4）：视为新一轮，从 1 计。
    state.force_connect_streak("zp/jt", 3, now - 2000.0, 240.0);
    assert_eq!(state.note_connect_failure_streak("zp/jt", 30.0), 30.0);
    // 紧接着再失败：继续退避到 60。
    assert_eq!(state.note_connect_failure_streak("zp/jt", 30.0), 60.0);
}

#[test]
fn connect_streak_backoff_caps_at_32x() {
    let mut state = RouterState::new(test_settings()).unwrap();
    for _ in 0..10 {
        state.note_connect_failure_streak("k", 30.0);
    }
    assert_eq!(
        state.note_connect_failure_streak("k", 30.0),
        30.0 * 32.0,
        "退避封顶 base×32"
    );
}

#[test]
fn frozen_single_key_candidate_yields_no_available_for_fast_fallback() {
    let mut state = RouterState::new(test_settings()).unwrap();
    let key = connect_test_key();
    // 此用例断言「单 key 候选被冻结后无可用 key」，必须用单 key 别名。
    let alias = connect_test_single_key_alias(&key);
    state
        .freeze("zp/jt", now_seconds() + 60.0, "connect_error")
        .unwrap();
    // key 被短熔断冻结后，该候选在选取阶段直接无可用 key，
    // 调用方据此快速落到下一候选（粘性哈希到死 key 的 session 快速落桶）。
    let err = state
        .select_key_excluding(&alias, Some("sess-x"), &HashSet::new())
        .unwrap_err();
    assert!(err.retry_after > 0);
}

// —— 嵌入错误形状识别（HTTP 200 + 错误体）——

#[test]
fn embedded_error_shapes_including_msg_alias() {
    // 智谱/BigModel：msg 别名 + 数字错误码（2026-11 zp 鉴权失败事故）
    let zhipu = json!({"code": 1000, "msg": "身份验证失败。", "success": false});
    assert_eq!(
        extract_stream_error_event(&zhipu),
        Some(("1000".to_string(), "身份验证失败。".to_string()))
    );
    // OpenAI 系 error 对象 + 字符串码
    let openai = json!({"error": {"code": "invalid_api_key", "message": "bad key"}});
    assert_eq!(
        extract_stream_error_event(&openai),
        Some(("invalid_api_key".to_string(), "bad key".to_string()))
    );
    // Responses 失败事件
    let failed =
        json!({"type": "response.failed", "response": {"error": {"code": "x", "message": "boom"}}});
    assert_eq!(
        extract_stream_error_event(&failed),
        Some(("x".to_string(), "boom".to_string()))
    );
    // 正常响应不误判
    assert_eq!(
        extract_stream_error_event(&json!({"id": "resp_1", "output": []})),
        None
    );
}

// —— key 值只来自明文 api-keys.json（2026-11-05 移除环境变量取值）——

fn source_test_key(name: &str) -> KeyRef {
    KeyRef {
        name: name.into(),
        weight: 1,
        provider: "ark".into(),
        billing_type: "subscription".into(),
        daily_token_quota: None,
    }
}

#[test]
fn value_source_hint_points_to_api_keys_json() {
    let key = source_test_key("hevin");
    let hint = key.value_source_hint();
    assert!(hint.contains("\"hevin\""), "提示应带 key 名: {hint}");
    assert!(hint.contains("api-keys.json"), "提示应指向明文配置: {hint}");
    assert!(!hint.contains("env"), "不应再提示环境变量: {hint}");
}

#[test]
fn apply_key_value_write_persists_and_deletes() {
    let mut stored: HashMap<String, String> = HashMap::new();
    let key = source_test_key("vault-key");
    apply_key_value_write(&key, "v1", &mut stored);
    assert_eq!(stored.get("vault-key").map(String::as_str), Some("v1"));
    // 空值 = 删除语义。
    apply_key_value_write(&key, "", &mut stored);
    assert!(!stored.contains_key("vault-key"));
}

/// 回归（2026-11-05 全池瘫痪事故）：单 key 池的 connect 失败不冻结。
///
/// 冻结本意是隔离坏 key + failover 到其他 key；池里只有一个 key 时没有
/// failover 目标，冻结它等于把「偶发一次连接失败」升级成「全池不可用」，
/// 且指数退避可长到 960s（OpenClash 代理链路抖动下极易发生）。
#[test]
fn connect_failure_does_not_freeze_single_key_pool() {
    let app = AppState::new(test_settings()).unwrap();
    let key = connect_test_key();
    let alias = connect_test_single_key_alias(&key);

    match handle_connect_failure(&app, &alias, &key, "connection refused", Some("s1")) {
        FailureAction::NextKeyTransient(_) => {}
        other => panic!("期望 NextKeyTransient，实际 {other:?}"),
    }

    let mut state = app.state.lock().unwrap();
    // 不冻结：frozen 里没有该 key，后续请求可以立刻重试建连。
    assert!(
        state.snapshot().unwrap()["frozen"]["zp/jt"].is_null(),
        "单 key 池不应因 connect 失败被冻结"
    );
    assert_eq!(
        state.connect_streak_count("zp/jt"),
        0,
        "单 key 池不应累计连败计数（否则冻结关闭后仍留脏状态）"
    );
    // 错误仍然要记录（可观测性不能丢）。
    assert!(
        state.recent_errors_iter().count() > 0,
        "失败仍应记入 recent errors"
    );
}

// —— 连接失败统计（可观测性，2026-11-05）——

#[test]
fn connect_failure_stats_counts_even_for_single_key_pool() {
    let app = AppState::new(test_settings()).unwrap();
    let key = connect_test_key();
    // 单 key 池：不冻结，但统计必须累计（否则代理链路不稳完全不可见）。
    let alias = connect_test_single_key_alias(&key);
    for _ in 0..3 {
        let _ = handle_connect_failure(&app, &alias, &key, "connection reset by peer", Some("s1"));
    }

    let mut state = app.state.lock().unwrap();
    let (keys, summary) = state.connect_failures_snapshot();
    assert_eq!(summary["total"], 3, "累计次数应为 3");
    assert_eq!(summary["recent_1h"], 3, "近 1h 次数应为 3");
    assert_eq!(summary["recent_24h"], 3, "近 24h 次数应为 3");
    assert_eq!(summary["affected_keys"], 1);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["key"], "zp/jt");
    assert_eq!(keys[0]["total"], 3);
    assert!(
        keys[0]["last_message"]
            .as_str()
            .unwrap()
            .contains("connection reset"),
        "应保留最近错误摘要"
    );
    // 单 key 池仍不冻结（回归保护）。
    assert!(state.snapshot().unwrap()["frozen"]["zp/jt"].is_null());
}

#[test]
fn connect_failure_stats_prunes_entries_older_than_retention() {
    let app = AppState::new(test_settings()).unwrap();
    let key = connect_test_key();
    let alias = connect_test_single_key_alias(&key);
    {
        let mut state = app.state.lock().unwrap();
        // 直接构造：一条 25 小时前的旧记录 + 一条当前记录
        let now = now_seconds();
        state.force_connect_failure_stat(
            "zp/jt",
            crate::features::router::state::ConnectFailureStat {
                total: 10,
                last_at: now,
                last_message: "reset".into(),
                recent: vec![now - 25.0 * 3600.0, now].into_iter().collect(),
            },
        );
    }
    // 再记一次失败：应淘汰 24h 窗口外的旧时间戳
    let _ = handle_connect_failure(&app, &alias, &key, "reset", Some("s1"));

    let state = app.state.lock().unwrap();
    let (keys, summary) = state.connect_failures_snapshot();
    assert_eq!(summary["total"], 11, "total 是生命周期累计值，不随窗口淘汰");
    assert_eq!(
        summary["recent_24h"], 2,
        "25h 前的记录应被淘汰，只剩最近两条"
    );
    assert_eq!(keys[0]["recent_1h"], 2);
}
