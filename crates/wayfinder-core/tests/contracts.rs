use serde_json::{json, Value as JsonValue};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use wayfinder_internal_core::calibrate::{
    calibrate, load_dataset, parse_dataset, CalibrationOptions, Sample,
};
use wayfinder_internal_core::complexity::{
    explain_score, extract_features, score_complexity, ClassifierModel, ClassifierWeights,
    FeatureCounts, RoutingConfig, DEFAULT_THRESHOLD, DEFAULT_WEIGHTS, FEATURE_ORDER,
};
use wayfinder_internal_core::config::{
    dump_routing_toml, routing_config_from_toml, WayfinderConfigError, THRESHOLD_ENV,
};
use wayfinder_internal_core::{feedback, pricing, threads, vkeys};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("core crate lives under crates/wayfinder-core")
        .to_path_buf()
}

fn fixture(path: &str) -> PathBuf {
    repo_root().join("tests/fixtures/contracts").join(path)
}

fn unique_temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "wayfinder-core-contract-{name}-{}-{seq}-{nanos}",
        std::process::id()
    ))
}

fn calibration_fixture(name: &str) -> JsonValue {
    let path = fixture(&format!("calibrate/{name}"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()))
}

#[test]
fn scoring_contract_fixtures_match_python_outputs() {
    for name in ["scoring/simple.json", "scoring/markdown-structure.json"] {
        let path = fixture(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
        let parsed: JsonValue = serde_json::from_str(&text)
            .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()));
        let prompt = parsed["prompt"].as_str().expect("fixture prompt");
        let actual = serde_json::to_value(score_complexity(prompt, &RoutingConfig::default()))
            .expect("score should serialize");

        assert_eq!(actual, parsed["expected"]);
    }
}

#[test]
fn calibration_contract_fixtures_match_python_outputs() {
    let cases = [
        (
            "threshold-accuracy.json",
            "threshold",
            CalibrationOptions::default(),
        ),
        (
            "threshold-cost-quality.json",
            "threshold",
            CalibrationOptions {
                objective: "cost-quality".to_string(),
                target_savings: Some(0.4),
                ..CalibrationOptions::default()
            },
        ),
        (
            "threshold-cost-quality-inverted-costs.json",
            "threshold",
            CalibrationOptions {
                objective: "cost-quality".to_string(),
                costs: Some(BTreeMap::from([
                    ("local".to_string(), 1.0),
                    ("cloud".to_string(), 0.1),
                ])),
                target_savings: Some(0.3),
                ..CalibrationOptions::default()
            },
        ),
        ("tiers.json", "tiers", CalibrationOptions::default()),
        (
            "classifier.json",
            "classifier",
            CalibrationOptions {
                iterations: 400,
                ..CalibrationOptions::default()
            },
        ),
    ];

    for (name, mode, options) in cases {
        let expected = calibration_fixture(name);
        let samples = parse_dataset(expected["dataset"].as_str().unwrap(), "dataset.jsonl")
            .unwrap_or_else(|err| panic!("{name} dataset should parse: {err}"));
        let actual = calibrate(&samples, mode, options)
            .unwrap_or_else(|err| panic!("{name} should calibrate: {err}"));

        assert_eq!(actual.toml, expected["toml"], "{name} TOML changed");
        assert_eq!(
            serde_json::to_value(actual.summary).unwrap(),
            expected["summary"],
            "{name} summary changed"
        );
    }
}

#[test]
fn calibration_knee_contract_fixture_matches_python_output() {
    let expected = calibration_fixture("threshold-knee.json");
    let samples = expected["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|raw| Sample {
            features: FeatureCounts::default(),
            label: raw["label"].as_str().unwrap().to_string(),
            score: raw["score"].as_f64().unwrap(),
        })
        .collect::<Vec<_>>();
    let actual = calibrate(
        &samples,
        "threshold",
        CalibrationOptions {
            objective: "knee".to_string(),
            ..CalibrationOptions::default()
        },
    )
    .expect("knee calibration should succeed");

    assert_eq!(actual.toml, expected["toml"]);
    assert_eq!(
        serde_json::to_value(actual.summary).unwrap(),
        expected["summary"]
    );
}

