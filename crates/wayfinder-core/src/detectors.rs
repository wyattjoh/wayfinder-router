use std::collections::BTreeMap;

use regex::Regex;
use serde_json::{json, Value as JsonValue};

#[derive(Clone, Debug)]
pub struct Detector {
    pub name: String,
    pattern: Regex,
    validator: Option<fn(&str) -> bool>,
}

impl Detector {
    pub fn new(name: &str, pattern: &str, validator: Option<fn(&str) -> bool>) -> Self {
        Self {
            name: name.to_owned(),
            pattern: Regex::new(pattern).expect("detector pattern should compile"),
            validator,
        }
    }

    pub fn detects(&self, text: &str) -> bool {
        self.pattern.find_iter(text).any(|matched| {
            self.validator
                .is_none_or(|validator| validator(matched.as_str()))
        })
    }

    pub fn pattern(&self) -> &str {
        self.pattern.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorpusItem {
    pub text: String,
    pub labels: Vec<String>,
}

impl CorpusItem {
    pub fn new(
        text: impl Into<String>,
        labels: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let mut labels = labels.into_iter().map(Into::into).collect::<Vec<_>>();
        labels.sort();
        labels.dedup();
        Self {
            text: text.into(),
            labels,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub tp: usize,
    pub fp: usize,
    pub fn_: usize,
    pub tn: usize,
}

impl Stats {
    pub fn precision(&self) -> f64 {
        let denom = self.tp + self.fp;
        if denom == 0 {
            0.0
        } else {
            self.tp as f64 / denom as f64
        }
    }

    pub fn recall(&self) -> f64 {
        let denom = self.tp + self.fn_;
        if denom == 0 {
            0.0
        } else {
            self.tp as f64 / denom as f64
        }
    }

    pub fn f1(&self) -> f64 {
        let precision = self.precision();
        let recall = self.recall();
        if precision + recall == 0.0 {
            0.0
        } else {
            2.0 * precision * recall / (precision + recall)
        }
    }
}

pub fn reference_detectors() -> Vec<Detector> {
    vec![
        Detector::new(
            "email",
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
            None,
        ),
        Detector::new("us_ssn", r"\b\d{3}-\d{2}-\d{4}\b", None),
        Detector::new(
            "aws_access_key",
            r"\b(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16}\b",
            None,
        ),
        Detector::new("github_pat", r"\bghp_[A-Za-z0-9]{36}\b", None),
        Detector::new(
            "slack_token",
            r"\bxox[baprs]-\d{10,}-[A-Za-z0-9-]{10,}\b",
            None,
        ),
        Detector::new(
            "private_key",
            r"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----",
            None,
        ),
        Detector::new("credit_card", r"\b(?:\d[ -]?){13,19}\b", Some(is_card)),
        Detector::new("high_entropy_hex", r"\b[0-9a-f]{32,}\b", None),
    ]
}

pub fn reference_detectors_by_name() -> BTreeMap<String, Detector> {
    reference_detectors()
        .into_iter()
        .map(|detector| (detector.name.clone(), detector))
        .collect()
}

pub fn token_items() -> Vec<CorpusItem> {
    let aws = ["AK", "IAIOSFODNN7EXAMPLE"].concat();
    let gh = ["gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789"].concat();
    let slack = ["xox", "b-", "240120480129-abcDEF123ghiJKL"].concat();
    vec![
        CorpusItem::new(format!("aws_access_key_id = {aws}"), ["aws_access_key"]),
        CorpusItem::new(
            format!("Rotate the key {aws} before Friday's audit."),
            ["aws_access_key"],
        ),
        CorpusItem::new(format!("export GH_TOKEN={gh}"), ["github_pat"]),
        CorpusItem::new(format!("CI uses {gh} for release tags."), ["github_pat"]),
        CorpusItem::new(format!("SLACK_BOT_TOKEN={slack}"), ["slack_token"]),
        CorpusItem::new(
            format!("The bot authenticates with {slack} nightly."),
            ["slack_token"],
        ),
        CorpusItem::new(
            format!("Provision a bot: slack {slack}, email ops@example.com."),
            ["slack_token", "email"],
        ),
    ]
}

pub fn evaluate(items: &[CorpusItem], detectors: &[Detector]) -> BTreeMap<String, Stats> {
    let mut stats = detectors
        .iter()
        .map(|detector| (detector.name.clone(), Stats::default()))
        .collect::<BTreeMap<_, _>>();
    for item in items {
        for detector in detectors {
            let fired = detector.detects(&item.text);
            let expected = item.labels.iter().any(|label| label == &detector.name);
            let s = stats
                .get_mut(&detector.name)
                .expect("stats should include every detector");
            match (fired, expected) {
                (true, true) => s.tp += 1,
                (true, false) => s.fp += 1,
                (false, true) => s.fn_ += 1,
                (false, false) => s.tn += 1,
            }
        }
    }
    stats
}

pub fn micro_macro(stats: &BTreeMap<String, Stats>) -> BTreeMap<String, f64> {
    let pooled = Stats {
        tp: stats.values().map(|stats| stats.tp).sum(),
        fp: stats.values().map(|stats| stats.fp).sum(),
        fn_: stats.values().map(|stats| stats.fn_).sum(),
        tn: 0,
    };
    let n = stats.len().max(1) as f64;
    BTreeMap::from([
        ("micro_precision".to_owned(), pooled.precision()),
        ("micro_recall".to_owned(), pooled.recall()),
        ("micro_f1".to_owned(), pooled.f1()),
        (
            "macro_precision".to_owned(),
            stats.values().map(Stats::precision).sum::<f64>() / n,
        ),
        (
            "macro_recall".to_owned(),
            stats.values().map(Stats::recall).sum::<f64>() / n,
        ),
        (
            "macro_f1".to_owned(),
            stats.values().map(Stats::f1).sum::<f64>() / n,
        ),
    ])
}

pub fn render_markdown(
    stats: &BTreeMap<String, Stats>,
    corpus_size: usize,
    source: &str,
) -> String {
    let rollup = micro_macro(stats);
    let mut lines = vec![
        "## Detector validation results".to_owned(),
        String::new(),
        format!("Reference detectors over `{source}` ({corpus_size} labeled items)."),
        String::new(),
        format!(
            "**Micro** (pooled): precision {:.3}, recall {:.3}, F1 {:.3}. **Macro** (mean of detectors): precision {:.3}, recall {:.3}, F1 {:.3}.",
            rollup["micro_precision"],
            rollup["micro_recall"],
            rollup["micro_f1"],
            rollup["macro_precision"],
            rollup["macro_recall"],
            rollup["macro_f1"],
        ),
        String::new(),
        "| detector | tp | fp | fn | tn | precision | recall | F1 |".to_owned(),
        "| --- | --- | --- | --- | --- | --- | --- | --- |".to_owned(),
    ];
    for (name, stats) in stats {
        lines.push(format!(
            "| {name} | {} | {} | {} | {} | {:.3} | {:.3} | {:.3} |",
            stats.tp,
            stats.fp,
            stats.fn_,
            stats.tn,
            stats.precision(),
            stats.recall(),
            stats.f1()
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn report_json(stats: &BTreeMap<String, Stats>) -> JsonValue {
    let detectors = stats
        .iter()
        .map(|(name, stats)| {
            (
                name.clone(),
                json!({
                    "tp": stats.tp,
                    "fp": stats.fp,
                    "fn": stats.fn_,
                    "tn": stats.tn,
                    "precision": stats.precision(),
                    "recall": stats.recall(),
                    "f1": stats.f1(),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "detectors": detectors,
        "rollup": micro_macro(stats),
    })
}

pub fn ai4privacy_items_from_records(records: &[JsonValue]) -> Vec<CorpusItem> {
    records
        .iter()
        .filter_map(|row| {
            let text = row.get("source_text")?.as_str()?.to_owned();
            let labels = row
                .get("privacy_mask")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
                .filter_map(|span| span.get("label").and_then(JsonValue::as_str))
                .filter_map(ai4privacy_label)
                .collect::<Vec<_>>();
            Some(CorpusItem::new(text, labels))
        })
        .collect()
}

pub fn ai4privacy_detectors() -> Vec<Detector> {
    let by_name = reference_detectors_by_name();
    ["credit_card", "email", "us_ssn"]
        .into_iter()
        .filter_map(|name| by_name.get(name).cloned())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitleaksComparison {
    pub detector: String,
    pub rule_id: String,
    pub mine: String,
    pub theirs: String,
    pub agree: usize,
    pub total: usize,
    pub mine_fires: usize,
    pub theirs_fires: usize,
}

pub fn gitleaks_compare(rules: &BTreeMap<String, String>) -> Vec<GitleaksComparison> {
    let detectors = reference_detectors_by_name();
    let probes = gitleaks_probes();
    gitleaks_rule_map()
        .into_iter()
        .map(|(name, rule_id)| {
            let detector = detectors
                .get(name)
                .expect("mapped detector should exist in reference set");
            let their_pattern = rules.get(rule_id).cloned().unwrap_or_default();
            let their = if their_pattern.is_empty() {
                None
            } else {
                Regex::new(&their_pattern).ok()
            };
            let samples = probes.get(name).cloned().unwrap_or_default();
            let mut agree = 0;
            let mut mine_fires = 0;
            let mut theirs_fires = 0;
            for sample in &samples {
                let mine = detector.detects(sample);
                let theirs = their
                    .as_ref()
                    .map(|regex| regex.is_match(sample))
                    .unwrap_or(false);
                mine_fires += usize::from(mine);
                theirs_fires += usize::from(theirs);
                agree += usize::from(mine == theirs);
            }
            GitleaksComparison {
                detector: name.to_owned(),
                rule_id: rule_id.to_owned(),
                mine: detector.pattern().to_owned(),
                theirs: their_pattern,
                agree,
                total: samples.len(),
                mine_fires,
                theirs_fires,
            }
        })
        .collect()
}

fn ai4privacy_label(label: &str) -> Option<&'static str> {
    match label {
        "EMAIL" => Some("email"),
        "SSN" => Some("us_ssn"),
        "CREDITCARDNUMBER" => Some("credit_card"),
        _ => None,
    }
}

fn gitleaks_rule_map() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("aws_access_key", "aws-access-token"),
        ("github_pat", "github-pat"),
        ("private_key", "private-key"),
        ("slack_token", "slack-bot-token"),
    ])
}

fn gitleaks_probes() -> BTreeMap<&'static str, Vec<String>> {
    let akia = ["AK", "IAIOSFODNN7EXAMPLE"].concat();
    let asia = ["AS", "IAROSFODNN7EXAMPL2"].concat();
    let gh = ["gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789"].concat();
    let slack = ["xox", "b-", "240120480129-abcDEF123ghiJKL"].concat();
    let pem_header = ["-----BEGIN RSA PRIVATE ", "KEY-----"].concat();
    BTreeMap::from([
        (
            "aws_access_key",
            vec![akia, asia, "the AKIA prefix".to_owned()],
        ),
        ("github_pat", vec![gh, "ghp_short".to_owned()]),
        ("slack_token", vec![slack, "xoxb-short".to_owned()]),
        (
            "private_key",
            vec![pem_header, "a private discussion".to_owned()],
        ),
    ])
}

fn luhn_ok(candidate: &str) -> bool {
    let digits = candidate
        .chars()
        .filter(|char| char.is_ascii_digit())
        .filter_map(|char| char.to_digit(10))
        .collect::<Vec<_>>();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let total = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            if index % 2 == 0 {
                *digit
            } else {
                let doubled = digit * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            }
        })
        .sum::<u32>();
    total % 10 == 0
}

fn is_card(candidate: &str) -> bool {
    if !luhn_ok(candidate) {
        return false;
    }
    let digits = candidate
        .chars()
        .filter(|char| char.is_ascii_digit())
        .collect::<String>();
    let len = digits.len();
    if digits.starts_with('4') && matches!(len, 13 | 16 | 19) {
        return true;
    }
    if matches!(&digits[..2], "34" | "37") && len == 15 {
        return true;
    }
    let first_two = digits[..2].parse::<usize>().unwrap_or_default();
    let first_three = digits[..3].parse::<usize>().unwrap_or_default();
    let first_four = digits[..4].parse::<usize>().unwrap_or_default();
    if ((51..=55).contains(&first_two) || (2221..=2720).contains(&first_four)) && len == 16 {
        return true;
    }
    if (digits.starts_with("6011")
        || digits.starts_with("65")
        || (644..=649).contains(&first_three))
        && len == 16
    {
        return true;
    }
    digits.starts_with("62") && (16..=19).contains(&len)
}
