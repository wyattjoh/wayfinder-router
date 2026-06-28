use std::env;
use std::fmt;

use toml::Value;

use crate::complexity::{
    binary_tiers, ClassifierModel, ClassifierWeights, FeatureWeights, Lexicon, RoutingConfig, Tier,
    DEFAULT_THRESHOLD, DEFAULT_WEIGHTS, FEATURE_ORDER,
};

pub const CONFIG_FILE: &str = "wayfinder-router.toml";
pub const THRESHOLD_ENV: &str = "WAYFINDER_ROUTER_THRESHOLD";
const MAX_LEXICON_TERMS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WayfinderConfigError {
    message: String,
}

impl WayfinderConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for WayfinderConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WayfinderConfigError {}

pub fn routing_config_from_toml(
    text: &str,
    where_: &str,
) -> Result<RoutingConfig, WayfinderConfigError> {
    let data: Value = text
        .parse()
        .map_err(|err| WayfinderConfigError::new(format!("{where_}: invalid TOML: {err}")))?;
    let routing = match data.get("routing") {
        Some(Value::Table(table)) => table,
        Some(_) => {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: '[routing]' must be a table"
            )));
        }
        None => {
            return Ok(RoutingConfig {
                tiers: binary_tiers(apply_env_threshold(DEFAULT_THRESHOLD)?),
                ..RoutingConfig::default()
            });
        }
    };

    let weights = parse_weights(where_, routing.get("weights"))?;
    let lexicon = match routing.get("lexicon") {
        Some(value) => parse_lexicon(where_, value)?,
        None => Lexicon::default(),
    };

    if let Some(value) = routing.get("classifier") {
        return Ok(RoutingConfig {
            weights,
            tiers: binary_tiers(DEFAULT_THRESHOLD),
            classifier: Some(parse_classifier(where_, value)?),
            lexicon,
        });
    }
    if let Some(value) = routing.get("tiers") {
        return Ok(RoutingConfig {
            weights,
            tiers: parse_tiers(where_, value)?,
            classifier: None,
            lexicon,
        });
    }

    let threshold = parse_threshold(where_, routing.get("threshold"), DEFAULT_THRESHOLD)?;
    Ok(RoutingConfig {
        weights,
        tiers: binary_tiers(apply_env_threshold(threshold)?),
        classifier: None,
        lexicon,
    })
}

pub fn dump_routing_toml(config: &RoutingConfig) -> String {
    let mut blocks = Vec::new();
    if config.weights != DEFAULT_WEIGHTS {
        let items = FEATURE_ORDER
            .iter()
            .map(|name| format!("{name} = {}", fmt_num(config.weights.get(name).unwrap())))
            .collect::<Vec<_>>()
            .join(", ");
        blocks.push(format!("[routing]\nweights = {{ {items} }}"));
    }

    if config.lexicon != Lexicon::default() {
        let mut lines = vec!["[routing.lexicon]".to_string()];
        let default = Lexicon::default();
        if config.lexicon.reasoning_terms != default.reasoning_terms {
            lines.push(format!(
                "reasoning_terms = [{}]",
                quoted_terms(&config.lexicon.reasoning_terms)
            ));
        }
        if config.lexicon.constraint_terms != default.constraint_terms {
            lines.push(format!(
                "constraint_terms = [{}]",
                quoted_terms(&config.lexicon.constraint_terms)
            ));
        }
        blocks.push(lines.join("\n"));
    }

    if let Some(classifier) = &config.classifier {
        let models = classifier
            .models
            .iter()
            .map(|model| quote(model))
            .collect::<Vec<_>>()
            .join(", ");
        let intercepts = classifier
            .intercepts
            .iter()
            .map(|value| fmt_num(*value))
            .collect::<Vec<_>>()
            .join(", ");
        let mut lines = vec![
            "[routing.classifier]".to_string(),
            format!("models = [{models}]"),
            format!("intercepts = [{intercepts}]"),
            String::new(),
            "[routing.classifier.weights]".to_string(),
        ];
        for name in FEATURE_ORDER {
            let weights = classifier
                .weights
                .get(name)
                .unwrap()
                .iter()
                .map(|value| fmt_num(*value))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("{name} = [{weights}]"));
        }
        blocks.push(lines.join("\n"));
    } else {
        blocks.push(
            config
                .tiers
                .iter()
                .map(dump_tier)
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }

    format!("{}\n", blocks.join("\n\n"))
}

