use std::env;
use std::fmt;
use std::fs;
use std::path::Path;

use toml::Value;

use crate::complexity::{
    binary_tiers, ClassifierModel, ClassifierWeights, FeatureWeights, Lexicon, RoutingConfig, Tier,
    DEFAULT_THRESHOLD, DEFAULT_WEIGHTS, FEATURE_ORDER,
};

pub const CONFIG_FILE: &str = "wayfinder-router.toml";
/// An explicit path to the config file, overriding the working-directory walk-up.
///
/// Lets a service-managed gateway (whose working directory is unpredictable) and a desktop
/// client agree on one well-known file. `serve --config PATH` sets it for the process.
pub const CONFIG_PATH_ENV: &str = "WAYFINDER_CONFIG";
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
            return Ok(default_binary_config(apply_env_threshold(
                DEFAULT_THRESHOLD,
            )?));
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

pub fn load_routing_config(start_dir: &Path) -> Result<RoutingConfig, WayfinderConfigError> {
    let Some(path) = find_config_file(start_dir) else {
        return Ok(default_binary_config(apply_env_threshold(
            DEFAULT_THRESHOLD,
        )?));
    };
    let text = fs::read_to_string(&path).map_err(|err| {
        WayfinderConfigError::new(format!("cannot read {}: {err}", path.display()))
    })?;
    routing_config_from_toml(&text, &path.to_string_lossy())
}

fn default_binary_config(threshold: f64) -> RoutingConfig {
    RoutingConfig {
        weights: DEFAULT_WEIGHTS,
        tiers: binary_tiers(threshold),
        classifier: None,
        lexicon: Lexicon::default(),
    }
}

/// The config file to load: an explicit [`CONFIG_PATH_ENV`] override, else the nearest
/// [`CONFIG_FILE`] at or above `start_dir`, else `None`.
///
/// The override is absolute. When [`CONFIG_PATH_ENV`] is set but names a file that is not
/// there, the result is `None` — a clear "your configured file is missing" — never a silent
/// walk up to some other config that happens to sit above the working directory.
///
/// This exists because a service-managed gateway (launchd, systemd) has an unpredictable
/// working directory, so walking up from it finds the wrong file or no file at all.
pub fn find_config_file(start_dir: &Path) -> Option<std::path::PathBuf> {
    if let Some(override_path) = config_path_override() {
        return override_path.is_file().then_some(override_path);
    }
    let current = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    current.ancestors().find_map(|directory| {
        let candidate = directory.join(CONFIG_FILE);
        candidate.is_file().then_some(candidate)
    })
}

/// The [`CONFIG_PATH_ENV`] override as a path, with a leading `~` expanded.
fn config_path_override() -> Option<std::path::PathBuf> {
    let raw = std::env::var(CONFIG_PATH_ENV).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(expand_tilde(&raw))
}

