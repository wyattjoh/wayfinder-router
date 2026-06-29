use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value as JsonValue;
use wayfinder_internal_core::calibrate::{
    calibrate, load_dataset, CalibrationError, CalibrationOptions,
};
use wayfinder_internal_core::feedback::read_labels;

use crate::{gateway_config_from_toml, GatewayError};

pub const DEFAULT_MIN_LABELS: usize = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct RecalibrationResult {
    pub written: bool,
    pub label_count: usize,
    pub summary: Option<JsonValue>,
    pub toml: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug)]
pub enum RecalibrationError {
    Io(io::Error),
    Calibration(CalibrationError),
    Config(GatewayError),
}

impl fmt::Display for RecalibrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => err.fmt(f),
            Self::Calibration(err) => err.fmt(f),
            Self::Config(err) => err.fmt(f),
        }
    }
}

impl Error for RecalibrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Calibration(err) => Some(err),
            Self::Config(err) => Some(err),
        }
    }
}

impl From<io::Error> for RecalibrationError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<CalibrationError> for RecalibrationError {
    fn from(err: CalibrationError) -> Self {
        Self::Calibration(err)
    }
}

impl From<GatewayError> for RecalibrationError {
    fn from(err: GatewayError) -> Self {
        Self::Config(err)
    }
}

pub fn recalibrate(
    log_path: impl AsRef<Path>,
    config_path: impl AsRef<Path>,
    mode: &str,
    min_labels: usize,
) -> Result<RecalibrationResult, RecalibrationError> {
    let log_path = log_path.as_ref();
    let rows = read_labels(log_path)?;
    if rows.len() < min_labels {
        return Ok(RecalibrationResult {
            written: false,
            label_count: rows.len(),
            summary: None,
            toml: None,
            reason: Some(format!("need >= {min_labels} labels, have {}", rows.len())),
        });
    }

    let samples = load_dataset(log_path)?;
    let result = calibrate(&samples, mode, CalibrationOptions::default())?;
    let gateway = gateway_block(config_path.as_ref())?;
    let summary = result.summary;
    let summary_bits = summary_bits(&summary);

    let mut parts = vec![
        format!("# recalibrated from feedback: {summary_bits}"),
        result.toml.trim_end_matches('\n').to_owned(),
    ];
    if let Some(gateway) = gateway {
        parts.push(gateway);
    }
    let text = format!("{}\n", parts.join("\n\n"));
    fs::write(config_path, &text)?;

    Ok(RecalibrationResult {
        written: true,
        label_count: rows.len(),
        summary: Some(summary),
        toml: Some(text),
        reason: None,
    })
}

fn gateway_block(config_path: &Path) -> Result<Option<String>, RecalibrationError> {
    if !config_path.is_file() {
        return Ok(None);
    }

    let text = fs::read_to_string(config_path)?;
    let where_ = config_path.to_string_lossy();
    gateway_config_from_toml(&text, &where_)?;
    Ok(extract_gateway_block(&text))
}

fn extract_gateway_block(text: &str) -> Option<String> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut in_gateway = false;

    for line in text.lines() {
        if let Some(path) = table_path(line) {
            if is_gateway_path(path) {
                in_gateway = true;
            } else if in_gateway {
                push_trimmed_block(&mut blocks, &current);
                current.clear();
                in_gateway = false;
            }
        }

        if in_gateway {
            current.push(line);
        }
    }

    if in_gateway {
        push_trimmed_block(&mut blocks, &current);
    }

    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

fn push_trimmed_block(blocks: &mut Vec<String>, lines: &[&str]) {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if end > 0 {
        blocks.push(lines[..end].join("\n"));
    }
}

fn table_path(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("[[") {
        let close = trimmed.find("]]")?;
        return Some(trimmed[2..close].trim());
    }
    if trimmed.starts_with('[') {
        let close = trimmed.find(']')?;
        return Some(trimmed[1..close].trim());
    }
    None
}

fn is_gateway_path(path: &str) -> bool {
    path == "gateway" || path.starts_with("gateway.")
}

fn summary_bits(summary: &JsonValue) -> String {
    let JsonValue::Object(values) = summary else {
        return py_value(summary);
    };

    let mut keys = match (
        values.get("mode").and_then(JsonValue::as_str),
        values.get("objective").and_then(JsonValue::as_str),
    ) {
        (Some("threshold"), Some("knee")) => vec![
            "mode",
            "objective",
            "threshold",
            "models",
            "accuracy",
            "quality_recovered",
            "cost_savings",
            "samples",
        ],
        (Some("threshold"), Some("cost-quality")) => vec![
            "mode",
            "objective",
            "threshold",
            "models",
            "accuracy",
            "cost_savings",
            "target_savings",
            "samples",
        ],
        (Some("threshold"), _) => {
            vec!["mode", "threshold", "models", "accuracy", "samples"]
        }
        (Some("tiers"), _) => {
            vec!["mode", "models", "breakpoints", "accuracy", "samples"]
        }
        (Some("classifier"), _) => {
            vec!["mode", "models", "iterations", "accuracy", "samples"]
        }
        _ => Vec::new(),
    };
    for key in values.keys() {
        if !keys.contains(&key.as_str()) {
            keys.push(key);
        }
    }

    keys.into_iter()
        .filter_map(|key| {
            values
                .get(key)
                .map(|value| format!("{key}={}", py_value(value)))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn py_value(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "None".to_owned(),
        JsonValue::Bool(value) => {
            if *value {
                "True".to_owned()
            } else {
                "False".to_owned()
            }
        }
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => value.clone(),
        JsonValue::Array(values) => {
            let inner = values.iter().map(py_repr).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        JsonValue::Object(_) => py_repr(value),
    }
}

fn py_repr(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => format!("'{}'", value.replace('\'', "\\'")),
        JsonValue::Array(values) => {
            let inner = values.iter().map(py_repr).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        JsonValue::Object(values) => {
            let inner = values
                .iter()
                .map(|(key, value)| format!("'{key}': {}", py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        _ => py_value(value),
    }
}
