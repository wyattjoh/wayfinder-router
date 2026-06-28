use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CHARS_PER_TOKEN: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageTokens {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub estimated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnCost {
    pub route: String,
    pub realized: f64,
    pub baseline: f64,
    pub savings: f64,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub estimated: bool,
}

pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        (text.chars().count() / CHARS_PER_TOKEN).max(1)
    }
}

pub fn price_table<I, K, L, M>(model_costs: I, tier_ladder: L) -> (BTreeMap<String, f64>, bool)
where
    I: IntoIterator<Item = (K, Option<f64>)>,
    K: AsRef<str>,
    L: IntoIterator<Item = M>,
    M: AsRef<str>,
{
    let costs: Vec<(String, Option<f64>)> = model_costs
        .into_iter()
        .map(|(name, cost)| (name.as_ref().to_string(), cost))
        .collect();
    let real = costs
        .iter()
        .filter_map(|(name, cost)| cost.map(|cost| (name.clone(), cost)))
        .collect::<BTreeMap<_, _>>();
    if !real.is_empty() {
        return (real, true);
    }

    let ladder = tier_ladder
        .into_iter()
        .map(|name| name.as_ref().to_string())
        .collect::<Vec<_>>();
    let ladder = if ladder.is_empty() {
        costs
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    } else {
        ladder
    };
    if ladder.is_empty() {
        return (BTreeMap::new(), false);
    }

    let low = 0.2;
    let high = 1.0;
    let step = (high - low) / (ladder.len().saturating_sub(1).max(1) as f64);
    let fallback = ladder
        .iter()
        .enumerate()
        .map(|(index, name)| (name.clone(), round_to(low + index as f64 * step, 3)))
        .collect();
    (fallback, false)
}

pub fn table_version<I, K>(costs: I) -> String
where
    I: IntoIterator<Item = (K, f64)>,
    K: AsRef<str>,
{
    let costs = costs
        .into_iter()
        .map(|(name, cost)| (name.as_ref().to_string(), cost))
        .collect::<BTreeMap<_, _>>();
    let mut parts = Vec::with_capacity(costs.len());
    for (name, cost) in costs {
        parts.push(format!("{}:{}", json_string(&name), json_number(cost)));
    }
    let blob = format!("{{{}}}", parts.join(","));
    let digest = Sha256::digest(blob.as_bytes());
    hex_lower(&digest)[..12].to_string()
}

pub fn usage_tokens(response: &Value, prompt_text: &str, completion_text: &str) -> UsageTokens {
    if let Some(usage) = response.get("usage").and_then(Value::as_object) {
        let prompt = int_field(usage, "prompt_tokens");
        let completion = int_field(usage, "completion_tokens");
        if let (Some(prompt_tokens), Some(completion_tokens)) = (prompt, completion) {
            return UsageTokens {
                prompt_tokens,
                completion_tokens,
                estimated: false,
            };
        }
        if let Some(total_tokens) = int_field(usage, "total_tokens") {
            let known = prompt.unwrap_or(0);
            return UsageTokens {
                prompt_tokens: known,
                completion_tokens: total_tokens.saturating_sub(known),
                estimated: false,
            };
        }
    }

    UsageTokens {
        prompt_tokens: estimate_tokens(prompt_text),
        completion_tokens: estimate_tokens(completion_text),
        estimated: true,
    }
}

pub fn turn_cost<I, K>(
    route: &str,
    prompt_tokens: usize,
    completion_tokens: usize,
    costs: I,
    estimated: bool,
    baseline: Option<&str>,
) -> TurnCost
where
    I: IntoIterator<Item = (K, f64)>,
    K: AsRef<str>,
{
    let costs = costs
        .into_iter()
        .map(|(name, cost)| (name.as_ref().to_string(), cost))
        .collect::<BTreeMap<_, _>>();
    let total_k = (prompt_tokens + completion_tokens) as f64 / 1000.0;
    let dearest = costs.values().copied().fold(0.0, f64::max);
    let baseline_per_1k = baseline
        .and_then(|name| costs.get(name).copied())
        .unwrap_or(dearest);
    let chosen_per_1k = costs.get(route).copied().unwrap_or(dearest);
    let realized = round_to(chosen_per_1k * total_k, 6);
    let base = round_to(baseline_per_1k * total_k, 6);

    TurnCost {
        route: route.to_string(),
        realized,
        baseline: base,
        savings: round_to(base - realized, 6),
        prompt_tokens,
        completion_tokens,
        estimated,
    }
}

