use std::cmp::Ordering;
use std::time::Duration;

use serde_json::{json, Value};
use wayfinder_internal_core::complexity::FeatureContribution;

use crate::Decision;

/// Build a [`Decision`] from a gateway `X-Wayfinder-Debug` `wayfinder` payload.
///
/// Lets the `--base-url` thin client render the same decision-first line and "why"
/// breakdown the in-process backend shows, from the remote gateway's response. The
/// natural route is the highest tier whose `min_score` the score clears; when the
/// gateway pinned the call `payload["model"]` is the forced target, not this, so the
/// view is derived from score + tiers like the local path (see [`crate::decide`]).
pub fn decision_from_debug(payload: &Value, text: &str) -> Decision {
    let mut tiers: Vec<&Value> = payload
        .get("tiers")
        .and_then(Value::as_array)
        .map(|tiers| tiers.iter().collect())
        .unwrap_or_default();
    tiers.sort_by(|a, b| {
        tier_min_score(a)
            .partial_cmp(&tier_min_score(b))
            .unwrap_or(Ordering::Equal)
    });

    let score = payload.get("score").and_then(Value::as_f64).unwrap_or(0.0);

    let mut nat_idx = 0;
    for (i, tier) in tiers.iter().enumerate() {
        if score >= tier_min_score(tier) {
            nat_idx = i;
        }
    }

    let model = match tiers.get(nat_idx) {
        Some(tier) => tier_model(tier),
        None => payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned(),
    };

    let contributions = payload
        .get("contributions")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(contribution_from).collect())
        .unwrap_or_default();

    Decision {
        text: text.to_owned(),
        model,
        score,
        mode: payload
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        is_local: !tiers.is_empty() && nat_idx == 0,
        contributions,
        threshold: None,
        targets: tiers.iter().map(|tier| tier_model(tier)).collect(),
    }
}

fn tier_min_score(tier: &Value) -> f64 {
    tier.get("min_score").and_then(Value::as_f64).unwrap_or(0.0)
}

fn tier_model(tier: &Value) -> String {
    tier.get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn contribution_from(value: &Value) -> FeatureContribution {
    FeatureContribution {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        value: value.get("value").and_then(Value::as_u64).unwrap_or(0) as usize,
        normalized: value
            .get("normalized")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        weight: value.get("weight").and_then(Value::as_f64).unwrap_or(0.0),
        contribution: value
            .get("contribution")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    }
}

/// POST to a running gateway's `/v1/chat/completions`; return `(decision, reply)`.
///
/// The thin-client backend: the remote gateway makes the routing decision (surfaced
/// via `X-Wayfinder-Debug`) and the reply. Non-streaming, mirroring the Python path.
/// `model` is the OpenAI `model` field: `"auto"` routes, a concrete name or
/// `prefer-local` / `prefer-hosted` forces the call server-side. Transport errors map
/// to a friendly hint via [`friendly_error`].
pub fn remote_reply(
    base_url: &str,
    messages: &[Value],
    model: &str,
    threshold: Option<f64>,
    timeout: Duration,
) -> Result<(Option<Decision>, Option<String>), String> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let body = json!({ "model": model, "messages": messages });

    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| friendly_error(&err.to_string(), base_url))?;

    let mut request = client
        .post(&url)
        .header("X-Wayfinder-Debug", "1")
        .json(&body);
    if let Some(threshold) = threshold {
        request = request.header("X-Wayfinder-Threshold", threshold.to_string());
    }

    let response = request
        .send()
        .map_err(|err| friendly_error(&err.to_string(), base_url))?;
    let status = response.status();
    let data: Value = response
        .json()
        .map_err(|_| format!("gateway returned non-JSON ({})", status.as_u16()))?;

    let decision = data
        .get("wayfinder")
        .filter(|wayfinder| wayfinder.is_object())
        .map(|wayfinder| decision_from_debug(wayfinder, ""));
    let reply = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    Ok((decision, reply))
}

/// Turn a raw relay error into a hint when the endpoint looks simply unreachable.
///
/// Mirrors the Python `_friendly_error`: connection / refused / timeout / name
/// resolution failures read as "can't reach", with a special case for the Ollama
/// `11434` port; anything else surfaces as an upstream error.
pub fn friendly_error(message: &str, base_url: &str) -> String {
    let low = message.to_lowercase();
    let unreachable = [
        "connect",
        "refused",
        "timed out",
        "timeout",
        "name or service",
    ]
    .iter()
    .any(|needle| low.contains(needle));
    if !unreachable {
        return format!("upstream error: {message}");
    }
    if base_url.contains("11434") {
        return format!(
            "can't reach the local model at {base_url} - is Ollama running? (`ollama serve`)"
        );
    }
    format!("can't reach {base_url} - is it running and reachable?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_from_debug_uses_natural_tier() {
        let payload = json!({
            "score": 0.62,
            "mode": "tiered",
            "model": "cloud-gpt",
            "tiers": [
                {"model": "local-llama", "min_score": 0.0},
                {"model": "cloud-gpt", "min_score": 0.5},
            ],
            "contributions": [
                {
                    "name": "word_count",
                    "value": 120,
                    "normalized": 0.6,
                    "weight": 1.0,
                    "contribution": 0.3,
                },
            ],
        });

        let decision = decision_from_debug(&payload, "explain this");

        assert_eq!(decision.text, "explain this");
        assert_eq!(decision.model, "cloud-gpt");
        assert!((decision.score - 0.62).abs() < 1e-9);
        assert_eq!(decision.mode, "tiered");
        assert!(!decision.is_local);
        assert_eq!(decision.targets, vec!["local-llama", "cloud-gpt"]);
        assert_eq!(decision.contributions.len(), 1);
        let contribution = &decision.contributions[0];
        assert_eq!(contribution.name, "word_count");
        assert_eq!(contribution.value, 120);
        assert!((contribution.normalized - 0.6).abs() < 1e-9);
    }

    #[test]
    fn decision_from_debug_keeps_low_score_local() {
        let payload = json!({
            "score": 0.1,
            "mode": "tiered",
            "tiers": [
                {"model": "local-llama", "min_score": 0.0},
                {"model": "cloud-gpt", "min_score": 0.5},
            ],
        });

        let decision = decision_from_debug(&payload, "");

        assert_eq!(decision.model, "local-llama");
        assert!(decision.is_local);
        assert!(decision.contributions.is_empty());
    }

    #[test]
    fn friendly_error_flags_unreachable_endpoint() {
        let hint = friendly_error("Connection refused (os error 61)", "http://localhost:8000");
        assert!(hint.contains("can't reach http://localhost:8000"));
        assert!(!hint.contains("Ollama"));
    }

    #[test]
    fn friendly_error_hints_ollama_port() {
        let hint = friendly_error("connection refused", "http://localhost:11434");
        assert!(hint.contains("Ollama"));
        assert!(hint.contains("ollama serve"));
    }

    #[test]
    fn friendly_error_passes_through_other_failures() {
        let hint = friendly_error("502 bad gateway", "http://localhost:8000");
        assert_eq!(hint, "upstream error: 502 bad gateway");
    }
}
