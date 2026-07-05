use std::collections::BTreeMap;

use serde_json::{json, Value as JsonValue};

use crate::judge::{HeuristicJudge, Judge, Verdict};
use crate::sufficiency::{cohens_kappa, confusion_matrix, ConfusionMatrix, DEFAULT_KAPPA_FLOOR};

pub const SUFFICIENT: &str = "sufficient";
pub const INSUFFICIENT: &str = "insufficient";
pub const GOLD_DEFINITIONS: [&str; 2] = ["absolute", "relative"];

#[derive(Clone, Debug, PartialEq)]
pub struct JudgeRow {
    pub prompt: String,
    pub cheap_text: String,
    pub expensive_text: String,
    pub local_score: f64,
    pub cloud_score: f64,
    pub bucket: String,
}

impl JudgeRow {
    pub fn new(
        prompt: impl Into<String>,
        cheap_text: impl Into<String>,
        expensive_text: impl Into<String>,
        local_score: f64,
        cloud_score: f64,
        bucket: impl Into<String>,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            cheap_text: cheap_text.into(),
            expensive_text: expensive_text.into(),
            local_score,
            cloud_score,
            bucket: bucket.into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GoldStats {
    pub pairs: Vec<(String, String)>,
}

impl GoldStats {
    pub fn add(&mut self, judge_label: &str, gold_label: &str) {
        self.pairs
            .push((judge_label.to_owned(), gold_label.to_owned()));
    }

    pub fn n(&self) -> usize {
        self.pairs.len()
    }

    pub fn accuracy(&self) -> f64 {
        if self.pairs.is_empty() {
            return 0.0;
        }
        self.pairs
            .iter()
            .filter(|(judge, gold)| judge == gold)
            .count() as f64
            / self.pairs.len() as f64
    }

    pub fn kappa(&self) -> f64 {
        cohens_kappa(&self.pairs)
    }

    pub fn confusion(&self) -> ConfusionMatrix {
        confusion_matrix(&self.pairs)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BucketReport {
    pub n: usize,
    pub abstained: usize,
    pub by_comparator: BTreeMap<String, usize>,
    pub gold: BTreeMap<String, GoldStats>,
    pub comparator_hits: BTreeMap<String, [usize; 2]>,
}

impl Default for BucketReport {
    fn default() -> Self {
        Self {
            n: 0,
            abstained: 0,
            by_comparator: BTreeMap::new(),
            gold: GOLD_DEFINITIONS
                .into_iter()
                .map(|name| (name.to_owned(), GoldStats::default()))
                .collect(),
            comparator_hits: BTreeMap::new(),
        }
    }
}

impl BucketReport {
    pub fn decided(&self) -> usize {
        self.n.saturating_sub(self.abstained)
    }

    pub fn abstention_rate(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.abstained as f64 / self.n as f64
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlwaysSufficientJudge {
    pub version: String,
}

impl Default for AlwaysSufficientJudge {
    fn default() -> Self {
        Self {
            version: "always-sufficient".to_owned(),
        }
    }
}

impl Judge for AlwaysSufficientJudge {
    fn version(&self) -> &str {
        &self.version
    }

    fn judge(&self, _prompt: &str, _cheap: &str, _expensive: &str) -> Verdict {
        Verdict::new(Some(true), "baseline: always sufficient", "baseline")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactMatchJudge {
    pub version: String,
}

impl Default for ExactMatchJudge {
    fn default() -> Self {
        Self {
            version: "exact-match".to_owned(),
        }
    }
}

impl Judge for ExactMatchJudge {
    fn version(&self) -> &str {
        &self.version
    }

    fn judge(&self, _prompt: &str, cheap: &str, expensive: &str) -> Verdict {
        if normalize(cheap) == normalize(expensive) {
            return Verdict::new(
                Some(true),
                "answers identical after normalization",
                "agreement",
            );
        }
        Verdict::new(None, "no exact match; abstain", "divergence")
    }
}

pub fn validate(
    rows: &[JudgeRow],
    judge: Option<&dyn Judge>,
    gold_threshold: f64,
) -> BTreeMap<String, BucketReport> {
    let default_judge = HeuristicJudge::default();
    let judge = judge.unwrap_or(&default_judge);
    let mut reports = BTreeMap::from([("overall".to_owned(), BucketReport::default())]);
    for row in rows {
        let verdict = judge.judge(&row.prompt, &row.cheap_text, &row.expensive_text);
        let gold = gold_labels(row, gold_threshold);
        for bucket in ["overall".to_owned(), row.bucket.clone()] {
            let report = reports.entry(bucket).or_default();
            report.n += 1;
            *report
                .by_comparator
                .entry(verdict.comparator.clone())
                .or_default() += 1;
            if verdict.sufficient.is_none() {
                report.abstained += 1;
                continue;
            }
            let judge_label = if verdict.sufficient == Some(true) {
                SUFFICIENT
            } else {
                INSUFFICIENT
            };
            for name in GOLD_DEFINITIONS {
                let gold_label = gold.get(name).expect("gold label should exist");
                report
                    .gold
                    .entry(name.to_owned())
                    .or_default()
                    .add(judge_label, gold_label);
            }
            let hits = report
                .comparator_hits
                .entry(verdict.comparator.clone())
                .or_default();
            hits[1] += 1;
            if judge_label == gold["absolute"] {
                hits[0] += 1;
            }
        }
    }
    reports
}

pub fn render_markdown(
    reports: &BTreeMap<String, BucketReport>,
    judge_version: &str,
    gold_threshold: f64,
    source: &str,
) -> String {
    let overall = reports
        .get("overall")
        .expect("reports should include overall bucket");
    let mut lines = vec![
        "## Judge validation results".to_owned(),
        String::new(),
        format!(
            "Judge `{judge_version}` replayed over `{source}` (absolute gold threshold {gold_threshold}; kappa floor {DEFAULT_KAPPA_FLOOR} for reference)."
        ),
        String::new(),
        format!(
            "**Overall:** n={}, decided={}, abstained={} ({:.1}%).",
            overall.n,
            overall.decided(),
            overall.abstained,
            overall.abstention_rate() * 100.0
        ),
        String::new(),
        "| bucket | n | abstain % | kappa (absolute) | acc (absolute) | kappa (relative) | acc (relative) |".to_owned(),
        "| --- | --- | --- | --- | --- | --- | --- |".to_owned(),
    ];
    for (name, report) in reports {
        let absolute = &report.gold["absolute"];
        let relative = &report.gold["relative"];
        lines.push(format!(
            "| {name} | {} | {:.1}% | {:.3} | {:.3} | {:.3} | {:.3} |",
            report.n,
            report.abstention_rate() * 100.0,
            absolute.kappa(),
            absolute.accuracy(),
            relative.kappa(),
            relative.accuracy()
        ));
    }
    lines.extend([
        String::new(),
        "### Overall confusion (absolute gold, decided rows only)".to_owned(),
        String::new(),
    ]);
    lines.extend(confusion_lines(&overall.gold["absolute"]));
    lines.extend([
        String::new(),
        "### By comparator (decided rows, accuracy vs absolute gold)".to_owned(),
        String::new(),
        "| comparator | fired | decided | accuracy |".to_owned(),
        "| --- | --- | --- | --- |".to_owned(),
    ]);
    for (comparator, fired) in &overall.by_comparator {
        let [correct, decided] = overall
            .comparator_hits
            .get(comparator)
            .copied()
            .unwrap_or([0, 0]);
        let accuracy = if decided == 0 {
            "-".to_owned()
        } else {
            format!("{:.3}", correct as f64 / decided as f64)
        };
        lines.push(format!(
            "| {comparator} | {fired} | {decided} | {accuracy} |"
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn report_json(reports: &BTreeMap<String, BucketReport>) -> JsonValue {
    let reports = reports
        .iter()
        .map(|(name, report)| {
            (
                name.clone(),
                json!({
                    "n": report.n,
                    "decided": report.decided(),
                    "abstained": report.abstained,
                    "abstention_rate": report.abstention_rate(),
                    "by_comparator": report.by_comparator,
                    "gold": {
                        "absolute": gold_json(&report.gold["absolute"]),
                        "relative": gold_json(&report.gold["relative"]),
                    },
                    "comparator_hits": report.comparator_hits,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    JsonValue::Object(reports)
}

fn gold_json(stats: &GoldStats) -> JsonValue {
    json!({
        "n": stats.n(),
        "accuracy": stats.accuracy(),
        "kappa": stats.kappa(),
        "confusion": stats.confusion(),
    })
}

fn gold_labels(row: &JudgeRow, threshold: f64) -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        (
            "absolute",
            if row.local_score >= threshold {
                SUFFICIENT
            } else {
                INSUFFICIENT
            },
        ),
        (
            "relative",
            if row.local_score >= row.cloud_score {
                SUFFICIENT
            } else {
                INSUFFICIENT
            },
        ),
    ])
}

fn confusion_lines(stats: &GoldStats) -> Vec<String> {
    let matrix = stats.confusion();
    let labels = matrix.keys().cloned().collect::<Vec<_>>();
    let mut lines = vec![
        format!("| judge \\ gold | {} |", labels.join(" | ")),
        format!("|{}|", " --- |".repeat(labels.len() + 1)),
    ];
    for row_label in &labels {
        let cells = labels
            .iter()
            .map(|col| {
                matrix
                    .get(row_label)
                    .and_then(|row| row.get(col))
                    .copied()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!("| {row_label} | {cells} |"));
    }
    lines
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