/// A proleptic-Gregorian calendar day, the unit the ledger buckets turns by.
///
/// Stored as ISO `YYYY-MM-DD` keys (mirroring Python's `date.isoformat()`) and compared
/// by an ordinal day count for the period windows. No timezone is attached; callers work
/// in UTC, matching the Python ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl Date {
    pub fn new(year: i32, month: u32, day: u32) -> Date {
        Date { year, month, day }
    }

    /// Today in UTC, derived from the system clock without pulling in a date crate.
    pub fn today_utc() -> Date {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let (year, month, day) = civil_from_days((secs / 86_400) as i64);
        Date { year, month, day }
    }

    /// ISO `YYYY-MM-DD`, the on-disk bucket key.
    pub fn to_iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Parse an ISO `YYYY-MM-DD` bucket key back into a `Date`.
    pub fn from_iso(value: &str) -> Option<Date> {
        let mut parts = value.split('-');
        let year = parts.next()?.parse().ok()?;
        let month = parts.next()?.parse().ok()?;
        let day = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Date { year, month, day })
    }

    /// A monotonic day count used only for window cutoffs, so the epoch is arbitrary
    /// (days since 1970-01-01); only differences between dates are meaningful.
    pub fn to_ordinal(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }
}

/// One day's rolled-up cost figures: the same fields Python's `_empty_route` accumulates,
/// plus the count of turns whose tokens were estimated.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct DayBucket {
    n: u64,
    realized: f64,
    baseline: f64,
    savings: f64,
    tokens: u64,
    estimated_n: u64,
}

impl DayBucket {
    fn accumulate(&mut self, tc: &TurnCost) {
        self.n += 1;
        self.realized = round_to(self.realized + tc.realized, 6);
        self.baseline = round_to(self.baseline + tc.baseline, 6);
        self.savings = round_to(self.savings + tc.savings, 6);
        self.tokens += (tc.prompt_tokens + tc.completion_tokens) as u64;
        if tc.estimated {
            self.estimated_n += 1;
        }
    }

    fn fold(&mut self, other: &DayBucket) {
        self.n += other.n;
        self.realized = round_to(self.realized + other.realized, 6);
        self.baseline = round_to(self.baseline + other.baseline, 6);
        self.savings = round_to(self.savings + other.savings, 6);
        self.tokens += other.tokens;
        self.estimated_n += other.estimated_n;
    }
}

/// One period's savings report, the shape `/cost` renders per window.
#[derive(Clone, Debug, PartialEq)]
pub struct PeriodSummary {
    pub period_days: Option<i64>,
    pub priced: bool,
    pub requests: u64,
    pub estimated_requests: u64,
    pub tokens: u64,
    pub realized: f64,
    pub baseline: f64,
    pub saved: f64,
    pub saved_pct: f64,
}

/// Daily-bucket accumulator of realized/baseline/savings across sessions (WF-DESIGN-0007).
///
/// In-memory, bounded to `max_days` (oldest buckets are dropped). `priced` records whether
/// the figures are dollars (real `cost_per_1k`) or relative units, so a loaded report is
/// never mistaken for currency. Persisted as JSON for cross-session continuity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavingsLedger {
    pub max_days: usize,
    pub priced: bool,
    days: BTreeMap<String, DayBucket>,
}

impl Default for SavingsLedger {
    fn default() -> SavingsLedger {
        SavingsLedger {
            max_days: 400,
            priced: true,
            days: BTreeMap::new(),
        }
    }
}

impl SavingsLedger {
    pub fn new(priced: bool) -> SavingsLedger {
        SavingsLedger {
            priced,
            ..SavingsLedger::default()
        }
    }

    /// Fold a turn into its day bucket, then drop buckets beyond `max_days`.
    pub fn record(&mut self, tc: &TurnCost, when: Date) {
        self.days.entry(when.to_iso()).or_default().accumulate(tc);
        self.prune();
    }

    fn prune(&mut self) {
        // BTreeMap keys are ISO dates, which sort chronologically, so the oldest are first.
        while self.days.len() > self.max_days {
            let Some(oldest) = self.days.keys().next().cloned() else {
                break;
            };
            self.days.remove(&oldest);
        }
    }

    /// Aggregate the last `days` buckets (`None` = all-time) relative to `today`
    /// (`None` = current UTC day) into a report. The window matches Python: `today=1`,
    /// `7`, `30` keep buckets whose ordinal day is `>= today - (days - 1)`.
    pub fn period(&self, days: Option<i64>, today: Option<Date>) -> PeriodSummary {
        let cutoff =
            days.map(|window| today.unwrap_or_else(Date::today_utc).to_ordinal() - (window - 1));

        let mut agg = DayBucket::default();
        for (key, bucket) in &self.days {
            if let Some(cutoff) = cutoff {
                match Date::from_iso(key) {
                    Some(date) if date.to_ordinal() >= cutoff => {}
                    _ => continue,
                }
            }
            agg.fold(bucket);
        }

        let saved_pct = if agg.baseline != 0.0 {
            round_to(100.0 * agg.savings / agg.baseline, 1)
        } else {
            0.0
        };
        PeriodSummary {
            period_days: days,
            priced: self.priced,
            requests: agg.n,
            estimated_requests: agg.estimated_n,
            tokens: agg.tokens,
            realized: round_to(agg.realized, 6),
            baseline: round_to(agg.baseline, 6),
            saved: round_to(agg.savings, 6),
            saved_pct,
        }
    }

