//! A/B onboarding harness for collecting feedback labels (WF-ADR-0006).
//!
//! The harness runs each prompt through every arm, asks an injected judge which
//! arm was sufficient, and appends that arm as a feedback label. A judge may
//! abstain by returning `None`, which skips the prompt without recording a row.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::Path;

use crate::feedback::record_label;
use crate::judge::OnboardOutputs;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OnboardSummary {
    pub judged: usize,
    pub abstained: usize,
    pub label_counts: BTreeMap<String, usize>,
}

pub fn run_onboarding<P, S, A, R, J, L>(
    prompts: P,
    arms: &[A],
    mut run_model: R,
    mut judge: J,
    log_path: L,
) -> io::Result<OnboardSummary>
where
    P: IntoIterator<Item = S>,
    S: AsRef<str>,
    A: AsRef<str>,
    R: FnMut(&str, &str) -> String,
    J: FnMut(&str, &OnboardOutputs) -> Option<String>,
    L: AsRef<Path>,
{
    if arms.len() < 2 {
        return Err(invalid_input(
            "onboarding needs at least two arms (e.g. a local and a hosted model)",
        ));
    }

    let arm_names = arms.iter().map(AsRef::as_ref).collect::<HashSet<_>>();
    let mut summary = OnboardSummary::default();

    for prompt in prompts {
        let prompt = prompt.as_ref();
        let outputs = arms
            .iter()
            .map(|arm| {
                let arm = arm.as_ref();
                (arm.to_string(), run_model(arm, prompt))
            })
            .collect::<OnboardOutputs>();

        let Some(label) = judge(prompt, &outputs) else {
            summary.abstained += 1;
            continue;
        };

        if !arm_names.contains(label.as_str()) {
            return Err(invalid_input(format!(
                "judge returned an unknown arm: {label:?}"
            )));
        }

        record_label(log_path.as_ref(), prompt, &label)?;
        summary.judged += 1;
        *summary.label_counts.entry(label).or_default() += 1;
    }

    Ok(summary)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
