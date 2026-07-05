---
name: sync-python-fork-to-rust
description: Use when a repository is a Rust rewrite or fork of an upstream Python project and the user wants to catch the fork up to upstream/main, audit upstream Python changes, port those Python-related behaviors into Rust, verify parity, and optionally land the result onto local or remote main.
---

# Sync Python Fork To Rust

## Core Rule

Treat upstream Python history as requirements, not as files to copy. Preserve the Rust implementation, record upstream ancestry, inspect every upstream Python-related delta, port the behavior into the Rust modules and tests that own the same surface, then verify before landing.

Respect the repo's local instructions. Direct pushes to `main` require either explicit user approval for this run or a project-local rule that permits them.

## Workflow

1. Inspect live state first:

```bash
git status --short --branch
git remote -v
gh repo view --json nameWithOwner,parent,defaultBranchRef,url
```

If the fork parent exists but no `upstream` remote is configured, add it and fetch:

```bash
git remote add upstream git@github.com:<parent-owner>/<parent-repo>.git
git fetch origin
git fetch upstream
```

2. Establish the comparison range:

```bash
git merge-base origin/main upstream/main
git rev-list --left-right --count origin/main...upstream/main
git log --oneline --reverse <fork-point>..upstream/main
```

Use the merge base as the fork point unless live history proves a different base.

3. Work on a branch unless the user explicitly asks to operate on `main`:

```bash
git switch -c <owner>/<short-sync-name>
```

If the Rust rewrite intentionally replaced the Python tree, do not perform a content merge from upstream. Record upstream ancestry while keeping the Rust tree:

```bash
git merge -s ours upstream/main -m "feat: sync upstream router changes"
```

Then implement the Rust ports on top of that ancestry merge. If a normal merge is appropriate for this repo, use it only after confirming it will not reintroduce obsolete Python implementation files.

4. Build a Python-delta checklist from source, not memory:

```bash
git diff --name-status <fork-point>..upstream/main -- '*.py'
git log --oneline --reverse <fork-point>..upstream/main -- '*.py'
```

Inspect the relevant upstream files and tests at the specific commits that changed them:

```bash
git show <commit>:<path>
git show --stat --oneline <commit> -- '*.py' 'tools/*' 'benchmarks/*'
```

Group the checklist by behavior, for example:

- gateway or service runtime behavior
- CLI command surface and error handling
- TUI or UI routing controls
- judge, sufficiency, calibration, pricing, thread, or cache logic
- detector, benchmark, validation, or golden-helper code
- Python tests that encode expected behavior
- Python package version or changelog changes, only when the Rust project has equivalent release metadata

5. Port each group to the Rust owner module:

- Find existing Rust owners with `rg` and nearby tests before adding files.
- Prefer existing module boundaries and fixture patterns.
- Convert upstream Python tests into Rust unit, integration, or contract tests.
- For upstream Python scripts such as benchmark or golden helpers, prefer deterministic Rust core modules and tests over shipping a copied Python script.
- For service managers, keep pure unit generation in a library module and keep live install/uninstall/status behavior in the CLI layer.
- Avoid committing live-looking secrets in fixtures. Assemble token probes from fragments when testing detectors.

Useful audit searches:

```bash
rg -n "offline|service|launchd|systemd|sufficien|terse|golden|detector|ai4privacy|gitleaks|sticky|scope|thread|rate limit|tool call|virtual|vkey|pricing|cache|auth" crates tests docs README.md
rg -n "AKIA[A-Z0-9]{16}|ASIA[A-Z0-9]{16}|ghp_[A-Za-z0-9]{36}|xox[baprs]-\\d{10,}-[A-Za-z0-9-]{10,}" crates tests README.md .github -g '*.rs' -g '*.md' -g '*.json' -g '*.toml'
```

6. Verify the port at the right scope:

```bash
cargo fmt --check
cargo clippy --all-targets --no-deps
cargo test
```

Run targeted tests while developing, but use the full verification set before pushing or declaring completion. If clippy emits warnings with exit code 0, report them as warnings, not failures.

7. Prove completion before landing:

```bash
git status --short --branch
git rev-list --left-right --count upstream/main...HEAD
git rev-list --left-right --count origin/main...HEAD
git merge-base --is-ancestor upstream/main HEAD
```

The branch should contain `upstream/main`, include Rust ports for every Python-delta checklist item, have full verification, and have no unrelated unstaged work.

## Landing Options

For a PR path:

1. Push the feature branch.
2. Draft the PR title and body.
3. Wait for user approval before creating the PR if local instructions require approval.

For direct local-main landing when explicitly requested:

```bash
git switch main
git merge --ff-only <feature-branch>
git status --short --branch
git rev-list --left-right --count origin/main...main
```

If fast-forward fails because `main` advanced, rebase the feature branch onto current `main`, re-run verification, then retry the fast-forward merge.

Before pushing `main`, run the secret scan and final verification. Then:

```bash
git push origin main
git fetch --prune origin
git rev-list --left-right --count origin/main...main
```

Delete the feature branch only after it is merged and pushed, and only if the user asked for deletion or it is clearly part of the requested cleanup:

```bash
git branch --merged main | rg '<feature-branch>'
git branch -d <feature-branch>
git push origin --delete <feature-branch>
```

## Final Report

State:

- the final branch and remote position
- the upstream commit contained
- the Python-delta categories ported
- verification commands and outcomes
- whether any feature branch remains
- any existing warnings or unverified surfaces
