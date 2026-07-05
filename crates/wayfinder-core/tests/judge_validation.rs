use wayfinder_internal_core::judge::Judge;
use wayfinder_internal_core::judge_validation::{
    render_markdown, report_json, validate, AlwaysSufficientJudge, ExactMatchJudge, JudgeRow,
    INSUFFICIENT, SUFFICIENT,
};

const DIVERGENT_A: &str =
    "The mitochondria is the powerhouse of the cell and produces ATP for energy.";
const DIVERGENT_B: &str =
    "Paris has been the capital of France since the late tenth century, roughly.";
const REFUSAL: &str = "I cannot help with that request at all, unfortunately.";
const ANSWER: &str = "The answer to the question is forty-two, computed directly.";

fn agreement_row(local_score: f64, bucket: &str) -> JudgeRow {
    JudgeRow::new("p", ANSWER, ANSWER, local_score, 1.0, bucket)
}

fn planted_rows() -> Vec<JudgeRow> {
    let mut rows = Vec::new();
    rows.extend((0..6).map(|_| agreement_row(1.0, "math")));
    rows.extend((0..2).map(|_| JudgeRow::new("p", REFUSAL, ANSWER, 0.0, 1.0, "qa")));
    rows.push(JudgeRow::new("p", ANSWER, REFUSAL, 0.0, 0.0, "qa"));
    rows.push(agreement_row(0.0, "qa"));
    rows.extend((0..2).map(|_| JudgeRow::new("p", DIVERGENT_A, DIVERGENT_B, 1.0, 1.0, "math")));
    rows
}

#[test]
fn planted_counts_and_abstention() {
    let reports = validate(&planted_rows(), None, 0.5);
    let overall = &reports["overall"];

    assert_eq!(overall.n, 12);
    assert_eq!(overall.abstained, 2);
    assert_eq!(overall.decided(), 10);
    assert_eq!(overall.abstention_rate(), 2.0 / 12.0);
    assert_eq!(overall.by_comparator["agreement"], 7);
    assert_eq!(overall.by_comparator["refusal"], 3);
    assert_eq!(overall.by_comparator["divergence"], 2);
}

#[test]
fn planted_kappa_matches_hand_computation() {
    let reports = validate(&planted_rows(), None, 0.5);
    let stats = &reports["overall"].gold["absolute"];

    assert_eq!(stats.n(), 10);
    assert_eq!(stats.accuracy(), 0.8);
    assert!((stats.kappa() - 6.0 / 11.0).abs() < 1e-12);
    assert_eq!(stats.confusion()[SUFFICIENT][SUFFICIENT], 6);
    assert_eq!(stats.confusion()[SUFFICIENT][INSUFFICIENT], 2);
    assert_eq!(stats.confusion()[INSUFFICIENT][SUFFICIENT], 0);
    assert_eq!(stats.confusion()[INSUFFICIENT][INSUFFICIENT], 2);
}

#[test]
fn relative_gold_forgives_shared_miss() {
    let reports = validate(&planted_rows(), None, 0.5);
    let absolute = &reports["overall"].gold["absolute"];
    let relative = &reports["overall"].gold["relative"];

    assert_eq!(absolute.accuracy(), 0.8);
    assert_eq!(relative.accuracy(), 0.9);
}

#[test]
fn buckets_partition_overall() {
    let reports = validate(&planted_rows(), None, 0.5);

    assert_eq!(reports["math"].n + reports["qa"].n, reports["overall"].n);
    assert_eq!(reports["math"].abstained, 2);
    assert_eq!(reports["qa"].abstained, 0);
}

#[test]
fn report_is_deterministic() {
    let first = validate(&planted_rows(), None, 0.5);
    let second = validate(&planted_rows(), None, 0.5);

    assert_eq!(report_json(&first), report_json(&second));
    let markdown = render_markdown(&first, "heuristic-2", 0.5, "planted");
    assert_eq!(
        markdown,
        render_markdown(&second, "heuristic-2", 0.5, "planted")
    );
    assert!(markdown.contains("| overall | 12 | 16.7% |"));
    assert!(markdown.contains("0.545"));
}

#[test]
fn comparator_accuracy_table() {
    let reports = validate(&planted_rows(), None, 0.5);
    let overall = &reports["overall"];

    assert_eq!(overall.comparator_hits["agreement"], [6, 7]);
    assert_eq!(overall.comparator_hits["refusal"], [2, 3]);
    assert!(!overall.comparator_hits.contains_key("divergence"));
}

#[test]
fn baseline_judges() {
    let always = AlwaysSufficientJudge::default();
    assert_eq!(always.version(), "always-sufficient");
    assert_eq!(always.judge("q", "anything", "else").sufficient, Some(true));

    let exact = ExactMatchJudge::default();
    assert_eq!(exact.version(), "exact-match");
    assert_eq!(
        exact.judge("q", "Same Text", "same   text").sufficient,
        Some(true)
    );
    assert_eq!(exact.judge("q", "one thing", "another").sufficient, None);
}
