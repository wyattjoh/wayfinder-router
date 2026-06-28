//! Append-only feedback label log (WF-ADR-0006).
//!
//! Mirrors `wayfinder_router/feedback.py`: every recorded label is one JSON
//! line shaped as `{"text": "...", "label": "..."}`. The log is pure file IO
//! so calibration can replay it deterministically.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const DEFAULT_LOG: &str = "wayfinder-router-feedback.jsonl";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelRow {
    pub text: String,
    pub label: String,
}

pub fn record_label(log_path: impl AsRef<Path>, text: &str, label: &str) -> io::Result<()> {
    if text.is_empty() {
        return Err(invalid_input("feedback needs a non-empty prompt text"));
    }
    if label.is_empty() {
        return Err(invalid_input("feedback needs a non-empty label"));
    }

    let line = encode_label_line(text, label)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn read_labels(log_path: impl AsRef<Path>) -> io::Result<Vec<LabelRow>> {
    let path = log_path.as_ref();
    if !path.is_file() {
        return Ok(Vec::new());
    }

    let text = fs::read_to_string(path)?;
    let mut rows = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        rows.push(serde_json::from_str(stripped).map_err(invalid_data)?);
    }
    Ok(rows)
}

fn encode_label_line(text: &str, label: &str) -> io::Result<String> {
    let text = serde_json::to_string(text).map_err(invalid_data)?;
    let label = serde_json::to_string(label).map_err(invalid_data)?;
    Ok(format!("{{\"text\": {text}, \"label\": {label}}}"))
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(err: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}
