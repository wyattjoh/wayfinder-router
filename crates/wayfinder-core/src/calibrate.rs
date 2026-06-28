use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use serde_json::{json, Value as JsonValue};

use crate::complexity::{
    extract_features_with_lexicon, normalized_features, recommend_tier, scalar_score,
    ClassifierModel, ClassifierWeights, FeatureCounts, FeatureWeights, Lexicon, Tier,
    DEFAULT_WEIGHTS, FEATURE_ORDER,
};

const DEFAULT_COST_LOW: f64 = 0.2;
const DEFAULT_COST_HIGH: f64 = 1.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub features: FeatureCounts,
    pub label: String,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationResult {
    pub toml: String,
    pub summary: JsonValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationError {
    message: String,
}

impl CalibrationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CalibrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CalibrationError {}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationOptions {
    pub models_order: Option<Vec<String>>,
    pub iterations: usize,
    pub l2: f64,
    pub objective: String,
    pub costs: Option<BTreeMap<String, f64>>,
    pub target_savings: Option<f64>,
    pub weights: Option<FeatureWeights>,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            models_order: None,
            iterations: 100,
            l2: 0.01,
            objective: "accuracy".to_string(),
            costs: None,
            target_savings: None,
            weights: None,
        }
    }
}

pub fn parse_dataset(text: &str, where_: &str) -> Result<Vec<Sample>, CalibrationError> {
    parse_dataset_with_lexicon(text, where_, &Lexicon::default())
}

pub fn parse_dataset_with_lexicon(
    text: &str,
    where_: &str,
    lexicon: &Lexicon,
) -> Result<Vec<Sample>, CalibrationError> {
    let mut samples = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let lineno = index + 1;
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let row: JsonValue = serde_json::from_str(line).map_err(|err| {
            CalibrationError::new(format!(
                "{where_}:{lineno}: invalid JSON: {}",
                python_json_error(line, &err)
            ))
        })?;
        let prompt = row.get("text").and_then(JsonValue::as_str);
        let label = row.get("label").and_then(JsonValue::as_str);
        let Some((prompt, label)) = prompt.zip(label).filter(|(_, label)| !label.is_empty()) else {
            return Err(CalibrationError::new(format!(
                "{where_}:{lineno}: each row needs string 'text' and non-empty 'label'"
            )));
        };
        let features = extract_features_with_lexicon(prompt, lexicon);
        samples.push(Sample {
            features,
            label: label.to_string(),
            score: default_score(&features),
        });
    }
    if samples.is_empty() {
        return Err(CalibrationError::new(format!(
            "{where_}: no labeled rows found"
        )));
    }
    Ok(samples)
}

pub fn load_dataset(path: &Path) -> Result<Vec<Sample>, CalibrationError> {
    load_dataset_with_lexicon(path, &Lexicon::default())
}

pub fn load_dataset_with_lexicon(
    path: &Path,
    lexicon: &Lexicon,
) -> Result<Vec<Sample>, CalibrationError> {
    let text = fs::read_to_string(path)
        .map_err(|err| CalibrationError::new(format!("cannot read {}: {err}", path.display())))?;
    parse_dataset_with_lexicon(&text, &path.to_string_lossy(), lexicon)
}

pub fn sweep_curve(samples: &[Sample]) -> Result<Vec<(f64, f64)>, CalibrationError> {
    let labels = labels_by_mean_score(samples);
    if labels.len() != 2 {
        return Err(CalibrationError::new(format!(
            "sweep needs exactly two labels, found {}: {}",
            labels.len(),
            py_list(&labels)
        )));
    }
    let high = &labels[1];
    let scored = samples
        .iter()
        .map(|sample| (sample.score, sample.label == *high))
        .collect::<Vec<_>>();
    let total = scored.len() as f64;
    Ok(candidates(&scored)
        .into_iter()
        .map(|cut| {
            let correct = scored
                .iter()
                .filter(|(score, is_high)| (*score >= cut) == *is_high)
                .count() as f64;
            (cut, correct / total)
        })
        .collect())
}

