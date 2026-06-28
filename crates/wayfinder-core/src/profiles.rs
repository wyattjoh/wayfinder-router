use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LexiconProfile {
    pub id: &'static str,
    pub name: &'static str,
    pub source: &'static str,
    pub reasoning_terms: &'static [&'static str],
    pub constraint_terms: &'static [&'static str],
    pub note: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LexiconProfileDict {
    pub id: &'static str,
    pub name: &'static str,
    pub source: &'static str,
    pub reasoning_terms: Vec<&'static str>,
    pub constraint_terms: Vec<&'static str>,
    pub note: &'static str,
}

impl LexiconProfile {
    pub fn to_dict(&self) -> LexiconProfileDict {
        LexiconProfileDict {
            id: self.id,
            name: self.name,
            source: self.source,
            reasoning_terms: self.reasoning_terms.to_vec(),
            constraint_terms: self.constraint_terms.to_vec(),
            note: self.note,
        }
    }
}

const PROOFS_MATH: LexiconProfile = LexiconProfile {
    id: "proofs-math",
    name: "Proofs & mathematics",
    source: "curated",
    reasoning_terms: &[
        "prove",
        "proof",
        "proofs",
        "theorem",
        "lemma",
        "corollary",
        "axiom",
        "conjecture",
        "induction",
        "contradiction",
        "qed",
        "derive",
        "derivation",
        "integral",
        "derivative",
        "eigenvalue",
        "asymptotic",
        "bijection",
        "isomorphism",
        "modulo",
        "recurrence",
        "polynomial",
        "monotonic",
        "invariant",
        "optimal",
        "optimality",
    ],
    constraint_terms: &["exactly", "minimize", "maximize", "subject"],
    note: "Hand-authored maths/CS reasoning vocabulary (close to the built-in default).",
};

const LAW_COMPLIANCE: LexiconProfile = LexiconProfile {
    id: "law-compliance",
    name: "Law & compliance",
    source: "curated",
    reasoning_terms: &[
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
        "contractual",
    ],
    constraint_terms: &[
        "shall",
        "must",
        "prohibited",
        "required",
        "notwithstanding",
        "provided",
    ],
    note: "Hand-authored legal/compliance vocabulary.",
};

const CODE_INFRA: LexiconProfile = LexiconProfile {
    id: "code-infra",
    name: "Code & infrastructure",
    source: "curated",
    reasoning_terms: &[
        "concurrency",
        "concurrent",
        "deadlock",
        "mutex",
        "idempotent",
        "idempotency",
        "latency",
        "throughput",
        "distributed",
        "consensus",
        "replication",
        "sharding",
        "rollback",
        "migration",
        "schema",
        "consistency",
        "atomicity",
        "serializable",
        "partition",
        "race",
        "lock",
    ],
    constraint_terms: &[],
    note: "Hand-authored systems/infrastructure vocabulary.",
};

const SCIENCE_MEDICINE: LexiconProfile = LexiconProfile {
    id: "science-medicine",
    name: "Science & medicine",
    source: "curated",
    reasoning_terms: &[
        "hypothesis",
        "pathogenesis",
        "etiology",
        "diagnosis",
        "prognosis",
        "cardiac",
        "hepatic",
        "renal",
        "membrane",
        "enzyme",
        "mitochondria",
        "pyruvate",
        "catalysis",
        "molecule",
        "atom",
        "orbital",
        "electron",
        "isotope",
        "contraindication",
        "dosage",
        "pharmacokinetics",
    ],
    constraint_terms: &[],
    note: "Hand-authored science/medicine vocabulary.",
};

const REAL_MINED_NOTE: &str =
    "Mined from RouterBench: real subject-matter vocabulary; still calibrate on your traffic.";
const WEAK_MINED_NOTE: &str = "Mined from RouterBench word-problem tasks: task-surface vocabulary, NOT difficulty — a cautionary example, not a recommendation.";

