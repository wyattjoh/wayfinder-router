use std::cell::Cell;
use std::time::Duration;

use wayfinder_internal_gateway::reliability::{
    delivery_plan, failover_candidates, is_retryable, precheck_ok, retry_delays, CircuitBreaker,
};

#[test]
fn is_retryable_matches_python_classification() {
    assert!(is_retryable(None));
    for status in [429, 500, 502, 503, 504] {
        assert!(is_retryable(Some(status)));
    }
    for status in [200, 400, 401, 403, 404, 422] {
        assert!(!is_retryable(Some(status)));
    }
}

#[test]
fn retry_delays_match_python_backoff_schedule() {
    let full = retry_delays(
        4,
        Duration::from_millis(200),
        Duration::from_secs(1),
        || 1.0,
    );
    assert_eq!(
        full,
        vec![
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(800),
            Duration::from_secs(1),
        ]
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
    let ladder = ["local", "mid", "cloud"];
    assert!(failover_candidates("mid", ladder, "same-tier").is_empty());
    assert_eq!(failover_candidates("mid", ladder, "degrade"), vec!["local"]);
    assert_eq!(
        failover_candidates("mid", ladder, "escalate"),
        vec!["cloud"]
    );
    assert_eq!(
        failover_candidates("local", ladder, "escalate"),
        vec!["mid", "cloud"]
    );
    assert_eq!(
        failover_candidates("cloud", ladder, "degrade"),
        vec!["mid", "local"]
    );
    assert!(failover_candidates("ghost", ["local", "cloud"], "degrade").is_empty());

    assert!(precheck_ok(500, None));
    assert!(precheck_ok(500, Some(1000)));
    assert!(!precheck_ok(1500, Some(1000)));
}