pub fn calibrate_threshold(
    samples: &[Sample],
    options: &CalibrationOptions,
) -> Result<CalibrationResult, CalibrationError> {
    let samples = rescore_samples(samples, options.weights);
    let prefix = weights_block(options.weights);
    let labels = labels_by_mean_score(&samples);
    if labels.len() != 2 {
        return Err(CalibrationError::new(format!(
            "threshold mode needs exactly two labels, found {}: {}",
            labels.len(),
            py_list(&labels)
        )));
    }

    if options.objective == "knee" {
        let (cost_low, cost_high, low, high) = cost_ordered_arms(&labels, options.costs.as_ref())?;
        let scored = samples
            .iter()
            .map(|sample| (sample.score, sample.label == high))
            .collect::<Vec<_>>();
        let (threshold, accuracy, savings, recall) = sweep_cut_knee(&scored, cost_low, cost_high);
        let tiers = vec![
            Tier {
                min_score: 0.0,
                model: low.clone(),
                cost: Some(cost_low),
            },
            Tier {
                min_score: threshold,
                model: high.clone(),
                cost: Some(cost_high),
            },
        ];
        return Ok(CalibrationResult {
            toml: format!("{prefix}{}", tiers_toml(&tiers)),
            summary: json!({
                "mode": "threshold",
                "objective": "knee",
                "threshold": threshold,
                "models": [low, high],
                "accuracy": round_to(accuracy, 4),
                "quality_recovered": round_to(recall, 4),
                "cost_savings": round_to(savings, 4),
                "samples": samples.len(),
            }),
        });
    }

    if options.objective == "cost-quality" {
        let Some(target_savings) = options.target_savings else {
            return Err(CalibrationError::new(
                "cost-quality objective needs a target_savings",
            ));
        };
        let (cost_low, cost_high, low, high) = cost_ordered_arms(&labels, options.costs.as_ref())?;
        let scored = samples
            .iter()
            .map(|sample| (sample.score, sample.label == high))
            .collect::<Vec<_>>();
        let (threshold, accuracy, savings) =
            sweep_cut_cost_aware(&scored, cost_low, cost_high, target_savings)?;
        let tiers = vec![
            Tier {
                min_score: 0.0,
                model: low.clone(),
                cost: Some(cost_low),
            },
            Tier {
                min_score: threshold,
                model: high.clone(),
                cost: Some(cost_high),
            },
        ];
        return Ok(CalibrationResult {
            toml: format!("{prefix}{}", tiers_toml(&tiers)),
            summary: json!({
                "mode": "threshold",
                "objective": "cost-quality",
                "threshold": threshold,
                "models": [low, high],
                "accuracy": round_to(accuracy, 4),
                "cost_savings": round_to(savings, 4),
                "target_savings": round_to(target_savings, 4),
                "samples": samples.len(),
            }),
        });
    }

    if options.objective != "accuracy" {
        return Err(CalibrationError::new(format!(
            "unknown objective: '{}'",
            options.objective
        )));
    }

    let low = &labels[0];
    let high = &labels[1];
    let scored = samples
        .iter()
        .map(|sample| (sample.score, sample.label == *high))
        .collect::<Vec<_>>();
    let (threshold, accuracy) = sweep_cut(&scored);
    let tiers = vec![
        Tier {
            min_score: 0.0,
            model: low.clone(),
            cost: None,
        },
        Tier {
            min_score: threshold,
            model: high.clone(),
            cost: None,
        },
    ];
    Ok(CalibrationResult {
        toml: format!("{prefix}{}", tiers_toml(&tiers)),
        summary: json!({
            "mode": "threshold",
            "threshold": threshold,
            "models": [low, high],
            "accuracy": round_to(accuracy, 4),
            "samples": samples.len(),
        }),
    })
}

