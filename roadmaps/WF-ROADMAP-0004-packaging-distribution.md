---
schema_version: 1
id: WF-ROADMAP-0004
type: roadmap
tags: [v0.3.0, packaging, distribution, cli, gateway, demo, desktop]
---

# Roadmap: Packaging & distribution (one-command run → native app)

## Status

In progress

## Context

v0.2.0 shipped the decision-first `/demo` UI and the `wayfinder-router webchat`
launcher (WF-ADR-0020), so the product is finally visible. The Rust port now
provides buildable gateway and TUI binaries (`serve` and `chat`), and the current
Dockerfile runs the Rust gateway. The Python/PyPI package remains available for
the Python API and Python-only commands that have not moved to Rust.

This roadmap closes that gap. It is the "next version" follow-on deliberately
deferred during the chat-demo work: turn "clone the repo and install" into "run
one command — or double-click an app." It is distribution-layer only; the
stdlib-only deterministic core (WF-ADR-0001) is never touched, and the base wheel
stays zero-dependency. Each initiative is independent and ships on its own, so the
cheap wins land first and the heavier desktop work never blocks them.

## Outcomes

- Anyone can run the Wayfinder demo with **one command**, regardless of toolchain:
  `uvx`/`pipx` for legacy Python users, a Rust container for operators, a Rust
  binary or a native window for everyone else.
- The "boring on purpose, vendor-the-file" ethos survives distribution: the base
  install stays zero-dependency; every heavier packaging path is **opt-in and
  additive**, never a tax on the core.
- The eventual product story (a native, double-click Wayfinder) is de-risked and
  specified *without* prematurely committing to maintain a full desktop app.

## Initiatives

Sequenced by risk; each is independent and ships on its own version line. They map
onto the three audiences for the gateway: Python users, operators, and
non-technical/desktop users.

### Initiative 1 — One-command run via uvx / pipx (`v0.3.0`, lowest risk)

The package already exposes the `wayfinder-router` console script and the `chat`
subcommand (WF-ADR-0020). Make "try it without installing" a single line —
`uvx --from "wayfinder-router[gateway]" wayfinder-router webchat` (and the `pipx run`
equivalent) — so an ephemeral environment pulls the `[gateway]` extra, boots the
gateway, and opens `/demo`. Additive, essentially no new code: verify the entry
point resolves the extra under uvx/pipx ephemeral envs and lead the README's "Try
the demo" with it. Ship first.

### Initiative 2 — Polish the container path (`v0.3.0`, low risk)

Partially complete. The current Dockerfile builds the Rust `wayfinder-router`
binary and runs `wayfinder-router serve --host 0.0.0.0 --port 8088`. Remaining
work: add an image healthcheck, publish the image to GHCR on tag, and document the
pull-and-run image once it exists.

### Initiative 3: Rust standalone binaries (`v0.3.x`, medium risk)

Partially complete. For users with no Python at all, the Rust workspace builds a
`wayfinder-router` executable that supports the gateway service (`serve`) and TUI
(`chat`). The tag workflow builds and smoke-tests a Linux artifact. Remaining
work: publish a cross-platform Linux/macOS/Windows artifact matrix on GitHub
Releases and decide signing/notarization posture.

### Initiative 4 — Native desktop window (`v0.3.x`, highest scope; evaluate Pake)

Wrap the `/demo` UI in a real app window so it feels like a product, not a
localhost tab. Two candidate paths — spike both, pick one, and record the choice as
an ADR:

- **(a) pywebview** — Python-native; launches the gateway in-process and opens the
  demo in the OS webview. Single toolchain, reuses `chat`, no new build system.
- **(b) Pake** (`https://github.com/tw93/pake`) — a Tauri/Rust wrapper that turns
  the localhost URL into a tiny (~5 MB) native macOS/Windows/Linux app. Lightest
  binary and the most "instant native app," but adds a Rust/Tauri build step
  outside the Python toolchain.