fn expand_tilde(raw: &str) -> std::path::PathBuf {
    let Some(rest) = raw.strip_prefix('~') else {
        return std::path::PathBuf::from(raw);
    };
    let Some(home) = std::env::var_os("HOME") else {
        return std::path::PathBuf::from(raw);
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return std::path::PathBuf::from(home);
    }
    std::path::PathBuf::from(home).join(rest)
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

    // Tier order is the routing ladder order, so the declared order is authoritative: an
    // out-of-order ladder is rejected rather than silently reordered. Validate the declared
    // sequence as-is instead of sorting first.
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

// --- Line-preserving TOML editors (WF-ADR-0044 config seam) ---
//
// The CLI is the only author of `wayfinder-router.toml`, so these edits must never clobber a
// hand-edited file: every line except the one they touch survives byte-for-byte, comments and
// blank lines included. Each is a pure text transform; every caller re-parses the result
// through the real config parsers before writing anything to disk (belt and braces).

/// A TOML basic-string literal: backslash and double-quote escaped, wrapped in quotes.
///
/// Deliberately narrower than [`quote`] (which uses Rust's debug escaping): it matches the
/// upstream seam's own renderer, which only ever handles values without control characters
/// (URLs, model ids, env-var names), so it escapes exactly the two characters TOML requires.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn line_indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

fn section_header(stripped: &str) -> Option<&str> {
    if stripped.starts_with('[') && stripped.ends_with(']') {
        Some(stripped[1..stripped.len() - 1].trim())
    } else {
        None
    }
}

/// Set `key = true|false` in the top-level `[table]` section, preserving every other line.
///
/// Three cases: an existing uncommented `key =` inside `[table]` is replaced in place; a
/// `[table]` without the key gains it directly under the header; a missing section is appended
/// (TOML allows declaring a super-table after its sub-tables, so a trailing `[gateway]` after
/// `[gateway.models.*]` is valid).
#[must_use]
pub fn set_toml_bool(text: &str, table: &str, key: &str, value: bool) -> String {
    let rendered = if value { "true" } else { "false" };
    set_scalar_line(text, table, key, rendered)
}

/// Set `key = ["a", "b"]` in the top-level `[table]` section, preserving every other line.
///
/// [`set_toml_bool`]'s sibling for a list-of-strings field. An empty list clears the key to
/// `[]` rather than removing the line, so re-applying the same edit twice is a no-op either way.
#[must_use]
pub fn set_toml_string_list(text: &str, table: &str, key: &str, values: &[String]) -> String {
    let rendered = format!(
        "[{}]",
        values
            .iter()
            .map(|value| toml_string(value))
            .collect::<Vec<_>>()
            .join(", ")
    );
    set_scalar_line(text, table, key, &rendered)
}

/// The shared line-preserving setter behind [`set_toml_bool`] and [`set_toml_string_list`].
fn set_scalar_line(text: &str, table: &str, key: &str, rendered: &str) -> String {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let mut section: Option<String> = None;
    let mut header_idx: Option<usize> = None;
    for i in 0..lines.len() {
        let stripped = lines[i].trim().to_owned();
        if let Some(name) = section_header(&stripped) {
            section = Some(name.to_owned());
            if name == table {
                header_idx = Some(i);
            }
            continue;
        }
        if section.as_deref() == Some(table) && !stripped.starts_with('#') {
            if let Some((name, _)) = stripped.split_once('=') {
                if name.trim() == key {
                    let indent = line_indent(&lines[i]).to_owned();
                    lines[i] = format!("{indent}{key} = {rendered}\n");
                    return lines.concat();
                }
            }
        }
    }
    if let Some(idx) = header_idx {
        lines.insert(idx + 1, format!("{key} = {rendered}\n"));
        return lines.concat();
    }
    let tail = if text.ends_with('\n') || text.is_empty() {
        ""
    } else {
        "\n"
    };
    format!("{text}{tail}\n[{table}]\n{key} = {rendered}\n")
}

/// Whether a `[gateway.models.<name>]` table header already appears in `text`.
fn has_model_table(text: &str, name: &str) -> bool {
    let target = format!("gateway.models.{name}");
    text.lines()
        .filter_map(|line| section_header(line.trim()))
        .any(|header| header.trim() == target)
}

/// Insert a new `[gateway.models.<name>]` table, without touching any existing line.
///
/// Always appended at the end (TOML lets a table appear anywhere relative to unrelated tables),
/// so this never has to parse or rewrite what is already there. Errors if a table by this name
/// already exists: unlike [`set_toml_bool`]'s idempotent update, two additions are never "the
/// same edit twice", so a name collision is always a mistake worth stopping on.
pub fn add_model_table(
    text: &str,
    name: &str,
    base_url: &str,
    model: &str,
    api_key_env: Option<&str>,
    api_key_cmd: Option<&str>,
    cost_per_1k: Option<f64>,
) -> Result<String, WayfinderConfigError> {
    if has_model_table(text, name) {
        return Err(WayfinderConfigError::new(format!(
            "a model named '{name}' already exists in this config"
        )));
    }
    let mut lines = vec![
        format!("[gateway.models.{name}]"),
        format!("base_url = {}", toml_string(base_url)),
        format!("model = {}", toml_string(model)),
    ];
    if let Some(api_key_env) = api_key_env {
        lines.push(format!("api_key_env = {}", toml_string(api_key_env)));
    }
    if let Some(api_key_cmd) = api_key_cmd {
        lines.push(format!("api_key_cmd = {}", toml_string(api_key_cmd)));
    }
    if let Some(cost_per_1k) = cost_per_1k {
        lines.push(format!("cost_per_1k = {}", fmt_num(cost_per_1k)));
    }
    let tail = if text.ends_with('\n') || text.is_empty() {
        ""
    } else {
        "\n"
    };
    Ok(format!("{text}{tail}\n{}\n", lines.join("\n")))
}

/// Set `min_score` on the `[[routing.tiers]]` entry whose `model` matches, preserving every
/// other line (including every other tier).
///
/// The seam's own array-of-tables editor: [`set_toml_bool`]'s single-named-table logic can't
/// match a repeated `[[table]]` header by a field value rather than by name. Errors if no tier
/// names `model`. A resulting monotonicity violation is caught by the caller's reparse, not here.
pub fn set_tier_min_score(
    text: &str,
    model: &str,
    min_score: f64,
) -> Result<String, WayfinderConfigError> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let block_starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == "[[routing.tiers]]")
        .map(|(i, _)| i)
        .collect();
    if block_starts.is_empty() {
        return Err(WayfinderConfigError::new(
            "no '[[routing.tiers]]' entries found in this config",
        ));
    }
    for (idx, &start) in block_starts.iter().enumerate() {
        let scan_from = start + 1;
        let mut end = block_starts.get(idx + 1).copied().unwrap_or(lines.len());
        // A tier block ends at the next section header (a nested `[routing.tiers.x]` or any
        // other table), whichever comes before the next `[[routing.tiers]]`.
        if let Some(offset) = lines[scan_from..end].iter().position(|line| {
            let stripped = line.trim();
            stripped.starts_with('[') && stripped != "[[routing.tiers]]"
        }) {
            end = scan_from + offset;
        }
        let mut block_model: Option<String> = None;
        let mut min_score_line: Option<usize> = None;
        for (offset, line) in lines[scan_from..end].iter().enumerate() {
            let stripped = line.trim();
            if stripped.is_empty() || stripped.starts_with('#') || !stripped.contains('=') {
                continue;
            }
            let (key, raw_value) = stripped.split_once('=').expect("checked for '=' above");
            match key.trim() {
                "model" => {
                    block_model = Some(raw_value.trim().trim_matches(['"', '\'']).to_owned())
                }
                "min_score" => min_score_line = Some(scan_from + offset),
                _ => {}
            }
        }
        if block_model.as_deref() != Some(model) {
            continue;
        }
        let rendered = fmt_num(min_score);
        if let Some(line) = min_score_line {
            let indent = line_indent(&lines[line]).to_owned();
            lines[line] = format!("{indent}min_score = {rendered}\n");
        } else {
            let indent = if start + 1 < end {
                line_indent(&lines[start + 1]).to_owned()
            } else {
                String::new()
            };
            lines.insert(start + 1, format!("{indent}min_score = {rendered}\n"));
        }
        return Ok(lines.concat());
    }
    Err(WayfinderConfigError::new(format!(
        "no '[[routing.tiers]]' entry has model = '{model}'"
    )))
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_threshold_env<T>(value: Option<&str>, test: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .lock()
            .expect("threshold env lock should not poison");
        let saved = env::var_os(THRESHOLD_ENV);
        match value {
            Some(value) => env::set_var(THRESHOLD_ENV, value),
            None => env::remove_var(THRESHOLD_ENV),
        }
        let result = catch_unwind(AssertUnwindSafe(test));
        match saved {
            Some(value) => env::set_var(THRESHOLD_ENV, value),
            None => env::remove_var(THRESHOLD_ENV),
        }
        match result {
            Ok(value) => value,
            Err(payload) => resume_unwind(payload),
        }
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "wayfinder-core-config-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    #[test]
    fn load_routing_config_parses_present_file() {
        with_threshold_env(None, || {
            let dir = unique_temp_dir();
            fs::write(dir.join(CONFIG_FILE), "[routing]\nthreshold = 0.8\n")
                .expect("config file should be writable");

            let config = load_routing_config(&dir).expect("present config should parse");
            assert_eq!(config.tiers[1].min_score, 0.8);

            fs::remove_dir_all(&dir).expect("temp dir should be removable");
        });
    }

    #[test]
    fn load_routing_config_finds_config_in_parent_directory() {
        with_threshold_env(None, || {
            let parent = unique_temp_dir();
            fs::write(parent.join(CONFIG_FILE), "[routing]\nthreshold = 0.8\n")
                .expect("config file should be writable");
            let child = parent.join("nested/deeper");
            fs::create_dir_all(&child).expect("child dirs should be creatable");

            let config =
                load_routing_config(&child).expect("ancestor config should be found from a subdir");
            assert_eq!(config.tiers[1].min_score, 0.8);

            fs::remove_dir_all(&parent).expect("temp dir should be removable");
        });
    }

    #[test]
    fn load_routing_config_returns_default_when_absent() {
        with_threshold_env(None, || {
            let dir = unique_temp_dir();

            let config = load_routing_config(&dir).expect("missing config should fall back");
            assert_eq!(config, RoutingConfig::default());

            fs::remove_dir_all(&dir).expect("temp dir should be removable");
        });
    }

    #[test]
    fn missing_routing_table_applies_env_threshold_to_explicit_defaults() {
        with_threshold_env(Some("0.2"), || {
            let config =
                routing_config_from_toml("[gateway]\n", "inline").expect("config should parse");

            assert_eq!(config.weights, DEFAULT_WEIGHTS);
            assert_eq!(config.tiers, binary_tiers(0.2));
            assert_eq!(config.classifier, None);
            assert_eq!(config.lexicon, Lexicon::default());
        });
    }

    #[test]
    fn load_routing_config_surfaces_invalid_toml() {
        let dir = unique_temp_dir();
        fs::write(dir.join(CONFIG_FILE), "this is not = = toml\n")
            .expect("config file should be writable");

        let err = load_routing_config(&dir).expect_err("invalid TOML should error");
        assert!(err.to_string().contains("invalid TOML"));

        fs::remove_dir_all(&dir).expect("temp dir should be removable");
    }

    #[test]
    fn set_toml_bool_replaces_in_place_without_touching_comments() {
        let text = "# top comment\n[gateway]\noffline = false  # trailing note\nretries = 3\n";
        let out = set_toml_bool(text, "gateway", "offline", true);
        assert_eq!(
            out, "# top comment\n[gateway]\noffline = true\nretries = 3\n",
            "the key line is replaced; every other line survives"
        );
    }

    #[test]
    fn set_toml_bool_inserts_under_an_existing_header() {
        let text = "[gateway]\nretries = 3\n";
        let out = set_toml_bool(text, "gateway", "offline", true);
        assert_eq!(out, "[gateway]\noffline = true\nretries = 3\n");
    }

    #[test]
    fn set_toml_bool_appends_a_missing_section() {
        let text = "[routing]\nthreshold = 0.2\n";
        let out = set_toml_bool(text, "gateway", "offline", false);
        assert_eq!(
            out,
            "[routing]\nthreshold = 0.2\n\n[gateway]\noffline = false\n"
        );
    }

    #[test]
    fn set_toml_bool_ignores_a_commented_key_line() {
        // A `# offline = true` example must not be mistaken for the real key.
        let text = "[gateway]\n# offline = true\nretries = 3\n";
        let out = set_toml_bool(text, "gateway", "offline", true);
        assert_eq!(
            out,
            "[gateway]\noffline = true\n# offline = true\nretries = 3\n"
        );
    }

    #[test]
    fn set_toml_string_list_renders_and_clears() {
        let text = "[gateway.models.cloud]\nmodel = \"m\"\n";
        let set = set_toml_string_list(
            text,
            "gateway.models.cloud",
            "fallbacks",
            &["local".to_owned()],
        );
        assert_eq!(
            set,
            "[gateway.models.cloud]\nfallbacks = [\"local\"]\nmodel = \"m\"\n"
        );
        let cleared = set_toml_string_list(&set, "gateway.models.cloud", "fallbacks", &[]);
        assert_eq!(
            cleared,
            "[gateway.models.cloud]\nfallbacks = []\nmodel = \"m\"\n"
        );
    }

    #[test]
    fn add_model_table_appends_and_rejects_duplicates() {
        let text = "[routing]\nthreshold = 0.2\n";
        let out = add_model_table(
            text,
            "anthropic",
            "https://api.anthropic.com/v1",
            "claude-x",
            Some("ANTHROPIC_API_KEY"),
            None,
            Some(0.009),
        )
        .expect("first add succeeds");
        assert_eq!(
            out,
            concat!(
                "[routing]\nthreshold = 0.2\n\n",
                "[gateway.models.anthropic]\n",
                "base_url = \"https://api.anthropic.com/v1\"\n",
                "model = \"claude-x\"\n",
                "api_key_env = \"ANTHROPIC_API_KEY\"\n",
                "cost_per_1k = 0.009\n"
            )
        );
        let dup = add_model_table(
            &out,
            "anthropic",
            "https://api.anthropic.com/v1",
            "claude-y",
            None,
            None,
            None,
        );
        assert_eq!(
            dup.unwrap_err().to_string(),
            "a model named 'anthropic' already exists in this config"
        );
    }

    #[test]
    fn tiers_must_be_declared_in_ascending_order() {
        // Declared order is the routing ladder order, so it is authoritative: an out-of-order
        // ladder is rejected, never silently sorted into shape.
        let text = concat!(
            "[[routing.tiers]]\nmin_score = 0.0\nmodel = \"small\"\n\n",
            "[[routing.tiers]]\nmin_score = 0.5\nmodel = \"large\"\n\n",
            "[[routing.tiers]]\nmin_score = 0.2\nmodel = \"medium\"\n"
        );
        let err = routing_config_from_toml(text, "fixture").expect_err("out-of-order tiers");
        assert!(err.to_string().contains("strictly ascending"), "got: {err}");

        // The same tiers in ascending order parse, and keep their declared order.
        let ordered = concat!(
            "[[routing.tiers]]\nmin_score = 0.0\nmodel = \"small\"\n\n",
            "[[routing.tiers]]\nmin_score = 0.2\nmodel = \"medium\"\n\n",
            "[[routing.tiers]]\nmin_score = 0.5\nmodel = \"large\"\n"
        );
        let config = routing_config_from_toml(ordered, "fixture").expect("ascending tiers parse");
        let models: Vec<&str> = config
            .tiers
            .iter()
            .map(|tier| tier.model.as_str())
            .collect();
        assert_eq!(models, ["small", "medium", "large"]);
    }

    #[test]
    fn set_tier_min_score_edits_the_matching_tier_only() {
        let text = concat!(
            "[[routing.tiers]]\nmin_score = 0.0\nmodel = \"small\"\n\n",
            "[[routing.tiers]]\nmin_score = 0.08\nmodel = \"large\"\n"
        );
        let out = set_tier_min_score(text, "large", 0.2).expect("large tier exists");
        assert_eq!(
            out,
            concat!(
                "[[routing.tiers]]\nmin_score = 0.0\nmodel = \"small\"\n\n",
                "[[routing.tiers]]\nmin_score = 0.2\nmodel = \"large\"\n"
            )
        );
    }

    #[test]
    fn set_tier_min_score_errors_on_an_unknown_model() {
        let text = "[[routing.tiers]]\nmin_score = 0.0\nmodel = \"small\"\n";
        let err = set_tier_min_score(text, "missing", 0.5).expect_err("no such tier");
        assert_eq!(
            err.to_string(),
            "no '[[routing.tiers]]' entry has model = 'missing'"
        );
    }
}