fn parse_threshold(
    where_: &str,
    value: Option<&Value>,
    default: f64,
) -> Result<f64, WayfinderConfigError> {
    match value {
        None => Ok(default),
        Some(Value::Float(value)) if (0.0..=1.0).contains(value) => Ok(*value),
        Some(Value::Integer(value)) if (0..=1).contains(value) => Ok(*value as f64),
        _ => Err(WayfinderConfigError::new(format!(
            "{where_}: 'routing.threshold' must be a number in 0.0-1.0"
        ))),
    }
}

fn parse_weights(
    where_: &str,
    value: Option<&Value>,
) -> Result<FeatureWeights, WayfinderConfigError> {
    let mut weights = DEFAULT_WEIGHTS;
    let Some(value) = value else {
        return Ok(weights);
    };
    let Value::Table(table) = value else {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: 'routing.weights' must be a table"
        )));
    };
    for (name, value) in table {
        if !FEATURE_ORDER.contains(&name.as_str()) {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: 'routing.weights.{name}' is not a known feature (one of {})",
                FEATURE_ORDER.join(", ")
            )));
        }
        let Some(weight) = non_negative_number(value) else {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: 'routing.weights.{name}' must be a non-negative number"
            )));
        };
        weights.set(name, weight);
    }
    Ok(weights)
}

fn parse_lexicon(where_: &str, value: &Value) -> Result<Lexicon, WayfinderConfigError> {
    let Value::Table(table) = value else {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: '[routing.lexicon]' must be a table"
        )));
    };
    for key in table.keys() {
        if key != "reasoning_terms" && key != "constraint_terms" {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: unknown 'routing.lexicon' keys: {key} (known: constraint_terms, reasoning_terms)"
            )));
        }
    }
    let mut lexicon = Lexicon::default();
    if let Some(value) = table.get("reasoning_terms") {
        lexicon.reasoning_terms = term_list(where_, "routing.lexicon.reasoning_terms", value)?;
    }
    if let Some(value) = table.get("constraint_terms") {
        lexicon.constraint_terms = term_list(where_, "routing.lexicon.constraint_terms", value)?;
    }
    Ok(lexicon)
}

fn term_list(
    where_: &str,
    label: &str,
    value: &Value,
) -> Result<Vec<String>, WayfinderConfigError> {
    let Value::Array(values) = value else {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: '{label}' must be a list of non-empty strings"
        )));
    };
    if values.len() > MAX_LEXICON_TERMS {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: '{label}' has more than {MAX_LEXICON_TERMS} terms"
        )));
    }
    let mut terms = Vec::with_capacity(values.len());
    for value in values {
        match value {
            Value::String(term) if !term.trim().is_empty() => {
                terms.push(term.trim().to_lowercase());
            }
            _ => {
                return Err(WayfinderConfigError::new(format!(
                    "{where_}: '{label}' must be a list of non-empty strings"
                )));
            }
        }
    }
    terms.sort();
    terms.dedup();
    Ok(terms)
}

fn parse_tiers(where_: &str, value: &Value) -> Result<Vec<Tier>, WayfinderConfigError> {
    let Value::Array(entries) = value else {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: 'routing.tiers' must be a non-empty array of tables"
        )));
    };
    if entries.is_empty() {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: 'routing.tiers' must be a non-empty array of tables"
        )));
    }

    let mut tiers = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Table(table) = entry else {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: each '[[routing.tiers]]' must be a table"
            )));
        };
        let min_score = match table.get("min_score") {
            Some(Value::Float(value)) if (0.0..=1.0).contains(value) => *value,
            Some(Value::Integer(value)) if (0..=1).contains(value) => *value as f64,
            _ => {
                return Err(WayfinderConfigError::new(format!(
                    "{where_}: tier 'min_score' must be a number in 0.0-1.0"
                )));
            }
        };
        let model = match table.get("model") {
            Some(Value::String(model)) if !model.is_empty() => model.clone(),
            _ => {
                return Err(WayfinderConfigError::new(format!(
                    "{where_}: tier 'model' must be a non-empty string"
                )));
            }
        };
        let cost = match table.get("cost") {
            None => None,
            Some(value) => match non_negative_number(value) {
                Some(cost) => Some(cost),
                None => {
                    return Err(WayfinderConfigError::new(format!(
                        "{where_}: tier 'cost' must be a non-negative number"
                    )));
                }
            },
        };
        tiers.push(Tier {
            min_score,
            model,
            cost,
        });
    }

    tiers.sort_by(|a, b| a.min_score.total_cmp(&b.min_score));
    if tiers[0].min_score != 0.0 {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: the first tier must have min_score = 0.0"
        )));
    }
    for pair in tiers.windows(2) {
        if pair[1].min_score <= pair[0].min_score {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: tier 'min_score' values must be strictly ascending"
            )));
        }
    }
    Ok(tiers)
}

