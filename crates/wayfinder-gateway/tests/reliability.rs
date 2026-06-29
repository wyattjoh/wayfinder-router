use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value as JsonValue;
use wayfinder_internal_gateway::reliability::{
    delivery_plan, failover_candidates, is_retryable, precheck_ok, retry_delays, CircuitBreaker,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("gateway crate lives under crates/wayfinder-gateway")
        .to_path_buf()
}

fn fixture(path: &str) -> JsonValue {
    let path = repo_root().join("tests/fixtures/contracts").join(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()))
}

#[test]
fn is_retryable_matches_python_classification() {
    let expected = fixture("reliability/retry-failover.json");

    for status in expected["retryable"].as_array().unwrap() {
        let status = status.as_u64().map(|value| value as u16);
        assert!(is_retryable(status));
    }
    for status in expected["not_retryable"].as_array().unwrap() {
        assert!(!is_retryable(Some(status.as_u64().unwrap() as u16)));
    }
}

#[test]
fn retry_delays_match_python_backoff_schedule() {
    let expected = fixture("reliability/retry-failover.json");
    let full = retry_delays(
        expected["retry_delay_case"]["retries"].as_u64().unwrap() as usize,
        Duration::from_millis(expected["retry_delay_case"]["base_ms"].as_u64().unwrap()),
        Duration::from_millis(expected["retry_delay_case"]["cap_ms"].as_u64().unwrap()),
        || 1.0,
    );
    assert_eq!(
        full.iter()
            .map(|duration| duration.as_millis() as u64)
            .collect::<Vec<_>>(),
        expected["retry_delay_case"]["expected_ms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap())
            .collect::<Vec<_>>()
    );
    let zero = retry_delays(
        3,
        Duration::from_millis(200),
        Duration::from_secs(5),
        || 0.0,
    );
    assert_eq!(zero, vec![Duration::ZERO, Duration::ZERO, Duration::ZERO]);
    assert!(retry_delays(
        0,
        Duration::from_millis(200),
        Duration::from_secs(5),
        || 1.0
    )
    .is_empty());
}

#[test]
fn breaker_opens_after_threshold_then_probes_after_cooldown() {
    let clock = Cell::new(Duration::ZERO);
    let mut breaker = CircuitBreaker::new(3, Duration::from_secs(30));
    assert!(breaker.allow("cloud", clock.get()));

    breaker.record("cloud", false, clock.get());
    breaker.record("cloud", false, clock.get());
    assert!(breaker.allow("cloud", clock.get()));

    breaker.record("cloud", false, clock.get());
    assert!(breaker.is_open("cloud", clock.get()));
    clock.set(Duration::from_secs(29));
    assert!(!breaker.allow("cloud", clock.get()));
    clock.set(Duration::from_secs(30));
    assert!(breaker.allow("cloud", clock.get()));
}

#[test]
fn breaker_success_closes_and_probe_failure_reopens() {
    let clock = Cell::new(Duration::ZERO);
    let mut breaker = CircuitBreaker::new(2, Duration::from_secs(10));

    breaker.record("x", false, clock.get());
    breaker.record("x", false, clock.get());
    assert!(breaker.is_open("x", clock.get()));

    clock.set(Duration::from_secs(10));
    assert!(breaker.allow("x", clock.get()));
    breaker.record("x", false, clock.get());
    assert!(breaker.is_open("x", clock.get()));

    clock.set(Duration::from_secs(20));
    assert!(breaker.allow("x", clock.get()));
    breaker.record("x", true, clock.get());
    assert!(breaker.allow("x", clock.get()));
    assert!(!breaker.is_open("x", clock.get()));
}

#[test]
fn delivery_plan_orders_dedups_and_filters_targets() {
    let mut breaker = CircuitBreaker::new(1, Duration::from_secs(999));
    assert_eq!(
        delivery_plan(
            "cloud",
            ["cloud", "cloud-2", "local"],
            Some(&breaker),
            |_| true
        ),
        vec!["cloud".to_owned(), "cloud-2".to_owned(), "local".to_owned()]
    );

    breaker.record("cloud", false, Duration::ZERO);
    assert_eq!(
        delivery_plan("cloud", ["cloud-2"], Some(&breaker), |name| name
            != "cloud-2"),
        Vec::<String>::new()
    );
}

#[test]
fn failover_candidates_and_precheck_match_python() {
    let expected = fixture("reliability/retry-failover.json");
    for case in expected["failover_cases"].as_array().unwrap() {
        let ladder = case["ladder"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        let actual = failover_candidates(
            case["chosen"].as_str().unwrap(),
            ladder,
            case["policy"].as_str().unwrap(),
        );
        let expected = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    for case in expected["precheck_cases"].as_array().unwrap() {
        assert_eq!(
            precheck_ok(
                case["estimated_tokens"].as_u64().unwrap() as usize,
                case["context_window"].as_u64().map(|value| value as usize),
            ),
            case["expected"].as_bool().unwrap()
        );
    }
}
