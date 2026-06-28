use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

use crate::SCORING_SCHEMA_VERSION;

pub const DEFAULT_THRESHOLD: f64 = 0.5;
pub const FEATURE_ORDER: [&str; 11] = [
    "word_count",
    "heading_count",
    "max_heading_depth",
    "list_item_count",
    "link_count",
    "code_block_count",
    "table_row_count",
    "reasoning_term_count",
    "math_symbol_count",
    "constraint_term_count",
    "question_count",
];

const REASONING_TERMS: &[&str] = &[
    "prove",
    "proof",
    "proofs",
    "proven",
    "derive",
    "derives",
    "derivation",
    "theorem",
    "theorems",
    "lemma",
    "lemmas",
    "corollary",
    "axiom",
    "axioms",
    "irrational",
    "undecidable",
    "undecidability",
    "decidable",
    "infinitely",
    "asymptotic",
    "complexity",
    "invariant",
    "invariants",
    "concurrency",
    "concurrent",
    "deadlock",
    "induction",
    "contradiction",
    "optimal",
    "optimality",
    "optimize",
    "optimise",
    "minimise",
    "minimize",
    "maximise",
    "maximize",
    "recurrence",
    "halting",
    "eigenvalue",
    "eigenvalues",
    "integral",
    "derivative",
    "polynomial",
    "prime",
    "primes",
    "modulo",
    "isomorphism",
    "monotonic",
    "bijection",
    "injective",
    "surjective",
    "combinatorial",
];

