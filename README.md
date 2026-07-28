# ping_monitor

Real-time terminal network monitor for your own internet connection. Watches multiple targets in parallel, learns what's normal for the link you're on, and chimes only on genuine connection-level state transitions — not on individual target blips.

Distinguishes "the internet is down for me" from "a single target is unreachable" using **multi-target consensus** plus a **DNS differential matrix**, then plays pleasant audio cues that map to state changes.

Uses general recursive signal-detection ideas as engineering inspiration. `ping_monitor` is not an earthquake-warning system and is not affiliated with the cited projects or authors.

## Why

Born during a stormy afternoon: the power went out, the fiber link died, and the household fell back on a single shared mobile hotspot. When video calls stutter and pages time out, the human holding the phone gets blamed — "the Wi-Fi is bad" — even when the real cause is upstream: a saturated cell tower, rain fade on the radio link, or no nearby antennas to hand off to.

`ping_monitor` exists to settle that argument with data. It runs locally, learns what *your* link's normal latency and jitter look like (whether you're on fiber, a hotspot, or anything in between), and chimes on real connection-level state transitions — not on every individual packet blip. So you can tell, with evidence, whether the problem is the Wi-Fi, the ISP, the weather, or just the cell tower down the road buckling under load.

Other monitors alert through the network. An outage notifier that needs the network to ring the bell is useless the moment the network is the outage. This one uses local audio and desktop notifications only.

## Features

- **Multi-target consensus** — pings 3 primaries (Cloudflare/Google/Quad9), strict majority vote (`n/2 + 1`) decides link state. Loss on one target ≠ outage. Even-target ties produce an inconclusive result — state is held, recovery is paused.
- **Adaptive baseline** — learns p90 latency and jitter from the rolling sample window. Thresholds scale to *your* link, not hardcoded numbers. Latency and jitter baselines are tracked independently.
- **DNS matrix** — 3 resolvers × 3 domains (differential diagnosis). Tells apart "this site is down" from "DNS is broken" from "my resolver is unreachable".
- **Insights panel** — probable-cause diagnosis from live metrics (router vs ISP vs Wi-Fi vs DNS vs the remote site), ranked with evidence and a suggested action. Always on, right under the header.
- **Gateway auto-detection** — default route discovered on macOS/Linux/Windows and probed as extra `[gw]`. An actively rejected connection (refused or reset) counts as reachable — a TCP RST confirms that the network path responded, feeding the router-vs-ISP insight with zero configuration. Re-detected every 15s: roam to another network or start a VPN and the probe follows the new gateway live, logging the change.
- **Connection recovery** — stepwise recovery through Degraded (one 15s dwell per step). A noisy minority target does not reset recovery while a strict recovering majority remains.
- **Optional isolated-target cue** — one quiet, rate-limited sound when exactly one target degrades while the connection stays Up. Enable with `PM_PREALERT=1`.
- **Session summaries** — uptime %, MTTR, outages/recoveries counters, top-3 latency and jitter spikes with unix timestamps. Auto-exported as TSV on every state transition.
- **Hardware-accurate audio** — pitched chimes generated via `rodio` synth (E6 shimmer for degraded, ADSR chord for down/recover), fading on transitions only. Mute with `m` cuts everything.
- **Clean TUI** — `ratatui` 256-color, pooled charts with gap-free rendering, heatmap history, sparkline loss.

## Install

Requires Rust 1.85+ (see `Cargo.toml` for exact `rust-version`).

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
PM_PREALERT=1
PM_RECURSIVE_MODE=legacy
```

Internals clamp everything to safe ranges on startup; you can't break it with bad env values.

`PM_EXTRAS` is optional for gateway monitoring: the default gateway is auto-detected at startup and probed as extra `[gw]`, with re-detection every 15s so network/roaming changes are followed live. Auto-detection (and re-detection) is skipped if you already list the gateway IP yourself.

`PM_PREALERT=1` enables an isolated-target anomaly cue (off by default). When exactly one target degrades while the connection stays Up, a quiet rate-limited sound plays. Global cooldown: 60 seconds.

`PM_RECURSIVE_MODE` selects the recursive detector mode: `legacy` (default, existing classifier only), `shadow` (compute recursive latches for diagnostics without affecting state), or `hybrid` (recursive latches may produce Degraded alongside legacy). Changing the default to `hybrid` requires a separate decision after shadow validation.

## Layout

```
┌ ping_monitor [cf:1.1.1.1:443, gg:8.8.8.8:443, q9:9.9.9.9:443] dns 3×3 remind 30s ● Online ┐ Quality Score
├ Insights  probable cause · evidence → suggested action (top 3, always on)                 │
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

132 tests covering config validation, strict-majority consensus (all 3^n combinations for n=1..5), target/connection state reducers, true-pause recovery, round-based batch protocol, optional jitter, independent baseline counts, recursive detector equations and warmup, gateway evidence freshness, PathCause, isolated-target cue, and every insight rule.

## Screenshots

<!-- Add screenshots here after first public release. Suggested: -->
<!-- - Normal state with all targets up -->
<!-- - Degraded state showing hysteresis in progress -->
<!-- - Down state with event log -->
<!-- Use a fenced block with image links, e.g. ![normal](docs/normal.png) -->

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The project is intentionally small in scope — please read the "Scope" section before opening a PR with a large feature. Any future audio or code asset must record source, version, author, and license before incorporation.

## License

MIT — see [LICENSE](LICENSE).
