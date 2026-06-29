use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use wayfinder_internal_cli::run_output;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("cli crate lives under crates/wayfinder-cli")
        .to_path_buf()
}

fn fixture(path: &str) -> PathBuf {
    repo_root().join("tests/fixtures/contracts").join(path)
}

fn json_fixture(path: &str) -> JsonValue {
    let path = fixture(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()))
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    dir
}

fn replace_placeholders(args: &[JsonValue], dir: &Path, dataset: Option<&Path>) -> Vec<String> {
    args.iter()
        .map(|value| {
            let text = value.as_str().expect("argv values should be strings");
            match text {
                "<tempdir>" => dir.to_string_lossy().into_owned(),
                "<dataset>" => dataset
                    .expect("dataset placeholder needs dataset fixture")
                    .to_string_lossy()
                    .into_owned(),
                other => other.to_owned(),
            }
        })
        .collect()
}

fn write_dataset_from_fixture(case: &JsonValue, dir: &Path) -> Option<PathBuf> {
    let dataset_fixture = case.get("dataset_fixture")?.as_str().unwrap();
    let source = json_fixture(dataset_fixture);
    let path = dir.join("dataset.jsonl");
    std::fs::write(&path, source["dataset"].as_str().unwrap()).expect("dataset should write");
    Some(path)
}

fn write_config_from_case(case: &JsonValue, dir: &Path) {
    if let Some(config) = case.get("config").and_then(JsonValue::as_str) {
        std::fs::write(dir.join("wayfinder-router.toml"), config).expect("config should write");
    }
}

fn assert_contains_all(haystack: &str, needles: &JsonValue) {
    let Some(needles) = needles.as_array() else {
        return;
    };
    for needle in needles {
        let needle = needle.as_str().unwrap();
        assert!(
            haystack.contains(needle),
            "expected output to contain {needle:?}, got {haystack:?}"
        );
    }
}

#[test]
fn cli_commands_match_static_contract_fixture() {
    let fixture = json_fixture("cli/commands.json");

    for case in fixture["commands"].as_array().unwrap() {
        let dir = unique_temp_dir(case["name"].as_str().unwrap());
        write_config_from_case(case, &dir);
        let dataset = write_dataset_from_fixture(case, &dir);
        let args = replace_placeholders(case["argv"].as_array().unwrap(), &dir, dataset.as_deref());
        let stdin = case
            .get("stdin")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let expected = &case["expected"];
        let result = run_output(args, stdin);

        match expected["ok"].as_bool().unwrap() {
            true => {
                let output = result.unwrap_or_else(|err| {
                    panic!("{} should succeed: {err}", case["name"].as_str().unwrap())
                });
                assert_contains_all(&output.stdout, &expected["stdout_contains"]);
                assert_contains_all(&output.stderr, &expected["stderr_contains"]);
                if expected["stderr_empty"].as_bool().unwrap_or(false) {
                    assert!(output.stderr.is_empty());
                }
                if expected["verify_generated_key"].as_bool().unwrap_or(false) {
                    let key = output
                        .stdout
                        .lines()
                        .find(|line| line.starts_with("wf-"))
                        .expect("plaintext key should be printed");
                    let hash = output
                        .stdout
                        .lines()
                        .find_map(|line| line.strip_prefix("hash = \""))
                        .and_then(|line| line.strip_suffix('"'))
                        .expect("hash line should be printed");
                    assert!(wayfinder_internal_core::vkeys::verify(key, hash));
                }
            }
            false => {
                let err = result.unwrap_err();
                assert_eq!(
                    err.exit_code(),
                    expected["exit_code"].as_i64().unwrap() as i32
                );
                assert_contains_all(err.stdout(), &expected["stdout_contains"]);
                assert_contains_all(err.stderr(), &expected["stderr_contains"]);
                assert_contains_all(&err.to_string(), &expected["message_contains"]);
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
