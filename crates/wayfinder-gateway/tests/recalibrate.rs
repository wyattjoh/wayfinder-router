use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;
use wayfinder_internal_core::feedback::record_label;
use wayfinder_internal_gateway::recalibrate::{recalibrate, DEFAULT_MIN_LABELS};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("gateway crate lives under crates/wayfinder-gateway")
        .to_path_buf()
}

fn fixture(path: &str) -> PathBuf {
    repo_root().join("tests/fixtures/contracts").join(path)
}

fn threshold_dataset() -> String {
    let text = std::fs::read_to_string(fixture("calibrate/threshold-accuracy.json"))
        .expect("threshold fixture should be readable");
    let value: JsonValue = serde_json::from_str(&text).expect("threshold fixture should be JSON");
    value["dataset"].as_str().unwrap().to_owned()
}

fn write_threshold_log(path: &Path) {
    std::fs::write(path, threshold_dataset()).expect("feedback log should be writable");
}

#[test]
fn skips_below_min_labels_without_touching_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("feedback.jsonl");
    let config = dir.path().join("wayfinder-router.toml");
    let original = "[routing]\nthreshold = 0.99\n";
    std::fs::write(&config, original).expect("config should be writable");
    record_label(&log, "hi", "local").expect("label should record");

    let result = recalibrate(&log, &config, "threshold", DEFAULT_MIN_LABELS)
        .expect("short log should not error");

    assert!(!result.written);
    assert_eq!(result.label_count, 1);
    assert_eq!(result.reason.as_deref(), Some("need >= 2 labels, have 1"));
    assert_eq!(
        std::fs::read_to_string(&config).expect("config should still exist"),
        original
    );
}

#[test]
fn skips_below_min_labels_without_creating_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("feedback.jsonl");
    let config = dir.path().join("wayfinder-router.toml");
    record_label(&log, "hi", "local").expect("label should record");

    let result = recalibrate(&log, &config, "threshold", 2).expect("short log should not error");

    assert!(!result.written);
    assert_eq!(result.reason.as_deref(), Some("need >= 2 labels, have 1"));
    assert!(!config.exists());
}

#[test]
fn rewrites_routing_and_preserves_gateway_block_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("feedback.jsonl");
    let config = dir.path().join("wayfinder-router.toml");
    write_threshold_log(&log);
    let gateway = r#"[gateway.models.local]
base_url = "http://l/v1"
model = "l"
api_key_env = "K1"

# keep this comment and integer spelling
[gateway.models.cloud]
base_url = "http://c/v1"
model = "c"
api_key_env = "K2""#;
    let old_config = format!("[routing]\nthreshold = 0.99\n\n{gateway}\n");
    std::fs::write(&config, old_config).expect("config should be writable");

    let result = recalibrate(&log, &config, "threshold", DEFAULT_MIN_LABELS).expect("recalibrate");
    let text = std::fs::read_to_string(&config).expect("config should be readable");

    assert!(result.written);
    assert!(text.starts_with("# recalibrated from feedback: "));
    assert!(text.contains("[[routing.tiers]]"));
    assert!(!text.contains("threshold = 0.99"));
    assert!(text.contains(gateway));
}

#[test]
fn emitted_config_matches_python_recalibrate_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("feedback.jsonl");
    let config = dir.path().join("wayfinder-router.toml");
    write_threshold_log(&log);
    std::fs::copy(
        fixture("gateway/gateway-config-roundtrip.expected.toml"),
        &config,
    )
    .expect("gateway fixture should copy");

    let result = recalibrate(&log, &config, "threshold", DEFAULT_MIN_LABELS).expect("recalibrate");
    let expected = std::fs::read_to_string(fixture("gateway/recalibrate.expected.toml")).unwrap();

    assert!(result.written);
    assert_eq!(result.toml.as_deref(), Some(expected.as_str()));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), expected);
}

#[test]
fn propagates_calibration_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("feedback.jsonl");
    let config = dir.path().join("wayfinder-router.toml");
    record_label(&log, "hi", "local").expect("label should record");
    record_label(&log, "hello", "local").expect("label should record");
    record_label(&log, "hey", "local").expect("label should record");

    let err = recalibrate(&log, &config, "threshold", DEFAULT_MIN_LABELS)
        .expect_err("one-label threshold calibration should fail");

    assert_eq!(
        err.to_string(),
        "threshold mode needs exactly two labels, found 1: ['local']"
    );
}

#[test]
fn propagates_gateway_config_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("feedback.jsonl");
    let config = dir.path().join("wayfinder-router.toml");
    write_threshold_log(&log);
    std::fs::write(
        &config,
        r#"[gateway.models.cloud]
base_url = "https://api.example.com/v1"
model = "big-model"
api_key_cmd = "op read op://Private/example/credential"
"#,
    )
    .expect("config should be writable");

    let err = recalibrate(&log, &config, "threshold", DEFAULT_MIN_LABELS).expect_err("bad gateway");

    assert_eq!(
        err.to_string(),
        format!(
            "{}: 'gateway.models.cloud.api_key_cmd' needs 'api_key_env' to name the variable it fills",
            config.display()
        )
    );
}
