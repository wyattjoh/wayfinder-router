use std::cmp::Ordering;
use std::path::Path;

use wayfinder_internal_core::complexity::{
    binary_tiers, explain_score, score_complexity, FeatureContribution, RoutingConfig,
    DEFAULT_THRESHOLD,
};
use wayfinder_internal_core::config::{load_routing_config, WayfinderConfigError};
use wayfinder_internal_gateway::RelayMessage;

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

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionContext {
    pub scope: String,
    pub sticky: bool,
    pub cooldown: u32,
    pub messages: Vec<RelayMessage>,
}

impl Default for DecisionContext {
    fn default() -> Self {
        Self {
            scope: "turn".to_owned(),
            sticky: false,
            cooldown: 0,
            messages: Vec::new(),
        }
    }
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
    decide_with_context(text, start_dir, threshold, DecisionContext::default())
}

pub fn decide_with_context(
    text: &str,
    start_dir: &Path,
    threshold: Option<f64>,
    context: DecisionContext,
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

    let routed_text = scoped_text(text, &context);
    let score = score_complexity(&routed_text, &config);

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
    let targets = tiers
        .iter()
        .map(|tier| tier.model.clone())
        .collect::<Vec<_>>();
    let mut model = score.recommendation.clone();
    let mut mode = score.mode.to_owned();
    if context.sticky {
        let sticky_idx = sticky_tier_index(&context.messages, &config, tiers.len());
        if sticky_idx > idx {
            idx = sticky_idx;
            if let Some(sticky_model) = tiers.get(sticky_idx).map(|tier| tier.model.clone()) {
                model = sticky_model;
                mode = "sticky".to_owned();
            }
        }
    }

    Ok(Decision {
        text: text.to_owned(),
        model,
        score: score.score,
        mode,
        is_local: idx == 0,
        contributions: explain_score(&score.features, config.weights),
        threshold,
        targets,
    })
}

fn scoped_text(text: &str, context: &DecisionContext) -> String {
    if context.messages.is_empty() {
        return text.to_owned();
    }
    let mut parts = Vec::new();
    match context.scope.as_str() {
        "all" => {
            parts.extend(context.messages.iter().filter_map(message_content));
        }
        "user" => {
            parts.extend(
                context
                    .messages
                    .iter()
                    .filter(|message| message.role == "user")
                    .filter_map(message_content),
            );
        }
        "last_user" => {
            if let Some(content) = context
                .messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .and_then(message_content)
            {
                parts.push(content);
            }
        }
        _ => {
            parts.extend(
                context
                    .messages
                    .iter()
                    .filter(|message| message.role == "system")
                    .filter_map(message_content),
            );
            if let Some(content) = context
                .messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .and_then(message_content)
            {
                parts.push(content);
            }
        }
    }
    if parts.is_empty() {
        text.to_owned()
    } else {
        parts.join("\n")
    }
}

fn message_content(message: &RelayMessage) -> Option<String> {
    let content = message.content.trim();
    (!content.is_empty()).then(|| content.to_owned())
}

fn sticky_tier_index(
    messages: &[RelayMessage],
    config: &RoutingConfig,
    tier_count: usize,
) -> usize {
    if tier_count == 0 {
        return 0;
    }
    messages
        .iter()
        .filter(|message| message.role == "user")
        .filter_map(message_content)
        .map(|content| score_complexity(&content, config).score)
        .map(|score| tier_index(score, &config.tiers))
        .max()
        .unwrap_or(0)
        .min(tier_count.saturating_sub(1))
}

fn tier_index(score: f64, tiers: &[wayfinder_internal_core::complexity::Tier]) -> usize {
    let mut ordered = tiers.to_vec();
    if ordered.is_empty() {
        ordered = binary_tiers(DEFAULT_THRESHOLD);
    }
    ordered.sort_by(|a, b| {
        a.min_score
            .partial_cmp(&b.min_score)
            .unwrap_or(Ordering::Equal)
    });
    let mut idx = 0;
    for (i, tier) in ordered.iter().enumerate() {
        if score >= tier.min_score {
            idx = i;
        }
    }
    idx
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
