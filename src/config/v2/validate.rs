use super::types::V2Config;
use anyhow::anyhow;
pub fn validate(cfg: &V2Config) -> anyhow::Result<()> {
    for (provider_name, provider) in &cfg.providers {
        // 三类地址至少填一种：Chat Completions API(base_url) / Responses API(responses_base_url) / Anthropic API(anthropic_base_url)
        let has_any_url = !provider.base_url.trim().is_empty()
            || provider
                .responses_base_url
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            || provider
                .anthropic_base_url
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
        if !has_any_url {
            return Err(anyhow!(
                "provider {provider_name}: at least one of Chat Completions API / Responses API / Anthropic API base URL must be set"
            ));
        }
        for (key_name, key) in &provider.keys {
            if key.env_var.is_empty() {
                return Err(anyhow!(
                    "provider {provider_name} key {key_name}: env_var is empty"
                ));
            }
        }
    }
    for (model_id, model) in &cfg.models {
        if !cfg.providers.contains_key(&model.provider) {
            return Err(anyhow!(
                "model {model_id}: references unknown provider '{}'",
                model.provider
            ));
        }
        if let Some(family) = &model.family {
            // family 允许指向未显式声明的族（隐式族），不强制校验。
            let _ = family;
        }
        if let Some(w) = model.context_window {
            if w == 0 {
                return Err(anyhow!("model {model_id}: context_window must be > 0"));
            }
        }
        if let Some(m) = model.max_output_tokens {
            if m == 0 {
                return Err(anyhow!("model {model_id}: max_output_tokens must be > 0"));
            }
        }
    }
    for (alias, lm) in &cfg.logical_models {
        if lm.route.targets.is_empty() {
            return Err(anyhow!("logical model {alias}: route has no targets"));
        }
        for target in &lm.route.targets {
            let known_physical = cfg.models.contains_key(&target.model);
            let known_logical = cfg.logical_models.contains_key(&target.model)
                && target.model.as_str() != alias.as_str();
            let known_virtual = cfg.virtual_models.contains_key(&target.model)
                || is_provider_scoped_virtual(cfg, &target.model);
            if !known_physical && !known_logical && !known_virtual {
                return Err(anyhow!(
                    "logical model {alias}: target references unknown physical model or logical model '{}'",
                    target.model
                ));
            }
        }
    }
    Ok(())
}

/// 判断 `provider/virtual` 形式（限定某供应商下的虚拟模型）是否有效。
pub fn is_provider_scoped_virtual(cfg: &V2Config, target: &str) -> bool {
    let Some((provider, rest)) = target.split_once('/') else {
        return false;
    };
    cfg.providers.contains_key(provider)
        && cfg
            .virtual_models
            .get(rest)
            .is_some_and(|m| m.contains_key(provider))
}
