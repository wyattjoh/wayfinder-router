use std::cmp::Ordering;
use std::path::Path;

use wayfinder_internal_core::complexity::{
    binary_tiers, explain_score, score_complexity, FeatureContribution, RoutingConfig,
    DEFAULT_THRESHOLD,
};
use wayfinder_internal_core::config::{load_routing_config, WayfinderConfigError};

/// The live settings the chat manages: surfaced by `/settings`, set by commands.
///
/// Mirrors the Python `TuiState`. `pinned` is a standing route override (a configured
/// model name, or the sentinels `prefer-local` / `prefer-hosted`, or `None` for normal
/// routing). One-shot forces bypass this.
#[derive(Clone, Debug, PartialEq)]
pub struct TuiState {
    pub threshold: Option<f64>,
    pub scope: String,
    pub sticky: bool,
    pub cooldown: u32,
    pub show_why: bool,
    pub stream: bool,
    pub theme: String,
    pub pinned: Option<String>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            threshold: None,
            scope: "turn".to_owned(),
            sticky: false,
            cooldown: 0,
            show_why: false,
            stream: true,
            theme: "dark".to_owned(),
            pinned: None,
        }
    }
}

/// A scored turn: the recommendation plus the "why", for inline rendering.
///
/// `targets` is the configured model names in tier order (cheapest to most capable);
/// it resolves forced routes (`prefer-local` / `prefer-hosted`) against the same tiers.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub text: String,
    pub model: String,
    pub score: f64,
    pub mode: String,
    pub is_local: bool,
    pub contributions: Vec<FeatureContribution>,
    pub threshold: Option<f64>,
    pub targets: Vec<String>,
}

/// Score `text` and classify the route: the same path as `wayfinder-router route`.
///
/// `is_local` is true when the recommendation falls in the lowest tier (the cheap,
/// local arm); any escalation reads as cloud. Pure and offline (WF-ADR-0001).
pub fn decide(
    text: &str,
    start_dir: &Path,
    threshold: Option<f64>,
) -> Result<Decision, WayfinderConfigError> {
    let base = load_routing_config(start_dir)?;
    let config = match threshold {
        Some(threshold) => RoutingConfig {
            weights: base.weights,
            tiers: binary_tiers(threshold),
            classifier: None,
            lexicon: base.lexicon,
        },
        None => base,
    };

    let score = score_complexity(text, &config);

    let mut tiers = config.tiers.clone();
    if tiers.is_empty() {
        tiers = binary_tiers(DEFAULT_THRESHOLD);
    }
    tiers.sort_by(|a, b| {
        a.min_score
            .partial_cmp(&b.min_score)
            .unwrap_or(Ordering::Equal)
    });

    let mut idx = 0;
    for (i, tier) in tiers.iter().enumerate() {
        if score.score >= tier.min_score {
            idx = i;
        }
    }

    Ok(Decision {
        text: text.to_owned(),
        model: score.recommendation,
        score: score.score,
        mode: score.mode.to_owned(),
        is_local: idx == 0,
        contributions: explain_score(&score.features, config.weights),
        threshold,
        targets: tiers.into_iter().map(|tier| tier.model).collect(),
    })
}

/// Resolve a forced route to `(model_name, is_local)` against the decision's tiers.
///
/// `pin` is a model name, the sentinel `prefer-local` / `prefer-hosted` (cheapest /
/// most-capable tier), or `None` for the natural route. Mirrors the gateway's
/// `resolve_pin` so in-process and `--base-url` agree on what a force means.
pub fn resolve_target(pin: Option<&str>, decision: &Decision) -> (String, bool) {
    let Some(pin) = pin else {
        return (decision.model.clone(), decision.is_local);
    };
    let targets: &[String] = if decision.targets.is_empty() {
        std::slice::from_ref(&decision.model)
    } else {
        &decision.targets
    };
    let name = match pin {
        "prefer-local" => targets.first().cloned().unwrap_or_default(),
        "prefer-hosted" => targets.last().cloned().unwrap_or_default(),
        other => other.to_owned(),
    };
    let is_local = targets.first().map(|first| *first == name).unwrap_or(false);
    (name, is_local)
}

/// A short human label for a pin (sentinels read as local/cloud).
pub fn pin_label(pin: Option<&str>) -> String {
    match pin {
        None => "auto".to_owned(),
        Some("prefer-local") => "local".to_owned(),
        Some("prefer-hosted") => "cloud".to_owned(),
        Some(name) => name.to_owned(),
    }
}
