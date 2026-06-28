use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::path::{Path, PathBuf};

use wayfinder_internal_core::profiles::{PROFILES, PROFILES_BY_ID};

#[derive(Debug, Deserialize)]
struct ProfileFixture {
    profiles: Vec<ExpectedProfile>,
}

#[derive(Debug, Deserialize)]
struct ExpectedProfile {
    id: String,
    name: String,
    source: String,
    reasoning_terms: Vec<String>,
    constraint_terms: Vec<String>,
    note: String,
    reasoning_term_count: usize,
    constraint_term_count: usize,
}

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

fn expected_profiles() -> ProfileFixture {
    let path = fixture("profiles/stock-lexicons.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("fixture {} should be readable: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {} should be JSON: {err}", path.display()))
}

#[test]
fn stock_profiles_match_python_profiles_by_id_fixture() {
    let fixture = expected_profiles();
    let actual_ids = PROFILES
        .iter()
        .map(|profile| profile.id)
        .collect::<Vec<_>>();
    let expected_ids = fixture
        .profiles
        .iter()
        .map(|profile| profile.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(actual_ids, expected_ids);

    for expected in fixture.profiles {
        let actual = PROFILES_BY_ID
            .get(expected.id.as_str())
            .unwrap_or_else(|| panic!("missing profile id {}", expected.id));
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.source, expected.source);
        assert_eq!(actual.reasoning_terms, expected.reasoning_terms.as_slice());
        assert_eq!(
            actual.constraint_terms,
            expected.constraint_terms.as_slice()
        );
        assert_eq!(actual.reasoning_terms.len(), expected.reasoning_term_count);
        assert_eq!(
            actual.constraint_terms.len(),
            expected.constraint_term_count
        );
        assert_eq!(actual.note, expected.note);
    }
}

#[test]
fn to_dict_serializes_like_python_profile_dict() {
    let profile = PROFILES_BY_ID
        .get("law-compliance")
        .expect("law-compliance profile");
    let actual = serde_json::to_value(profile.to_dict()).expect("profile dict should serialize");
    let actual_object = actual
        .as_object()
        .expect("profile dict should be an object");

    assert_eq!(
        actual_object.keys().collect::<Vec<_>>(),
        vec![
            "constraint_terms",
            "id",
            "name",
            "note",
            "reasoning_terms",
            "source"
        ]
    );

    let expected = json!({
        "id": "law-compliance",
        "name": "Law & compliance",
        "source": "curated",
        "reasoning_terms": [
            "liable",
            "liability",
            "indemnify",
            "indemnification",
            "pursuant",
            "herein",
            "hereto",
            "whereas",
            "statute",
            "statutory",
            "jurisdiction",
            "plaintiff",
            "defendant",
            "tort",
            "breach",
            "covenant",
            "waiver",
            "arbitration",
            "negligence",
            "damages",
            "contractual"
        ],
        "constraint_terms": [
            "shall",
            "must",
            "prohibited",
            "required",
            "notwithstanding",
            "provided"
        ],
        "note": "Hand-authored legal/compliance vocabulary."
    });

    assert_eq!(actual, expected);
    assert!(matches!(actual["reasoning_terms"], JsonValue::Array(_)));
    assert!(matches!(actual["constraint_terms"], JsonValue::Array(_)));
}