    /// Serialize to JSON, writing through a temp file so a crash never leaves a half-written
    /// report (atomic rename on POSIX), mirroring the Python `save`.
    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Load a ledger previously written by [`SavingsLedger::save`].
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<SavingsLedger> {
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data).map_err(std::io::Error::other)
    }
}

fn int_field(map: &Map<String, Value>, key: &str) -> Option<usize> {
    map.get(key)
        .and_then(Value::as_i64)
        .and_then(|value| usize::try_from(value).ok())
}

fn round_to(value: f64, places: i32) -> f64 {
    let factor = 10_f64.powi(places);
    (value * factor).round() / factor
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization should not fail")
}

fn json_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        let mut text = format!("{value}");
        if text.contains('e') {
            text = format!("{value:?}");
        }
        text
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Days from 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year } as i64;
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let yoe = year - era * 400; // [0, 399]
    let month = month as i64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: a civil date from a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (
        (if month <= 2 { year + 1 } else { year }) as i32,
        month,
        day,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(savings: f64, estimated: bool) -> TurnCost {
        TurnCost {
            route: "fast".to_string(),
            realized: 1.0,
            baseline: 1.0 + savings,
            savings,
            prompt_tokens: 10,
            completion_tokens: 10,
            estimated,
        }
    }

    #[test]
    fn date_ordinal_round_trips_and_measures_gaps() {
        let today = Date::new(2026, 6, 28);
        assert_eq!(Date::from_iso(&today.to_iso()), Some(today));
        assert_eq!(today.to_ordinal() - Date::new(2026, 6, 25).to_ordinal(), 3);
        assert_eq!(today.to_ordinal() - Date::new(2026, 4, 29).to_ordinal(), 60);
    }

    #[test]
    fn period_windows_aggregate_requests_and_saved() {
        let today = Date::new(2026, 6, 28);
        let mut ledger = SavingsLedger::default();
        ledger.record(&turn(0.5, false), today); // today
        ledger.record(&turn(0.25, false), Date::new(2026, 6, 25)); // 3 days ago
        ledger.record(&turn(0.1, false), Date::new(2026, 6, 18)); // 10 days ago
        ledger.record(&turn(1.0, true), Date::new(2026, 4, 29)); // 60 days ago

        let day = ledger.period(Some(1), Some(today));
        assert_eq!(day.requests, 1);
        assert_eq!(day.saved, 0.5);

        let week = ledger.period(Some(7), Some(today));
        assert_eq!(week.requests, 2);
        assert_eq!(week.saved, 0.75);

        let month = ledger.period(Some(30), Some(today));
        assert_eq!(month.requests, 3);
        assert_eq!(month.saved, 0.85);

        let all = ledger.period(None, Some(today));
        assert_eq!(all.requests, 4);
        assert_eq!(all.saved, 1.85);
        assert_eq!(all.estimated_requests, 1);
        assert_eq!(all.tokens, 80);
    }

    #[test]
    fn prune_drops_oldest_buckets_past_max_days() {
        let mut ledger = SavingsLedger {
            max_days: 2,
            ..SavingsLedger::default()
        };
        ledger.record(&turn(0.1, false), Date::new(2026, 1, 1));
        ledger.record(&turn(0.1, false), Date::new(2026, 1, 2));
        ledger.record(&turn(0.1, false), Date::new(2026, 1, 3));

        assert_eq!(ledger.days.len(), 2);
        assert!(!ledger.days.contains_key("2026-01-01"));
        assert!(ledger.days.contains_key("2026-01-03"));
    }

    #[test]
    fn save_load_round_trips_including_priced_flag() {
        let mut ledger = SavingsLedger::new(false);
        ledger.record(&turn(0.5, false), Date::new(2026, 6, 28));
        ledger.record(&turn(0.25, true), Date::new(2026, 6, 25));

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("wayfinder-savings-{nanos}.json"));

        ledger.save(&path).expect("save should succeed");
        let loaded = SavingsLedger::load(&path).expect("load should succeed");
        std::fs::remove_file(&path).ok();

        assert!(!loaded.priced);
        assert_eq!(loaded, ledger);
    }
}
