use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const CHARS_PER_TOKEN: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageTokens {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub estimated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnCost {
    pub route: String,
    pub realized: f64,
    pub baseline: f64,
    pub savings: f64,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub estimated: bool,
}

pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        (text.len() / CHARS_PER_TOKEN).max(1)
    }
}

pub fn price_table<I, K, L, M>(model_costs: I, tier_ladder: L) -> (BTreeMap<String, f64>, bool)
where
    I: IntoIterator<Item = (K, Option<f64>)>,
    K: AsRef<str>,
    L: IntoIterator<Item = M>,
    M: AsRef<str>,
{
    let costs: Vec<(String, Option<f64>)> = model_costs
        .into_iter()
        .map(|(name, cost)| (name.as_ref().to_string(), cost))
        .collect();
    let real = costs
        .iter()
        .filter_map(|(name, cost)| cost.map(|cost| (name.clone(), cost)))
        .collect::<BTreeMap<_, _>>();
    if !real.is_empty() {
        return (real, true);
    }

    let ladder = tier_ladder
        .into_iter()
        .map(|name| name.as_ref().to_string())
        .collect::<Vec<_>>();
    let ladder = if ladder.is_empty() {
        costs
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    } else {
        ladder
    };
    if ladder.is_empty() {
        return (BTreeMap::new(), false);
    }

    let low = 0.2;
    let high = 1.0;
    let step = (high - low) / (ladder.len().saturating_sub(1).max(1) as f64);
    let fallback = ladder
        .iter()
        .enumerate()
        .map(|(index, name)| (name.clone(), round_to(low + index as f64 * step, 3)))
        .collect();
    (fallback, false)
}

pub fn table_version<I, K>(costs: I) -> String
where
    I: IntoIterator<Item = (K, f64)>,
    K: AsRef<str>,
{
    let costs = costs
        .into_iter()
        .map(|(name, cost)| (name.as_ref().to_string(), cost))
        .collect::<BTreeMap<_, _>>();
    let mut parts = Vec::with_capacity(costs.len());
    for (name, cost) in costs {
        parts.push(format!("{}:{}", json_string(&name), json_number(cost)));
    }
    let blob = format!("{{{}}}", parts.join(","));
    let digest = Sha256::digest(blob.as_bytes());
    hex_lower(&digest)[..12].to_string()
}

pub fn usage_tokens(response: &Value, prompt_text: &str, completion_text: &str) -> UsageTokens {
    if let Some(usage) = response.get("usage").and_then(Value::as_object) {
        let prompt = int_field(usage, "prompt_tokens");
        let completion = int_field(usage, "completion_tokens");
        if let (Some(prompt_tokens), Some(completion_tokens)) = (prompt, completion) {
            return UsageTokens {
                prompt_tokens,
                completion_tokens,
                estimated: false,
            };
        }
        if let Some(total_tokens) = int_field(usage, "total_tokens") {
            let known = prompt.unwrap_or(0);
            return UsageTokens {
                prompt_tokens: known,
                completion_tokens: total_tokens.saturating_sub(known),
                estimated: false,
            };
        }
    }

    UsageTokens {
        prompt_tokens: estimate_tokens(prompt_text),
        completion_tokens: estimate_tokens(completion_text),
        estimated: true,
    }
}

pub fn turn_cost<I, K>(
    route: &str,
    prompt_tokens: usize,
    completion_tokens: usize,
    costs: I,
    estimated: bool,
    baseline: Option<&str>,
) -> TurnCost
where
    I: IntoIterator<Item = (K, f64)>,
    K: AsRef<str>,
{
    let costs = costs
        .into_iter()
        .map(|(name, cost)| (name.as_ref().to_string(), cost))
        .collect::<BTreeMap<_, _>>();
    let total_k = (prompt_tokens + completion_tokens) as f64 / 1000.0;
    let dearest = costs.values().copied().fold(0.0, f64::max);
    let baseline_per_1k = baseline
        .and_then(|name| costs.get(name).copied())
        .unwrap_or(dearest);
    let chosen_per_1k = costs.get(route).copied().unwrap_or(dearest);
    let realized = round_to(chosen_per_1k * total_k, 6);
    let base = round_to(baseline_per_1k * total_k, 6);

    TurnCost {
        route: route.to_string(),
        realized,
        baseline: base,
        savings: round_to(base - realized, 6),
        prompt_tokens,
        completion_tokens,
        estimated,
    }
}

fn int_field(map: &Map<String, Value>, key: &str) -> Option<usize> {
    map.get(key)
        .and_then(Value::as_i64)
        .and_then(|value| usize::try_from(value).ok())
}

fn round_to(value: f64, places: i32) -> f64 {
    let factor = 10_f64.powi(places);
    (value * factor).round() / factor
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization should not fail")
}

fn json_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        let mut text = format!("{value}");
        if text.contains('e') {
            text = format!("{value:?}");
        }
        text
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
