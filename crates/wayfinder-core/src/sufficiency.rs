use std::collections::{BTreeMap, BTreeSet};

use crate::calibrate::{calibrate_threshold, CalibrationOptions, Sample};
use crate::complexity::{recommend_tier, Tier};

pub const DEFAULT_KAPPA_FLOOR: f64 = 0.6;
pub const DEFAULT_CV_FOLDS: usize = 5;
pub const DEFAULT_MIN_LIFT: f64 = 0.0;
pub const DEFAULT_DEGENERATE_FRACTION: f64 = 0.95;

pub type ConfusionMatrix = BTreeMap<String, BTreeMap<String, usize>>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvaluateOptions {
    pub kappa_floor: f64,
    pub min_lift: f64,
    pub k: usize,
    pub gold_abstained: usize,
    pub degenerate_fraction: f64,
}

impl Default for EvaluateOptions {
    fn default() -> Self {
        Self {
            kappa_floor: DEFAULT_KAPPA_FLOOR,
            min_lift: DEFAULT_MIN_LIFT,
            k: DEFAULT_CV_FOLDS,
            gold_abstained: 0,
            degenerate_fraction: DEFAULT_DEGENERATE_FRACTION,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateReport {
    pub kappa: f64,
    pub kappa_floor: f64,
    pub n_gold: usize,
    pub gold_abstained: usize,
    pub confusion: ConfusionMatrix,
    pub cv_accuracy: f64,
    pub majority_baseline: f64,
    pub lift: f64,
    pub label_counts: Vec<(String, usize)>,
    pub degenerate: bool,
    pub passed: bool,
    pub failures: Vec<String>,
}

impl GateReport {
    pub fn render(&self) -> String {
        let mut lines = vec![
            format!(
                "judge-vs-gold kappa: {:.2} (floor {:.2}, n={}, abstained={})",
                self.kappa, self.kappa_floor, self.n_gold, self.gold_abstained
            ),
            format!(
                "out-of-fold accuracy: {:.2} vs majority baseline {:.2} (lift {:+.2})",
                self.cv_accuracy, self.majority_baseline, self.lift
            ),
            format!(
                "label distribution: {}",
                render_label_counts(&self.label_counts)
            ),
        ];

        if !self.confusion.is_empty() {
            lines.push("confusion (rows=judge, cols=gold):".to_string());
            let cols = self
                .confusion
                .values()
                .flat_map(|row| row.keys().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            lines.push(format!(
                "            {}",
                cols.iter()
                    .map(|col| format!("{col:>10}"))
                    .collect::<Vec<_>>()
                    .join("  ")
            ));
            for (row_label, row) in &self.confusion {
                let cells = cols
                    .iter()
                    .map(|col| format!("{:>10}", row.get(col).copied().unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join("  ");
                lines.push(format!("{row_label:>10}  {cells}"));
            }
        }

        let verdict = if self.passed { "PASS" } else { "REFUSED" };
        lines.push(format!("trust gates: {verdict}"));
        lines.extend(self.failures.iter().map(|failure| format!("  - {failure}")));
        lines.join("\n")
    }
}

pub fn cohens_kappa<J, G>(pairs: &[(J, G)]) -> f64
where
    J: AsRef<str>,
    G: AsRef<str>,
{
    let n = pairs.len();
    if n == 0 {
        return 0.0;
    }

    let labels = pairs
        .iter()
        .flat_map(|(judge, gold)| [judge.as_ref().to_string(), gold.as_ref().to_string()])
        .collect::<BTreeSet<_>>();
    let observed = pairs
        .iter()
        .filter(|(judge, gold)| judge.as_ref() == gold.as_ref())
        .count() as f64
        / n as f64;
    let expected = labels
        .iter()
        .map(|label| {
            let p_judge = pairs
                .iter()
                .filter(|(judge, _)| judge.as_ref() == label)
                .count() as f64
                / n as f64;
            let p_gold = pairs
                .iter()
                .filter(|(_, gold)| gold.as_ref() == label)
                .count() as f64
                / n as f64;
            p_judge * p_gold
        })
        .sum::<f64>();

    if expected >= 1.0 {
        return if observed >= 1.0 { 1.0 } else { 0.0 };
    }
    (observed - expected) / (1.0 - expected)
}

pub fn confusion_matrix<J, G>(pairs: &[(J, G)]) -> ConfusionMatrix
where
    J: AsRef<str>,
    G: AsRef<str>,
{
    let labels = pairs
        .iter()
        .flat_map(|(judge, gold)| [judge.as_ref().to_string(), gold.as_ref().to_string()])
        .collect::<BTreeSet<_>>();
    let mut matrix = labels
        .iter()
        .map(|row| {
            (
                row.clone(),
                labels
                    .iter()
                    .map(|col| (col.clone(), 0usize))
                    .collect::<BTreeMap<_, _>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    for (judge, gold) in pairs {
        let count = matrix
            .get_mut(judge.as_ref())
            .and_then(|row| row.get_mut(gold.as_ref()))
            .expect("matrix should include every observed label");
        *count += 1;
    }
    matrix
}

pub fn majority_baseline(samples: &[Sample]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut counts = BTreeMap::<&str, usize>::new();
    for sample in samples {
        *counts.entry(&sample.label).or_default() += 1;
    }
    counts.values().copied().max().unwrap_or_default() as f64 / samples.len() as f64
}

pub fn cross_validated_accuracy(samples: &[Sample], k: usize) -> f64 {
    let n = samples.len();
    if n < 2 || k == 0 {
        return 0.0;
    }

    let k = k.min(n);
    let folds = (0..k)
        .map(|offset| {
            samples
                .iter()
                .skip(offset)
                .step_by(k)
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut accuracies = Vec::new();

    for index in 0..k {
        let test = &folds[index];
        let train = folds
            .iter()
            .enumerate()
            .filter(|(fold_index, _)| *fold_index != index)
            .flat_map(|(_, fold)| fold.iter().cloned())
            .collect::<Vec<_>>();
        if test.is_empty() || train.is_empty() {
            continue;
        }

        let result = match calibrate_threshold(
            &train,
            &CalibrationOptions {
                objective: "accuracy".to_string(),
                ..CalibrationOptions::default()
            },
        ) {
            Ok(result) => result,
            Err(_) => continue,
        };
        let Some(threshold) = result.summary["threshold"].as_f64() else {
            continue;
        };
        let Some(models) = result.summary["models"].as_array() else {
            continue;
        };
        let Some(low) = models.first().and_then(|model| model.as_str()) else {
            continue;
        };
        let Some(high) = models.get(1).and_then(|model| model.as_str()) else {
            continue;
        };
        let tiers = [
            Tier {
                min_score: 0.0,
                model: low.to_string(),
                cost: None,
            },
            Tier {
                min_score: threshold,
                model: high.to_string(),
                cost: None,
            },
        ];
        let correct = test
            .iter()
            .filter(|sample| recommend_tier(sample.score, &tiers) == sample.label)
            .count();
        accuracies.push(correct as f64 / test.len() as f64);
    }

    if accuracies.is_empty() {
        return 0.0;
    }
    accuracies.iter().sum::<f64>() / accuracies.len() as f64
}

pub fn evaluate<J, G>(gold_pairs: &[(J, G)], samples: &[Sample]) -> GateReport
where
    J: AsRef<str>,
    G: AsRef<str>,
{
    evaluate_with_options(gold_pairs, samples, EvaluateOptions::default())
}

pub fn evaluate_with_options<J, G>(
    gold_pairs: &[(J, G)],
    samples: &[Sample],
    options: EvaluateOptions,
) -> GateReport
where
    J: AsRef<str>,
    G: AsRef<str>,
{
    let kappa = cohens_kappa(gold_pairs);
    let confusion = confusion_matrix(gold_pairs);
    let label_counts = label_counts(samples);
    let majority = majority_baseline(samples);
    let cv_accuracy = cross_validated_accuracy(samples, options.k);
    let lift = cv_accuracy - majority;
    let degenerate = label_counts.len() < 2 || majority > options.degenerate_fraction;

    let mut failures = Vec::new();
    if gold_pairs.is_empty() {
        failures.push(
            "no gold agreement measured \u{2014} pass a human-labeled --gold set".to_string(),
        );
    } else if kappa < options.kappa_floor {
        failures.push(format!(
            "judge-vs-gold kappa {:.2} < floor {:.2}",
            kappa, options.kappa_floor
        ));
    }
    if degenerate {
        failures.push(
            "labels degenerate \u{2014} need both arms meaningfully represented, not ~all one arm"
                .to_string(),
        );
    } else if lift <= options.min_lift {
        failures.push(format!(
            "no out-of-fold lift \u{2014} cv accuracy {:.2} does not beat majority baseline {:.2}",
            cv_accuracy, majority
        ));
    }

    GateReport {
        kappa,
        kappa_floor: options.kappa_floor,
        n_gold: gold_pairs.len(),
        gold_abstained: options.gold_abstained,
        confusion,
        cv_accuracy,
        majority_baseline: majority,
        lift,
        label_counts,
        degenerate,
        passed: failures.is_empty(),
        failures,
    }
}

fn label_counts(samples: &[Sample]) -> Vec<(String, usize)> {
    let mut counts = Vec::<(String, usize)>::new();
    for sample in samples {
        if let Some((_, count)) = counts.iter_mut().find(|(label, _)| label == &sample.label) {
            *count += 1;
        } else {
            counts.push((sample.label.clone(), 1));
        }
    }
    counts
}

fn render_label_counts(counts: &[(String, usize)]) -> String {
    let body = counts
        .iter()
        .map(|(label, count)| format!("'{label}': {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{body}}}")
}
