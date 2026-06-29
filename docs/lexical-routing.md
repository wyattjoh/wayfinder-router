# Lexical Routing

Wayfinder's default scorer is structural: length, headings, lists, code, tables,
questions, and related shape signals. Lexical routing is the opt-in path for
traffic where hard prompts use recognizable vocabulary, such as proof terms,
math symbols, legal constraints, medical terminology, or infrastructure words.

## The Recipe

Copy [`../examples/wayfinder-router.lexical.toml`](../examples/wayfinder-router.lexical.toml)
to `wayfinder-router.toml` or add the same settings to your config:

```toml
[routing]
threshold = 0.09
weights = { reasoning_term_count = 5.0, math_symbol_count = 3.0, constraint_term_count = 1.5 }
```

Only the lexical weights are raised. The decision is still local, deterministic,
and free of model calls.

## When It Helps

Lexical signals detect vocabulary, not difficulty in general. They help when your
traffic expresses hardness in terms the scorer can count. They do not solve
short-but-hard prompts whose difficulty is purely semantic.

Treat any copied threshold as a starter value. Recalibrate on your own labels
before deploying it.

## Recalibrate The Threshold

Label representative prompts as `local` or `cloud`, then let the Rust calibrator
place the cut:

```bash
wayfinder-router calibrate your-data.jsonl --mode threshold --objective knee \
  --costs local=0.0001,cloud=0.003 \
  --weights reasoning_term_count=5,math_symbol_count=3,constraint_term_count=1.5 \
  --out wayfinder-router.toml
```

`--objective knee` balances quality recovered against cost saved, so you do not
need to guess a savings target. Aim for enough labels to check held-out behavior,
not just a tiny smoke test.

## Bring Your Own Lexicon

The trigger words are config, not code (WF-ADR-0019). Supply domain vocabulary
under `[routing.lexicon]`:

```toml
[routing.lexicon]
reasoning_terms = ["differential", "contraindication", "etiology", "pathophysiology"]
# constraint_terms = [...]
```

A lexicon family stays at its built-in default when omitted. It has no effect
until the matching feature weight is non-zero.

## Stock Profiles

The Rust core ships stock lexicon profiles in
[`../crates/wayfinder-core/src/profiles.rs`](../crates/wayfinder-core/src/profiles.rs)
(WF-ADR-0024). The gateway serves them at `GET /router/profiles`, and the browser
demo can load them from Advanced settings.

Profiles are starting vocabularies, not finished routers. Load one, calibrate on
your own labels, then re-check with held-out prompts before relying on it.