fn parse_classifier(where_: &str, value: &Value) -> Result<ClassifierModel, WayfinderConfigError> {
    let Value::Table(table) = value else {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: '[routing.classifier]' must be a table"
        )));
    };
    let models = parse_models(where_, table.get("models"))?;
    let count = models.len();
    let intercepts = number_vector(
        where_,
        "routing.classifier.intercepts",
        table.get("intercepts"),
        count,
    )?;

    let raw_weights = match table.get("weights") {
        Some(Value::Table(weights)) => weights,
        _ => {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: '[routing.classifier.weights]' must be a table"
            )));
        }
    };
    for name in raw_weights.keys() {
        if !FEATURE_ORDER.contains(&name.as_str()) {
            return Err(WayfinderConfigError::new(format!(
                "{where_}: 'routing.classifier.weights.{name}' is not a known feature"
            )));
        }
    }

    let mut weights = ClassifierWeights::zeros(count);
    for name in FEATURE_ORDER {
        if let Some(value) = raw_weights.get(name) {
            weights.set(
                name,
                number_vector(
                    where_,
                    &format!("routing.classifier.weights.{name}"),
                    Some(value),
                    count,
                )?,
            );
        }
    }

    Ok(ClassifierModel {
        models,
        weights,
        intercepts,
    })
}

fn parse_models(where_: &str, value: Option<&Value>) -> Result<Vec<String>, WayfinderConfigError> {
    let Some(Value::Array(values)) = value else {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: 'routing.classifier.models' must be 2+ unique non-empty strings"
        )));
    };
    let mut models = Vec::with_capacity(values.len());
    for value in values {
        match value {
            Value::String(model) if !model.is_empty() => models.push(model.clone()),
            _ => {
                return Err(WayfinderConfigError::new(format!(
                    "{where_}: 'routing.classifier.models' must be 2+ unique non-empty strings"
                )));
            }
        }
    }
    let mut sorted = models.clone();
    sorted.sort();
    sorted.dedup();
    if models.len() < 2 || sorted.len() != models.len() {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: 'routing.classifier.models' must be 2+ unique non-empty strings"
        )));
    }
    Ok(models)
}

fn number_vector(
    where_: &str,
    label: &str,
    value: Option<&Value>,
    count: usize,
) -> Result<Vec<f64>, WayfinderConfigError> {
    let Some(Value::Array(values)) = value else {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: '{label}' must be a list of {count} numbers"
        )));
    };
    if values.len() != count {
        return Err(WayfinderConfigError::new(format!(
            "{where_}: '{label}' must be a list of {count} numbers"
        )));
    }
    values
        .iter()
        .map(|value| match value {
            Value::Float(value) => Ok(*value),
            Value::Integer(value) => Ok(*value as f64),
            _ => Err(WayfinderConfigError::new(format!(
                "{where_}: '{label}' must be a list of {count} numbers"
            ))),
        })
        .collect()
}

fn apply_env_threshold(default: f64) -> Result<f64, WayfinderConfigError> {
    let Ok(raw) = env::var(THRESHOLD_ENV) else {
        return Ok(default);
    };
    if raw.is_empty() {
        return Ok(default);
    }
    let value = raw.parse::<f64>().map_err(|_| {
        WayfinderConfigError::new(format!("{THRESHOLD_ENV} must be a number, got {raw:?}"))
    })?;
    if !(0.0..=1.0).contains(&value) {
        return Err(WayfinderConfigError::new(format!(
            "{THRESHOLD_ENV} must be between 0.0 and 1.0, got {value}"
        )));
    }
    Ok(value)
}

fn non_negative_number(value: &Value) -> Option<f64> {
    match value {
        Value::Float(value) if *value >= 0.0 => Some(*value),
        Value::Integer(value) if *value >= 0 => Some(*value as f64),
        _ => None,
    }
}

fn dump_tier(tier: &Tier) -> String {
    let mut lines = vec![
        "[[routing.tiers]]".to_string(),
        format!("min_score = {}", fmt_num(tier.min_score)),
        format!("model = {}", quote(&tier.model)),
    ];
    if let Some(cost) = tier.cost {
        lines.push(format!("cost = {}", fmt_num(cost)));
    }
    lines.join("\n")
}

fn fmt_num(value: f64) -> String {
    let rounded = (value * 1_000_000.0).round() / 1_000_000.0;
    let mut text = format!("{rounded:?}");
    if !text.contains('.') && !text.contains('e') {
        text.push_str(".0");
    }
    text
}

fn quoted_terms(terms: &[String]) -> String {
    let mut sorted = terms.to_vec();
    sorted.sort();
    sorted
        .iter()
        .map(|term| quote(term))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote(value: &str) -> String {
    format!("{value:?}")
}
