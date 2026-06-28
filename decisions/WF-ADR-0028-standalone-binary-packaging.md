---
schema_version: 1
id: WF-ADR-0028
type: decision
tags: [packaging, distribution, cli, gateway, binary]
---

# WF-ADR-0028: Rust Gateway and TUI Binaries for No-Python Users

## Status

Accepted, partially implemented

> Amendment: the Rust port supersedes the earlier Python freezer direction for
> the gateway and TUI. The buildable binaries are `wayfinder-router`,
> `wayfinder-router-gateway`, and `wayfinder-router-tui`; the supported public
> Rust surfaces are the gateway service and TUI. A full cross-platform release
> artifact matrix remains follow-up work.

## Category

Technical

## Context

WF-ROADMAP-0004 (Initiative 3) wanted a downloadable executable so people with
**no Python toolchain** could run the gateway and demo. The Rust port now provides
that direction directly for the supported runtime surfaces: `serve` for the
OpenAI-compatible gateway and `chat` for the TUI. The Python/PyPI package remains
available for the Python API and commands not yet ported to Rust.

## Decision

Build Rust executables for the gateway and TUI. The root `wayfinder-router`
binary dispatches `serve` and `chat`, while crate-specific binaries remain useful
for internal packaging and smoke tests. The Docker image builds and runs the Rust
`wayfinder-router serve` binary instead of installing the Python gateway extra.

The tag workflow builds and uploads a Linux `wayfinder-router` workflow artifact
as the first binary artifact path. A full GitHub Release matrix for Linux,
macOS, and Windows remains a roadmap item.

## Consequences

### Positive

- Non-Python users can build or run the Rust gateway and TUI without the Python
  package.
- The Docker runtime image is smaller in scope and does not install Python gateway
  dependencies.
- Tag builds now exercise the Rust binary path alongside the PyPI publish.

### Negative

- A full per-OS release matrix still needs to be designed and maintained.
- Python-only commands still require the PyPI package until they are ported or
  explicitly retired.

### Risks

- **Release coverage gap.** A Linux workflow artifact is not the same as a
  polished release matrix. Mitigation: keep the current artifact as a smoke path
  and add cross-platform release assets as a follow-up.
- **Surface confusion.** The Rust workspace has internal crates, but the public
  promise is not a Rust library. Mitigation: docs name only `serve` and `chat` as
  supported Rust surfaces.
- **Signing/AV friction.** Unsigned binaries trip macOS Gatekeeper / Windows
  SmartScreen. Mitigation: code-signing/notarization is explicitly a non-goal for
  the first cut (WF-ROADMAP-0004) and revisited on demand.

## Alternatives Considered

### Python freezers

PyInstaller and Nuitka could bundle the Python gateway extra, but that now adds
more moving parts than the Rust gateway path. Keep them rejected unless a future
Python-only command needs a no-Python wrapper.

### Docker only

The Rust Docker image is the operator path, but a container still requires Docker.
It does not replace downloadable binaries for users who want a local executable.

### shiv / pex (zipapp)

Simpler to build, but the produced zipapp requires Python on the target, which
fails the no-Python goal. Usable only if the goal is relaxed to Python present,
which the PyPI path already covers.

## Success Measures

- `cargo build --release --bin wayfinder-router` produces a binary that runs
  `serve` and `chat` with no Python package installed.
- The Docker image runs the Rust gateway binary.
- The release workflow builds and smoke-tests at least one Rust CLI artifact on
  tag.
- Follow-up: attach a cross-platform binary matrix to GitHub Releases.

## Related

- WF-ROADMAP-0004 (Initiative 3 — the binary initiative this decides)
- WF-ADR-0008 (packaging and integration; the `[gateway]` extra and release path)
- WF-ADR-0020 (the `/demo` page the Rust gateway serves)
- WF-ADR-0004 (the OpenAI-compatible gateway being bundled)
- WF-ADR-0001 (the zero-dependency core preserved; only the wrapper is new)
