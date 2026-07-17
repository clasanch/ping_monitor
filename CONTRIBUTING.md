# Contributing to ping_monitor

Thanks for considering a contribution. This is a small project with a focused scope, so the rules are correspondingly small.

## Scope

`ping_monitor` is intentionally narrow: a local terminal tool that tells you whether *your* internet connection is up, learns what's normal for your link, and chimes on real state transitions.

Project principles:

- **YAGNI** — don't build what isn't needed yet.
- **Prefer the stdlib and platform features** over new dependencies.
- **Never use the network to alert about no-network.** Local audio + desktop notifications only.
- **Tests over comments** — non-trivial logic should have a check (a unit test, an assert, the smallest thing that fails if the logic breaks).
- **Comment-light code** — names and types do the explaining. Comments are for *why*, not *what*, and only when the code genuinely isn't self-explanatory.

If your feature needs the network to fire alerts, requires a database, or adds a GUI layer, it probably belongs in a fork rather than this repo.

## Setup

```
git clone https://github.com/clasanch/ping_monitor
cd ping_monitor
cargo build
cargo test
```

Rust 1.85+ (see `rust-version` in `Cargo.toml`).

Linux builds need ALSA headers for audio output:

```
sudo apt-get install libasound2-dev pkg-config
```

## Before opening a PR

1. **`cargo fmt`** — formatting must be clean.
2. **`cargo clippy --all-targets -- -D warnings`** — zero warnings. CI enforces this.
3. **`cargo test`** — all tests pass. If you add logic, add a test.
4. **Commits**: small, focused, with a clear message. Don't squash unrelated changes into one commit. Don't rewrite history on `main`.

## Commit message style

```
area: short summary in present tense

Body explaining *why*, not *what*. Reference issue numbers if relevant.
```

Examples:

```
fix(header): state label truncated on narrow terminals
feat: session summaries — uptime%, MTTR, outages, top-3 spikes
refactor: extract step_dwell, fix pooled_last, validate config
```

## Reviewing

PRs are reviewed within a week. Small PRs (tests, typo fixes, dependency bumps, single-feature additions) usually merge quickly. Large architectural changes will be discussed before merging — please open an issue first to discuss the design.

## Licensing

By contributing you agree that your changes are licensed under the MIT license, same as the rest of the project.