Recommendation: spike **Pake** first for the "native app around the URL we already
serve" win; fall back to **pywebview** if staying single-toolchain matters more
than binary size. The decision (and the trade-off that drove it) becomes a new ADR.

## Constraints

- **WF-ADR-0001 boundary preserved.** Packaging is distribution-layer only. The
  Rust gateway and TUI are supported runtime surfaces, while the Python/PyPI
  package remains the legacy Python API and command surface.
- **Secrets stay in the environment** (WF-ADR-0008): no API keys baked into images
  or binaries.
- **No CLI regressions.** `serve` stays the raw/headless gateway and `chat` stays
  the terminal TUI. Packaging wraps these, it does not change them.

## Non-Goals

- A hosted/SaaS routing service — against the self-hostable, BYO-key posture
  (WF-ADR-0008).
- Code-signing/notarization and app-store distribution pipelines (revisit only on
  demand).
- Desktop auto-update infrastructure.
- Framework-specific adapters (already recorded as future in WF-ADR-0008; the
  gateway covers those clients via `base_url`).

## Assumptions

- The thing being packaged is the OpenAI-compatible gateway plus the `/demo` UI.
  No API or routing changes are required. The Rust launch path is
  `wayfinder-router serve`; the legacy Python browser launcher remains
  `wayfinder-router webchat`.
- Users fall into three buckets: Python (pip/uvx), operators (Docker), and
  non-technical/desktop (binary/app). The four initiatives cover them.
- `uvx`/`pipx` can resolve the `[gateway]` extra for ephemeral runs.
- A thin webview/URL wrapper is sufficient for the "native app" feel; a bespoke
  desktop shell is not needed for the demo.

## Success Measures

- **I1:** on a clean machine with only `uv` installed,
  `uvx --from "wayfinder-router[gateway]" wayfinder-router webchat` opens the demo.
- **I2:** `docker run -p 8088:8088 <image>` serves the Rust gateway, the
  `HEALTHCHECK` reports healthy, and the image is published on tag.
- **I3:** a downloaded Rust binary runs `wayfinder-router serve` and
  `wayfinder-router chat` on each target OS with no Python present.
- **I4:** a double-clicked app opens the demo in a native window, and the chosen
  approach (Pake vs pywebview) is recorded in an ADR with binary-size and
  build-complexity rationale.

## Risks

- **Toolchain creep (I3/I4).** Rust release targets and Tauri each pull build
  complexity and a per-OS CI matrix. Mitigation: keep every initiative independent
  and shippable alone; source builds, PyPI, and Docker remain available while
  binary/app artifacts mature.
- **Extra-resolution surprises (I1).** `uvx`/`pipx` may not surface the `[gateway]`
  extra cleanly. Mitigation: test on a clean env in CI; document the explicit
  `--from "wayfinder-router[gateway]"` form.
- **Artifact polish (I3).** A raw workflow artifact is not a signed,
  cross-platform release. Mitigation: publish the current Linux artifact only as a
  smoke path until the release matrix, naming, and signing posture are settled.
- **Desktop maintenance tail (I4).** A native app is one more surface to keep
  working across OS updates. Mitigation: prefer the thinnest wrapper (Pake/pywebview)
  over a bespoke shell; treat it as a demo accelerator, not a second product — the
  same discipline WF-ADR-0020 applied to the demo UI.

## Related Decisions

- WF-ADR-0008 (packaging & integration — this roadmap delivers its "Deployment"
  line beyond the build-it-yourself container)
- WF-ADR-0020 (decision-first demo UI; the Rust gateway now serves the same `/demo`
  page)
- WF-ADR-0004 (the OpenAI-compatible gateway being shipped)
- WF-ADR-0001 (the deterministic boundary preserved throughout)
- WF-ROADMAP-0002 (core hardening — the engine work this distribution effort sits on)
