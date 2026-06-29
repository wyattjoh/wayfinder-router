# FAQ

Straight answers about what Wayfinder does, where it is useful, and where it is
only a proxy.

## Does Wayfinder call a model to make the routing decision?

No. The decision is a deterministic, offline scan of the prompt's structure
(length, headings, lists, code, tables), turned into a 0 to 1 score and compared
to routing config. No model, key, or network is in the scored path
([WF-ADR-0001](../decisions/WF-ADR-0001-standalone-deterministic-router.md)).
The chosen upstream model is called only after routing.

## Is this just a length threshold?

Length is one of the strongest signals, but it is not the only one. Wayfinder also
scores formatting, lists, headings, code blocks, tables, questions, and optional
lexical cues. That still makes it a proxy for prompt heaviness, not a semantic
understanding of the task.

## When should I use it?

Use it when your expensive traffic often looks structurally heavier than your
cheap traffic, such as agent traces, document work, multi-part prompts, code
blocks, and long instructions. If your hardest prompts are short and semantic,
you should expect an explicit pin or a semantic router to beat a structural scan.

## Why not always use the cheap model, or always the big one?

For some workloads, one of those baselines is correct. Routing is useful in the
middle: some traffic is fine on the cheap model, some needs the capable one, and
you have enough labels to separate them. Calibrate on your own data before
treating the default threshold as meaningful.

## What about lexical difficulty keywords?

They ship off by default. Lexical cues can help when your hard prompts use
domain-specific vocabulary that the scorer can scan, such as proof terms, math
symbols, legal constraints, or clinical language. They can also overfit to an
author's vocabulary. Turn them on only after calibrating and checking held-out
labels from your own traffic. See [lexical-routing.md](lexical-routing.md).

## How fast is the decision?

It is a deterministic text scan on one core. Exact timings depend on prompt
length and hardware, but the decision is designed to be negligible next to the
inference request it gates. Run `cargo test` for the Rust contracts and route your
own prompts through `wayfinder-router route --json` if you need local evidence.

## Do my prompts or keys leave the machine?

The scorer is offline and never opens a socket. Keys live in gateway model config
as environment variable names (`api_key_env`) or key commands (`api_key_cmd`), not
as raw secrets in routing config. The gateway is the only component that holds a
resolved key or makes an upstream network call.

## What models or providers can I route between?

Any OpenAI-compatible endpoints, plus the gateway's Anthropic Messages adapter for
clients that speak `/v1/messages`. A tier is a `base_url`, upstream model id, and
optional key source. The common setup is local Ollama or vLLM for the cheap arm
and a hosted frontier model for the expensive arm, but the tiers can be any two or
more configured endpoints.

## Why not an LLM-as-judge router instead?

An LLM judge can understand fuzzy semantics better than a structural scan. The
cost is one extra model call per request to decide whether to make the real model
call. Wayfinder is for the serving path where the decision itself should be free,
deterministic, self-hosted, and explainable. A judge is still useful for training
or labeling, which is why the Rust CLI includes `judge`.

## Does it handle streaming, chat, and multi-turn?

Yes. The gateway scores the request up front, forwards to the chosen upstream,
and relays streaming Server-Sent Events as they arrive. For multi-turn chat, the
gateway scores the configured routing scope rather than feeding model replies
back into the decision. Per-request pins are available through the `model` field,
slash directives when enabled, and the `X-Wayfinder-Threshold` header.

## Is it production-ready? Who maintains it?

It is early and intentionally small. The maintained project surface is the Rust
workspace: CLI, scorer, gateway, terminal chat, local tuning UI, calibration,
onboarding, judge, key minting, and Docker image. It is self-hosted under
Apache-2.0 and has no hosted control plane.

## How do I tune it to my own traffic?

Label representative prompts as `local` or `cloud`, then calibrate:

```bash
wayfinder-router calibrate your-data.jsonl --mode threshold --objective knee --out wayfinder-router.toml
```

Keep collecting feedback through `/v1/feedback`, then run:

```bash
wayfinder-router recalibrate --min-labels 50
```
