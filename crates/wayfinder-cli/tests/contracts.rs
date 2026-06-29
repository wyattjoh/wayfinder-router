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

fn expected_text(expected: &JsonValue, name: &str) -> String {
    let lines = expected[format!("{name}_lines")]
        .as_array()
        .unwrap_or_else(|| panic!("expected {name}_lines fixture"));
    let mut text = lines
        .iter()
        .map(|value| value.as_str().expect("line should be a string"))
        .collect::<Vec<_>>()
        .join("\n");
    if expected[format!("{name}_trailing_newline")]
        .as_bool()
        .unwrap_or(false)
    {
        text.push('\n');
    }
    text
}

fn normalize_dynamic_text(text: &str, dir: &Path, generated: Option<(&str, &str)>) -> String {
    let mut paths = vec![dir.to_string_lossy().to_string()];
    if let Ok(canonical) = dir.canonicalize() {
        paths.push(canonical.to_string_lossy().to_string());
    }
    paths.sort_by_key(|path| std::cmp::Reverse(path.len()));

    let mut text = text.to_owned();
    for path in paths {
        text = text.replace(&path, "<tempdir>");
    }
    if let Some((key, hash)) = generated {
        text = text.replace(hash, "<generated-key-hash>");
        text = text.replace(key, "<generated-key>");
    }
    text
}

fn generated_key_material(stdout: &str) -> Option<(&str, &str)> {
    let key = stdout.lines().find(|line| line.starts_with("wf-"))?;
    let hash = stdout
        .lines()
        .find_map(|line| line.strip_prefix("hash = \""))
        .and_then(|line| line.strip_suffix('"'))?;
    Some((key, hash))
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
                let generated = generated_key_material(&output.stdout);
                if expected["verify_generated_key"].as_bool().unwrap_or(false) {
                    let (key, hash) = generated.expect("generated key material should print");
                    assert!(wayfinder_internal_core::vkeys::verify(key, hash));
                }

                if let Some(expected_json) = expected.get("stdout_json") {
                    let actual: JsonValue =
                        serde_json::from_str(&output.stdout).expect("stdout should be valid JSON");
                    assert_eq!(&actual, expected_json);
                } else {
                    assert_eq!(
                        normalize_dynamic_text(&output.stdout, &dir, generated),
                        expected_text(expected, "stdout"),
                        "{} stdout changed",
                        case["name"].as_str().unwrap()
                    );
                }
                assert_eq!(
                    normalize_dynamic_text(&output.stderr, &dir, generated),
                    expected_text(expected, "stderr"),
                    "{} stderr changed",
                    case["name"].as_str().unwrap()
                );
            }
            false => {
                let err = result.unwrap_err();
                assert_eq!(
                    err.exit_code(),
                    expected["exit_code"].as_i64().unwrap() as i32
                );
                assert_eq!(
                    normalize_dynamic_text(err.stdout(), &dir, None),
                    expected_text(expected, "stdout")
                );
                assert_eq!(
                    normalize_dynamic_text(err.stderr(), &dir, None),
                    expected_text(expected, "stderr")
                );
                assert_eq!(err.to_string(), expected["message"].as_str().unwrap());
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
