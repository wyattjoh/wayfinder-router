use std::collections::BTreeMap;

use serde_json::json;
use wayfinder_internal_core::detectors::{
    ai4privacy_items_from_records, evaluate, gitleaks_compare, micro_macro,
    reference_detectors_by_name, render_markdown, report_json, token_items, CorpusItem, Detector,
    Stats,
};

fn planted() -> Vec<CorpusItem> {
    vec![
        CorpusItem::new("this has SECRET inside", ["flag"]),
        CorpusItem::new("SECRET but not labelled", [] as [&str; 0]),
        CorpusItem::new("labelled but absent", ["flag"]),
        CorpusItem::new("clean text here", [] as [&str; 0]),
    ]
}

#[test]
fn planted_confusion_counts() {
    let flag = Detector::new("flag", "SECRET", None);
    let stats = evaluate(&planted(), &[flag]);

    assert_eq!(
        stats["flag"],
        Stats {
            tp: 1,
            fp: 1,
            fn_: 1,
            tn: 1,
        }
    );
}

#[test]
fn precision_recall_f1_math() {
    let stats = Stats {
        tp: 3,
        fp: 1,
        fn_: 1,
        tn: 5,
    };

    assert_eq!(stats.precision(), 0.75);
    assert_eq!(stats.recall(), 0.75);
    assert_eq!(stats.f1(), 0.75);
    assert_eq!(Stats::default().precision(), 0.0);
    assert_eq!(Stats::default().recall(), 0.0);
    assert_eq!(Stats::default().f1(), 0.0);
}

#[test]
fn micro_and_macro_rollup() {
    let flag = Detector::new("flag", "SECRET", None);
    let mark = Detector::new("mark", "MARK", None);
    let items = vec![
        CorpusItem::new("SECRET MARK", ["flag", "mark"]),
        CorpusItem::new("SECRET only", ["flag"]),
        CorpusItem::new("MARK unlabelled", [] as [&str; 0]),
    ];
    let stats = evaluate(&items, &[flag, mark]);
    let rollup = micro_macro(&stats);

    assert_eq!(stats["flag"].tp, 2);
    assert_eq!(stats["flag"].fp, 0);
    assert_eq!(stats["mark"].tp, 1);
    assert_eq!(stats["mark"].fp, 1);
    assert_eq!(rollup["micro_precision"], 0.75);
    assert_eq!(rollup["macro_precision"], 0.75);
    assert_eq!(rollup["macro_recall"], 1.0);
}

#[test]
fn report_is_deterministic() {
    let flag = Detector::new("flag", "SECRET", None);
    let first = evaluate(&planted(), std::slice::from_ref(&flag));
    let second = evaluate(&planted(), &[flag]);

    assert_eq!(report_json(&first), report_json(&second));
    let markdown = render_markdown(&first, planted().len(), "planted");
    assert_eq!(
        markdown,
        render_markdown(&second, planted().len(), "planted")
    );
    assert!(markdown.contains("| flag | 1 | 1 | 1 | 1 | 0.500 | 0.500 | 0.500 |"));
}

#[test]
fn reference_detectors_match_provider_shapes() {
    let detectors = reference_detectors_by_name();
    let credit_card = &detectors["credit_card"];
    assert!(credit_card.detects("card 4111 1111 1111 1111 on file"));
    assert!(!credit_card.detects("code 4111 1111 1111 1112 rejected"));
    assert!(!credit_card.detects("dotted 4111.1111.1111.1111 missed"));

    let aws = &detectors["aws_access_key"];
    let aws_example = ["AK", "IAIOSFODNN7EXAMPLE"].concat();
    assert!(aws.detects(&aws_example));
    assert!(!aws.detects("the AKIA prefix alone"));

    let github = &detectors["github_pat"];
    let github_example = ["gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789"].concat();
    assert!(github.detects(&github_example));
    assert!(!github.detects("ghp_short"));
}

#[test]
fn assembled_token_items_fire_their_detectors() {
    let detectors = reference_detectors_by_name();

    for item in token_items() {
        for label in &item.labels {
            assert!(
                detectors[label].detects(&item.text),
                "{label} missed its own positive"
            );
        }
    }
}

#[test]
fn ai4privacy_label_mapping() {
    let records = vec![
        json!({"source_text": "mail x@y.com", "privacy_mask": [{"label": "EMAIL"}]}),
        json!({"source_text": "id 123-45-6789", "privacy_mask": [{"label": "SSN"}, {"label": "FIRSTNAME"}]}),
        json!({"source_text": "nothing here", "privacy_mask": [{"label": "JOBAREA"}]}),
    ];
    let items = ai4privacy_items_from_records(&records);

    assert_eq!(items[0].labels, vec!["email"]);
    assert_eq!(items[1].labels, vec!["us_ssn"]);
    assert!(items[2].labels.is_empty());
}

#[test]
fn gitleaks_compare_is_pure_and_correct() {
    let rules = BTreeMap::from([("github-pat".to_owned(), r"ghp_[0-9a-zA-Z]{36}".to_owned())]);
    let comparisons = gitleaks_compare(&rules)
        .into_iter()
        .map(|comparison| (comparison.detector.clone(), comparison))
        .collect::<BTreeMap<_, _>>();
    let github = &comparisons["github_pat"];
    assert_eq!(github.agree, github.total);

    let aws = &comparisons["aws_access_key"];
    assert_eq!(aws.mine_fires, 2);
    assert_eq!(aws.theirs_fires, 0);
}
