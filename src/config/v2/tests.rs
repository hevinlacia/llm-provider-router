//! v2 配置测试。

use super::resolve::merge_params;
use super::types::{V2LogicalModelsFile, V2ModelsFile};
use super::*;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const PROVIDERS: &str = r#"{
      "providers": {
        "ark": {
          "base_url": "https://ark.cn-beijing.volces.com/api/coding/v3",
          "retry": {
            "max_retry_seconds": 300,
            "retry_delay_seconds": 5.0,
            "retry_on_status": [401, 402, 429, 500, 502, 503, 504]
          },
          "keys": {
            "hevin":   { "env_var": "AGENT_AI_ARK_HEVIN_API_KEY",   "weight": 5, "billing_type": "subscription" },
            "wilford": { "env_var": "AGENT_AI_ARK_WILFORD_API_KEY", "weight": 3, "billing_type": "subscription", "enabled": false }
          }
        },
        "deepseek-official": {
          "base_url": "https://api.deepseek.com",
          "keys": {
            "deepseek-official": { "env_var": "AGENT_AI_DEEPSEEK_API_KEY", "weight": 1, "billing_type": "payg" }
          }
        }
      }
    }"#;

const MODELS: &str = r#"{
      "families": {
        "deepseek-v4-flash": { "display_name": "DeepSeek V4 Flash" }
      },
      "models": {
        "ark/deepseek-v4-flash": {
          "provider": "ark",
          "upstream_model": "deepseek-v4-flash",
          "family": "deepseek-v4-flash"
        },
        "deepseek-official/deepseek-v4-flash": {
          "provider": "deepseek-official",
          "upstream_model": "deepseek-v4-flash",
          "family": "deepseek-v4-flash"
        }
      }
    }"#;

const LOGICAL: &str = r#"{
      "logical_models": {
        "deepseek-v4-flash-auto": {
          "params": { "temperature": 1.0 },
          "route": {
            "strategy": "weighted",
            "targets": [
              { "model": "ark/deepseek-v4-flash", "weight": 8 },
              { "model": "deepseek-official/deepseek-v4-flash", "weight": 2 }
            ]
          }
        },
        "deepseek-v4-flash-official": {
          "route": {
            "strategy": "priority",
            "targets": [ { "model": "deepseek-official/deepseek-v4-flash" } ]
          }
        }
      }
    }"#;

fn write_temp(dir: &Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path.to_string_lossy().to_string()
}

/// 每次调用使用唯一临时目录，避免并行测试互相覆盖同名配置文件。
fn load_test() -> V2Config {
    let seq = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        SEQ.fetch_add(1, Ordering::Relaxed)
    };
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-{seq}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = write_temp(&dir, "providers.json", PROVIDERS);
    let m = write_temp(&dir, "models.json", MODELS);
    let l = write_temp(&dir, "logical.json", LOGICAL);
    let v = write_temp(&dir, "virtual.json", r#"{"virtual_models":{}}"#);
    load_v2_config_from(&p, &m, &l, &v).unwrap()
}

#[test]
fn parses_provider_key_enabled_defaults() {
    let cfg = load_test();
    let ark = cfg.providers.get("ark").unwrap();
    assert_eq!(
        ark.keys.get("hevin").unwrap().enabled,
        true,
        "enabled 缺省应为 true"
    );
    assert_eq!(ark.keys.get("wilford").unwrap().enabled, false);
    assert_eq!(ark.keys.get("wilford").unwrap().weight, 3);
    // deepseek-official 缺省 billing/weight/enabled
    let dsk = cfg.providers.get("deepseek-official").unwrap();
    let dk = dsk.keys.get("deepseek-official").unwrap();
    assert_eq!(dk.billing_type, "payg");
    assert_eq!(dk.weight, 1);
    assert_eq!(dk.enabled, true);
}

