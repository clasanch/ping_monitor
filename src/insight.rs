//! Probable-cause diagnosis from live metrics.
//!
//! Pure rules engine: each rule inspects the current [`App`] snapshot and
//! returns an [`Insight`] when its pattern matches. Rules run in priority
//! order (local causes first, then upstream, then DNS); the first matches
//! are shown. No I/O, no hidden state — trivially testable.

use crate::app::{rssi_verdict_grade, App, Level, LinkState};

/// A single diagnosed probable cause with supporting evidence.
#[derive(Clone)]
pub struct Insight {
    pub severity: Level,
    pub cause: String,
    pub evidence: String,
    pub action: String,
}

impl Insight {
    fn new(
        severity: Level,
        cause: impl Into<String>,
        evidence: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            cause: cause.into(),
            evidence: evidence.into(),
            action: action.into(),
        }
    }
}

type Rule = fn(&App) -> Option<Insight>;

/// Ordered by diagnostic priority. Add new rules by appending functions —
/// no changes to existing rules required.
const RULES: &[Rule] = &[
    router_down,
    wifi_weak_rule,
    isp_outage,
    upstream_congestion,
    sys_dns_dead,
    dns_blocked,
    packet_loss,
    jitter_high,
    latency_creep,
    domain_down,
    resolver_down,
    dns_slow,
    single_target_bad,
];

/// Diagnose the current snapshot. Always returns at least one insight:
/// when nothing is wrong, a "healthy" summary is produced instead.
pub fn diagnose(app: &App) -> Vec<Insight> {
    let mut out: Vec<Insight> = RULES.iter().filter_map(|r| r(app)).take(3).collect();
    if out.is_empty() {
        out.push(healthy(app));
    }
    out
}

fn all_primaries_down(app: &App) -> bool {
    !app.primaries.is_empty() && app.primaries.iter().all(|p| p.state == LinkState::Down)
}

fn down_count(app: &App) -> usize {
    app.primaries
        .iter()
        .filter(|p| p.state == LinkState::Down)
        .count()
}

fn state_word(s: LinkState) -> &'static str {
    match s {
        LinkState::Up => "up",
        LinkState::Degraded => "degraded",
        LinkState::Down => "down",
    }
}

fn wifi_weak(app: &App) -> bool {
    app.wifi_grade.map(|g| g <= 1).unwrap_or(false)
}

// --- rules -----------------------------------------------------------------

/// All targets down AND a LAN extra (e.g. the gateway) also down:
/// the failure is on the local side, not upstream.
fn router_down(app: &App) -> Option<Insight> {
    if !all_primaries_down(app) {
        return None;
    }
    let gw = app.extras.iter().find(|e| e.state == LinkState::Down)?;
    Some(Insight::new(
        Level::Bad,
        "local router/gateway down",
        format!(
            "{}/{} targets lost · {} also down",
            down_count(app),
            app.primaries.len(),
            gw.label
        ),
        "reboot router / check cabling — the LAN itself is failing",
    ))
}

/// Link bad while the Wi-Fi signal is weak or worse.
fn wifi_weak_rule(app: &App) -> Option<Insight> {
    let g = app.wifi_grade?;
    if g > 1 || app.state == LinkState::Up {
        return None;
    }
    let rssi = app.wifi_rssi.unwrap_or(0);
    Some(Insight::new(
        if app.state == LinkState::Down {
            Level::Bad
        } else {
            Level::Warn
        },
        "weak Wi-Fi signal",
        format!(
            "{} dBm ({}) while link {}",
            rssi,
            rssi_verdict_grade(g),
            state_word(app.state)
        ),
        "move closer to the AP / reduce interference",
    ))
}

/// All targets down but the local side looks fine: the outage is upstream.
fn isp_outage(app: &App) -> Option<Insight> {
    if !all_primaries_down(app) || wifi_weak(app) {
        return None;
    }
    // a dead LAN extra points at the router instead
    if app.extras.iter().any(|e| e.state == LinkState::Down) {
        return None;
    }
    let mut ev = format!("{}/{} targets lost", down_count(app), app.primaries.len());
    if let (Some(r), Some(g)) = (app.wifi_rssi, app.wifi_grade) {
        ev.push_str(&format!(" · wifi {} ({} dBm)", rssi_verdict_grade(g), r));
    }
    Some(Insight::new(
        Level::Bad,
        "upstream outage (ISP / cell tower)",
        ev,
        "wait it out or call your ISP — your local side looks fine",
    ))
}

