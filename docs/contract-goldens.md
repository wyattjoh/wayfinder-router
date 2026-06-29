# Contract Golden Regeneration

These fixtures are static snapshots generated from the Python implementation before the Rust-only cutover. Rust tests must read these files directly and must not import or shell out to Python at test runtime.

Run regeneration commands from the repository root with the Python package available. Commit the resulting fixture changes with the matching Rust assertions.

| Fixture | Source command |
| --- | --- |
| `tests/fixtures/contracts/scoring/simple.json` | `python - <<'PY'` with `from wayfinder_router import score_complexity` and the fixture prompt, then JSON dump the prompt plus expected score payload. |
| `tests/fixtures/contracts/scoring/markdown-structure.json` | `python - <<'PY'` with `from wayfinder_router import score_complexity` and the markdown fixture prompt, then JSON dump the prompt plus expected score payload. |
| `tests/fixtures/contracts/calibrate/threshold-accuracy.json` | `python - <<'PY'` with `from wayfinder_router.calibrate import calibrate` over the embedded JSONL dataset using `mode="threshold"`. |
| `tests/fixtures/contracts/calibrate/threshold-cost-quality.json` | `python - <<'PY'` with `calibrate(..., mode="threshold", objective="cost-quality", target_savings=0.4)`. |
| `tests/fixtures/contracts/calibrate/threshold-cost-quality-inverted-costs.json` | `python - <<'PY'` with `calibrate(..., mode="threshold", objective="cost-quality", costs={"local": 1.0, "cloud": 0.1}, target_savings=0.3)`. |
| `tests/fixtures/contracts/calibrate/threshold-knee.json` | `python - <<'PY'` with `calibrate(samples, mode="threshold", objective="knee")` over the embedded scored samples. |
| `tests/fixtures/contracts/calibrate/tiers.json` | `python - <<'PY'` with `calibrate(samples, mode="tiers")` over the embedded JSONL dataset. |
| `tests/fixtures/contracts/calibrate/classifier.json` | `python - <<'PY'` with `calibrate(samples, mode="classifier", iterations=400)`. |
| `tests/fixtures/contracts/calibrate/classifier-negative-zero-emitter.json` | `python - <<'PY'` with `wayfinder_router.calibrate.emit_classifier_toml` for a classifier containing negative zero coefficients. |
| `tests/fixtures/contracts/calibrate/classifier-exponent-emitter.json` | `python - <<'PY'` with `wayfinder_router.calibrate.emit_classifier_toml` for a classifier containing exponent-sized coefficients. |
| `tests/fixtures/contracts/calibrate/parse-errors.json` | `python - <<'PY'` with `from wayfinder_router.calibrate import load_dataset` and each malformed JSONL row, capturing exception strings. |
| `tests/fixtures/contracts/config/minimal-routing.toml` | Hand-minimized TOML parsed by Python `wayfinder_router.config.load_routing_config`, then kept as the Rust parse fixture. |
| `tests/fixtures/contracts/profiles/stock-lexicons.json` | `python - <<'PY'` with `from wayfinder_router.profiles import PROFILES_BY_ID` and JSON dump each profile dict plus term counts. |
| `tests/fixtures/contracts/gateway/gateway-config-roundtrip.toml` | Hand-authored gateway TOML input accepted by Python `wayfinder_router.gateway.gateway_config_from_toml`. |
| `tests/fixtures/contracts/gateway/gateway-config-roundtrip.expected.toml` | `python - <<'PY'` with `from wayfinder_router.gateway import gateway_config_from_toml, dump_gateway_toml` over `gateway-config-roundtrip.toml`. |
| `tests/fixtures/contracts/gateway/recalibrate.expected.toml` | `python - <<'PY'` with `from wayfinder_router.recalibrate import recalibrate` using `threshold-accuracy.json` labels and `gateway-config-roundtrip.expected.toml` as the existing config. |
| `tests/fixtures/contracts/gateway/chat-completions-debug.json` | `python - <<'PY'` with `wayfinder_router.gateway.build_app` in dry-run mode, `X-Wayfinder-Debug: true`, and the fixture request body. Replace generated request ids with `<opaque-request-id>`. |
| `tests/fixtures/contracts/sufficiency/evaluate.json` | `python - <<'PY'` with `from wayfinder_router.sufficiency import cohens_kappa, evaluate` over the embedded pairs and samples. |
| `tests/fixtures/contracts/feedback/log.json` | `python - <<'PY'` with `from wayfinder_router import record_label, read_labels` over the embedded rows, then capture JSONL text and invalid-input messages. |
| `tests/fixtures/contracts/onboard/session.json` | `python - <<'PY'` with `from wayfinder_router import run_onboarding, read_labels` and deterministic local stub runners for the embedded prompts and arms. |
| `tests/fixtures/contracts/judge/heuristic.json` | `python - <<'PY'` with `from wayfinder_router import HeuristicJudge` over the embedded cases, dumping sufficient, reason, and comparator. |
| `tests/fixtures/contracts/reliability/retry-failover.json` | `python - <<'PY'` with `from wayfinder_router import reliability` over retryable statuses, retry delay inputs, failover candidates, and precheck cases. |
| `tests/fixtures/contracts/cli/commands.json` | `python -m wayfinder_router.cli ...` for each command argv listed in the fixture, with dynamic key material checked by shape and hash verification rather than exact text. |
| `tests/fixtures/contracts/ui/onboard-recalibrate.json` | `python - <<'PY'` with `from wayfinder_router.ui import build_ui_app` and a deterministic onboard invoker over the listed endpoint requests. |
