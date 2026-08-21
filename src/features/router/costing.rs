//! 用量快照计费：按 token 价格把用量桶折算成成本（纯函数）。

use crate::config::aliases;
use crate::json_config::TokenPrice;
use serde_json::{json, Value};
use std::collections::HashMap;

pub(crate) fn default_token_prices() -> HashMap<String, TokenPrice> {
    let mut prices = HashMap::new();
    for model in aliases().keys() {
        prices.insert(model.clone(), TokenPrice::default());
    }
    prices
}

pub(crate) fn apply_costs(snapshot: &mut Value, prices: &HashMap<String, TokenPrice>) {
    let by_model_costs = snapshot
        .get("by_model")
        .and_then(Value::as_object)
        .map(|models| {
            models
                .iter()
                .map(|(model, bucket)| (model.clone(), cost_for_bucket(bucket, prices.get(model))))
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    let total = sum_costs(by_model_costs.values());
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("by_model_cost".to_string(), Value::Object(by_model_costs));
        object.insert("total_cost".to_string(), total);
    }
}

pub(crate) fn cost_for_bucket(bucket: &Value, price: Option<&TokenPrice>) -> Value {
    let prompt = bucket
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cached = bucket
        .get("cached_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let uncached = bucket
        .get("prompt_uncached_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| (prompt - cached).max(0));
    let output = bucket
        .get("completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let price = price.cloned().unwrap_or_default();
    let (input_uncached_cost, input_cached_cost, output_cost) =
        price.cost_parts(uncached, cached, output);
    let total_cost = round_money(input_uncached_cost + input_cached_cost + output_cost);
    json!({
        "input_uncached_cost": round_money(input_uncached_cost),
        "input_cached_cost": round_money(input_cached_cost),
        "output_cost": round_money(output_cost),
        "total_cost": total_cost,
    })
}

fn sum_costs<'a>(items: impl Iterator<Item = &'a Value>) -> Value {
    let mut input_uncached_cost = 0.0;
    let mut input_cached_cost = 0.0;
    let mut output_cost = 0.0;
    let mut total_cost = 0.0;
    for item in items {
        input_uncached_cost += item
            .get("input_uncached_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        input_cached_cost += item
            .get("input_cached_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        output_cost += item
            .get("output_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        total_cost += item
            .get("total_cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
    }
    json!({
        "input_uncached_cost": round_money(input_uncached_cost),
        "input_cached_cost": round_money(input_cached_cost),
        "output_cost": round_money(output_cost),
        "total_cost": round_money(total_cost),
    })
}

pub(crate) fn round_money(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}