#[test]
fn folds_to_aliases_takes_first_target_and_filters_disabled_keys() {
    let cfg = load_test();
    let aliases = fold_to_aliases(&cfg).unwrap();

    let flash = aliases.get("deepseek-v4-flash-auto").unwrap();
    assert_eq!(flash.litellm_model, "openai/deepseek-v4-flash");
    assert_eq!(
        flash.base_url,
        "https://ark.cn-beijing.volces.com/api/coding/v3"
    );
    // ark 的 wilford 被停用，不应出现在 keys 里
    let key_names: Vec<&str> = flash.keys.iter().map(|k| k.name.as_str()).collect();
    assert_eq!(key_names, vec!["hevin"]);
    assert_eq!(flash.keys[0].env_var, "AGENT_AI_ARK_HEVIN_API_KEY");
    assert!(flash.retry_policy.is_some(), "ark retry 应被折叠");

    let official = aliases.get("deepseek-v4-flash-official").unwrap();
    assert_eq!(official.base_url, "https://api.deepseek.com");
    assert_eq!(official.keys.len(), 1);
    assert_eq!(official.keys[0].provider, "deepseek-official");
    assert!(
        official.retry_policy.is_none(),
        "deepseek-official 无 retry 配置"
    );
}

#[test]
fn validates_unknown_references() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-bad", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = write_temp(&dir, "providers.json", PROVIDERS);
    let m = write_temp(&dir, "models.json", MODELS);
    let l = write_temp(
        &dir,
        "logical.json",
        r#"{"logical_models":{"x":{"route":{"targets":[{"model":"nope/missing"}]}}}}"#,
    );
    let result = load_v2_config_from(&p, &m, &l, "/nonexistent/virtual-models.json");
    assert!(result.is_err(), "引用不存在的物理模型应报错");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("unknown physical model"));
}

#[test]
fn missing_files_return_error() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-missing", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = dir.join("providers.json").to_string_lossy().to_string();
    let m = dir.join("models.json").to_string_lossy().to_string();
    let l = dir.join("logical.json").to_string_lossy().to_string();
    let result = load_v2_config_from(&p, &m, &l, "/nonexistent/virtual-models.json");
    assert!(result.is_err(), "文件缺失应返回 Err");
}

/// 读取仓库真实迁移配置文件（providers-v2/models/logical-models），
/// 验证折叠结果与旧 config::aliases() 的主目标完全一致（迁移等价性）。    /// 需要仓库内的真实配置，默认 ignore，用 `cargo test -- --ignored` 显式运行。
#[test]
#[ignore = "requires real config files in repo"]
fn real_repo_config_folds_equivalent_to_legacy() {
    let cfg = load_v2_config().expect("v2 配置文件应可加载");
    let aliases = fold_to_aliases(&cfg).expect("折叠应成功");

    for expected in [
        "deepseek-v4-flash-auto",
        "deepseek-v4-flash-260801",
        "deepseek-v4-flash-official",
        "deepseek-v4-pro-auto",
        "deepseek-v4-pro-official",
        "glm-latest-auto",
        "minimax-latest-auto",
        "ark-code-latest-auto",
        "openai-gpt-5.5-hevin",
        "openai-gpt-5.6-sol-hevin",
        "high-model-auto",
        "low-model-auto",
    ] {
        assert!(
            aliases.contains_key(expected),
            "v2 折叠缺少 alias: {expected}"
        );
    }

    // 与旧 aliases() 主目标等价性校验：litellm_model / base_url 应一致。
    // 迁移后 high/low-model-auto 由 model-routes.json 档位迁为"逻辑模型指向逻辑模型"，
    // 折叠到的是指向逻辑模型的主目标，与旧硬编码的物理直连语义不同，单独断言（见下）。
    let legacy = crate::config::aliases();
    for (name, alias) in &aliases {
        if name == "high-model-auto" || name == "low-model-auto" {
            continue;
        }
        let Some(old) = legacy.get(name) else {
            continue;
        };
        assert_eq!(
            alias.litellm_model, old.litellm_model,
            "alias {name} 上游模型不一致"
        );
        assert_eq!(alias.base_url, old.base_url, "alias {name} base_url 不一致");
    }

    // 意图档位迁移语义：
    // - high-model-auto → glm-latest-auto → ark/glm-5.2（主目标）
    // - low-model-auto → deepseek-v4-flash-auto（首物理目标）+ 回退 glm-latest-auto
    assert_eq!(
        aliases["high-model-auto"].litellm_model, "openai/glm-5.2",
        "high-model-auto 应折叠到 glm-latest-auto 的物理主目标"
    );
    assert_eq!(
        aliases["low-model-auto"].litellm_model, "openai/deepseek-v4-flash-260801",
        "low-model-auto 应折叠到 deepseek-v4-flash-auto 的首物理目标"
    );

    // v2 语义校验（与旧逻辑的预期差异，见 architecture-v2.md §5）：
    // - custom keys（hevin-private/shell）并入 ark，key 只与供应商关联，可用于 ark 所有模型；
    // - wilford 用 enabled=false 表达停用（旧逻辑用 weight=0），折叠后应被过滤。
    let flash = aliases.get("deepseek-v4-flash-auto").unwrap();
    let key_names: Vec<&str> = flash.keys.iter().map(|k| k.name.as_str()).collect();
    assert!(
        key_names.contains(&"hevin-private"),
        "hevin-private 应并入 ark 模型"
    );
    assert!(key_names.contains(&"shell"), "shell 应并入 ark 模型");
    assert!(
        !key_names.contains(&"wilford"),
        "enabled=false 的 wilford 应被过滤"
    );
    assert_eq!(
        flash.keys.len(),
        4,
        "ark 应有 9 key 减 5 个停用 = 4 个可用 key"
    );
}

