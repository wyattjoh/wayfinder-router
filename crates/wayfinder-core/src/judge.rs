//! Automated sufficiency judges for offline calibration (WF-ADR-0037).
//!
//! The judge is pure text comparison. It does not touch model APIs, network,
//! or gateway state, so saved comparison logs can be replayed deterministically.

use std::collections::HashMap;

pub const DEFAULT_REFUSAL_MARKERS: &[&str] = &[
    "i can't help",
    "i cannot help",
    "i can't assist",
    "i cannot assist",
    "i'm unable to",
    "i am unable to",
    "i'm not able to",
    "i am not able to",
    "i'm sorry, but i can",
    "as an ai language model",
    "i cannot provide",
    "i can't provide",
];

pub const DEFAULT_MIN_ANSWER_CHARS: usize = 16;
pub const DEFAULT_SIMILARITY_SUFFICIENT: f64 = 0.8;
pub const HEURISTIC_JUDGE_VERSION: &str = "heuristic-2";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub sufficient: Option<bool>,
    pub reason: String,
    pub comparator: String,
}

impl Verdict {
    pub fn new(sufficient: Option<bool>, reason: impl Into<String>, comparator: &str) -> Self {
        Self {
            sufficient,
            reason: reason.into(),
            comparator: comparator.to_string(),
        }
    }
}

pub trait Judge {
    fn version(&self) -> &str;
    fn judge(&self, prompt: &str, cheap: &str, expensive: &str) -> Verdict;
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeuristicJudge {
    pub similarity_sufficient: f64,
    pub min_answer_chars: usize,
    pub refusal_markers: Vec<String>,
    pub version: String,
}

impl HeuristicJudge {
    pub fn new() -> Self {
        Self::default()
    }

    fn is_non_answer(&self, normalized: &str) -> bool {
        if normalized.is_empty() {
            return true;
        }

        self.refusal_markers
            .iter()
            .any(|marker| normalized.contains(marker))
    }
}

impl Default for HeuristicJudge {
    fn default() -> Self {
        Self {
            similarity_sufficient: DEFAULT_SIMILARITY_SUFFICIENT,
            min_answer_chars: DEFAULT_MIN_ANSWER_CHARS,
            refusal_markers: DEFAULT_REFUSAL_MARKERS
                .iter()
                .map(|marker| marker.to_string())
                .collect(),
            version: HEURISTIC_JUDGE_VERSION.to_string(),
        }
    }
}

impl Judge for HeuristicJudge {
    fn version(&self) -> &str {
        &self.version
    }

    fn judge(&self, _prompt: &str, cheap: &str, expensive: &str) -> Verdict {
        let cheap_norm = normalize(cheap);
        let expensive_norm = normalize(expensive);

        let cheap_bad = self.is_non_answer(&cheap_norm);
        let expensive_bad = self.is_non_answer(&expensive_norm);
        if cheap_bad && expensive_bad {
            return Verdict::new(None, "both arms refused or returned no answer", "refusal");
        }
        if cheap_bad {
            return Verdict::new(
                Some(false),
                "cheap arm refused/empty while the dear arm answered",
                "refusal",
            );
        }
        if expensive_bad {
            return Verdict::new(
                Some(true),
                "dear arm refused/empty while the cheap arm answered",
                "refusal",
            );
        }

        if cheap_norm == expensive_norm {
            return Verdict::new(
                Some(true),
                "answers identical after normalization",
                "agreement",
            );
        }

        let ratio = sequence_matcher_ratio(&cheap_norm, &expensive_norm);
        let long_enough = cheap_norm.chars().count() >= self.min_answer_chars
            && expensive_norm.chars().count() >= self.min_answer_chars;
        if long_enough && ratio >= self.similarity_sufficient {
            return Verdict::new(
                Some(true),
                format!(
                    "answers {ratio:.2} similar (>= {:.2})",
                    self.similarity_sufficient
                ),
                "similarity",
            );
        }

        Verdict::new(
            None,
            format!("answers diverge ({ratio:.2} similar); heuristic cannot adjudicate"),
            "divergence",
        )
    }
}

pub type OnboardOutputs = HashMap<String, String>;

pub fn as_onboard_judge<'a, J>(
    judge: &'a J,
    cheap_arm: impl Into<String>,
    expensive_arm: impl Into<String>,
) -> impl Fn(&str, &OnboardOutputs) -> Option<String> + 'a
where
    J: Judge + ?Sized,
{
    let cheap_arm = cheap_arm.into();
    let expensive_arm = expensive_arm.into();

    move |prompt, outputs| {
        let cheap = outputs
            .get(&cheap_arm)
            .unwrap_or_else(|| panic!("missing cheap arm output for {cheap_arm}"));
        let expensive = outputs
            .get(&expensive_arm)
            .unwrap_or_else(|| panic!("missing expensive arm output for {expensive_arm}"));
        let verdict = judge.judge(prompt, cheap, expensive);

        match verdict.sufficient {
            Some(true) => Some(cheap_arm.clone()),
            Some(false) => Some(expensive_arm.clone()),
            None => None,
        }
    }
}