#[test]
fn parse_dataset_rejects_malformed_rows_with_python_messages() {
    let expected = calibration_fixture("parse-errors.json");
    let inputs = [
        ("empty", "\n"),
        ("missing_label", "{\"text\":\"hi\"}\n"),
        ("empty_label", "{\"text\":\"hi\",\"label\":\"\"}\n"),
        ("non_string_text", "{\"text\":1,\"label\":\"local\"}\n"),
        ("invalid_json", "not-json\n"),
        (
            "extra_data",
            "{\"text\": \"hi\", \"label\": \"local\"} garbage\n",
        ),
        ("missing_comma", "{\"text\": \"hi\" \"label\": \"local\"}\n"),
    ];

    for (name, text) in inputs {
        let err = match parse_dataset(text, "dataset.jsonl") {
            Ok(_) => panic!("{name} should be rejected"),
            Err(err) => err,
        };
        assert_eq!(err.to_string(), expected[name].as_str().unwrap(), "{name}");
    }
}

#[test]
fn load_dataset_reads_jsonl_rows_from_disk() {
    let expected = calibration_fixture("threshold-accuracy.json");
    let path = std::env::temp_dir().join(format!(
        "wayfinder-core-calibrate-{}-data.jsonl",
        std::process::id()
    ));
    std::fs::write(&path, expected["dataset"].as_str().unwrap())
        .expect("dataset fixture should be writable");

    let samples = load_dataset(&path).expect("dataset should load");
    let actual = calibrate(&samples, "threshold", CalibrationOptions::default())
        .expect("loaded samples should calibrate");

    assert_eq!(actual.toml, expected["toml"]);
    std::fs::remove_file(path).expect("temp dataset should be removable");
}

#[test]
fn feature_extraction_strips_frontmatter_and_ignores_code_fence_structure() {
    let body = "# Task\n\nDo the thing.\n\n## Steps\n\n- one\n- two\n";
    let with_frontmatter = format!("---\nschema_version: 1\n---\n{body}");
    assert_eq!(extract_features(&with_frontmatter), extract_features(body));

    let features = extract_features("```\n## Not a heading\n- not a list\n| a | b |\n```\n");
    assert_eq!(features.heading_count, 0);
    assert_eq!(features.list_item_count, 0);
    assert_eq!(features.table_row_count, 0);
    assert_eq!(features.code_block_count, 1);
}

#[test]
fn lexical_counts_match_python_defaults_but_do_not_affect_default_score() {
    let features = extract_features(r"Prove theorem ∑ and \frac{x}{2}. Must it work?");
    assert_eq!(features.reasoning_term_count, 2);
    assert_eq!(features.math_symbol_count, 2);
    assert_eq!(features.constraint_term_count, 1);
    assert_eq!(features.question_count, 1);

    let easy = score_complexity(
        "What is the capital of France?",
        &RoutingConfig::binary(0.1),
    );
    let hard = score_complexity(
        "Prove that the square root of 2 is irrational.",
        &RoutingConfig::binary(0.1),
    );
    assert_eq!(easy.recommendation, "local");
    assert_eq!(hard.recommendation, "local");
}

#[test]
fn weighted_lexical_signal_can_route_a_short_prompt_up() {
    let mut weights = DEFAULT_WEIGHTS;
    weights.reasoning_term_count = 5.0;
    let config = RoutingConfig::binary_with_weights(0.1, weights);
    let result = score_complexity("Prove that the square root of 2 is irrational.", &config);
    assert_eq!(result.recommendation, "cloud");
}

#[test]
fn classifier_prediction_uses_normalized_features_and_stable_tie_breaks() {
    let mut weights = ClassifierWeights::zeros(2);
    weights.word_count = vec![0.0, 5.0];
    let classifier = ClassifierModel {
        models: vec!["small".to_string(), "big".to_string()],
        weights,
        intercepts: vec![1.0, 0.0],
    };
    let config = RoutingConfig {
        classifier: Some(classifier),
        ..RoutingConfig::default()
    };

    assert_eq!(
        score_complexity("Say hello.", &config).recommendation,
        "small"
    );
    let big = score_complexity(&"word ".repeat(500), &config);
    assert_eq!(big.recommendation, "big");
    assert_eq!(big.mode, "classifier");
}