pub fn calibrate_tiers(
    samples: &[Sample],
    options: &CalibrationOptions,
) -> Result<CalibrationResult, CalibrationError> {
    let samples = rescore_samples(samples, options.weights);
    let order = options
        .models_order
        .clone()
        .unwrap_or_else(|| labels_by_mean_score(&samples));
    let present = labels_present(&samples);
    if BTreeSet::from_iter(order.iter().cloned()) != present {
        return Err(CalibrationError::new(format!(
            "--models {} does not match dataset labels {}",
            py_list(&order),
            py_list(&present.into_iter().collect::<Vec<_>>())
        )));
    }
    if order.len() < 2 {
        return Err(CalibrationError::new(
            "tiers mode needs at least two labels",
        ));
    }

    let rank = order
        .iter()
        .enumerate()
        .map(|(index, label)| (label.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let mut tiers = vec![Tier {
        min_score: 0.0,
        model: order[0].clone(),
        cost: None,
    }];
    let mut previous = 0.0;
    for index in 0..order.len() - 1 {
        let lo = &order[index];
        let hi = &order[index + 1];
        let pair = samples
            .iter()
            .filter(|sample| sample.label == *lo || sample.label == *hi)
            .map(|sample| {
                (
                    sample.score,
                    rank.get(&sample.label).copied().unwrap() > index,
                )
            })
            .collect::<Vec<_>>();
        let (cut, _) = sweep_cut(&pair);
        let cut = cut.max(previous);
        tiers.push(Tier {
            min_score: cut,
            model: hi.clone(),
            cost: None,
        });
        previous = cut;
    }

    let accuracy = accuracy(&samples, |features| {
        let score = if let Some(weights) = options.weights {
            scalar_score(features, weights)
        } else {
            default_score(features)
        };
        recommend_tier(score, &tiers)
    });
    let breakpoints = tiers
        .iter()
        .skip(1)
        .map(|tier| tier.min_score)
        .collect::<Vec<_>>();
    Ok(CalibrationResult {
        toml: format!("{}{}", weights_block(options.weights), tiers_toml(&tiers)),
        summary: json!({
            "mode": "tiers",
            "models": order,
            "breakpoints": breakpoints,
            "accuracy": round_to(accuracy, 4),
            "samples": samples.len(),
        }),
    })
}

pub fn fit_classifier(
    samples: &[Sample],
    options: &CalibrationOptions,
) -> Result<CalibrationResult, CalibrationError> {
    let order = options
        .models_order
        .clone()
        .unwrap_or_else(|| labels_by_mean_score(samples));
    let present = labels_present(samples);
    if BTreeSet::from_iter(order.iter().cloned()) != present {
        return Err(CalibrationError::new(format!(
            "--models {} does not match dataset labels {}",
            py_list(&order),
            py_list(&present.into_iter().collect::<Vec<_>>())
        )));
    }
    if order.len() < 2 {
        return Err(CalibrationError::new(
            "classifier mode needs at least two labels",
        ));
    }

    let index = order
        .iter()
        .enumerate()
        .map(|(index, label)| (label.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let feat_n = FEATURE_ORDER.len();
    let class_n = order.len();
    let rows = samples
        .iter()
        .map(|sample| {
            let normalized = normalized_features(&sample.features);
            let mut row = FEATURE_ORDER
                .iter()
                .map(|name| normalized.get(name).unwrap())
                .collect::<Vec<_>>();
            row.push(1.0);
            row
        })
        .collect::<Vec<_>>();
    let targets = samples
        .iter()
        .map(|sample| index[&sample.label])
        .collect::<Vec<_>>();
    let params = feat_n + 1;
    let size = class_n * params;
    let mut theta = vec![0.0; size];
    let mut iterations_run = 0;

    for _ in 0..options.iterations {
        iterations_run += 1;
        let mut gradient = vec![0.0; size];
        let mut hessian = vec![vec![0.0; size]; size];
        for (row, target) in rows.iter().zip(targets.iter()) {
            let logits = (0..class_n)
                .map(|class| dot(&theta[class * params..(class + 1) * params], row))
                .collect::<Vec<_>>();
            let probs = softmax(&logits);
            for class in 0..class_n {
                let resid = probs[class] - if class == *target { 1.0 } else { 0.0 };
                let base_c = class * params;
                for j in 0..params {
                    gradient[base_c + j] += resid * row[j];
                }
                for other in 0..class_n {
                    let weight =
                        probs[class] * (if class == other { 1.0 } else { 0.0 } - probs[other]);
                    if weight == 0.0 {
                        continue;
                    }
                    let base_d = other * params;
                    for j in 0..params {
                        let weighted = weight * row[j];
                        for k in 0..params {
                            hessian[base_c + j][base_d + k] += weighted * row[k];
                        }
                    }
                }
            }
        }
        for p in 0..size {
            gradient[p] += options.l2 * theta[p];
            hessian[p][p] += options.l2;
        }
        let step = solve(hessian, gradient);
        for p in 0..size {
            theta[p] -= step[p];
        }
        if step.iter().map(|value| value.abs()).fold(0.0, f64::max) < 1e-8 {
            break;
        }
    }

    let weights_by_class = (0..class_n)
        .map(|class| theta[class * params..(class + 1) * params].to_vec())
        .collect::<Vec<_>>();
    let mut weights = ClassifierWeights::zeros(class_n);
    for (feature_index, name) in FEATURE_ORDER.iter().enumerate() {
        weights.set(
            name,
            (0..class_n)
                .map(|class| weights_by_class[class][feature_index])
                .collect(),
        );
    }
    let classifier = ClassifierModel {
        models: order.clone(),
        weights,
        intercepts: (0..class_n)
            .map(|class| weights_by_class[class][feat_n])
            .collect(),
    };
    let accuracy = accuracy(samples, |features| classifier.predict(features));

    Ok(CalibrationResult {
        toml: classifier_toml(&classifier),
        summary: json!({
            "mode": "classifier",
            "models": order,
            "iterations": iterations_run,
            "accuracy": round_to(accuracy, 4),
            "samples": samples.len(),
        }),
    })
}

pub fn calibrate(
    samples: &[Sample],
    mode: &str,
    options: CalibrationOptions,
) -> Result<CalibrationResult, CalibrationError> {
    if options.objective != "accuracy" && mode != "threshold" {
        return Err(CalibrationError::new(format!(
            "objective '{}' is only available in threshold mode",
            options.objective
        )));
    }
    match mode {
        "threshold" => calibrate_threshold(samples, &options),
        "tiers" => calibrate_tiers(samples, &options),
        "classifier" => fit_classifier(samples, &options),
        _ => Err(CalibrationError::new(format!(
            "unknown calibration mode: '{mode}'"
        ))),
    }
}

fn default_score(features: &FeatureCounts) -> f64 {
    scalar_score(features, DEFAULT_WEIGHTS)
}

fn rescore_samples(samples: &[Sample], weights: Option<FeatureWeights>) -> Vec<Sample> {
    let Some(weights) = weights else {
        return samples.to_vec();
    };
    samples
        .iter()
        .map(|sample| Sample {
            features: sample.features,
            label: sample.label.clone(),
            score: scalar_score(&sample.features, weights),
        })
        .collect()
}

fn labels_by_mean_score(samples: &[Sample]) -> Vec<String> {
    let mut totals: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for sample in samples {
        let entry = totals.entry(sample.label.clone()).or_insert((0.0, 0));
        entry.0 += sample.score;
        entry.1 += 1;
    }
    let mut labels = totals
        .into_iter()
        .map(|(label, (total, count))| (label, total / count as f64))
        .collect::<Vec<_>>();
    labels.sort_by(|(label_a, mean_a), (label_b, mean_b)| {
        mean_a.total_cmp(mean_b).then_with(|| label_a.cmp(label_b))
    });
    labels.into_iter().map(|(label, _)| label).collect()
}

fn labels_present(samples: &[Sample]) -> BTreeSet<String> {
    samples.iter().map(|sample| sample.label.clone()).collect()
}

fn sweep_cut(scored: &[(f64, bool)]) -> (f64, f64) {
    let total = scored.len() as f64;
    let mut best_acc = -1.0;
    let mut best_cuts = Vec::new();
    for cut in candidates(scored) {
        let correct = scored
            .iter()
            .filter(|(score, is_high)| (*score >= cut) == *is_high)
            .count() as f64;
        let acc = correct / total;
        if acc > best_acc {
            best_acc = acc;
            best_cuts = vec![cut];
        } else if acc == best_acc {
            best_cuts.push(cut);
        }
    }
    (best_cuts[best_cuts.len() / 2], best_acc)
}

fn candidates(scored: &[(f64, bool)]) -> Vec<f64> {
    let mut candidates = vec![0.0];
    candidates.extend(scored.iter().map(|(score, _)| round_to(*score, 4)));
    candidates.sort_by(|a, b| a.total_cmp(b));
    candidates.dedup_by(|a, b| *a == *b);
    candidates
}

fn cost_ordered_arms(
    labels: &[String],
    costs: Option<&BTreeMap<String, f64>>,
) -> Result<(f64, f64, String, String), CalibrationError> {
    let low = labels[0].clone();
    let high = labels[1].clone();
    let Some(costs) = costs else {
        return Ok((DEFAULT_COST_LOW, DEFAULT_COST_HIGH, low, high));
    };
    let missing = [low.clone(), high.clone()]
        .into_iter()
        .filter(|label| !costs.contains_key(label))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(CalibrationError::new(format!(
            "--costs must give a cost for each label; missing: {}",
            missing.join(", ")
        )));
    }
    let (low, high) = if costs[&low] <= costs[&high] {
        (low, high)
    } else {
        (high, low)
    };
    let cost_low = costs[&low];
    let cost_high = costs[&high];
    if cost_high <= 0.0 {
        return Err(CalibrationError::new(format!(
            "the high-cost arm ('{high}') must have a positive cost"
        )));
    }
    Ok((cost_low, cost_high, low, high))
}

fn savings_at(scored: &[(f64, bool)], cut: f64, cost_low: f64, cost_high: f64) -> f64 {
    let total = scored.len() as f64;
    let n_high = scored.iter().filter(|(score, _)| *score >= cut).count() as f64;
    let mean_cost = (n_high * cost_high + (total - n_high) * cost_low) / total;
    (cost_high - mean_cost) / cost_high
}

fn sweep_cut_cost_aware(
    scored: &[(f64, bool)],
    cost_low: f64,
    cost_high: f64,
    target_savings: f64,
) -> Result<(f64, f64, f64), CalibrationError> {
    let total = scored.len() as f64;
    let mut feasible = Vec::new();
    let mut best_savings: f64 = 0.0;
    for cut in candidates(scored) {
        let savings = savings_at(scored, cut, cost_low, cost_high);
        best_savings = best_savings.max(savings);
        if savings + 1e-9 >= target_savings {
            let correct = scored
                .iter()
                .filter(|(score, is_high)| (*score >= cut) == *is_high)
                .count() as f64;
            feasible.push((correct / total, cut));
        }
    }
    if feasible.is_empty() {
        return Err(CalibrationError::new(format!(
            "no cut reaches target savings {:.2}; the most achievable is {:.2}",
            target_savings, best_savings
        )));
    }
    let best_acc = feasible
        .iter()
        .map(|(accuracy, _)| *accuracy)
        .fold(-1.0, f64::max);
    let mut best_cuts = feasible
        .into_iter()
        .filter(|(accuracy, _)| *accuracy == best_acc)
        .map(|(_, cut)| cut)
        .collect::<Vec<_>>();
    best_cuts.sort_by(|a, b| a.total_cmp(b));
    let chosen = best_cuts[best_cuts.len() / 2];
    Ok((
        chosen,
        best_acc,
        savings_at(scored, chosen, cost_low, cost_high),
    ))
}

fn sweep_cut_knee(scored: &[(f64, bool)], cost_low: f64, cost_high: f64) -> (f64, f64, f64, f64) {
    let total = scored.len() as f64;
    let n_high = scored.iter().filter(|(_, is_high)| *is_high).count() as f64;
    let mut best_obj = -1.0;
    let mut best_cuts = Vec::new();
    for cut in candidates(scored) {
        let recall = scored
            .iter()
            .filter(|(score, is_high)| *is_high && *score >= cut)
            .count() as f64
            / n_high;
        let obj = recall * savings_at(scored, cut, cost_low, cost_high);
        if obj > best_obj {
            best_obj = obj;
            best_cuts = vec![cut];
        } else if obj == best_obj {
            best_cuts.push(cut);
        }
    }
    let chosen = best_cuts[best_cuts.len() / 2];
    let accuracy = scored
        .iter()
        .filter(|(score, is_high)| (*score >= chosen) == *is_high)
        .count() as f64
        / total;
    let recall = scored
        .iter()
        .filter(|(score, is_high)| *is_high && *score >= chosen)
        .count() as f64
        / n_high;
    (
        chosen,
        accuracy,
        savings_at(scored, chosen, cost_low, cost_high),
        recall,
    )
}

fn accuracy(samples: &[Sample], predict: impl Fn(&FeatureCounts) -> String) -> f64 {
    samples
        .iter()
        .filter(|sample| predict(&sample.features) == sample.label)
        .count() as f64
        / samples.len() as f64
}

fn dot(weights: &[f64], x: &[f64]) -> f64 {
    weights
        .iter()
        .zip(x.iter())
        .map(|(weight, value)| weight * value)
        .sum()
}

fn solve(matrix: Vec<Vec<f64>>, vector: Vec<f64>) -> Vec<f64> {
    let n = vector.len();
    let mut augmented = matrix
        .into_iter()
        .enumerate()
        .map(|(index, mut row)| {
            row.push(vector[index]);
            row
        })
        .collect::<Vec<_>>();
    for col in 0..n {
        let pivot = (col..n)
            .max_by(|a, b| {
                augmented[*a][col]
                    .abs()
                    .total_cmp(&augmented[*b][col].abs())
            })
            .unwrap();
        if pivot != col {
            augmented.swap(col, pivot);
        }
        let pivot_val = augmented[col][col];
        for row in col + 1..n {
            let factor = augmented[row][col] / pivot_val;
            if factor == 0.0 {
                continue;
            }
            let (before_row, from_row) = augmented.split_at_mut(row);
            let pivot_row = &before_row[col];
            let target_row = &mut from_row[0];
            for (cell, pivot_cell) in target_row[col..=n]
                .iter_mut()
                .zip(pivot_row[col..=n].iter())
            {
                *cell -= factor * *pivot_cell;
            }
        }
    }
    let mut solution = vec![0.0; n];
    for row in (0..n).rev() {
        let acc = augmented[row][n]
            - (row + 1..n)
                .map(|index| augmented[row][index] * solution[index])
                .sum::<f64>();
        solution[row] = acc / augmented[row][row];
    }
    solution
}

fn softmax(logits: &[f64]) -> Vec<f64> {
    let top = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exps = logits
        .iter()
        .map(|logit| (logit - top).exp())
        .collect::<Vec<_>>();
    let total = exps.iter().sum::<f64>();
    exps.into_iter().map(|value| value / total).collect()
}

fn weights_block(weights: Option<FeatureWeights>) -> String {
    let Some(weights) = weights else {
        return String::new();
    };
    if weights == DEFAULT_WEIGHTS {
        return String::new();
    }
    let mut diff = FEATURE_ORDER
        .iter()
        .filter(|name| weights.get(name).unwrap() != DEFAULT_WEIGHTS.get(name).unwrap())
        .copied()
        .collect::<Vec<_>>();
    if diff.is_empty() {
        return String::new();
    }
    diff.sort();
    let inner = diff
        .iter()
        .map(|name| format!("{name} = {}", fmt_float(weights.get(name).unwrap())))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[routing]\nweights = {{ {inner} }}\n\n")
}

fn tiers_toml(tiers: &[Tier]) -> String {
    tiers
        .iter()
        .map(|tier| {
            let mut block = format!(
                "[[routing.tiers]]\nmin_score = {}\nmodel = \"{}\"\n",
                fmt_float(tier.min_score),
                tier.model
            );
            if let Some(cost) = tier.cost {
                block.push_str(&format!("cost = {}\n", fmt_float(cost)));
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn classifier_toml(classifier: &ClassifierModel) -> String {
    let models = classifier
        .models
        .iter()
        .map(|model| format!("\"{model}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let intercepts = classifier
        .intercepts
        .iter()
        .map(|value| fmt_float(*value))
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
        let vector = classifier
            .weights
            .get(name)
            .unwrap()
            .iter()
            .map(|value| fmt_float(*value))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("{name} = [{vector}]"));
    }
    format!("{}\n", lines.join("\n"))
}

fn fmt_float(value: f64) -> String {
    let rounded = round_to(value, 6);
    let mut text = python_exponent_format(format!("{rounded:?}"));
    if !text.contains('.') && !text.contains('e') {
        text.push_str(".0");
    }
    text
}

fn python_exponent_format(text: String) -> String {
    let Some((mantissa, exponent)) = text.split_once('e') else {
        return text;
    };
    let exponent = exponent
        .parse::<i32>()
        .expect("Rust float debug exponent should be an integer");
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

fn round_to(value: f64, places: i32) -> f64 {
    let factor = 10_f64.powi(places);
    let scaled = value * factor;
    let floor = scaled.floor();
    let fraction = scaled - floor;
    let rounded = if (fraction - 0.5).abs() <= 1e-12 {
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else if fraction < 0.5 {
        floor
    } else {
        floor + 1.0
    };
    let rounded = rounded / factor;
    if rounded == 0.0 && value.is_sign_negative() {
        -0.0
    } else {
        rounded
    }
}

fn py_list(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|item| format!("'{item}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn python_json_error(line: &str, err: &serde_json::Error) -> String {
    let message = err.to_string();
    if message.starts_with("trailing characters") {
        let column = err.column().max(1);
        let char_index = column.saturating_sub(1);
        return format!("Extra data: line 1 column {column} (char {char_index})");
    }
    if message.starts_with("expected `,` or `}`") {
        let column = err.column().max(1);
        let char_index = column.saturating_sub(1);
        return format!("Expecting ',' delimiter: line 1 column {column} (char {char_index})");
    }
    if message.starts_with("expected `:`") {
        let column = err.column().max(1);
        let char_index = column.saturating_sub(1);
        return format!("Expecting ':' delimiter: line 1 column {column} (char {char_index})");
    }
    if message.starts_with("trailing comma") {
        let column = err.column().max(1);
        let char_index = column.saturating_sub(1);
        return format!(
            "Expecting property name enclosed in double quotes: line 1 column {column} (char {char_index})"
        );
    }
    if err.is_syntax() || err.is_eof() {
        let column = python_error_column(line, err);
        let char_index = column.saturating_sub(1);
        return format!("Expecting value: line 1 column {column} (char {char_index})");
    }
    message
}

fn python_error_column(line: &str, err: &serde_json::Error) -> usize {
    let trimmed = line.trim_start();
    let leading = line.len() - trimmed.len();
    let starts_like_value = trimmed
        .chars()
        .next()
        .is_some_and(|ch| matches!(ch, '{' | '[' | '"' | 't' | 'f' | 'n' | '-' | '0'..='9'));
    if starts_like_value && !trimmed.starts_with("not-") {
        err.column().max(1)
    } else {
        leading + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("core crate lives under crates/wayfinder-core")
            .join("tests/fixtures/contracts")
            .join(path);
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()))
    }

    #[test]
    fn classifier_toml_preserves_python_negative_zero_formatting() {
        let mut weights = ClassifierWeights::zeros(2);
        weights.word_count = vec![-1e-8, 1e-8];
        weights.heading_count = vec![-4e-7, 4e-7];
        let classifier = ClassifierModel {
            models: vec!["local".to_string(), "cloud".to_string()],
            weights,
            intercepts: vec![-1e-8, 1e-8],
        };
        let expected: JsonValue =
            serde_json::from_str(&fixture("calibrate/classifier-negative-zero-emitter.json"))
                .expect("fixture should be JSON");

        assert_eq!(
            classifier_toml(&classifier),
            expected["toml"].as_str().unwrap()
        );
    }

    #[test]
    fn classifier_toml_uses_python_exponent_formatting() {
        let mut weights = ClassifierWeights::zeros(2);
        weights.word_count = vec![0.000001, -0.000001];
        weights.heading_count = vec![1e20, -1e20];
        let classifier = ClassifierModel {
            models: vec!["local".to_string(), "cloud".to_string()],
            weights,
            intercepts: vec![0.000001, 1e20],
        };
        let expected: JsonValue =
            serde_json::from_str(&fixture("calibrate/classifier-exponent-emitter.json"))
                .expect("fixture should be JSON");

        assert_eq!(
            classifier_toml(&classifier),
            expected["toml"].as_str().unwrap()
        );
    }
}
