//! Precedence of the `WAYFINDER_CONFIG` override over the working-directory walk-up.
//!
//! These cases share one test on purpose. `WAYFINDER_CONFIG` is process-global and changes
//! how *every* config load in the process resolves, so setting it from several tests running
//! in parallel would make each of them depend on the others' timing. One sequential test in
//! its own binary keeps the override contained.

use std::fs;

use wayfinder_internal_core::config::{find_config_file, CONFIG_FILE, CONFIG_PATH_ENV};

#[test]
fn config_path_override_takes_precedence_over_the_walk_up() {
    let root = tempfile::tempdir().expect("temp dir");
    let nested = root.path().join("nested/deeper");
    fs::create_dir_all(&nested).expect("nested dirs");

    // A config above the start dir, which the walk-up would find.
    let walked = root.path().join(CONFIG_FILE);
    fs::write(&walked, "[routing]\nthreshold = 0.5\n").expect("walk-up config");
    // And an unrelated one somewhere else entirely, only reachable by naming it.
    let elsewhere = root.path().join("elsewhere.toml");
    fs::write(&elsewhere, "[routing]\nthreshold = 0.9\n").expect("override config");

    // Unset: the walk-up is unchanged.
    std::env::remove_var(CONFIG_PATH_ENV);
    assert_eq!(
        find_config_file(&nested).as_deref(),
        Some(walked.canonicalize().expect("walked path").as_path()),
        "with no override the nearest config at or above the start dir wins"
    );

    // Set and present: the override wins, even though a walk-up config exists.
    std::env::set_var(CONFIG_PATH_ENV, &elsewhere);
    assert_eq!(
        find_config_file(&nested).as_deref(),
        Some(elsewhere.as_path()),
        "an explicit override outranks any config the walk-up would reach"
    );

    // Set but missing: None, never a silent fall back to the walk-up. A gateway told to
    // load a specific file should say that file is missing, not quietly load another one.
    std::env::set_var(CONFIG_PATH_ENV, root.path().join("not-there.toml"));
    assert_eq!(
        find_config_file(&nested),
        None,
        "a missing override resolves to None rather than walking up to a different config"
    );

    // Empty is treated as unset, so an exported-but-blank var does not disable discovery.
    std::env::set_var(CONFIG_PATH_ENV, "");
    assert_eq!(
        find_config_file(&nested).as_deref(),
        Some(walked.canonicalize().expect("walked path").as_path()),
        "an empty override is indistinguishable from an unset one"
    );

    std::env::remove_var(CONFIG_PATH_ENV);
}