#[test]
fn routing_config_fixture_parses_threshold_and_ignores_gateway_sections() {
    let path = fixture("config/minimal-routing.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    let config = routing_config_from_toml(&text, path.to_str().unwrap()).expect("config");

    assert!(config.classifier.is_none());
    assert_eq!(config.tiers[0].model, "local");
    assert_eq!(config.tiers[1].model, "cloud");
    assert_eq!(config.tiers[1].min_score, 0.2);
}

#[test]
fn env_threshold_overrides_binary_threshold_only() {
    std::env::set_var(THRESHOLD_ENV, "0.2");
    let binary = routing_config_from_toml("[routing]\nthreshold = 0.8\n", "inline").unwrap();
    std::env::remove_var(THRESHOLD_ENV);
    assert_eq!(binary.tiers[1].min_score, 0.2);

    let tiers = routing_config_from_toml(
        "[[routing.tiers]]\nmin_score = 0.0\nmodel = \"local\"\n\n[[routing.tiers]]\nmin_score = 0.7\nmodel = \"cloud\"\n",
        "inline",
    )
    .unwrap();

    assert_eq!(tiers.tiers[1].min_score, 0.7);
}

#[test]
fn malformed_toml_returns_stable_config_errors() {
    let err = routing_config_from_toml("[routing]\nthreshold = 2.0\n", "bad.toml")
        .expect_err("threshold should be invalid");
    assert_eq!(
        err.to_string(),
        "bad.toml: 'routing.threshold' must be a number in 0.0-1.0"
    );

    let err =
        routing_config_from_toml("routing = 1\n", "bad.toml").expect_err("routing must be table");
    assert!(matches!(err, WayfinderConfigError { .. }));
    assert_eq!(err.to_string(), "bad.toml: '[routing]' must be a table");
}

#[test]
fn dump_routing_toml_round_trips_classifier_config() {
    let text = r#"
[routing]
weights = { word_count = 4.0 }

[routing.classifier]
models = ["local", "cloud"]
intercepts = [0.5, -0.5]

[routing.classifier.weights]
word_count = [0.0, 2.0]
"#;
    let config = routing_config_from_toml(text, "inline").expect("config");
    let dumped = dump_routing_toml(&config);
    let reparsed = routing_config_from_toml(&dumped, "dumped").expect("dumped config");

    assert_eq!(reparsed, config);
}

#[test]
fn explicit_default_lexicon_terms_do_not_dump_as_custom() {
    let mut terms = RoutingConfig::default().lexicon.reasoning_terms;
    terms.reverse();
    let quoted = terms
        .iter()
        .map(|term| format!("{term:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let config = routing_config_from_toml(
        &format!("[routing.lexicon]\nreasoning_terms = [{quoted}]\n"),
        "inline",
    )
    .expect("config");

    assert_eq!(config.lexicon, RoutingConfig::default().lexicon);
    assert!(!dump_routing_toml(&config).contains("[routing.lexicon]"));
}

#[test]
fn pricing_math_matches_python_cost_contracts() {
    assert_eq!(pricing::estimate_tokens(""), 0);
    assert_eq!(pricing::estimate_tokens("a"), 1);
    assert_eq!(pricing::estimate_tokens(&"x".repeat(40)), 10);
    assert_eq!(pricing::estimate_tokens("🙂🙂🙂🙂"), 1);

    let (fallback, priced) =
        pricing::price_table([("local", None), ("cloud", None)], ["local", "cloud"]);
    assert!(!priced);
    assert_eq!(fallback.get("local"), Some(&0.2));
    assert_eq!(fallback.get("cloud"), Some(&1.0));

    let version_a = pricing::table_version([("local", 0.0), ("cloud", 0.009)]);
    let version_b = pricing::table_version([("cloud", 0.009), ("local", 0.0)]);
    assert_eq!(version_a, version_b);
    assert_eq!(version_a, "e5701a6566b7");
    assert_eq!(version_a.len(), 12);

    let turn = pricing::turn_cost(
        "local",
        1000,
        0,
        [("local", 0.0), ("cloud", 0.009)],
        false,
        None,
    );
    assert_eq!(turn.realized, 0.0);
    assert_eq!(turn.baseline, 0.009);
    assert_eq!(turn.savings, 0.009);
}

#[test]
fn usage_tokens_prefers_upstream_usage_then_estimates() {
    let with_usage = json!({ "usage": { "prompt_tokens": 100, "total_tokens": 130 } });
    assert_eq!(
        pricing::usage_tokens(&with_usage, "", ""),
        pricing::UsageTokens {
            prompt_tokens: 100,
            completion_tokens: 30,
            estimated: false,
        }
    );

    assert_eq!(
        pricing::usage_tokens(&json!({}), &"x".repeat(40), &"y".repeat(80)),
        pricing::UsageTokens {
            prompt_tokens: 10,
            completion_tokens: 20,
            estimated: true,
        }
    );
    assert_eq!(
        pricing::usage_tokens(&json!({}), "🙂🙂🙂🙂", "éééééééé"),
        pricing::UsageTokens {
            prompt_tokens: 1,
            completion_tokens: 2,
            estimated: true,
        }
    );
}

#[test]
fn virtual_keys_hash_verify_match_extract_and_generate() {
    let hash = vkeys::hash_key("wf-abc");
    assert_eq!(hash.len(), 64);
    assert!(vkeys::verify("wf-abc", &hash.to_uppercase()));
    assert!(!vkeys::verify("wf-abd", &hash));

    let key_hashes = [
        ("team-a", vkeys::hash_key("ka")),
        ("team-b", vkeys::hash_key("kb")),
    ];
    assert_eq!(
        vkeys::match_key(Some("ka"), key_hashes.clone()),
        Some("team-a".to_string())
    );
    assert_eq!(vkeys::match_key(Some("nope"), key_hashes), None);
    assert_eq!(
        vkeys::extract_bearer(Some("Bearer wf-xyz")),
        Some("wf-xyz".to_string())
    );
    assert_eq!(vkeys::extract_bearer(Some("Bearer ")), None);

    let generated = vkeys::generate("wf");
    assert!(generated.plaintext.starts_with("wf-"));
    assert!(vkeys::verify(&generated.plaintext, &generated.hash));
}

#[test]
fn thread_json_shape_round_trips_and_title_matches_python() {
    let messages = vec![
        json!({"role": "assistant", "content": "hi"}),
        json!({"role": "user", "content": "  In one sentence,\n what is an API?  "}),
    ];
    assert_eq!(
        threads::title_from(&messages, 50),
        "In one sentence, what is an API?"
    );
    assert_eq!(
        threads::title_from(&[json!({"role": "user", "content": "x".repeat(80)})], 50),
        format!("{}\u{2026}", "x".repeat(50))
    );
    assert_eq!(threads::title_from(&[], 50), "(empty)");

    let thread = threads::Thread {
        id: "20260623T120000-abcd".to_string(),
        title: "what is an API?".to_string(),
        created: "2026-06-23T12:00:00Z".to_string(),
        updated: "2026-06-23T12:01:00Z".to_string(),
        messages: vec![
            json!({"role": "user", "content": "what is an API?"}),
            json!({"role": "assistant", "content": "A contract between programs."}),
        ],
    };
    let encoded = serde_json::to_string_pretty(&thread).expect("thread should serialize");
    let decoded: threads::Thread = serde_json::from_str(&encoded).expect("thread should parse");

    assert_eq!(decoded, thread);
}

#[test]
fn feedback_log_round_trips_rows_in_append_order() {
    let dir = unique_temp_dir("feedback-round-trip");
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let log = dir.join("feedback.jsonl");

    feedback::record_label(&log, "hi", "local").expect("label should record");
    feedback::record_label(&log, "Prove theorem ∑", "cloud").expect("label should record");

    let rows = feedback::read_labels(&log).expect("labels should read");

    assert_eq!(
        rows,
        vec![
            feedback::LabelRow {
                text: "hi".to_string(),
                label: "local".to_string(),
            },
            feedback::LabelRow {
                text: "Prove theorem ∑".to_string(),
                label: "cloud".to_string(),
            },
        ]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn feedback_absent_log_is_empty() {
    let dir = unique_temp_dir("feedback-absent");
    let rows = feedback::read_labels(dir.join("missing.jsonl")).expect("absent log should read");

    assert!(rows.is_empty());
}

#[test]
fn feedback_rejects_empty_text_or_label_with_python_messages() {
    let dir = unique_temp_dir("feedback-invalid");
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let log = dir.join("feedback.jsonl");

    let err = feedback::record_label(&log, "", "local").expect_err("empty text should fail");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(err.to_string(), "feedback needs a non-empty prompt text");

    let err = feedback::record_label(&log, "hi", "").expect_err("empty label should fail");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(err.to_string(), "feedback needs a non-empty label");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn feedback_json_line_matches_python_format_and_preserves_non_ascii() {
    let dir = unique_temp_dir("feedback-json-line");
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let log = dir.join(feedback::DEFAULT_LOG);

    feedback::record_label(&log, "café ∑", "cloud").expect("label should record");

    let text = std::fs::read_to_string(&log).expect("log should read");
    assert_eq!(text, "{\"text\": \"café ∑\", \"label\": \"cloud\"}\n");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn explain_score_breaks_down_contributions_matching_python() {
    // Ground truth from wayfinder_router.complexity.explain_score with DEFAULT_WEIGHTS.
    let prompt = "# Task\n\nProve that the square root of 2 is irrational.\n\n## Steps\n\n- derive a contradiction\n- show it must be exact\n\nSee [proof](http://example.com/proof). Is it optimal?\n";
    // (name, value, normalized, weight, contribution) in FEATURE_ORDER order.
    let expected: [(&str, usize, f64, f64, f64); 11] = [
        ("word_count", 28, 0.07, 3.0, 0.0191),
        ("heading_count", 2, 0.25, 1.5, 0.0341),
        ("max_heading_depth", 2, 0.5, 1.0, 0.0455),
        ("list_item_count", 2, 0.1333, 2.0, 0.0242),
        ("link_count", 1, 0.1, 1.0, 0.0091),
        ("code_block_count", 0, 0.0, 1.5, 0.0),
        ("table_row_count", 0, 0.0, 1.0, 0.0),
        ("reasoning_term_count", 7, 1.0, 0.0, 0.0),
        ("math_symbol_count", 0, 0.0, 0.0, 0.0),
        ("constraint_term_count", 1, 0.3333, 0.0, 0.0),
        ("question_count", 1, 0.3333, 0.0, 0.0),
    ];

    let contributions = explain_score(&extract_features(prompt), DEFAULT_WEIGHTS);
    assert_eq!(contributions.len(), FEATURE_ORDER.len());
    for (actual, (name, value, normalized, weight, contribution)) in
        contributions.iter().zip(expected)
    {
        assert_eq!(actual.name, name);
        assert_eq!(actual.value, value);
        assert!((actual.normalized - normalized).abs() < 1e-9);
        assert!((actual.weight - weight).abs() < 1e-9);
        assert!((actual.contribution - contribution).abs() < 1e-9);
    }
    // The breakdown spans every weighted feature, in order.
    let names: Vec<&str> = contributions.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, FEATURE_ORDER);
}

#[test]
fn feature_order_and_defaults_stay_schema_compatible() {
    assert_eq!(wayfinder_internal_core::SCORING_SCHEMA_VERSION, "3");
    assert_eq!(DEFAULT_THRESHOLD, 0.5);
    assert_eq!(
        FEATURE_ORDER,
        [
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
        ]
    );
}
