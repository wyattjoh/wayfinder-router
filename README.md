<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/banner-dark.png">
  <img alt="Wayfinder" src="docs/banner-light.png" width="640">
</picture>

<p><strong>Deterministic prompt-complexity routing - send each prompt to your
local or cloud model, offline, with no model call to decide.</strong></p>

<p>
  <a href="#quickstart">Quickstart</a> ·
  <a href="#command-reference">Commands</a> ·
  <a href="#deploy-and-integrate">Deploy</a> ·
  <a href="EXPLAINER.md">Explainer</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

<p>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
  <a href="https://github.com/wyattjoh/wayfinder-router/actions/workflows/ci.yml"><img src="https://github.com/wyattjoh/wayfinder-router/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/core-Rust-dea584.svg?logo=rust&logoColor=white" alt="Rust core">
</p>

</div>

> **Fork notice.** This is a Rust fork of
> [`itsthelore/wayfinder-router`](https://github.com/itsthelore/wayfinder-router).
> The routing and scoring core, OpenAI/Anthropic gateway, calibration tools,
> terminal chat, browser demo, local tuning UI, onboarding loop, automated judge,
> and operator commands are implemented in Rust under [`crates/`](crates).

<table align="center">
<tr>
<td align="center"><b>No model call</b><br>to decide the route</td>
<td align="center"><b>Deterministic</b><br>and fully offline</td>
</tr>
<tr>
<td align="center"><b>Calibrate</b><br>on your own data</td>
<td align="center"><b>Bring your own key</b><br>self-hosted</td>
</tr>
</table>

Wayfinder looks at a prompt's structure (length, headings, lists, code) and its
wording (proofs, math, hard constraints), then tells you whether to send it to a
small local model or a larger cloud model. It decides in microseconds, runs
offline, and never calls another model to make the call. You get a score and a
recommendation, and your gateway or client decides where the request goes.

Cheap prompts stay local and hard ones go to the expensive model, so you stop
paying top-tier prices for "summarize this" and "fix my typo."

## Quickstart

Put Wayfinder in front of your models. Your app keeps speaking the OpenAI API;
you only change the `base_url`.

1. Build the Rust CLI:

   ```bash
   cargo build --release --bin wayfinder-router
   ```

2. Scaffold a starter config:

   ```bash
   ./target/release/wayfinder-router init --preset hybrid
   ./target/release/wayfinder-router doctor
   ```

   `init` writes `wayfinder-router.toml` and `.env.example`. It stores only
   environment variable names in config, never raw secrets.

3. Set the key variables named by your config, then start the gateway:

   ```bash
   export OPENAI_API_KEY="sk-..."
   ./target/release/wayfinder-router serve --port 8088
   ```

4. Point an OpenAI-compatible client at the gateway:

   ```bash
   export OPENAI_BASE_URL="http://localhost:8088/v1"
   export OPENAI_API_KEY="unused"
   ```

Requests with `model: "auto"` are scored and routed. Use `model: "local"`,
`model: "cloud"`, `model: "prefer-local"`, or `model: "prefer-hosted"` to pin a
single request.

Check that it is running:

```bash
curl -s http://localhost:8088/healthz

curl -s -D - -o /dev/null http://localhost:8088/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"auto","messages":[{"role":"user","content":"hi"}]}' \
  | grep -i x-wayfinder-router
```

No backends yet? Use `serve --dry-run` to return routing decisions without
calling upstream models.

## Try The Demo

Run a terminal decision preview with no keys and no upstream calls:

```bash
cargo run --bin wayfinder-router -- chat --dry-run --why "Summarise this in one sentence."
```

![Wayfinder terminal chat, a routed prompt, the decision, the reply, and the running savings](docs/tui-chat.png)

Run the browser demo from the Rust gateway:

```bash
cargo run --bin wayfinder-router -- webchat --dry-run --port 8088
# open http://127.0.0.1:8088/demo
```

`webchat` starts the same gateway as `serve`, points the browser at `/demo`, and
can run in `--dry-run` mode for a decision-only public demo.

## Install

Wayfinder is a Rust workspace. The supported local install path is the Rust
binary built from this repository:

| command | what you get |
| --- | --- |
| `cargo build --release --bin wayfinder-router` | the complete Rust CLI |
| `./target/release/wayfinder-router route prompt.md` | offline prompt scoring |
| `./target/release/wayfinder-router serve --port 8088` | OpenAI/Anthropic-compatible routing gateway |
| `./target/release/wayfinder-router chat` | terminal chat UI |
| `./target/release/wayfinder-router ui --port 8099` | local explain, calibrate, configure, onboard, and recalibrate UI |
| `docker build -t wayfinder-router .` | container image for the Rust gateway binary |

The release workflow packages the Rust CLI. The Dockerfile builds and runs the
same binary.

## Command Reference

Every command below is served by the Rust `wayfinder-router` binary.

| command | purpose |
| --- | --- |
| `route <prompt\|->` | Score a prompt from a file or stdin and recommend a model. Use `--json`, `--explain`, or `--threshold <0..1>`. |
| `calibrate <dataset>` | Turn labeled JSONL (`{"text": "...", "label": "local"}`) into routing config. Supports `--mode threshold|tiers|classifier`, cost-aware objectives, custom weights, and `--out`. |
| `serve` | Run the OpenAI/Anthropic-compatible gateway. Supports `--host`, `--port`, `--dry-run`, and `--timeout`. |
| `chat [prompt]` | Open the terminal chat UI, or print a scriptable transcript when a prompt or stdin is supplied. Supports `--dry-run`, `--why`, `--theme`, `--base-url`, `--thread-dir`, and `--no-stream`. |
| `ui` | Run the local web UI for explain, calibration, config editing, onboarding, and recalibration. |
| `webchat` | Run the gateway and open `/demo`. Supports the same bind and dry-run options as `serve`, plus `--no-open`. |
| `onboard <prompts>` | A/B configured gateway arms, ask which output is good enough, append labels, and optionally emit calibrated config with `--calibrate`. |
| `judge <prompts>` | Auto-label prompts by comparing two tiers behind trust checks. Supports `--gold`, `--kappa-floor`, `--folds`, `--limit`, and `--save-comparisons`. |
| `recalibrate` | Re-fit a config from the feedback log, preserving gateway settings. Supports `--log`, `--out`, `--mode`, and `--min-labels`. |
| `init` | Scaffold `wayfinder-router.toml` and `.env.example` from `hybrid`, `openai`, or `gemini` presets. |
| `doctor` | Check config discovery, routing mode, gateway models, and key readiness. |
| `keys new` | Mint a virtual API key for `[gateway.keys.<id>]`; only the hash is stored in config. |

Use `wayfinder-router --help` or `wayfinder-router <command> --help` for exact
flags.

## How It Works

Wayfinder sits behind whatever OpenAI-compatible client you already use:

```text
  your client
       |
       v
  Wayfinder gateway scores, picks a model
       |
       |-- low score  -> local endpoint
       |-- high score -> hosted endpoint
       |
       v
  response returns through the same client,
  with x-wayfinder-router-* headers
```

The interface in front is yours: a chat GUI, IDE assistant, agent framework, CLI,
or application code. The local and hosted models are just upstream endpoints with
model ids and key environment variables.

The score is computed, not guessed by a second model. Wayfinder scans prompt
structure and optional lexical cues into a `0.0` to `1.0` value and compares it to
your threshold or tier rules. Same prompt, same config, same route.

## Configure Routing

Wayfinder reads `wayfinder-router.toml`, found by walking up from the current
directory. There are three routing modes, in precedence order: classifier, tiers,
then threshold.

Binary routing uses one cut:

```toml
[routing]
threshold = 0.6
weights = { word_count = 4.0, list_item_count = 2.5 }
```

Tiered routing maps score bands to model names:

```toml
[[routing.tiers]]
min_score = 0.0
model = "llama-3b"

[[routing.tiers]]
min_score = 0.6
model = "claude-cloud"
```

Gateway models map those route names to upstream endpoints:

```toml
[gateway.models.local]
base_url = "http://localhost:11434/v1"
model = "llama3.2"

[gateway.models.cloud]
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
api_key_env = "OPENAI_API_KEY"
```

Add `api_key_cmd` when you want the gateway to read a secret from a local secret
store at startup. The command output is held in memory and is never written back
to config.

## Calibrate On Your Data

The cut is a proxy, so tune it against your own traffic. `calibrate` reads a
labeled JSONL dataset and prints a config fragment:

```bash
wayfinder-router calibrate data.jsonl --mode threshold
wayfinder-router calibrate data.jsonl --mode tiers --models local,cloud
wayfinder-router calibrate data.jsonl --mode classifier --out wayfinder-router.toml
```

Use a cost-aware objective when you care about savings as well as accuracy:

```bash
wayfinder-router calibrate data.jsonl --mode threshold --objective knee \
  --costs local=0.2,cloud=1.0 \
  --weights reasoning_term_count=5,math_symbol_count=3,constraint_term_count=1.5
```

The calibrator runs offline. Cost is metadata for choosing a cut and reporting
savings, not a per-request input to the live route.

## Learn From Feedback

Collect labels, then recalibrate:

```bash
curl http://localhost:8088/v1/feedback \
  -H "Content-Type: application/json" \
  -d '{"text":"...","label":"cloud"}'

wayfinder-router recalibrate --min-labels 50
```

Bootstrap labels manually with `onboard`:

```bash
wayfinder-router onboard prompts.jsonl --arms local,cloud --calibrate > wayfinder-router.toml
```

Or let `judge` compare two configured arms and emit labels only when trust checks
pass:

```bash
wayfinder-router judge prompts.jsonl --arms local,cloud --gold gold.jsonl > wayfinder-router.toml
```

`judge` runs upstream models through the gateway layer. The scorer remains pure
and offline.

## Local UI

Run the Rust web UI when you want a browser surface for tuning:

```bash
wayfinder-router ui --port 8099
# open http://127.0.0.1:8099
```

The UI exposes the same core operations as the CLI:

- explain a prompt and see per-feature contributions
- calibrate a pasted dataset
- validate and save `wayfinder-router.toml`
- run onboarding against configured gateway models
- recalibrate from the feedback log

## Deploy And Integrate

Build and run the Rust gateway as a service, sidecar, or standalone container:

```bash
docker build -t wayfinder-router .
docker run --rm -p 8088:8088 -v "$PWD/data:/data" wayfinder-router

# or
docker compose up gateway
```

Anything that speaks the OpenAI API can point at `http://localhost:8088/v1`,
including chat UIs, IDE assistants, agent frameworks, and command-line tools. See
[Integration recipes](docs/integrations.md) for concrete setup examples.

Claude Code speaks Anthropic's Messages API, so the gateway also exposes
`POST /v1/messages`. Point `ANTHROPIC_BASE_URL` at the gateway root:

```bash
export ANTHROPIC_BASE_URL="http://localhost:8088"
export ANTHROPIC_API_KEY="unused"
claude
```

The gateway exposes:

| surface | purpose |
| --- | --- |
| `GET /healthz` | health and configured model ids |
| `POST /v1/chat/completions` | OpenAI-compatible chat routing |
| `POST /v1/messages` | Anthropic Messages adapter |
| `GET /v1/models` | routing directives and configured model names |
| `POST /v1/feedback` | label collection for recalibration |
| `GET /router` and `/router/recent` | recent routing decisions, without prompt text in the dashboard |
| `GET /metrics` | Prometheus metrics |
| `GET /demo` | browser demo |

## Repository Layout

```text
wayfinder-router/
  crates/wayfinder-cli/       top-level Rust CLI
  crates/wayfinder-core/      scoring, config, calibration, feedback,
                              onboarding, sufficiency, profiles, and virtual keys
  crates/wayfinder-gateway/   OpenAI/Anthropic gateway, demo, metrics,
                              feedback endpoint, key checks, and recalibration
  crates/wayfinder-tui/       terminal chat UI
  crates/wayfinder-ui/        local browser UI for tuning
  tests/fixtures/contracts/   frozen golden snapshots for Rust parity tests
  docs/                       user docs and operational notes
  decisions/                  ADRs
  designs/                    design notes
  examples/                   config and integration examples
  Dockerfile                  Rust gateway image
  docker-compose.example.yml  local gateway compose example
```

## Test

```bash
cargo fmt --check
cargo clippy --all-targets --no-deps
cargo test
```
