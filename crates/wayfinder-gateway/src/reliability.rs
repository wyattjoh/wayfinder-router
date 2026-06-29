use std::collections::BTreeMap;
use std::time::Duration;

pub const FAILOVER_POLICIES: [&str; 3] = ["same-tier", "degrade", "escalate"];

const RETRYABLE_STATUS: [u16; 5] = [429, 500, 502, 503, 504];

pub fn is_retryable(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(status) => RETRYABLE_STATUS.contains(&status),
    }
}

pub fn retry_delays(
    retries: usize,
    base: Duration,
    cap: Duration,
    mut rng: impl FnMut() -> f64,
) -> Vec<Duration> {
    let mut delays = Vec::with_capacity(retries);
    for i in 0..retries {
        let factor = 2_f64.powi(i as i32);
        let slot = duration_min(cap, base.mul_f64(factor));
        delays.push(slot.mul_f64(rng().clamp(0.0, 1.0)));
    }
    delays
}

fn duration_min(left: Duration, right: Duration) -> Duration {
    if left <= right {
        left
    } else {
        right
    }
}

#[derive(Clone, Debug)]
pub struct CircuitBreaker {
    threshold: usize,
    cooldown: Duration,
    fails: BTreeMap<String, usize>,
    opened_at: BTreeMap<String, Duration>,
}

impl CircuitBreaker {
    pub fn new(threshold: usize, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            fails: BTreeMap::new(),
            opened_at: BTreeMap::new(),
        }
    }

    pub fn allow(&self, target: &str, now: Duration) -> bool {
        let Some(opened) = self.opened_at.get(target) else {
            return true;
        };
        now.checked_sub(*opened)
            .map(|elapsed| elapsed >= self.cooldown)
            .unwrap_or(false)
    }

    pub fn is_open(&self, target: &str, now: Duration) -> bool {
        !self.allow(target, now)
    }

    pub fn record(&mut self, target: &str, ok: bool, now: Duration) {
        if ok {
            self.fails.remove(target);
            self.opened_at.remove(target);
            return;
        }
        let count = self.fails.get(target).copied().unwrap_or(0) + 1;
        self.fails.insert(target.to_owned(), count);
        if count >= self.threshold {
            self.opened_at.insert(target.to_owned(), now);
        }
    }
}

pub fn delivery_plan(
    primary: &str,
    fallbacks: impl IntoIterator<Item = impl AsRef<str>>,
    breaker: Option<&CircuitBreaker>,
    allow: impl Fn(&str) -> bool,
) -> Vec<String> {
    delivery_plan_at(primary, fallbacks, breaker, Duration::ZERO, allow)
}

pub fn delivery_plan_at(
    primary: &str,
    fallbacks: impl IntoIterator<Item = impl AsRef<str>>,
    breaker: Option<&CircuitBreaker>,
    now: Duration,
    allow: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut targets = vec![primary.to_owned()];
    targets.extend(fallbacks.into_iter().map(|item| item.as_ref().to_owned()));
    let mut plan = Vec::new();
    for target in targets {
        if plan.iter().any(|item| item == &target) {
            continue;
        }
        if breaker.is_some_and(|breaker| !breaker.allow(&target, now)) {
            continue;
        }
        if !allow(&target) {
            continue;
        }
        plan.push(target);
    }
    plan
}

pub fn failover_candidates(
    chosen: &str,
    ladder: impl IntoIterator<Item = impl AsRef<str>>,
    policy: &str,
) -> Vec<String> {
    let seq = ladder
        .into_iter()
        .map(|item| item.as_ref().to_owned())
        .collect::<Vec<_>>();
    let Some(idx) = seq.iter().position(|item| item == chosen) else {
        return Vec::new();
    };
    match policy {
        "degrade" => seq[..idx].iter().rev().cloned().collect(),
        "escalate" => seq[idx + 1..].to_vec(),
        _ => Vec::new(),
    }
}

pub fn precheck_ok(estimated_tokens: usize, context_window: Option<usize>) -> bool {
    context_window.is_none_or(|window| estimated_tokens <= window)
}
