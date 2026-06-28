use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub messages: Vec<Value>,
}

pub fn title_from(messages: &[Value], limit: usize) -> String {
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        let truncated = text.chars().take(limit).collect::<String>();
        if text.chars().count() > limit {
            return format!("{truncated}\u{2026}");
        }
        return truncated;
    }
    "(empty)".to_string()
}
