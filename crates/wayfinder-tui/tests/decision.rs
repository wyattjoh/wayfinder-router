use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use wayfinder_internal_gateway::RelayMessage;
use wayfinder_internal_tui::{
    decide, decide_with_context, pin_label, resolve_target, DecisionContext,
};

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
fn context_scope_and_sticky_reach_local_decision() {
    let dir = clean_start_dir();
    let heavy = "# Plan\n\n## Steps\n\n".to_owned()
        + &(0..20)
            .map(|index| format!("- work through detailed subtask number {index}\n"))
            .collect::<String>();
    let messages = vec![
        RelayMessage::new("user", heavy.clone()),
        RelayMessage::new("user", "ok thanks"),
    ];
    let last_user = decide_with_context(
        "ok thanks",
        &dir,
        Some(1.0),
        DecisionContext {
            scope: "last_user".to_owned(),
            messages: messages.clone(),
            ..DecisionContext::default()
        },
    )
    .expect("last-user scope should score");
    let all = decide_with_context(
        "ok thanks",
        &dir,
        Some(1.0),
        DecisionContext {
            scope: "all".to_owned(),
            messages: messages.clone(),
            ..DecisionContext::default()
        },
    )
    .expect("all scope should score");
    let cut = (last_user.score + all.score) / 2.0;
    let plain = decide_with_context(
        "ok thanks",
        &dir,
        Some(cut),
        DecisionContext {
            scope: "last_user".to_owned(),
            messages: messages.clone(),
            ..DecisionContext::default()
        },
    )
    .expect("plain scoped decision should score");
    let sticky = decide_with_context(
        "ok thanks",
        &dir,
        Some(cut),
        DecisionContext {
            scope: "last_user".to_owned(),
            sticky: true,
            messages,
            ..DecisionContext::default()
        },
    )
    .expect("sticky scoped decision should score");

    assert!(all.score > last_user.score);
    assert!(plain.is_local);
    assert!(!sticky.is_local);
    assert_eq!(sticky.mode, "sticky");
    assert_eq!(sticky.text, "ok thanks");
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