/// Degraded consensus driven by latency, not loss: classic upstream
/// congestion / saturated tower pattern.
fn upstream_congestion(app: &App) -> Option<Insight> {
    if app.state != LinkState::Degraded {
        return None;
    }
    let lat = app.last_value_view()?;
    let warn = app.lat_warn_ms();
    if lat <= warn || app.pooled_loss_pct() >= app.cfg.degraded_loss_pct {
        return None;
    }
    Some(Insight::new(
        Level::Warn,
        "upstream congestion / tower saturation",
        format!(
            "latency {:.0}ms > warn {:.0}ms · loss {:.0}%",
            lat,
            warn,
            app.pooled_loss_pct()
        ),
        "heavy load upstream — usually temporary",
    ))
}

/// System resolver dead while alternates still answer: the router's DNS
/// forwarder is the usual suspect.
fn sys_dns_dead(app: &App) -> Option<Insight> {
    let row = app.dns.cells.first()?;
    if row.is_empty() || !row.iter().all(|c| c.last.is_none()) {
        return None;
    }
    let alternates_ok = app
        .dns
        .cells
        .iter()
        .skip(1)
        .flatten()
        .any(|c| c.last.is_some());
    if !alternates_ok {
        return None;
    }
    Some(Insight::new(
        Level::Bad,
        "system DNS resolver down",
        "sys failing · alternates OK",
        "switch DNS to 1.1.1.1 / 8.8.8.8 — router forwarder likely dead",
    ))
}

/// Every resolver fails while pings still work: DNS itself is filtered
/// or broken upstream.
fn dns_blocked(app: &App) -> Option<Insight> {
    if app.dns.cells.is_empty() {
        return None;
    }
    let all_fail = app.dns.cells.iter().flatten().all(|c| c.last.is_none());
    let pings_ok = app.primaries.iter().any(|p| p.state == LinkState::Up);
    if !all_fail || !pings_ok {
        return None;
    }
    Some(Insight::new(
        Level::Bad,
        "all DNS failing but pings OK",
        "full matrix down · targets reachable",
        "UDP 53 likely filtered upstream",
    ))
}

/// Pooled loss above the degraded threshold while not fully down.
fn packet_loss(app: &App) -> Option<Insight> {
    if app.state == LinkState::Down {
        return None;
    }
    let loss = app.pooled_loss_pct();
    if loss < app.cfg.degraded_loss_pct {
        return None;
    }
    Some(Insight::new(
        Level::Warn,
        format!("packet loss {:.0}%", loss),
        format!(
            "{}/{} probes lost (pooled)",
            app.pooled_lost(),
            app.pooled_total()
        ),
        "check physical medium / Wi-Fi interference",
    ))
}

/// Jitter above the adaptive threshold: unstable link, real-time apps suffer.
fn jitter_high(app: &App) -> Option<Insight> {
    if app.state == LinkState::Down {
        return None;
    }
    let jit = app.jitter_view()?;
    let warn = app.jit_warn_ms();
    if jit <= warn {
        return None;
    }
    Some(Insight::new(
        Level::Warn,
        "unstable link (high jitter)",
        format!("jitter {:.0}ms > warn {:.0}ms", jit, warn),
        "voice/video will stutter — check Wi-Fi or upstream load",
    ))
}

/// Latency above the adaptive threshold while consensus is still Up:
/// early warning before hysteresis trips.
fn latency_creep(app: &App) -> Option<Insight> {
    if app.state != LinkState::Up {
        return None;
    }
    let lat = app.last_value_view()?;
    let warn = app.lat_warn_ms();
    if lat <= warn {
        return None;
    }
    Some(Insight::new(
        Level::Warn,
        "latency elevated",
        format!("median {:.0}ms > warn {:.0}ms", lat, warn),
        "possible queue saturation — check for heavy uploads",
    ))
}

/// One domain fails on every resolver: that site is down, not the link.
fn domain_down(app: &App) -> Option<Insight> {
    let cols = app.dns.names.len();
    if cols == 0 || app.dns.cells.is_empty() {
        return None;
    }
    for d in 0..cols {
        let col_fail = app
            .dns
            .cells
            .iter()
            .all(|row| row.get(d).map(|c| c.last.is_none()).unwrap_or(false));
        let others_ok = (0..cols).filter(|&x| x != d).any(|x| {
            app.dns
                .cells
                .iter()
                .any(|row| row.get(x).map(|c| c.last.is_some()).unwrap_or(false))
        });
        if col_fail && others_ok {
            return Some(Insight::new(
                Level::Info,
                format!("site unreachable: {}", app.dns.names[d]),
                "fails on every resolver",
                "that site is down — not your connection",
            ));
        }
    }
    None
}

