# Contributing to ping_monitor

Thanks for considering a contribution. This is a small project with a focused scope, so the rules are correspondingly small.

## Scope

`ping_monitor` is intentionally narrow: a local terminal tool that tells you whether *your* internet connection is up, learns what's normal for your link, and chimes on real state transitions.

Project rules of thumb (lifted from the lazy-senior playbook):

- **No abstractions that weren't asked for.**
- **No new dependency if the stdlib or a platform feature covers it.**
- **Never use the network to alert about no-network.** Local notifications + audio only.
- **Mark intentional shortcuts** with a `// ponytail:` comment naming the ceiling and the upgrade path.
- **Every non-trivial change leaves a check behind** — a test, an assert-based self-check, the smallest thing that fails if the logic breaks. No new test frameworks or fixtures.

If your feature needs the network to fire alerts, requires a database, or adds a GUI layer, it probably belongs in a fork rather than this repo.

## Setup

```
git clone https://github.com/clasanch/ping_monitor
cd ping_monitor
cargo build
cargo test
```

Rust 1.74+ (see `rust-version` in `Cargo.toml`).

## Before opening a PR

1. **`cargo fmt`** — formatting must be clean.
2. **`cargo clippy --all-targets -- -D warnings`** — zero warnings. CI enforces this.
3. **`cargo test`** — all tests pass. If you add logic, add a test.
4. **Don't add comments** unless a `// ponytail:` shortcut ceiling needs documenting, or the code genuinely isn't self-explanatory. The codebase is deliberately comment-light.
5. **Commits**: small, focused, with a clear message. Don't squash unrelated changes into one commit. Don't rewrite history on `main`.

## Commit message style

```
area: short summary in present tense

Body explaining *why*, not *what*. Reference issue numbers if relevant.
```

Examples from this repo's history:

```
fix(header): state label truncado — split 60/40 → 70/30 + padding tight
feat: session resúmenes históricos — uptime%, MTTR, outages, top-3 spikes
refactor: extract step_dwell, fix pooled_last, validate config
```

## Reviewing

A maintainer will look at your PR within a week. Small PRs (tests, typo fixes, dependency bumps, single-feature additions) usually get merged quickly. Large architectural changes will be discussed before merging — please open an issue first to discuss the design.

## Licensing

By contributing you agree that your changes are licensed under the MIT license, same as the rest of the project.