pub fn as_onboard_judge_with_callback<'a, J, F>(
    judge: &'a J,
    cheap_arm: impl Into<String>,
    expensive_arm: impl Into<String>,
    mut on_verdict: F,
) -> impl FnMut(&str, &OnboardOutputs) -> Option<String> + 'a
where
    J: Judge + ?Sized,
    F: FnMut(&str, &OnboardOutputs, &Verdict) + 'a,
{
    let cheap_arm = cheap_arm.into();
    let expensive_arm = expensive_arm.into();

    move |prompt, outputs| {
        let cheap = outputs
            .get(&cheap_arm)
            .unwrap_or_else(|| panic!("missing cheap arm output for {cheap_arm}"));
        let expensive = outputs
            .get(&expensive_arm)
            .unwrap_or_else(|| panic!("missing expensive arm output for {expensive_arm}"));
        let verdict = judge.judge(prompt, cheap, expensive);
        on_verdict(prompt, outputs, &verdict);

        match verdict.sufficient {
            Some(true) => Some(cheap_arm.clone()),
            Some(false) => Some(expensive_arm.clone()),
            None => None,
        }
    }
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn sequence_matcher_ratio(a: &str, b: &str) -> f64 {
    let matcher = SequenceMatcher::new(a, b);
    matcher.ratio()
}

#[derive(Debug)]
struct SequenceMatcher {
    a: Vec<char>,
    b: Vec<char>,
    b2j: HashMap<char, Vec<usize>>,
}

impl SequenceMatcher {
    fn new(a: &str, b: &str) -> Self {
        let a = a.chars().collect::<Vec<_>>();
        let b = b.chars().collect::<Vec<_>>();
        let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();

        for (index, value) in b.iter().copied().enumerate() {
            b2j.entry(value).or_default().push(index);
        }

        if b.len() >= 200 {
            let popularity_cutoff = b.len() / 100 + 1;
            b2j.retain(|_, indexes| indexes.len() <= popularity_cutoff);
        }

        Self { a, b, b2j }
    }

    fn ratio(&self) -> f64 {
        let total = self.a.len() + self.b.len();
        if total == 0 {
            return 1.0;
        }

        let matches = self
            .matching_blocks()
            .iter()
            .map(|(_, _, size)| size)
            .sum::<usize>();
        2.0 * matches as f64 / total as f64
    }

    fn matching_blocks(&self) -> Vec<(usize, usize, usize)> {
        let mut queue = vec![(0, self.a.len(), 0, self.b.len())];
        let mut matching_blocks = Vec::new();

        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (best_i, best_j, best_size) = self.find_longest_match(alo, ahi, blo, bhi);
            if best_size == 0 {
                continue;
            }

            matching_blocks.push((best_i, best_j, best_size));
            if alo < best_i && blo < best_j {
                queue.push((alo, best_i, blo, best_j));
            }
            if best_i + best_size < ahi && best_j + best_size < bhi {
                queue.push((best_i + best_size, ahi, best_j + best_size, bhi));
            }
        }

        matching_blocks.sort_unstable();

        let mut non_adjacent = Vec::new();
        for (i, j, size) in matching_blocks {
            if let Some((last_i, last_j, last_size)) = non_adjacent.last_mut() {
                if *last_i + *last_size == i && *last_j + *last_size == j {
                    *last_size += size;
                    continue;
                }
            }
            non_adjacent.push((i, j, size));
        }
        non_adjacent.push((self.a.len(), self.b.len(), 0));
        non_adjacent
    }

    fn find_longest_match(
        &self,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let mut best_i = alo;
        let mut best_j = blo;
        let mut best_size = 0;
        let mut j2len: HashMap<usize, usize> = HashMap::new();

        for i in alo..ahi {
            let mut new_j2len = HashMap::new();
            if let Some(indexes) = self.b2j.get(&self.a[i]) {
                for &j in indexes {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }

                    let previous = j.checked_sub(1).and_then(|prev| j2len.get(&prev).copied());
                    let size = previous.unwrap_or(0) + 1;
                    new_j2len.insert(j, size);
                    if size > best_size {
                        best_i = i + 1 - size;
                        best_j = j + 1 - size;
                        best_size = size;
                    }
                }
            }
            j2len = new_j2len;
        }

        while best_i > alo && best_j > blo && self.a[best_i - 1] == self.b[best_j - 1] {
            best_i -= 1;
            best_j -= 1;
            best_size += 1;
        }

        while best_i + best_size < ahi
            && best_j + best_size < bhi
            && self.a[best_i + best_size] == self.b[best_j + best_size]
        {
            best_size += 1;
        }

        (best_i, best_j, best_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARIS: &str = "The capital of France is Paris.";
    const PARIS_BANG: &str = "The capital of France is Paris!";
    const CELL: &str = "The mitochondria is the powerhouse of the cell, an organelle.";

    #[test]
    fn heuristic_verdicts_match_python_fixture() {
        let judge = HeuristicJudge::default();
        let cases = [
            ("", PARIS, Some(false), "refusal"),
            ("42", PARIS, None, "divergence"),
            (
                "I can't help with that, sorry.",
                PARIS,
                Some(false),
                "refusal",
            ),
            ("", "   ", None, "refusal"),
            (PARIS, "I'm unable to answer that.", Some(true), "refusal"),
            (PARIS, PARIS, Some(true), "agreement"),
            (PARIS, PARIS_BANG, Some(true), "similarity"),
            (PARIS, CELL, None, "divergence"),
        ];

        for (cheap, expensive, sufficient, comparator) in cases {
            let verdict = judge.judge("q", cheap, expensive);
            assert_eq!(verdict.sufficient, sufficient);
            assert_eq!(verdict.comparator, comparator);
        }
    }

    #[test]
    fn terse_answers_are_answers_not_refusals() {
        let judge = HeuristicJudge::default();

        let short = judge.judge("q", "C", "C");
        assert_eq!(short.sufficient, Some(true));
        assert_eq!(short.comparator, "agreement");

        let dear_short = judge.judge("q", CELL, "C");
        assert_eq!(dear_short.sufficient, None);
        assert_ne!(dear_short.comparator, "refusal");

        let fuzzy_short = judge.judge("q", "cat", "car");
        assert_eq!(fuzzy_short.sufficient, None);
        assert_eq!(fuzzy_short.comparator, "divergence");
    }

    #[test]
    fn heuristic_judge_is_deterministic_and_versioned() {
        let judge = HeuristicJudge::default();
        assert_eq!(judge.version(), "heuristic-2");
        assert_eq!(judge.version(), HEURISTIC_JUDGE_VERSION);
        assert_eq!(judge.judge("q", PARIS, CELL), judge.judge("q", PARIS, CELL));
    }

    #[test]
    fn sequence_matcher_ratio_matches_python_difflib_fixture() {
        let cases = [
            ("", "", 1.0),
            ("abc", "abc", 1.0),
            ("abc", "abX", 0.666_666_666_666_666_6),
            (PARIS, PARIS_BANG, 0.967741935483871),
            (PARIS, CELL, 0.32608695652173914),
            ("diet", "tide", 0.5),
            (
                "private Thread currentThread;",
                "private volatile Thread currentThread;",
                0.865_671_641_791_044_7,
            ),
            (&"abcd".repeat(80), &"bcde".repeat(80), 0.0),
            (
                &"x".repeat(210),
                &format!("{}y", "x".repeat(209)),
                0.995_238_095_238_095_3,
            ),
            ("a b\n c\t d", "a b c d", 0.875),
        ];

        for (a, b, expected) in cases {
            let actual = sequence_matcher_ratio(a, b);
            assert!(
                (actual - expected).abs() <= 1e-9,
                "{a:?} vs {b:?}: expected {expected}, got {actual}"
            );
        }
    }

    #[test]
    fn adapter_maps_sufficient_to_cheap_arm() {
        let judge = FixedJudge(Verdict::new(Some(true), "", "x"));
        let choose = as_onboard_judge(&judge, "local", "cloud");
        assert_eq!(
            choose("p", &outputs("local", "answer", "cloud", "other")),
            Some("local".to_string())
        );
    }

    #[test]
    fn adapter_maps_insufficient_to_expensive_arm() {
        let judge = FixedJudge(Verdict::new(Some(false), "", "x"));
        let choose = as_onboard_judge(&judge, "local", "cloud");
        assert_eq!(
            choose("p", &outputs("local", "answer", "cloud", "other")),
            Some("cloud".to_string())
        );
    }

    #[test]
    fn adapter_maps_abstain_to_none() {
        let judge = FixedJudge(Verdict::new(None, "", "x"));
        let choose = as_onboard_judge(&judge, "local", "cloud");
        assert_eq!(
            choose("p", &outputs("local", "answer", "cloud", "other")),
            None
        );
    }

    #[test]
    fn adapter_invokes_on_verdict_callback() {
        let judge = FixedJudge(Verdict::new(Some(true), "why", "x"));
        let mut seen = Vec::new();
        let mut choose =
            as_onboard_judge_with_callback(&judge, "local", "cloud", |prompt, _, verdict| {
                seen.push((prompt.to_string(), verdict.reason.clone()));
            });

        assert_eq!(
            choose("p", &outputs("local", "answer", "cloud", "other")),
            Some("local".to_string())
        );
        drop(choose);
        assert_eq!(seen, vec![("p".to_string(), "why".to_string())]);
    }

    struct FixedJudge(Verdict);

    impl Judge for FixedJudge {
        fn version(&self) -> &str {
            "fixed"
        }

        fn judge(&self, _prompt: &str, _cheap: &str, _expensive: &str) -> Verdict {
            self.0.clone()
        }
    }

    fn outputs(
        cheap_arm: &str,
        cheap_output: &str,
        expensive_arm: &str,
        expensive_output: &str,
    ) -> OnboardOutputs {
        HashMap::from([
            (cheap_arm.to_string(), cheap_output.to_string()),
            (expensive_arm.to_string(), expensive_output.to_string()),
        ])
    }
}
