//! On-disk conversation persistence for the terminal chat (WF-ADR-0030).
//!
//! The disk sibling of the demo's localStorage threads (WF-ADR-0026): a thread is
//! the saved transcript, JSON on the user's own disk. Titles come from the first user
//! message, with no model call to name a chat (WF-ADR-0026). The gateway stays
//! stateless (WF-ADR-0022); this is purely client-side and pure/stdlib, so it is
//! testable without a terminal. Mirrors `wayfinder_router/threads.py`.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// First user message, truncated; matches the Python `title_from` default.
const TITLE_LIMIT: usize = 50;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub messages: Vec<Value>,
}

/// Where conversations are stored: `$WAYFINDER_DATA_DIR` or the XDG data home.
pub fn threads_dir() -> PathBuf {
    if let Some(base) = env::var_os("WAYFINDER_DATA_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(base).join("threads");
    }
    let root = match env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => home_dir().join(".local").join("share"),
    };
    root.join("wayfinder").join("threads")
}

/// A fresh, empty thread with a sortable, collision-resistant id.
pub fn new_thread() -> Thread {
    let now = now_iso();
    let suffix: u64 = rand::random();
    Thread {
        id: format!("{}-{suffix:016x}", now_stamp()),
        title: String::new(),
        created: now.clone(),
        updated: now,
        messages: Vec::new(),
    }
}

/// The first user message, whitespace-collapsed and truncated, with no model call.
pub fn title_from(messages: &[Value], limit: usize) -> String {
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        let truncated = text.chars().take(limit).collect::<String>();
        if text.chars().count() > limit {
            return format!("{truncated}\u{2026}");
        }
        return truncated;
    }
    "(empty)".to_string()
}

/// Write `thread` to `<dir>/<id>.json`, refreshing its title and updated time.
pub fn save_thread(thread: &mut Thread, dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let now = now_iso();
    if thread.created.is_empty() {
        thread.created = now.clone();
    }
    thread.updated = now;
    thread.title = title_from(&thread.messages, TITLE_LIMIT);
    let path = dir.join(format!("{}.json", thread.id));
    let encoded = serde_json::to_string_pretty(thread).map_err(invalid_data)?;
    fs::write(&path, format!("{encoded}\n"))?;
    Ok(path)
}

/// Read a single thread from its JSON file.
pub fn load_thread(path: &Path) -> io::Result<Thread> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(invalid_data)
}

/// All saved threads, most-recently-updated first; unreadable files are skipped.
pub fn list_threads(dir: &Path) -> io::Result<Vec<Thread>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut threads = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Ok(thread) = load_thread(&path) {
            threads.push(thread);
        }
    }
    threads.sort_by(|a, b| b.updated.cmp(&a.updated));
    Ok(threads)
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn invalid_data(err: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// `%Y-%m-%dT%H:%M:%SZ` for created/updated stamps.
fn now_iso() -> String {
    let (year, month, day, hour, minute, second) = utc_parts(now_secs());
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `%Y%m%dT%H%M%S` for the sortable id prefix.
fn now_stamp() -> String {
    let (year, month, day, hour, minute, second) = utc_parts(now_secs());
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}")
}

/// Split unix seconds into UTC calendar parts (Howard Hinnant's civil-from-days).
fn utc_parts(secs: u64) -> (i64, i64, i64, u64, u64, u64) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir() -> PathBuf {
        // A process-wide counter is the reliable discriminator: both tests run in
        // the same process (same pid), so a clock sample alone can collide under
        // parallel load and let one test's teardown delete another's fixtures.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "wayfinder-core-threads-{}-{seq}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = unique_temp_dir();
        let mut thread = new_thread();
        thread.messages = vec![
            json!({ "role": "user", "content": "What is DNS?" }),
            json!({ "role": "assistant", "content": "A naming system." }),
        ];

        let path = save_thread(&mut thread, &dir).expect("thread should save");
        let loaded = load_thread(&path).expect("thread should load");

        assert_eq!(thread, loaded);
        assert_eq!(loaded.title, "What is DNS?");
        assert_eq!(loaded.messages.len(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_threads_orders_by_updated_descending() {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).expect("temp dir should be created");
        for (id, updated) in [
            ("a", "2024-01-01T00:00:00Z"),
            ("b", "2024-03-01T00:00:00Z"),
            ("c", "2024-02-01T00:00:00Z"),
        ] {
            let thread = Thread {
                id: id.to_string(),
                title: id.to_string(),
                created: updated.to_string(),
                updated: updated.to_string(),
                messages: Vec::new(),
            };
            fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string(&thread).expect("thread should encode"),
            )
            .expect("thread file should write");
        }

        let listed = list_threads(&dir).expect("threads should list");
        let ids = listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>();

        assert_eq!(ids, vec!["b", "c", "a"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_thread_uses_iso_timestamps_and_random_suffix() {
        let thread = new_thread();
        assert!(thread.created.ends_with('Z'));
        assert_eq!(thread.created, thread.updated);
        let (stamp, suffix) = thread.id.split_once('-').expect("id has a suffix");
        assert_eq!(stamp.len(), 15); // YYYYMMDDTHHMMSS
        assert_eq!(suffix.len(), 16); // eight random bytes as hex
    }

    #[test]
    fn new_thread_ids_do_not_collide_in_a_burst() {
        let ids = (0..2000)
            .map(|_| new_thread().id)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(ids.len(), 2000);
    }
}