/// One non-system resolver fails entirely while others answer.
fn resolver_down(app: &App) -> Option<Insight> {
    for (i, row) in app.dns.cells.iter().enumerate() {
        if i == 0 || row.is_empty() {
            continue; // the system row is covered by sys_dns_dead
        }
        let row_fail = row.iter().all(|c| c.last.is_none());
        if !row_fail {
            continue;
        }
        let others_ok = app
            .dns
            .cells
            .iter()
            .enumerate()
            .any(|(j, r)| j != i && r.iter().any(|c| c.last.is_some()));
        if others_ok {
            return Some(Insight::new(
                Level::Warn,
                format!("resolver {} unreachable", app.dns.resolvers[i].0),
                "whole row failing · alternates OK",
                "may be blocked upstream — alternates cover you",
            ));
        }
    }
    None
}

/// System resolver answers but slowly: browsing feels heavy on a fine link.
fn dns_slow(app: &App) -> Option<Insight> {
    let worst = app.system_resolver_worst()?;
    let warn = app.dns_warn_ms();
    if worst <= warn {
        return None;
    }
    Some(Insight::new(
        Level::Warn,
        "slow DNS",
        format!("system resolver {:.0}ms > warn {:.0}ms", worst, warn),
        "browsing feels heavy though ping is fine",
    ))
}

/// Exactly one target misbehaving with healthy consensus: their side.
fn single_target_bad(app: &App) -> Option<Insight> {
    if app.state != LinkState::Up {
        return None;
    }
    let bad: Vec<_> = app
        .primaries
        .iter()
        .filter(|p| p.state != LinkState::Up)
        .collect();
    if bad.len() != 1 {
        return None;
    }
    Some(Insight::new(
        Level::Info,
        format!("target {} {}", bad[0].label, state_word(bad[0].state)),
        "other targets healthy",
        "problem is on that target's side, not yours",
    ))
}

