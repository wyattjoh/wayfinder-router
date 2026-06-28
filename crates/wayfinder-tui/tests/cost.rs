use wayfinder_internal_core::pricing::{Date, SavingsLedger};
use wayfinder_internal_tui::{account_turn, cost_summary, fold_turn, savings_path, SessionCost};

#[test]
fn account_turn_counts_calls_and_local() {
    let mut tally = SessionCost::default();

    account_turn(&mut tally, true, 2000, None, None);
    account_turn(&mut tally, false, 2000, None, None);

    assert_eq!(tally.calls, 2);
    assert_eq!(tally.local, 1);
    assert!(!tally.priced, "unpriced turns leave the tally unpriced");
    assert_eq!(tally.spent, 0.0);
    assert_eq!(tally.saved, 0.0);
}

#[test]
fn account_turn_prices_only_with_both_costs() {
    let mut tally = SessionCost::default();

    // One arm priced is not enough to mark the tally priced or move the dollars.
    account_turn(&mut tally, true, 2000, Some(0.5), None);
    account_turn(&mut tally, true, 2000, None, Some(2.0));
    assert!(!tally.priced);
    assert_eq!(tally.spent, 0.0);
    assert_eq!(tally.saved, 0.0);

    // Both arms priced: 2000 tokens => 2.0 units; spend 0.5, save (2.0 - 0.5).
    account_turn(&mut tally, true, 2000, Some(0.5), Some(2.0));
    assert!(tally.priced);
    assert_eq!(tally.spent, 1.0);
    assert_eq!(tally.saved, 3.0);
}

#[test]
fn account_turn_never_records_negative_savings() {
    let mut tally = SessionCost::default();

    // Chosen dearer than cloud: spend accrues, savings clamp to zero.
    account_turn(&mut tally, false, 1000, Some(2.0), Some(0.5));

    assert!(tally.priced);
    assert_eq!(tally.spent, 2.0);
    assert_eq!(tally.saved, 0.0);
}

#[test]
fn cost_summary_is_empty_before_any_call() {
    assert_eq!(cost_summary(&SessionCost::default()), "");
}

#[test]
fn cost_summary_shows_mix_then_savings() {
    let mut tally = SessionCost::default();

    account_turn(&mut tally, true, 0, None, None);
    assert_eq!(cost_summary(&tally), "1/1 local");

    account_turn(&mut tally, false, 2000, Some(0.5), Some(2.0));
    assert_eq!(cost_summary(&tally), "1/2 local · ~$3.0000 saved");
}

#[test]
fn savings_path_sits_next_to_the_threads_dir() {
    assert_eq!(savings_path("/data/threads"), {
        let mut path = std::path::PathBuf::from("/data/threads");
        path.push("savings.json");
        path
    });
}

#[test]
fn fold_turn_records_priced_savings() {
    let day = Date::new(2026, 6, 28);
    let mut ledger = SavingsLedger::new(false);

    // 2000 tokens => 2.0 units; baseline cloud 2.0 vs chosen local 0.5 => 3.0 saved.
    fold_turn(
        &mut ledger,
        2000,
        Some(0.5),
        Some(2.0),
        "local",
        "cloud",
        day,
    );

    let period = ledger.period(None, Some(day));
    assert!(ledger.priced, "a priced turn marks the ledger priced");
    assert!(period.priced);
    assert_eq!(period.requests, 1);
    assert_eq!(period.saved, 3.0);
}

#[test]
fn fold_turn_records_unpriced_turn_without_savings() {
    let day = Date::new(2026, 6, 28);
    let mut ledger = SavingsLedger::new(false);

    fold_turn(&mut ledger, 2000, None, None, "local", "cloud", day);

    let period = ledger.period(None, Some(day));
    assert!(
        !ledger.priced,
        "an unpriced turn leaves the ledger unpriced"
    );
    assert!(!period.priced);
    assert_eq!(period.requests, 1);
    assert_eq!(period.saved, 0.0);
}
