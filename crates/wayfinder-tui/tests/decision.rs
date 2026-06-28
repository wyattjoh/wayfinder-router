use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use wayfinder_internal_tui::{decide, pin_label, resolve_target};

#[test]
fn low_score_prompt_routes_local() {
    let dir = clean_start_dir();
    let decision = decide("What is DNS?", &dir, None).expect("decide should score the prompt");

    assert!(decision.is_local, "a simple prompt should keep local");
    assert_eq!(decision.model, "local");
    assert_eq!(
        decision.targets,
        vec!["local".to_string(), "cloud".to_string()]
    );
    assert!(
        !decision.contributions.is_empty(),
        "explain_score should populate the why breakdown"
    );
}

#[test]
fn prefer_hosted_forces_cloud_route() {
    let dir = clean_start_dir();
    let decision = decide("What is DNS?", &dir, None).expect("decide should score the prompt");

    let (model, is_local) = resolve_target(Some("prefer-hosted"), &decision);
    assert_eq!(model, "cloud");
    assert!(!is_local, "a forced cloud route is never local");

    let (auto_model, auto_local) = resolve_target(None, &decision);
    assert_eq!(auto_model, decision.model);
    assert_eq!(auto_local, decision.is_local);
}

#[test]
fn threshold_override_changes_the_route() {
    let dir = clean_start_dir();
    let prompt = "Design a fault-tolerant distributed queue with exactly-once delivery.";

    // A cut at 1.0 lands everything below it in the local arm.
    let kept_local = decide(prompt, &dir, Some(1.0)).expect("decide should score the prompt");
    assert!(kept_local.is_local);
    assert_eq!(kept_local.model, "local");
    assert_eq!(kept_local.threshold, Some(1.0));

    // A cut at 0.0 escalates the same prompt to cloud.
    let forced_cloud = decide(prompt, &dir, Some(0.0)).expect("decide should score the prompt");
    assert!(!forced_cloud.is_local);
    assert_eq!(forced_cloud.model, "cloud");
}

#[test]
fn pin_label_maps_sentinels_to_human_names() {
    assert_eq!(pin_label(None), "auto");
    assert_eq!(pin_label(Some("prefer-local")), "local");
    assert_eq!(pin_label(Some("prefer-hosted")), "cloud");
    assert_eq!(pin_label(Some("gpt-4o")), "gpt-4o");
}

/// A fresh, empty directory so `decide` falls back to the default routing config rather
/// than discovering a `wayfinder-router.toml` from the workspace.
fn clean_start_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "wayfinder-decision-test-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    dir
}