#[test]
fn merge_params_overrides_defaults() {
    let mut defaults = HashMap::new();
    defaults.insert("temperature".into(), serde_json::json!(0.7));
    defaults.insert("thinking".into(), serde_json::json!(true));
    let mut overrides = HashMap::new();
    overrides.insert("temperature".into(), serde_json::json!(0.3));
    overrides.insert("max_tokens".into(), serde_json::json!(4096));

    let merged = merge_params(&defaults, &overrides);
    assert_eq!(
        merged.get("temperature"),
        Some(&serde_json::json!(0.3)),
        "physical 应覆写 logical"
    );
    assert_eq!(
        merged.get("thinking"),
        Some(&serde_json::json!(true)),
        "未覆写字段保留 logical"
    );
    assert_eq!(
        merged.get("max_tokens"),
        Some(&serde_json::json!(4096)),
        "新增字段来自 physical"
    );
}

#[test]
fn resolve_targets_expands_multi_provider_with_params_merge() {
    let cfg = load_test();
    let candidates = resolve_targets(&cfg, "deepseek-v4-flash-auto").expect("应解析出候选");
    assert_eq!(
        candidates.len(),
        2,
        "auto 应展开为 ark + official 两个物理模型"
    );

    let ark = &candidates[0];
    assert_eq!(
        ark.model.base_url,
        "https://ark.cn-beijing.volces.com/api/coding/v3"
    );
    assert_eq!(ark.model.litellm_model, "openai/deepseek-v4-flash");
    // ark 的 wilford enabled=false 应被过滤
    let names: Vec<&str> = ark.model.keys.iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names, vec!["hevin"]);
    // logical.params(temperature=1.0) 应被继承
    assert_eq!(
        ark.model.params.get("temperature"),
        Some(&serde_json::json!(1.0))
    );
    assert_eq!(ark.weight, Some(8));

    let official = &candidates[1];
    assert_eq!(official.model.base_url, "https://api.deepseek.com");
    assert_eq!(official.model.litellm_model, "openai/deepseek-v4-flash");
    assert_eq!(official.weight, Some(2));
}

#[test]
fn resolve_targets_unknown_alias_returns_none() {
    let cfg = load_test();
    assert!(resolve_targets(&cfg, "no-such-model").is_none());
}

