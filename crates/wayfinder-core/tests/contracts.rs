use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("core crate lives under crates/wayfinder-core")
        .to_path_buf()
}

fn fixture(path: &str) -> PathBuf {
    repo_root().join("tests/fixtures/contracts").join(path)
}

#[test]
fn scoring_contract_fixtures_are_parseable() {
    for name in ["scoring/simple.json", "scoring/markdown-structure.json"] {
        let path = fixture(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
        let parsed: JsonValue = serde_json::from_str(&text)
            .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()));
        assert_eq!(
            parsed["expected"]["schema_version"],
            wayfinder_internal_core::SCORING_SCHEMA_VERSION
        );
        assert!(parsed["prompt"].is_string());
        assert!(parsed["expected"]["features"].is_object());
    }
}

#[test]
fn routing_config_fixture_is_parseable_toml() {
    let path = fixture("config/minimal-routing.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    let parsed: TomlValue = toml::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be TOML: {err}", path.display()));
    assert!(parsed.get("routing").is_some());
}

#[test]
fn cli_command_shape_fixture_is_parseable() {
    let path = fixture("cli/commands.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    let parsed: JsonValue = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()));
    let commands = parsed["commands"]
        .as_array()
        .expect("commands fixture should contain an array");
    assert!(commands.iter().any(|command| command["name"] == "serve"));
    assert!(commands.iter().any(|command| command["name"] == "chat"));
}

#[test]
fn gateway_contract_fixture_is_parseable() {
    let path = fixture("gateway/chat-completions-debug.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    let parsed: JsonValue = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()));
    assert_eq!(parsed["request"]["path"], "/v1/chat/completions");
    assert!(parsed["expected_response"]["headers"]["x-wayfinder-router-model"].is_string());
    assert!(parsed["expected_response"]["body"]["wayfinder"]["features"].is_object());
}