const CONSTRAINT_TERMS: &[&str] = &[
    "must",
    "without",
    "only",
    "ensure",
    "exactly",
    "guarantee",
    "constraint",
    "constraints",
    "subject",
    "preserving",
    "preserve",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FeatureWeights {
    pub word_count: f64,
    pub heading_count: f64,
    pub max_heading_depth: f64,
    pub list_item_count: f64,
    pub link_count: f64,
    pub code_block_count: f64,
    pub table_row_count: f64,
    pub reasoning_term_count: f64,
    pub math_symbol_count: f64,
    pub constraint_term_count: f64,
    pub question_count: f64,
}

pub const DEFAULT_WEIGHTS: FeatureWeights = FeatureWeights {
    word_count: 3.0,
    list_item_count: 2.0,
    heading_count: 1.5,
    code_block_count: 1.5,
    table_row_count: 1.0,
    link_count: 1.0,
    max_heading_depth: 1.0,
    reasoning_term_count: 0.0,
    math_symbol_count: 0.0,
    constraint_term_count: 0.0,
    question_count: 0.0,
};

const SATURATION: FeatureWeights = FeatureWeights {
    word_count: 400.0,
    heading_count: 8.0,
    max_heading_depth: 4.0,
    list_item_count: 15.0,
    link_count: 10.0,
    code_block_count: 4.0,
    table_row_count: 12.0,
    reasoning_term_count: 2.0,
    math_symbol_count: 6.0,
    constraint_term_count: 3.0,
    question_count: 3.0,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lexicon {
    pub reasoning_terms: Vec<String>,
    pub constraint_terms: Vec<String>,
}

impl Default for Lexicon {
    fn default() -> Self {
        Self {
            reasoning_terms: REASONING_TERMS
                .iter()
                .map(|term| term.to_string())
                .collect(),
            constraint_terms: CONSTRAINT_TERMS
                .iter()
                .map(|term| term.to_string())
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FeatureCounts {
    pub word_count: usize,
    pub heading_count: usize,
    pub max_heading_depth: usize,
    pub list_item_count: usize,
    pub link_count: usize,
    pub code_block_count: usize,
    pub table_row_count: usize,
    pub reasoning_term_count: usize,
    pub math_symbol_count: usize,
    pub constraint_term_count: usize,
    pub question_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tier {
    pub min_score: f64,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassifierWeights {
    pub word_count: Vec<f64>,
    pub heading_count: Vec<f64>,
    pub max_heading_depth: Vec<f64>,
    pub list_item_count: Vec<f64>,
    pub link_count: Vec<f64>,
    pub code_block_count: Vec<f64>,
    pub table_row_count: Vec<f64>,
    pub reasoning_term_count: Vec<f64>,
    pub math_symbol_count: Vec<f64>,
    pub constraint_term_count: Vec<f64>,
    pub question_count: Vec<f64>,
}

impl ClassifierWeights {
    pub fn zeros(count: usize) -> Self {
        let zero = vec![0.0; count];
        Self {
            word_count: zero.clone(),
            heading_count: zero.clone(),
            max_heading_depth: zero.clone(),
            list_item_count: zero.clone(),
            link_count: zero.clone(),
            code_block_count: zero.clone(),
            table_row_count: zero.clone(),
            reasoning_term_count: zero.clone(),
            math_symbol_count: zero.clone(),
            constraint_term_count: zero.clone(),
            question_count: zero,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassifierModel {
    pub models: Vec<String>,
    pub weights: ClassifierWeights,
    pub intercepts: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutingConfig {
    pub weights: FeatureWeights,
    pub tiers: Vec<Tier>,
    pub classifier: Option<ClassifierModel>,
    pub lexicon: Lexicon,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            weights: DEFAULT_WEIGHTS,
            tiers: binary_tiers(DEFAULT_THRESHOLD),
            classifier: None,
            lexicon: Lexicon::default(),
        }
    }
}

impl RoutingConfig {
    pub fn binary(threshold: f64) -> Self {
        Self {
            tiers: binary_tiers(threshold),
            ..Self::default()
        }
    }

    pub fn binary_with_weights(threshold: f64, weights: FeatureWeights) -> Self {
        Self {
            weights,
            tiers: binary_tiers(threshold),
            ..Self::default()
        }
    }
}

pub fn binary_tiers(threshold: f64) -> Vec<Tier> {
    vec![
        Tier {
            min_score: 0.0,
            model: "local".to_string(),
            cost: None,
        },
        Tier {
            min_score: threshold,
            model: "cloud".to_string(),
            cost: None,
        },
    ]
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ComplexityScore {
    pub schema_version: &'static str,
    pub score: f64,
    pub recommendation: String,
    pub mode: &'static str,
    pub features: FeatureCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<Tier>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

pub fn strip_frontmatter(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return text.to_string();
    }
    for (index, line) in lines.iter().enumerate().skip(1) {
        if matches!(line.trim(), "---" | "...") {
            return lines[index + 1..].join("\n");
        }
    }
    text.to_string()
}

pub fn extract_features(text: &str) -> FeatureCounts {
    extract_features_with_lexicon(text, &Lexicon::default())
}

pub fn extract_features_with_lexicon(text: &str, lexicon: &Lexicon) -> FeatureCounts {
    let body = strip_frontmatter(text);
    let mut features = FeatureCounts {
        word_count: body.split_whitespace().count(),
        question_count: body.matches('?').count(),
        ..FeatureCounts::default()
    };

    let mut in_fence = false;
    for line in body.lines() {
        if fence_re().is_match(line) {
            if !in_fence {
                features.code_block_count += 1;
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        if let Some(capture) = heading_re().captures(line) {
            features.heading_count += 1;
            features.max_heading_depth = features.max_heading_depth.max(capture[1].chars().count());
        } else if list_re().is_match(line) {
            features.list_item_count += 1;
        } else if table_row_re().is_match(line) {
            features.table_row_count += 1;
        }
        features.link_count += link_re().find_iter(line).count();
    }

    let lower = body.to_lowercase();
    let tokens: Vec<&str> = word_token_re()
        .find_iter(&lower)
        .map(|found| found.as_str())
        .collect();
    features.reasoning_term_count = tokens
        .iter()
        .filter(|token| lexicon.reasoning_terms.iter().any(|term| term == **token))
        .count();
    features.constraint_term_count = tokens
        .iter()
        .filter(|token| lexicon.constraint_terms.iter().any(|term| term == **token))
        .count();
    features.math_symbol_count = math_symbol_re().find_iter(&body).count();
    features
}

pub fn normalized_features(features: &FeatureCounts) -> FeatureWeights {
    FeatureWeights {
        word_count: normalize(features.word_count, SATURATION.word_count),
        heading_count: normalize(features.heading_count, SATURATION.heading_count),
        max_heading_depth: normalize(features.max_heading_depth, SATURATION.max_heading_depth),
        list_item_count: normalize(features.list_item_count, SATURATION.list_item_count),
        link_count: normalize(features.link_count, SATURATION.link_count),
        code_block_count: normalize(features.code_block_count, SATURATION.code_block_count),
        table_row_count: normalize(features.table_row_count, SATURATION.table_row_count),
        reasoning_term_count: normalize(
            features.reasoning_term_count,
            SATURATION.reasoning_term_count,
        ),
        math_symbol_count: normalize(features.math_symbol_count, SATURATION.math_symbol_count),
        constraint_term_count: normalize(
            features.constraint_term_count,
            SATURATION.constraint_term_count,
        ),
        question_count: normalize(features.question_count, SATURATION.question_count),
    }
}

pub fn scalar_score(features: &FeatureCounts, weights: FeatureWeights) -> f64 {
    let normalized = normalized_features(features);
    let total_weight = weights.sum();
    if total_weight == 0.0 {
        return 0.0;
    }
    round_to(
        (weights.word_count * normalized.word_count
            + weights.heading_count * normalized.heading_count
            + weights.max_heading_depth * normalized.max_heading_depth
            + weights.list_item_count * normalized.list_item_count
            + weights.link_count * normalized.link_count
            + weights.code_block_count * normalized.code_block_count
            + weights.table_row_count * normalized.table_row_count
            + weights.reasoning_term_count * normalized.reasoning_term_count
            + weights.math_symbol_count * normalized.math_symbol_count
            + weights.constraint_term_count * normalized.constraint_term_count
            + weights.question_count * normalized.question_count)
            / total_weight,
        2,
    )
}

pub fn recommend_tier(score: f64, tiers: &[Tier]) -> String {
    let mut chosen = tiers
        .first()
        .map(|tier| tier.model.clone())
        .unwrap_or_else(|| "local".to_string());
    for tier in tiers {
        if score >= tier.min_score {
            chosen = tier.model.clone();
        } else {
            break;
        }
    }
    chosen
}

pub fn score_complexity(text: &str, config: &RoutingConfig) -> ComplexityScore {
    let features = extract_features_with_lexicon(text, &config.lexicon);
    let score = scalar_score(&features, config.weights);
    if let Some(classifier) = &config.classifier {
        return ComplexityScore {
            schema_version: SCORING_SCHEMA_VERSION,
            score,
            recommendation: classifier.predict(&features),
            mode: "classifier",
            features,
            tiers: None,
            models: Some(classifier.models.clone()),
        };
    }

    ComplexityScore {
        schema_version: SCORING_SCHEMA_VERSION,
        score,
        recommendation: recommend_tier(score, &config.tiers),
        mode: "tiered",
        features,
        tiers: Some(config.tiers.clone()),
        models: None,
    }
}

impl FeatureWeights {
    pub fn sum(self) -> f64 {
        self.word_count
            + self.heading_count
            + self.max_heading_depth
            + self.list_item_count
            + self.link_count
            + self.code_block_count
            + self.table_row_count
            + self.reasoning_term_count
            + self.math_symbol_count
            + self.constraint_term_count
            + self.question_count
    }

    pub fn get(self, name: &str) -> Option<f64> {
        match name {
            "word_count" => Some(self.word_count),
            "heading_count" => Some(self.heading_count),
            "max_heading_depth" => Some(self.max_heading_depth),
            "list_item_count" => Some(self.list_item_count),
            "link_count" => Some(self.link_count),
            "code_block_count" => Some(self.code_block_count),
            "table_row_count" => Some(self.table_row_count),
            "reasoning_term_count" => Some(self.reasoning_term_count),
            "math_symbol_count" => Some(self.math_symbol_count),
            "constraint_term_count" => Some(self.constraint_term_count),
            "question_count" => Some(self.question_count),
            _ => None,
        }
    }

    pub fn set(&mut self, name: &str, value: f64) -> bool {
        match name {
            "word_count" => self.word_count = value,
            "heading_count" => self.heading_count = value,
            "max_heading_depth" => self.max_heading_depth = value,
            "list_item_count" => self.list_item_count = value,
            "link_count" => self.link_count = value,
            "code_block_count" => self.code_block_count = value,
            "table_row_count" => self.table_row_count = value,
            "reasoning_term_count" => self.reasoning_term_count = value,
            "math_symbol_count" => self.math_symbol_count = value,
            "constraint_term_count" => self.constraint_term_count = value,
            "question_count" => self.question_count = value,
            _ => return false,
        }
        true
    }
}

impl ClassifierWeights {
    pub fn get(&self, name: &str) -> Option<&Vec<f64>> {
        match name {
            "word_count" => Some(&self.word_count),
            "heading_count" => Some(&self.heading_count),
            "max_heading_depth" => Some(&self.max_heading_depth),
            "list_item_count" => Some(&self.list_item_count),
            "link_count" => Some(&self.link_count),
            "code_block_count" => Some(&self.code_block_count),
            "table_row_count" => Some(&self.table_row_count),
            "reasoning_term_count" => Some(&self.reasoning_term_count),
            "math_symbol_count" => Some(&self.math_symbol_count),
            "constraint_term_count" => Some(&self.constraint_term_count),
            "question_count" => Some(&self.question_count),
            _ => None,
        }
    }

    pub fn set(&mut self, name: &str, values: Vec<f64>) -> bool {
        match name {
            "word_count" => self.word_count = values,
            "heading_count" => self.heading_count = values,
            "max_heading_depth" => self.max_heading_depth = values,
            "list_item_count" => self.list_item_count = values,
            "link_count" => self.link_count = values,
            "code_block_count" => self.code_block_count = values,
            "table_row_count" => self.table_row_count = values,
            "reasoning_term_count" => self.reasoning_term_count = values,
            "math_symbol_count" => self.math_symbol_count = values,
            "constraint_term_count" => self.constraint_term_count = values,
            "question_count" => self.question_count = values,
            _ => return false,
        }
        true
    }
}

impl ClassifierModel {
    pub fn predict(&self, features: &FeatureCounts) -> String {
        let normalized = normalized_features(features);
        let mut best = 0;
        let mut best_logit = self.logit(0, normalized);
        for index in 1..self.models.len() {
            let logit = self.logit(index, normalized);
            if logit > best_logit {
                best = index;
                best_logit = logit;
            }
        }
        self.models[best].clone()
    }

    fn logit(&self, index: usize, normalized: FeatureWeights) -> f64 {
        self.intercepts[index]
            + self.weights.word_count[index] * normalized.word_count
            + self.weights.heading_count[index] * normalized.heading_count
            + self.weights.max_heading_depth[index] * normalized.max_heading_depth
            + self.weights.list_item_count[index] * normalized.list_item_count
            + self.weights.link_count[index] * normalized.link_count
            + self.weights.code_block_count[index] * normalized.code_block_count
            + self.weights.table_row_count[index] * normalized.table_row_count
            + self.weights.reasoning_term_count[index] * normalized.reasoning_term_count
            + self.weights.math_symbol_count[index] * normalized.math_symbol_count
            + self.weights.constraint_term_count[index] * normalized.constraint_term_count
            + self.weights.question_count[index] * normalized.question_count
    }
}

fn normalize(value: usize, saturation: f64) -> f64 {
    (value as f64 / saturation).min(1.0)
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
    rounded / factor
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})\s+\S").unwrap())
}

fn list_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+\S").unwrap())
}

fn table_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\|.*\|\s*$").unwrap())
}

fn fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:```|~~~)").unwrap())
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[[^\]]+\]\([^)]+\)").unwrap())
}

fn word_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-zA-Z][a-zA-Z'\-]*").unwrap())
}

fn math_symbol_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[∑∫√≤≥≠≈∞∂∈∉∀∃⊆⊂∪∩∇±×÷πθλμσΣΠ]|\\[a-zA-Z]+").unwrap())
}
