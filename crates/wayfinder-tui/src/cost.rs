//! Session cost accounting ported from the Python TUI (`SessionCost`, `account_turn`,
//! `cost_summary`) plus the glue that folds each turn into the persisted
//! [`SavingsLedger`] so `/cost` can show savings that accrue across sessions
//! (WF-DESIGN-0007).

use std::path::{Path, PathBuf};

use wayfinder_internal_core::pricing::{turn_cost, Date, SavingsLedger};

/// A running tally of model calls and their estimated cost vs always-cloud.
///
/// Mirrors the Python `SessionCost`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionCost {
    pub calls: usize,
    pub local: usize,
    pub spent: f64,
    pub saved: f64,
    /// A turn had `cost_per_1k` for both the chosen and cloud arms.
    pub priced: bool,
}

/// Fold one model call into `tally`: spend, and savings vs routing it all to cloud.
///
/// Mirrors the Python `account_turn`. `spent` and `saved` only accumulate when both the
/// chosen and cloud arms are priced; `units` are thousands of tokens.
pub fn account_turn(
    tally: &mut SessionCost,
    is_local: bool,
    tokens: usize,
    chosen_cost: Option<f64>,
    cloud_cost: Option<f64>,
) {
    tally.calls += 1;
    if is_local {
        tally.local += 1;
    }
    if let (Some(chosen), Some(cloud)) = (chosen_cost, cloud_cost) {
        tally.priced = true;
        let units = tokens as f64 / 1000.0;
        tally.spent += chosen * units;
        tally.saved += ((cloud - chosen) * units).max(0.0);
    }
}

/// The footer tally line, or `""` before any model call this session.
///
/// Mirrors the Python `cost_summary`.
pub fn cost_summary(tally: &SessionCost) -> String {
    if tally.calls == 0 {
        return String::new();
    }
    let mut summary = format!("{}/{} local", tally.local, tally.calls);
    if tally.priced {
        summary.push_str(&format!(" · ~${:.4} saved", tally.saved));
    }
    summary
}

/// Where the chat's savings ledger persists: alongside saved threads (WF-DESIGN-0007).
///
/// Mirrors the Python `_savings_path`. The caller supplies the threads/data dir (once the
/// core `threads_dir()` helper lands it is the source), so the data-dir resolution stays in
/// one place rather than being duplicated here.
pub fn savings_path(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir.as_ref().join("savings.json")
}

/// Load the persisted savings ledger, or start a fresh (unpriced) one.
///
/// Mirrors the Python `_load_ledger`: a missing or unreadable report is not an error, it
/// just means there is nothing to accrue against yet.
pub fn load_ledger(data_dir: impl AsRef<Path>) -> SavingsLedger {
    SavingsLedger::load(savings_path(data_dir)).unwrap_or_else(|_| SavingsLedger::new(false))
}

/// Fold a turn into the persisted `ledger` so `/cost` can show periods.
///
/// Mirrors the ledger half of the Python `_account` glue. `route` names the chosen arm and
/// `baseline` the always-cloud arm; an unpriced turn (either arm `None`) still records its
/// tokens but contributes no savings and leaves the ledger unpriced.
pub fn fold_turn(
    ledger: &mut SavingsLedger,
    tokens: usize,
    chosen_cost: Option<f64>,
    cloud_cost: Option<f64>,
    route: &str,
    baseline: &str,
    when: Date,
) {
    let priced = chosen_cost.is_some() && cloud_cost.is_some();
    let costs = [
        (route.to_string(), chosen_cost.unwrap_or(0.0)),
        (baseline.to_string(), cloud_cost.unwrap_or(0.0)),
    ];
    let tc = turn_cost(route, tokens, 0, costs, true, Some(baseline));
    if priced {
        ledger.priced = true;
    }
    ledger.record(&tc, when);
}
