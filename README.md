# ping_monitor

Real-time terminal network monitor for your own internet connection. Watches multiple targets in parallel, learns what's normal for the link you're on, and chimes only on genuine connection-level state transitions — not on individual target blips.

Distinguishes "the internet is down for me" from "a single target is unreachable" using **multi-target consensus** plus a **DNS differential matrix**, then plays pleasant audio cues that map to state changes.

## Why

Standard `ping -t` floods you with packets and tells you nothing about *whether your actual connection is broken*. Other monitors alert over the network — but the rule of an outage notifier is: **never use network traffic to alert about no-network**. This tool runs locally, learns your link's normal latency/jitter floor, and notifies via local audio + desktop notifications.

## Features

- **Multi-target consensus** — pings 3 primaries (Cloudflare/Google/Quad9), majority vote decides link state. Loss on one target ≠ outage.
- **Adaptive baseline** — learns p90 latency and jitter from the last 5 minutes. Thresholds scale to *your* link, not hardcoded numbers.
- **DNS matrix** — 3 resolvers × 3 domains (differential diagnosis). Tells apart "this site is down" from "DNS is broken" from "my resolver is unreachable".
- **Asymmetric hysteresis** — bad streak of 3 to trip; good streak of 8 to recover. Anti-flap with a 15s dwell.
- **Session summaries** — uptime %, MTTR, outages/recoveries counters, top-3 latency and jitter spikes with unix timestamps. Auto-exported as TSV on every state transition.
- **Hardware-accurate audio** — pitched chimes generated via `rodio` synth (E6 shimmer for degraded, ADSR chord for down/recover), fading on transitions only. Mute with `m` cuts everything.
- **Clean TUI** — `ratatui` 256-color, pooled charts, heatmap history, sparkline loss.

## Install

Requires Rust 1.70+ (uses edition 2021).

```
cargo build --release
./target/release/ping_monitor
```

## Keys

| Key | Action |
|-----|--------|
| `m` | toggle sound |
| `r` | reset session stats |
| `e` | export session to TSV |
| `t` | traceroute from first up primary |
| `q` / Ctrl-C / SIGTERM | quit |

## Configuration

All knobs via env vars; defaults are sensible.

```
PM_TARGETS=cf:1.1.1.1:443,gg:8.8.8.8:443,q9:9.9.9.9:443
PM_EXTRAS=local:192.168.1.1:80,work:vpn.example.com:443
PM_DNS_NAMES=www.google.com,www.cloudflare.com,www.amazon.com
PM_TIMEOUT_MS=1500
PM_REMINDER_S=30
```

Internals clamp everything to safe ranges on startup; you can't break it with bad env values.

## Layout

```
┌ ping_monitor [cf:1.1.1.1:443, gg:8.8.8.8:443, q9:9.9.9.9:443] dns 3×3 remind 30s ● Online ┐ Quality Score
├ Latency                                  │ Jitter                                            │
│ pooled, median-of-3, warn reference line  │ pooled, same treatment                            │
├ Targets cf ● gg ● q9 ● / Extras / Loss   │ Streaks: uptime%, MTTR, top-3 spikes              │
├ Heatmap (1h, 30s buckets)                │ Events (24 ring, newest first)                   │
└ DNS matrix 3×3                           │                                                  │
```

## Tests

```
cargo test
```

20 tests covering config validation, hysteresis, pooled stats, baseline percentile, session accrual, ticks.

## Screenshots

<!-- Add screenshots here after first public release. Suggested: -->
<!-- - Normal state with all targets up -->
<!-- - Degraded state showing hysteresis in progress -->
<!-- - Down state with event log -->
<!-- Use a fenced block with image links, e.g. ![normal](docs/normal.png) -->

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The project is intentionally small in scope — please read the "Scope" section before opening a PR with a large feature.

## License

MIT — see [LICENSE](LICENSE).