/// Fallback when no rule fires: confirm everything is nominal.
fn healthy(app: &App) -> Insight {
    let up = app.cur_uptime_secs();
    Insight::new(
        Level::Good,
        "link healthy",
        format!(
            "uptime {:02}:{:02}:{:02} · score {:.0}/100 · loss {:.1}%",
            up / 3600,
            (up % 3600) / 60,
            up % 60,
            app.score(),
            app.pooled_loss_pct()
        ),
        "all metrics nominal",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Config, ExtraProbe, PrimaryProbe, Ring};

    fn base_app() -> App {
        let mut a = App::new(Config::default());
        for (label, host) in [("cf", "1.1.1.1"), ("gg", "8.8.8.8"), ("q9", "9.9.9.9")] {
            a.primaries.push(PrimaryProbe::new(label, host, 443));
        }
        set_dns(&mut a, Some(30.0));
        a
    }

    fn set_dns(app: &mut App, v: Option<f64>) {
        for row in app.dns.cells.iter_mut() {
            for c in row.iter_mut() {
                c.last = v;
            }
        }
    }

    fn add_extra(app: &mut App, state: LinkState) {
        app.extras.push(ExtraProbe {
            label: "gw".into(),
            host: "192.168.1.1".into(),
            port: 80,
            last: Some(2.0),
            state,
            total: 10,
            lost: 0,
            ring: Ring::new(30),
            consec_loss: 0,
            last_sample_at: None,
        });
    }

    fn all_down(app: &mut App) {
        for p in app.primaries.iter_mut() {
            p.state = LinkState::Down;
        }
        app.state = LinkState::Down;
    }

    #[test]
    fn healthy_when_nothing_wrong() {
        let a = base_app();
        let out = diagnose(&a);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].severity, Level::Good));
        assert!(out[0].cause.contains("healthy"));
    }

    #[test]
    fn router_down_when_gateway_also_down() {
        let mut a = base_app();
        all_down(&mut a);
        add_extra(&mut a, LinkState::Down);
        let out = diagnose(&a);
        assert!(matches!(out[0].severity, Level::Bad));
        assert!(out[0].cause.contains("router"));
    }

    #[test]
    fn isp_outage_when_lan_ok() {
        let mut a = base_app();
        all_down(&mut a);
        add_extra(&mut a, LinkState::Up);
        let out = diagnose(&a);
        assert!(out[0].cause.contains("upstream outage"));
        assert!(
            !out[0].evidence.contains("LAN OK"),
            "LAN OK overclaim removed"
        );
    }

    #[test]
    fn isp_outage_skipped_when_wifi_weak() {
        let mut a = base_app();
        all_down(&mut a);
        add_extra(&mut a, LinkState::Up);
        a.set_wifi_rssi(Some(-80));
        let out = diagnose(&a);
        assert!(out[0].cause.contains("Wi-Fi"));
        assert!(matches!(out[0].severity, Level::Bad));
        assert!(!out.iter().any(|i| i.cause.contains("upstream outage")));
    }

    #[test]
    fn wifi_weak_warns_on_degraded() {
        let mut a = base_app();
        a.state = LinkState::Degraded;
        a.set_wifi_rssi(Some(-80));
        let out = diagnose(&a);
        assert!(out[0].cause.contains("Wi-Fi"));
        assert!(matches!(out[0].severity, Level::Warn));
    }

    #[test]
    fn upstream_congestion_on_degraded_latency() {
        let mut a = base_app();
        a.state = LinkState::Degraded;
        for p in a.primaries.iter_mut() {
            p.last_value = Some(300.0);
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("congestion"));
    }

    #[test]
    fn packet_loss_early_warning() {
        let mut a = base_app();
        for p in a.primaries.iter_mut() {
            p.total = 10;
            p.lost = 3;
            p.last_value = Some(20.0);
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("packet loss"));
        assert!(matches!(out[0].severity, Level::Warn));
    }

    #[test]
    fn jitter_high_detected() {
        let mut a = base_app();
        for p in a.primaries.iter_mut() {
            p.jitter_ring.push(Some(100.0));
            p.last_value = Some(20.0);
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("jitter"));
    }

    #[test]
    fn latency_creep_early_warning() {
        let mut a = base_app();
        for p in a.primaries.iter_mut() {
            p.last_value = Some(300.0);
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("latency elevated"));
    }

    #[test]
    fn sys_dns_dead_with_alternates_ok() {
        let mut a = base_app();
        for c in a.dns.cells[0].iter_mut() {
            c.last = None;
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("system DNS"));
        assert!(matches!(out[0].severity, Level::Bad));
    }

    #[test]
    fn dns_blocked_when_pings_ok() {
        let mut a = base_app();
        set_dns(&mut a, None);
        let out = diagnose(&a);
        assert!(out[0].cause.contains("all DNS failing"));
    }

    #[test]
    fn domain_down_single_column() {
        let mut a = base_app();
        for row in a.dns.cells.iter_mut() {
            row[0].last = None;
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("www.google.com"));
        assert!(matches!(out[0].severity, Level::Info));
    }

    #[test]
    fn resolver_down_single_row() {
        let mut a = base_app();
        for c in a.dns.cells[1].iter_mut() {
            c.last = None;
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("resolver cf unreachable"));
    }

    #[test]
    fn dns_slow_system_resolver() {
        let mut a = base_app();
        for c in a.dns.cells[0].iter_mut() {
            c.last = Some(250.0);
        }
        let out = diagnose(&a);
        assert!(out[0].cause.contains("slow DNS"));
    }

    #[test]
    fn single_target_bad_info() {
        let mut a = base_app();
        a.primaries[1].state = LinkState::Degraded;
        let out = diagnose(&a);
        assert!(out[0].cause.contains("gg"));
        assert!(matches!(out[0].severity, Level::Info));
    }

    #[test]
    fn diagnose_caps_at_three_with_priority_order() {
        let mut a = base_app();
        // sys DNS dead + high jitter + elevated latency, all at once
        for c in a.dns.cells[0].iter_mut() {
            c.last = None;
        }
        for p in a.primaries.iter_mut() {
            p.jitter_ring.push(Some(100.0));
            p.last_value = Some(300.0);
        }
        let out = diagnose(&a);
        assert_eq!(out.len(), 3);
        assert!(out[0].cause.contains("system DNS"));
        assert!(out[1].cause.contains("jitter"));
        assert!(out[2].cause.contains("latency elevated"));
    }

    #[test]
    fn jitter_high_skips_when_none() {
        let mut a = base_app();
        a.state = LinkState::Degraded;
        // All jitter rings empty → jitter_view() returns None.
        let out = diagnose(&a);
        assert!(
            !out.iter().any(|i| i.cause.contains("jitter")),
            "jitter insight should not appear when jitter_view is None"
        );
    }
}