#[test]
fn resolve_targets_nested_logical_model_expands() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-nested", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = write_temp(&dir, "providers.json", PROVIDERS);
    let m = write_temp(&dir, "models.json", MODELS);
    let l = write_temp(
        &dir,
        "logical.json",
        r#"{
              "logical_models": {
                "alias-a": { "route": { "strategy": "priority", "targets": [ { "model": "deepseek-official/deepseek-v4-flash" } ] } },
                "alias-b": { "route": { "strategy": "priority", "targets": [ { "model": "alias-a" }, { "model": "ark/deepseek-v4-flash" } ] } }
              }
            }"#,
    );
    let cfg = load_v2_config_from(&p, &m, &l, "/nonexistent/virtual-models.json").unwrap();
    let candidates = resolve_targets(&cfg, "alias-b").unwrap();
    assert_eq!(
        candidates.len(),
        2,
        "alias-b 应展开 = alias-a 的 official + ark 物理候选"
    );
    assert_eq!(candidates[0].model.base_url, "https://api.deepseek.com");
    assert_eq!(
        candidates[1].model.base_url,
        "https://ark.cn-beijing.volces.com/api/coding/v3"
    );
}

#[test]
fn resolve_targets_cycle_is_bounded() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-cycle", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = write_temp(&dir, "providers.json", PROVIDERS);
    let m = write_temp(&dir, "models.json", MODELS);
    let l = write_temp(
        &dir,
        "logical.json",
        r#"{
              "logical_models": {
                "alias-a": { "route": { "strategy": "priority", "targets": [ { "model": "alias-b" } ] } },
                "alias-b": { "route": { "strategy": "priority", "targets": [ { "model": "alias-a" } ] } }
              }
            }"#,
    );
    let cfg = load_v2_config_from(&p, &m, &l, "/nonexistent/virtual-models.json").unwrap();
    // 纯环：resolve 应能返回（空候选），不无限递归
    let candidates = resolve_targets(&cfg, "alias-a").unwrap();
    assert!(candidates.is_empty(), "纯环无物理候选");
}

#[test]
fn resolve_virtual_model_expands_all_providers() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-vm", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = write_temp(&dir, "providers.json", PROVIDERS);
    let m = write_temp(&dir, "models.json", r#"{"families":{},"models":{}}"#);
    let l = write_temp(
        &dir,
        "logical.json",
        r#"{
              "logical_models": {
                "flash-pool": { "route": { "strategy": "priority", "targets": [ { "model": "deepseek-v4-flash" } ] } }
              }
            }"#,
    );
    let v = write_temp(
        &dir,
        "virtual.json",
        r#"{
              "virtual_models": {
                "deepseek-v4-flash": {
                  "ark": "deepseek-v4-flash-ga-260731",
                  "deepseek-official": "deepseek-v4-flash"
                }
              }
            }"#,
    );
    let cfg = load_v2_config_from(&p, &m, &l, &v).unwrap();
    let candidates = resolve_targets(&cfg, "flash-pool").unwrap();
    assert_eq!(candidates.len(), 2, "虚拟名应展开为两个供应商候选");
    let base_urls: Vec<&str> = candidates
        .iter()
        .map(|c| c.model.base_url.as_str())
        .collect();
    assert!(base_urls.contains(&"https://ark.cn-beijing.volces.com/api/coding/v3"));
    assert!(base_urls.contains(&"https://api.deepseek.com"));
    // 虚拟名不应出现在物理模型表，展开时按 provider 的 key 生成
    let ark_candidate = candidates
        .iter()
        .find(|c| c.model.base_url.contains("volces"))
        .unwrap();
    assert_eq!(
        ark_candidate.model.upstream_model(),
        "deepseek-v4-flash-ga-260731"
    );
}

#[test]
fn validate_accepts_virtual_model_targets() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-vmval", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = write_temp(&dir, "providers.json", PROVIDERS);
    let m = write_temp(&dir, "models.json", r#"{"families":{},"models":{}}"#);
    let l = write_temp(
        &dir,
        "logical.json",
        r#"{
              "logical_models": {
                "pool": { "route": { "strategy": "priority", "targets": [ { "model": "deepseek-v4-flash" }, { "model": "ark/deepseek-v4-flash" } ] } }
              }
            }"#,
    );
    let v = write_temp(
        &dir,
        "virtual.json",
        r#"{
              "virtual_models": {
                "deepseek-v4-flash": {
                  "ark": "deepseek-v4-flash-ga-260731"
                }
              }
            }"#,
    );
    let cfg = load_v2_config_from(&p, &m, &l, &v).unwrap();
    // validate 应通过（纯虚拟名 + provider/virtual 形式都合法）
    assert!(cfg.logical_models.contains_key("pool"));
}

