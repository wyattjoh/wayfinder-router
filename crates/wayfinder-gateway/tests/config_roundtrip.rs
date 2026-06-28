use wayfinder_internal_gateway::{
    dump_gateway_toml, gateway_config_from_toml, validate_gateway_toml,
};

const GATEWAY_FIXTURE: &str =
    include_str!("../../../tests/fixtures/contracts/gateway/gateway-config-roundtrip.toml");
const PYTHON_EXPECTED: &str = include_str!(
    "../../../tests/fixtures/contracts/gateway/gateway-config-roundtrip.expected.toml"
);

#[test]
fn gateway_config_round_trips_like_python_dump() {
    std::env::set_var("EXAMPLE_API_KEY", "resolved-secret-never-dumped");

    let config = gateway_config_from_toml(GATEWAY_FIXTURE, "fixture").expect("config should parse");
    let dumped = dump_gateway_toml(&config);

    assert_eq!(dumped, PYTHON_EXPECTED.trim_end_matches('\n'));
    assert!(dumped.contains("api_key_env = \"EXAMPLE_API_KEY\""));
    assert!(dumped.contains("api_key_cmd = \"op read op://Private/example/credential\""));
    assert!(!dumped.contains("resolved-secret-never-dumped"));

    let again = gateway_config_from_toml(&dumped, "dumped").expect("dump should parse");
    assert_eq!(
        again.models["cloud"].api_key_env.as_deref(),
        Some("EXAMPLE_API_KEY")
    );
    assert_eq!(
        again.models["cloud"].api_key_cmd.as_deref(),
        Some("op read op://Private/example/credential")
    );
}

#[test]
fn validate_gateway_toml_rejects_malformed_gateway_blocks() {
    let bad = r#"
[gateway.models.cloud]
base_url = "https://api.example.com/v1"
model = "big-model"
api_key_cmd = "op read op://Private/example/credential"
"#;

    let err = validate_gateway_toml(bad, "bad.toml").expect_err("config should be rejected");

    assert_eq!(
        err.to_string(),
        "bad.toml: 'gateway.models.cloud.api_key_cmd' needs 'api_key_env' to name the variable it fills"
    );
}