const MINED_SCIENCE: LexiconProfile = LexiconProfile {
    id: "mined-science",
    name: "Science (RouterBench)",
    source: "mined",
    reasoning_terms: &[
        "hypertension",
        "center",
        "learning",
        "objects",
        "cardiac",
        "pyruvate",
        "mild",
        "parents",
        "phase",
        "region",
        "products",
        "membrane",
        "anterior",
        "element",
        "orbit",
        "chain",
        "atoms",
        "neck",
        "rapid",
        "potential",
    ],
    constraint_terms: &[],
    note: REAL_MINED_NOTE,
};

const MINED_GENERAL: LexiconProfile = LexiconProfile {
    id: "mined-general",
    name: "General knowledge (RouterBench)",
    source: "mined",
    reasoning_terms: &[
        "committee",
        "planning",
        "taxes",
        "measure",
        "identity",
        "punishment",
        "procedures",
        "cultural",
        "industry",
        "areas",
        "ethics",
        "organization",
        "share",
        "falls",
        "local",
        "skills",
        "curve",
        "identify",
        "unemployment",
        "spending",
    ],
    constraint_terms: &[],
    note: REAL_MINED_NOTE,
};

const MINED_HUMANITIES: LexiconProfile = LexiconProfile {
    id: "mined-humanities",
    name: "Humanities (RouterBench)",
    source: "mined",
    reasoning_terms: &[
        "classes",
        "function",
        "expression",
        "extension",
        "latin",
        "russia",
        "yard",
        "facilities",
        "famous",
        "republics",
        "settlement",
        "socialist",
        "materials",
        "morality",
        "western",
        "colonial",
        "fallacy",
        "consequences",
        "cultural",
        "nations",
    ],
    constraint_terms: &[],
    note: REAL_MINED_NOTE,
};

const MINED_COMMONSENSE: LexiconProfile = LexiconProfile {
    id: "mined-commonsense",
    name: "Commonsense (RouterBench)",
    source: "mined",
    reasoning_terms: &[
        "carry",
        "sheet",
        "morally",
        "scenarios",
        "scenario",
        "standards",
        "moral",
        "character",
        "ordinary",
        "wrong",
        "buying",
        "wedding",
        "oven",
        "major",
        "adjust",
        "growth",
    ],
    constraint_terms: &[],
    note: WEAK_MINED_NOTE,
};

const MINED_MATH: LexiconProfile = LexiconProfile {
    id: "mined-math",
    name: "Math word-problems (RouterBench)",
    source: "mined",
    reasoning_terms: &[
        "candy", "sunday", "mother", "saturday", "mile", "weighs", "cards", "birthday",
    ],
    constraint_terms: &[],
    note: WEAK_MINED_NOTE,
};

const MINED_MULTILINGUAL: LexiconProfile = LexiconProfile {
    id: "mined-multilingual",
    name: "Multilingual (RouterBench)",
    source: "mined",
    reasoning_terms: &[
        "dragon",
        "animal",
        "approximate",
        "birth",
        "digit",
        "estimated",
        "exact",
        "guesses",
        "sentences",
        "subject's",
        "wishing",
        "without",
        "year",
        "zodiac",
        "zones",
        "monkey",
        "chinese",
        "other",
        "translate",
        "snake",
    ],
    constraint_terms: &[],
    note: WEAK_MINED_NOTE,
};

pub static CURATED: &[LexiconProfile] =
    &[PROOFS_MATH, LAW_COMPLIANCE, CODE_INFRA, SCIENCE_MEDICINE];

pub static MINED: &[LexiconProfile] = &[
    MINED_SCIENCE,
    MINED_GENERAL,
    MINED_HUMANITIES,
    MINED_COMMONSENSE,
    MINED_MATH,
    MINED_MULTILINGUAL,
];

pub static PROFILES: &[LexiconProfile] = &[
    PROOFS_MATH,
    LAW_COMPLIANCE,
    CODE_INFRA,
    SCIENCE_MEDICINE,
    MINED_SCIENCE,
    MINED_GENERAL,
    MINED_HUMANITIES,
    MINED_COMMONSENSE,
    MINED_MATH,
    MINED_MULTILINGUAL,
];

pub static PROFILES_BY_ID: LazyLock<BTreeMap<&'static str, &'static LexiconProfile>> =
    LazyLock::new(|| {
        PROFILES
            .iter()
            .map(|profile| (profile.id, profile))
            .collect()
    });