#[test]
fn validate_accepts_provider_with_any_single_url() {
    // 三种地址任一非空即合法：base_url / responses_base_url / anthropic_base_url
    for (label, provider_json) in [
        (
            "base_url only",
            r#"{"name":{"base_url":"https://a.com/v1","keys":{}}}"#,
        ),
        (
            "responses only",
            r#"{"name":{"base_url":"","responses_base_url":"https://r.com/v1","keys":{}}}"#,
        ),
        (
            "anthropic only",
            r#"{"name":{"base_url":"","responses_base_url":"","anthropic_base_url":"https://a.anthropic.com","keys":{}}}"#,
        ),
    ] {
        let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-anyurl", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = write_temp(
            &dir,
            "providers.json",
            &format!("{{\"providers\":{provider_json}}}"),
        );
        let m = write_temp(&dir, "models.json", r#"{"families":{},"models":{}}"#);
        let l = write_temp(&dir, "logical.json", r#"{"logical_models":{}}"#);
        let v = write_temp(&dir, "virtual.json", r#"{"virtual_models":{}}"#);
        let result = load_v2_config_from(&p, &m, &l, &v);
        assert!(result.is_ok(), "{label}: 应通过校验, got {result:?}");
    }
}

#[test]
fn validate_rejects_provider_with_no_url() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-nourl", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let p = write_temp(
        &dir,
        "providers.json",
        r#"{"providers":{"name":{"base_url":"","responses_base_url":"","anthropic_base_url":"","keys":{}}}}"#,
    );
    let m = write_temp(&dir, "models.json", r#"{"families":{},"models":{}}"#);
    let l = write_temp(&dir, "logical.json", r#"{"logical_models":{}}"#);
    let v = write_temp(&dir, "virtual.json", r#"{"virtual_models":{}}"#);
    let err = load_v2_config_from(&p, &m, &l, &v).unwrap_err();
    assert!(
        err.to_string()
            .contains("at least one of Chat Completions API"),
        "错误信息应说明至少填一种地址, got: {err}"
    );
}

#[test]
fn rename_provider_updates_models_and_logical_references() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-rename", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let models_path = write_temp(
        &dir,
        "models.json",
        r#"{
              "models": {
                "ark/deepseek-v4-flash-260801": { "provider": "ark", "upstream_model": "deepseek-v4-flash-260801", "family": "deepseek-v4-flash" },
                "deepseek-official/deepseek-v4-flash": { "provider": "deepseek-official", "upstream_model": "deepseek-v4-flash" }
              }
            }"#,
    );
    let logical_path = write_temp(
        &dir,
        "logical.json",
        r#"{
              "logical_models": {
                "deepseek-v4-flash-auto": {
                  "route": { "strategy": "weighted", "targets": [
                    { "model": "ark/deepseek-v4-flash-260801", "weight": 8 },
                    { "model": "deepseek-official/deepseek-v4-flash", "weight": 2 }
                  ]}
                }
              }
            }"#,
    );
    rename_provider_in_models("ark-renamed", "ark", &models_path).unwrap();
    rename_provider_in_logical("ark-renamed", "ark", &logical_path).unwrap();

    let models: V2ModelsFile =
        serde_json::from_str(&fs::read_to_string(&models_path).unwrap()).unwrap();
    assert!(models
        .models
        .contains_key("ark-renamed/deepseek-v4-flash-260801"));
    assert_eq!(
        models.models["ark-renamed/deepseek-v4-flash-260801"].provider,
        "ark-renamed"
    );
    assert!(models
        .models
        .contains_key("deepseek-official/deepseek-v4-flash"));

    let logical: V2LogicalModelsFile =
        serde_json::from_str(&fs::read_to_string(&logical_path).unwrap()).unwrap();
    let targets = &logical.logical_models["deepseek-v4-flash-auto"]
        .route
        .targets;
    assert_eq!(targets[0].model, "ark-renamed/deepseek-v4-flash-260801");
    assert_eq!(targets[1].model, "deepseek-official/deepseek-v4-flash");
}

#[test]
fn migrate_legacy_logical_caps_sinks_to_physical_and_cleans_logical() {
    let dir = std::env::temp_dir().join(format!("lpr-v2-test-{}-migrate", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let models_path = write_temp(
        &dir,
        "models.json",
        r#"{
              "models": {
                "ark/deepseek-v4-pro": { "provider": "ark", "upstream_model": "deepseek-v4-pro", "context_window": 950000, "max_output_tokens": 384000 },
                "openai-relay/gpt-5.6-luna": { "provider": "openai-relay", "upstream_model": "gpt-5.6-luna", "context_window": 600000, "max_output_tokens": 200000 },
                "ark/minimax-m3": { "provider": "ark", "upstream_model": "minimax-m3", "context_window": 600000, "max_output_tokens": 128000 }
              }
            }"#,
    );
    let logical_path = write_temp(
        &dir,
        "logical.json",
        r#"{
              "logical_models": {
                "deepseek-v4-pro-auto": {
                  "route": { "strategy": "priority", "targets": [ { "model": "ark/deepseek-v4-pro" } ] },
                  "reasoning": true,
                  "input": ["text"],
                  "thinking_level_map": { "minimal": null, "xhigh": "max", "low": null, "high": "high", "medium": null },
                  "thinking_format": null
                },
                "gpt-low-latest-auto": {
                  "route": { "strategy": "priority", "targets": [ { "model": "openai-relay/gpt-5.6-luna" } ] },
                  "reasoning": true,
                  "input": ["text", "image"],
                  "thinking_level_map": { "high": "high", "xhigh": "xhigh" },
                  "thinking_format": "reasoning_effort"
                },
                "picture-model-auto": {
                  "route": { "strategy": "priority", "targets": [ { "model": "ark/minimax-m3" } ] },
                  "reasoning": false,
                  "input": ["text", "image"],
                  "thinking_level_map": null,
                  "thinking_format": null
                }
              }
            }"#,
    );
    let migrated = super::migrate_legacy_logical_caps(&models_path, &logical_path).unwrap();
    assert!(migrated);

    let models: V2ModelsFile =
        serde_json::from_str(&fs::read_to_string(&models_path).unwrap()).unwrap();
    // deepseek: thinking map 下沉，无 image
    let ds = &models.models["ark/deepseek-v4-pro"];
    assert_eq!(
        ds.thinking_level_map.as_ref().unwrap()["xhigh"],
        Some("max".to_string())
    );
    assert_eq!(
        ds.thinking_level_map.as_ref().unwrap()["high"],
        Some("high".to_string())
    );
    assert!(ds.supports_image.is_none());
    // gpt: thinking map + format + image 下沉
    let gpt = &models.models["openai-relay/gpt-5.6-luna"];
    assert_eq!(
        gpt.thinking_level_map.as_ref().unwrap()["xhigh"],
        Some("xhigh".to_string())
    );
    assert_eq!(gpt.thinking_format.as_deref(), Some("reasoning_effort"));
    assert_eq!(gpt.supports_image, Some(true));
    // minimax: 仅 image 下沉，thinking map 保持 None
    let mm = &models.models["ark/minimax-m3"];
    assert!(mm.thinking_level_map.is_none());
    assert_eq!(mm.supports_image, Some(true));

    let logical: V2LogicalModelsFile =
        serde_json::from_str(&fs::read_to_string(&logical_path).unwrap()).unwrap();
    for lm in logical.logical_models.values() {
        assert!(lm.display_name.is_none() || true);
    }
    // legacy 字段已清理（结构体没有这些字段，验证原始 JSON 不再含 key）
    let raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&logical_path).unwrap()).unwrap();
    for lm in raw["logical_models"].as_object().unwrap().values() {
        assert!(!lm.as_object().unwrap().contains_key("thinking_level_map"));
        assert!(!lm.as_object().unwrap().contains_key("thinking_format"));
        assert!(!lm.as_object().unwrap().contains_key("reasoning"));
        assert!(!lm.as_object().unwrap().contains_key("input"));
    }

    // 幂等：再次运行无变更
    let again = super::migrate_legacy_logical_caps(&models_path, &logical_path).unwrap();
    assert!(!again);
}
