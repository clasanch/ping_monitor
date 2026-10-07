mod app;
mod checkpoint;
mod config;
mod detector;
mod gateway;
mod incident;
mod insight;
mod net;
mod sound;
mod state;
mod trace;
mod ui;
mod wifi;

use app::{App, Config, PrimaryBatch, RoundResult};
use checkpoint::{Checkpoint, StateStore};
use config::{RunMode, RuntimeConfig};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use incident::{IncidentMetadata, IncidentWriter, TraceRecord, TransitionNotice};
use ratatui::DefaultTerminal;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::interval;

#[derive(Debug)]
enum AppMsg {
    PrimaryBatch(PrimaryBatch),
    Dns(usize, usize, Option<f64>),
    ExtraPing(usize, net::PingSample),
    GatewayProbeResult(usize, u64, net::PingSample),
    TraceFinished {
        context: Option<IncidentMetadata>,
        summary: trace::TraceSummary,
    },
    TraceError {
        context: Option<IncidentMetadata>,
        error: String,
    },
    WifiRssi(Option<i16>),
    GatewayNew(String),
    Quit,
}

const DEFAULT_PRIMARIES: &[(&str, &str, u16)] = &[
    ("cf", "1.1.1.1", 443),
    ("gg", "8.8.8.8", 443),
    ("q9", "9.9.9.9", 443),
];

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let runtime = RuntimeConfig::from_process()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let mut cfg = Config::default();
    if let Ok(t) = std::env::var("PM_TIMEOUT_MS") {
        cfg.timeout_ms = t.parse().unwrap_or(cfg.timeout_ms);
    }
    if let Ok(r) = std::env::var("PM_REMINDER_S") {
        cfg.reminder_interval = Duration::from_secs(r.parse().unwrap_or(30));
    }
    for w in cfg.validate() {
        eprintln!("warning: {}", w);
    }

    if runtime.mode == RunMode::CheckConfig {
        configured_primary_targets()
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        configured_extra_targets()
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        println!("configuration valid");
        return Ok(());
    }

    let audio = sound::spawn_sound(runtime.sound, runtime.pc_speaker_device.clone());
    let (audio_tx, audio_state) = match &audio {
        Some((tx, st)) => (Some(tx.clone()), Some(Arc::clone(st))),
        None => (None, None),
    };
    if runtime.sound != config::SoundBackend::Off && audio_tx.is_none() {
        eprintln!("warning: audio device unavailable — running silent");
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<AppMsg>();
    let tx_clone = tx.clone();

    {
        let tx = tx.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::SignalKind;
                if let Ok(mut term) = tokio::signal::unix::signal(SignalKind::terminate()) {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                    }
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            let _ = tx.send(AppMsg::Quit);
        });
    }

    let mut app = App::new(cfg);
    if let Ok(raw) = std::env::var("PM_DNS_NAMES") {
        let names: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !names.is_empty() {
            let r = app.dns.resolvers.clone();
            app.dns = app::DnsMatrix::new(r, names);
        }
    }
    app.audio_state = audio_state;
    if runtime.mode == RunMode::Tui {
        app.notify_fn = Some(Arc::new(|msg: &str| {
            if cfg!(target_os = "macos") {
                let _ = std::process::Command::new("osascript")
                    .args([
                        "-e",
                        &format!(
                            "display notification \"{}\" with title \"ping_monitor\"",
                            msg
                        ),
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            } else if cfg!(target_os = "linux") {
                let _ = std::process::Command::new("notify-send")
                    .args(["ping_monitor", msg])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            } else if cfg!(target_os = "windows") {
                let _ = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "[reflection.assembly]::loadwithpartialname('System.Windows.Forms') | Out-Null; $balloon = New-Object System.Windows.Forms.NotifyIcon; $balloon.Icon = [System.Drawing.SystemIcons]::Information; $balloon.BalloonTipTitle = 'ping_monitor'; $balloon.BalloonTipText = '{}'; $balloon.Visible = $true; $balloon.ShowBalloonTip(5000)",
                        msg
                    ),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            }
        }));
    }
    let ping_interval_handle = Arc::clone(&app.interval_ms);

    let primaries = configured_primary_targets()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;

    for (label, host, port) in &primaries {
        app.primaries
            .push(app::PrimaryProbe::new(label, host, *port));
    }

    if app.primaries.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no valid primary targets. Set PM_TARGETS=label:host:port,...",
        ));
    }

    for (label, host, port) in configured_extra_targets()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?
    {
        app.extras.push(app::ExtraProbe {
            label,
            host,
            port,
            last: None,
            state: app::LinkState::Up,
            total: 0,
            lost: 0,
            consec_loss: 0,
            ring: app::Ring::new(30),
            last_sample_at: None,
        });
    }
    if !app.extras.is_empty() {
        app.log(
            app::Level::Info,
            format!("extras: {} probe(s)", app.extras.len()),
        );
    }

    // Auto-detect the default gateway and probe it as extra [gw]; feeds the
    // router-vs-ISP insight without requiring PM_EXTRAS configuration.
    // A re-detection task keeps the probe following network/roaming changes.
    let gw_now = gateway::default_gateway();
    if let Some(ref gw) = gw_now {
        let monitored =
            app.extras.iter().any(|e| e.host == *gw) || primaries.iter().any(|(_, h, _)| h == gw);
        if !monitored {
            app.auto_gw_idx = Some(app.extras.len());
            app.gw_role = app::GatewayRole::AutoExtra(app.extras.len());
            app.gateway_probe_idx = Some(app.extras.len());
            app.extras.push(app::ExtraProbe {
                label: "gw".into(),
                host: gw.clone(),
                port: 80,
                last: None,
                state: app::LinkState::Up,
                total: 0,
                lost: 0,
                consec_loss: 0,
                ring: app::Ring::new(30),
                last_sample_at: None,
            });
            app.log(
                app::Level::Info,
                format!("gateway auto-detected: {} (extra [gw])", gw),
            );
        }
    }
    if let Ok(mut cur) = app.gw_shared.lock() {
        cur.1 = gw_now.clone().unwrap_or_default();
    }

    // Re-detect the default route periodically: roaming to another network
    // (or a VPN taking over) changes the gateway, and [gw] must follow.
    // Skipped when the user monitors the gateway IP manually — their probe,
    // their semantics.
    let user_monitors_gw = gw_now.is_some() && app.auto_gw_idx.is_none();
    if !user_monitors_gw {
        let tx = tx.clone();
        let shared = Arc::clone(&app.gw_shared);
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                let new = tokio::task::spawn_blocking(gateway::default_gateway)
                    .await
                    .ok()
                    .flatten();
                match new {
                    Some(gw) => {
                        let changed = match shared.lock() {
                            Ok(mut cur) if cur.1 != gw => {
                                cur.0 += 1; // increment epoch on address change
                                cur.1 = gw.clone();
                                true
                            }
                            _ => false,
                        };
                        if changed {
                            let _ = tx.send(AppMsg::GatewayNew(gw));
                        }
                    }
                    None => {
                        // Detection failed — signal None to clear gateway evidence
                        let _ = tx.send(AppMsg::GatewayNew(String::new()));
                    }
                }
            }
        });
    }

    app.log(
        app::Level::Info,
        format!("primary targets: {}", primaries.len()),
    );
    for (l, h, p) in &primaries {
        app.log(
            app::Level::Info,
            format!("  [{}] {}:{}  (consensus member)", l, h, p),
        );
    }
    app.log(
        app::Level::Info,
        format!(
            "dns matrix: {} resolvers × {} domains  timeout {}ms",
            app.dns.resolvers.len(),
            app.dns.names.len(),
            app.cfg.timeout_ms
        ),
    );
    app.log(
        app::Level::Info,
        "keys: m/mute r/reset e/export t/traceroute q/quit",
    );

    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.tick().await;
            loop {
                tick.tick().await;
                let rssi = tokio::task::spawn_blocking(wifi::poll_rssi)
                    .await
                    .ok()
                    .flatten();
                let _ = tx.send(AppMsg::WifiRssi(rssi));
            }
        });
    }

    // Coordinator: owns all probes, launches per-round via JoinSet.
    {
        let tx = tx.clone();
        let interval_handle = Arc::clone(&ping_interval_handle);
        let reset_epoch = Arc::clone(&app.reset_epoch);
        let cfg_timeout_ms = app.cfg.timeout_ms;
        let n = app.primaries.len();
        let pingers: Vec<net::TcpPinger> = primaries
            .iter()
            .map(|(_label, host, port)| net::TcpPinger {
                addr: host.clone(),
                port: *port,
                timeout_ms: cfg_timeout_ms,
                alive_on_rejected: false,
            })
            .collect();
        tokio::spawn(async move {
            let mut round_id: u64 = 0;
            loop {
                let round_epoch = reset_epoch.load(std::sync::atomic::Ordering::Acquire);
                let started_at = Instant::now();

                let mut set = JoinSet::new();
                for (idx, pinger) in pingers.iter().enumerate() {
                    let pinger = pinger.clone();
                    set.spawn(async move { (idx, pinger.ping().await) });
                }

                // Deadline: probe's internal timeout + scheduling margin.
                const ROUND_MARGIN_MS: u64 = 200;
                let deadline_ms = cfg_timeout_ms + ROUND_MARGIN_MS;

                let results = match tokio::time::timeout(
                    Duration::from_millis(deadline_ms),
                    collect_round(&mut set, n),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        let mut r = vec![RoundResult::Missing; n];
                        while let Some(res) = set.try_join_next() {
                            if let Ok((idx, sample)) = res {
                                r[idx] = RoundResult::Observed(sample);
                            }
                        }
                        r
                    }
                };

                set.abort_all();

                let _ = tx.send(AppMsg::PrimaryBatch(PrimaryBatch {
                    round_id,
                    reset_epoch: round_epoch,
                    results,
                    started_at,
                }));
                round_id += 1;

                let elapsed = started_at.elapsed().as_millis() as u64;
                let interval = interval_handle
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(200);
                let sleep_ms = interval.saturating_sub(elapsed).max(200);
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
        });
    }

    let dns_interval_ms = app.cfg.dns_interval_ms;
    let dns_timeout_ms = app.cfg.timeout_ms;
    let dns_cfg: Vec<(usize, String, Option<String>, usize, String)> = app
        .dns
        .resolvers
        .iter()
        .enumerate()
        .flat_map(|(r_idx, (r_label, r_ip))| {
            app.dns
                .names
                .iter()
                .enumerate()
                .map(move |(d_idx, d_name)| {
                    (r_idx, r_label.clone(), r_ip.clone(), d_idx, d_name.clone())
                })
        })
        .collect();
    for (r_idx, r_label, r_ip, d_idx, d_name) in dns_cfg {
        let probe = match r_ip {
            Some(ref ip) => net::DnsProbe::custom(&d_name, ip, dns_timeout_ms),
            None => net::DnsProbe::system(&d_name, dns_timeout_ms).await,
        };
        if probe.is_none() {
            app.log(
                app::Level::Warn,
                format!("[DNS {}→{}] could not build resolver", r_label, d_name),
            );
            continue;
        }
        let probe = probe.unwrap();
        let tx = tx.clone();
        let n_cells = (app.dns.resolvers.len() * app.dns.names.len()).max(1) as u64;
        let stagger =
            ((r_idx * app.dns.names.len() + d_idx) as u64) * (dns_interval_ms / n_cells).max(50);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(stagger)).await;
            let mut tick = interval(Duration::from_millis(dns_interval_ms));
            tick.tick().await;
            loop {
                tick.tick().await;
                let v = probe.probe().await;
                let _ = tx.send(AppMsg::Dns(r_idx, d_idx, v));
            }
        });
    }

    for (i, ex) in app.extras.iter().enumerate() {
        if Some(i) == app.auto_gw_idx {
            continue; // the auto-gateway pinger below follows re-detection
        }
        let tx = tx.clone();
        let pinger = net::TcpPinger {
            addr: ex.host.clone(),
            port: ex.port,
            timeout_ms: app.cfg.timeout_ms,
            alive_on_rejected: false,
        };
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                let s = pinger.ping().await;
                let _ = tx.send(AppMsg::ExtraPing(i, s));
            }
        });
    }

    if let Some(idx) = app.auto_gw_idx {
        spawn_gateway_pinger(
            idx,
            Arc::clone(&app.gw_shared),
            app.cfg.timeout_ms,
            Arc::clone(&app.gw_cadence_ms),
            tx.clone(),
        );
    }

    if runtime.mode == RunMode::Headless {
        return run_headless(&mut app, &mut rx, &tx_clone, audio_tx.as_ref(), runtime).await;
    }

    let mut terminal = ratatui::init();
    let mut last_draw = Instant::now();
    let draw_period = Duration::from_millis(80);

    let result = run(
        &mut terminal,
        &mut app,
        &mut rx,
        &tx_clone,
        audio_tx.as_ref(),
        &mut last_draw,
        draw_period,
    )
    .await;

    ratatui::restore();
    result
}

async fn run_headless(
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppMsg>,
    tx: &mpsc::UnboundedSender<AppMsg>,
    audio: Option<&mpsc::UnboundedSender<sound::SoundEvent>>,
    runtime: RuntimeConfig,
) -> std::io::Result<()> {
    let mut incidents = match runtime.log_dir.as_ref() {
        Some(dir) => Some(IncidentWriter::new(dir)?),
        None => None,
    };
    let state_store = runtime
        .state_dir
        .as_ref()
        .map(|dir| StateStore::new(dir.join("state")));
    let config_fingerprint = runtime_fingerprint(&runtime);
    let checkpoint = state_store
        .as_ref()
        .map(StateStore::load)
        .transpose()?
        .flatten();
    if let Some(writer) = incidents.as_mut() {
        if reconcile_open_incident(
            writer,
            state_store.as_ref(),
            checkpoint.as_ref(),
            &config_fingerprint,
        )? {
            eprintln!("closed interrupted incident from previous process");
        }
    }
    let grace_until = Instant::now() + runtime.startup_grace;
    let mut grace_complete = runtime.startup_grace.is_zero();
    let mut startup_reconciled = grace_complete;
    let mut trace_started = false;

    eprintln!(
        "headless monitor started; startup grace {}s; automatic trace {}",
        runtime.startup_grace.as_secs(),
        if runtime.auto_trace {
            "enabled"
        } else {
            "disabled"
        }
    );

    loop {
        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        let msg = match received {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                close_incidents(&mut incidents, &state_store, &config_fingerprint, app)?;
                return Ok(());
            }
            Err(_) => {
                if !grace_complete && Instant::now() >= grace_until {
                    grace_complete = true;
                }
                if grace_complete {
                    if let Some((event, _, _)) = app.tick_reminder() {
                        emit(audio, Some(event), app.muted);
                    }
                }
                continue;
            }
        };

        match msg {
            AppMsg::PrimaryBatch(batch) => {
                let sound = app.ingest_generation(batch);
                let effect = app.take_transition_effect();
                if !grace_complete && Instant::now() >= grace_until {
                    grace_complete = true;
                }
                if grace_complete && !startup_reconciled {
                    startup_reconciled = true;
                    if app.state != app::LinkState::Up {
                        let notice = TransitionNotice {
                            from: "up".into(),
                            to: state_word(app.state).into(),
                            at: std::time::SystemTime::now(),
                            duration_ms: 0,
                            cause: "startup state reconciliation".into(),
                            evidence: app.evidence_snapshot(),
                        };
                        apply_notice(app, &mut incidents, &notice)?;
                        save_checkpoint(&state_store, &incidents, app, &config_fingerprint)?;
                        if app.state == app::LinkState::Down && runtime.auto_trace {
                            spawn_trace_for_app(
                                app,
                                tx,
                                &runtime,
                                incidents.as_ref().and_then(IncidentWriter::active_metadata),
                            );
                            trace_started = true;
                        }
                    }
                }
                if grace_complete {
                    emit(audio, sound, app.muted);
                    if let Some(effect) = effect {
                        let from = effect.from;
                        let to = effect.to;
                        let notice = notice_from_effect(&effect);
                        apply_notice(app, &mut incidents, &notice)?;
                        save_checkpoint(&state_store, &incidents, app, &config_fingerprint)?;
                        if runtime.auto_trace
                            && should_start_automatic_trace(from, to, trace_started)
                        {
                            spawn_trace_for_app(
                                app,
                                tx,
                                &runtime,
                                incidents.as_ref().and_then(IncidentWriter::active_metadata),
                            );
                            trace_started = true;
                        } else if to == app::LinkState::Up {
                            trace_started = false;
                        }
                    }
                }
            }
            AppMsg::Dns(r, d, value) => {
                let _ = app.ingest_dns(r, d, value);
            }
            AppMsg::ExtraPing(i, sample) => app.ingest_extra(i, sample),
            AppMsg::GatewayProbeResult(i, epoch, sample) => {
                app.ingest_gateway_probe(i, epoch, sample)
            }
            AppMsg::GatewayNew(gw) => {
                if gw.is_empty() {
                    app.gw_role = app::GatewayRole::Unknown;
                    eprintln!("warning: gateway detection failed");
                } else {
                    match app.apply_gateway_update(&gw) {
                        app::GatewayUpdate::Unchanged => {}
                        app::GatewayUpdate::Updated { old, new } => {
                            eprintln!("gateway changed: {} -> {}", old, new);
                        }
                        app::GatewayUpdate::Added { idx } => {
                            eprintln!("gateway detected: {}", gw);
                            spawn_gateway_pinger(
                                idx,
                                Arc::clone(&app.gw_shared),
                                app.cfg.timeout_ms,
                                Arc::clone(&app.gw_cadence_ms),
                                tx.clone(),
                            );
                        }
                    }
                }
            }
            AppMsg::TraceFinished { context, summary } => {
                eprintln!(
                    "automatic trace: {:?}; hops {}; last responsive hop {}; boundary {}",
                    summary.classification,
                    summary.hops.len(),
                    summary.last_responsive_hop.as_deref().unwrap_or("-"),
                    summary
                        .failure_boundary
                        .map(|ttl| ttl.to_string())
                        .unwrap_or_else(|| "-".into())
                );
                if let (Some(writer), Some(context)) = (incidents.as_mut(), context) {
                    writer.record_trace_for(
                        &context,
                        std::time::SystemTime::now(),
                        &TraceRecord {
                            classification: format!("{:?}", summary.classification),
                            last_responsive_hop: summary.last_responsive_hop,
                            failure_boundary: summary.failure_boundary,
                            reached_target: summary.reached_target,
                            raw_output: summary.raw_output,
                        },
                    )?;
                }
            }
            AppMsg::TraceError { context, error } => {
                eprintln!("warning: automatic trace: {}", error);
                if let (Some(writer), Some(context)) = (incidents.as_mut(), context) {
                    writer.record_trace_for(
                        &context,
                        std::time::SystemTime::now(),
                        &trace_error_record(&error),
                    )?;
                }
            }
            AppMsg::Quit => {
                close_incidents(&mut incidents, &state_store, &config_fingerprint, app)?;
                return Ok(());
            }
            AppMsg::WifiRssi(rssi) => app.set_wifi_rssi(rssi),
        }
    }
}

fn state_word(state: app::LinkState) -> &'static str {
    match state {
        app::LinkState::Up => "up",
        app::LinkState::Degraded => "degraded",
        app::LinkState::Down => "down",
    }
}

fn configured_primary_targets() -> Result<Vec<(String, String, u16)>, String> {
    let primaries = match std::env::var("PM_TARGETS") {
        Ok(raw) => parse_primary_targets(&raw)?,
        Err(_) => DEFAULT_PRIMARIES
            .iter()
            .map(|(label, host, port)| (label.to_string(), host.to_string(), *port))
            .collect(),
    };
    Ok(primaries)
}

fn parse_primary_targets(raw: &str) -> Result<Vec<(String, String, u16)>, String> {
    parse_target_list(raw, "primary target")
}

fn parse_target_list(raw: &str, setting: &str) -> Result<Vec<(String, String, u16)>, String> {
    let mut targets = Vec::new();
    for piece in raw.split(',') {
        targets.push(
            parse_target_piece(piece)
                .map_err(|_| format!("invalid {setting} '{piece}'. Use label:host:port"))?,
        );
    }
    if targets.is_empty() {
        return Err(format!("no valid entries in {setting}"));
    }
    Ok(targets)
}

fn configured_extra_targets_from(raw: Option<&str>) -> Result<Vec<(String, String, u16)>, String> {
    raw.map(|value| parse_target_list(value, "PM_EXTRAS"))
        .unwrap_or_else(|| Ok(Vec::new()))
}

fn configured_extra_targets() -> Result<Vec<(String, String, u16)>, String> {
    let raw = std::env::var("PM_EXTRAS").ok();
    configured_extra_targets_from(raw.as_deref())
}

fn parse_target_piece(piece: &str) -> Result<(String, String, u16), ()> {
    let (label, remainder) = piece.trim().split_once(':').ok_or(())?;
    let (host, port) = remainder.rsplit_once(':').ok_or(())?;
    let label = label.trim();
    let mut host = host.trim();
    if host.starts_with('[') || host.ends_with(']') {
        if !(host.starts_with('[') && host.ends_with(']')) {
            return Err(());
        }
        host = &host[1..host.len() - 1];
    }
    if label.is_empty() || host.is_empty() {
        return Err(());
    }
    if host.starts_with('-')
        || host
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(());
    }
    let port = port.trim().parse::<u16>().map_err(|_| ())?;
    if port == 0 {
        return Err(());
    }
    Ok((label.to_string(), host.to_string(), port))
}

fn cause_word(cause: app::PathCause) -> &'static str {
    match cause {
        app::PathCause::GatewayFailure => "gateway_failure",
        app::PathCause::WeakWifi => "weak_wifi",
        app::PathCause::BeyondFirstHop => "beyond_first_hop",
        app::PathCause::Unknown => "unknown",
    }
}

fn notice_from_effect(effect: &app::TransitionEffect) -> TransitionNotice {
    TransitionNotice {
        from: state_word(effect.from).into(),
        to: state_word(effect.to).into(),
        at: effect.at,
        duration_ms: effect.duration_ms,
        cause: cause_word(effect.cause).into(),
        evidence: effect.evidence.clone(),
    }
}

fn apply_notice(
    _app: &App,
    incidents: &mut Option<IncidentWriter>,
    notice: &TransitionNotice,
) -> std::io::Result<()> {
    eprintln!(
        "transition {} -> {} cause={}",
        notice.from, notice.to, notice.cause
    );
    if let Some(writer) = incidents.as_mut() {
        writer.apply(notice)?;
    }
    Ok(())
}

fn runtime_fingerprint(runtime: &RuntimeConfig) -> String {
    format!(
        "mode={:?};sound={:?};trace={};hops={};timeout={}",
        runtime.mode,
        runtime.sound,
        runtime.auto_trace,
        runtime.trace_max_hops,
        runtime.trace_timeout.as_secs()
    )
}

fn save_checkpoint(
    store: &Option<StateStore>,
    incidents: &Option<IncidentWriter>,
    app: &App,
    config_fingerprint: &str,
) -> std::io::Result<()> {
    let Some(store) = store.as_ref() else {
        return Ok(());
    };
    let metadata = incidents.as_ref().and_then(IncidentWriter::active_metadata);
    let checkpoint = Checkpoint {
        state: state_word(app.state).into(),
        episode_kind: metadata
            .as_ref()
            .map(|metadata| metadata.episode_kind.clone()),
        incident_id: metadata.as_ref().map(|metadata| metadata.id.clone()),
        started_at_ms: metadata.as_ref().and_then(|metadata| {
            metadata
                .started_at
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis() as u64)
        }),
        config_fingerprint: config_fingerprint.into(),
        evidence: app.evidence_snapshot(),
    };
    store.save(&checkpoint)
}

fn close_incidents(
    incidents: &mut Option<IncidentWriter>,
    state_store: &Option<StateStore>,
    config_fingerprint: &str,
    app: &App,
) -> std::io::Result<()> {
    if let Some(writer) = incidents.as_mut() {
        let evidence = app.evidence_snapshot();
        let duration = writer
            .active_metadata()
            .and_then(|metadata| {
                std::time::SystemTime::now()
                    .duration_since(metadata.started_at)
                    .ok()
            })
            .unwrap_or_else(|| app.state_since.elapsed());
        let close_result = writer.close_with_evidence(
            std::time::SystemTime::now(),
            duration,
            "monitor_stopped",
            &evidence,
        );
        if !close_succeeded(&close_result) {
            if let Err(error) = close_result {
                eprintln!("warning: incident close failed: {}", error);
                return Err(error);
            }
        }
    }
    if let Some(store) = state_store {
        let checkpoint = Checkpoint {
            state: "up".into(),
            episode_kind: None,
            incident_id: None,
            started_at_ms: None,
            config_fingerprint: config_fingerprint.into(),
            evidence: incident::Evidence::default(),
        };
        store.save(&checkpoint)?;
    }
    Ok(())
}

fn reconcile_open_incident(
    writer: &mut IncidentWriter,
    store: Option<&StateStore>,
    checkpoint: Option<&Checkpoint>,
    config_fingerprint: &str,
) -> std::io::Result<bool> {
    let recovered = writer.recover_open_episode()?;
    let mut closed_incident = false;
    if let Some(open) = recovered {
        let evidence = checkpoint
            .filter(|checkpoint| {
                checkpoint.incident_id.as_deref() == Some(open.metadata.id.as_str())
                    && checkpoint.episode_kind.as_deref()
                        == Some(open.metadata.episode_kind.as_str())
            })
            .map(|checkpoint| &checkpoint.evidence)
            .unwrap_or(&open.evidence);
        let started_at_ms = open
            .metadata
            .started_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        writer.resume(open.metadata.id, &open.metadata.episode_kind, started_at_ms);
        let duration = std::time::SystemTime::now()
            .duration_since(open.metadata.started_at)
            .unwrap_or_default();
        writer.close_with_evidence(
            std::time::SystemTime::now(),
            duration,
            "monitor_restarted",
            evidence,
        )?;
        closed_incident = true;
    }
    if let Some(store) = store {
        store.save(&Checkpoint {
            state: "up".into(),
            episode_kind: None,
            incident_id: None,
            started_at_ms: None,
            config_fingerprint: config_fingerprint.into(),
            evidence: incident::Evidence::default(),
        })?;
    }
    Ok(closed_incident)
}

fn close_succeeded(result: &std::io::Result<()>) -> bool {
    result.is_ok()
}

fn should_start_automatic_trace(
    from: app::LinkState,
    to: app::LinkState,
    trace_started: bool,
) -> bool {
    to == app::LinkState::Down
        && (from == app::LinkState::Degraded || (from == app::LinkState::Up && !trace_started))
}

fn should_auto_export_transition(mode: RunMode, has_transition: bool) -> bool {
    mode == RunMode::Tui && has_transition
}

fn trace_error_record(error: &str) -> TraceRecord {
    TraceRecord {
        classification: "unavailable".into(),
        last_responsive_hop: None,
        failure_boundary: None,
        reached_target: false,
        raw_output: error.into(),
    }
}

fn trace_target_for_app(app: &App) -> Option<String> {
    app.primaries
        .iter()
        .find(|probe| probe.state == app::LinkState::Down)
        .or_else(|| {
            app.primaries
                .iter()
                .find(|probe| probe.state == app::LinkState::Degraded)
        })
        .or_else(|| {
            app.primaries
                .iter()
                .find(|probe| probe.last_value.is_none())
        })
        .or_else(|| app.primaries.first())
        .map(|probe| probe.host.clone())
}

fn spawn_trace_for_app(
    app: &App,
    tx: &mpsc::UnboundedSender<AppMsg>,
    runtime: &RuntimeConfig,
    context: Option<IncidentMetadata>,
) {
    let target = trace_target_for_app(app).unwrap_or_default();
    if target.is_empty() {
        let _ = tx.send(AppMsg::TraceError {
            context,
            error: "no trace target available".into(),
        });
        return;
    }
    let request = trace::TraceRequest {
        target,
        gateway: app.gateway_host(),
        max_hops: runtime.trace_max_hops,
        timeout: runtime.trace_timeout,
    };
    spawn_trace_request(request, tx.clone(), context);
}

fn spawn_trace_request(
    request: trace::TraceRequest,
    tx: mpsc::UnboundedSender<AppMsg>,
    context: Option<IncidentMetadata>,
) {
    tokio::task::spawn_blocking(move || match trace::run_trace(&request) {
        Ok(summary) => {
            let _ = tx.send(AppMsg::TraceFinished { context, summary });
        }
        Err(error) => {
            let _ = tx.send(AppMsg::TraceError {
                context,
                error: error.to_string(),
            });
        }
    });
}

async fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppMsg>,
    tx: &mpsc::UnboundedSender<AppMsg>,
    audio: Option<&mpsc::UnboundedSender<sound::SoundEvent>>,
    last_draw: &mut Instant,
    draw_period: Duration,
) -> std::io::Result<()> {
    loop {
        while let Ok(msg) = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
            let Some(msg) = msg else {
                return Ok(());
            };
            match msg {
                AppMsg::PrimaryBatch(batch) => {
                    let sound = app.ingest_generation(batch);
                    let transition = app.take_transition_effect();
                    emit(audio, sound, app.muted);
                    if should_auto_export_transition(RunMode::Tui, transition.is_some()) {
                        match app.export_tsv() {
                            Ok(path) => {
                                app.log(app::Level::Good, format!("auto-exported → {}", path))
                            }
                            Err(error) => {
                                app.log(app::Level::Bad, format!("auto-export failed: {}", error))
                            }
                        }
                    }
                }
                AppMsg::Dns(r, d, v) => {
                    let _ = app.ingest_dns(r, d, v);
                }
                AppMsg::ExtraPing(i, s) => app.ingest_extra(i, s),
                AppMsg::GatewayProbeResult(i, epoch, s) => app.ingest_gateway_probe(i, epoch, s),
                AppMsg::TraceFinished { summary, .. } => app.log(
                    app::Level::Info,
                    format!(
                        "traceroute: {:?}, hops: {}, last responsive hop: {}",
                        summary.classification,
                        summary.hops.len(),
                        summary.last_responsive_hop.as_deref().unwrap_or("-")
                    ),
                ),
                AppMsg::TraceError { error, .. } => app.log(
                    app::Level::Warn,
                    format!("traceroute unavailable: {}", error),
                ),
                AppMsg::WifiRssi(v) => app.set_wifi_rssi(v),
                AppMsg::GatewayNew(gw) => {
                    if gw.is_empty() {
                        // Detection failed — clear gateway evidence
                        app.gw_role = app::GatewayRole::Unknown;
                        app.log(app::Level::Warn, "gateway detection failed");
                    } else {
                        match app.apply_gateway_update(&gw) {
                            app::GatewayUpdate::Unchanged => {}
                            app::GatewayUpdate::Updated { old, new } => {
                                app.log(
                                    app::Level::Info,
                                    format!(
                                        "gateway changed: {} → {} (network/roaming switch)",
                                        old, new
                                    ),
                                );
                            }
                            app::GatewayUpdate::Added { idx } => {
                                app.log(
                                    app::Level::Info,
                                    format!("gateway detected: {} (extra [gw])", gw),
                                );
                                spawn_gateway_pinger(
                                    idx,
                                    Arc::clone(&app.gw_shared),
                                    app.cfg.timeout_ms,
                                    Arc::clone(&app.gw_cadence_ms),
                                    tx.clone(),
                                );
                            }
                        }
                    }
                }
                AppMsg::Quit => return Ok(()),
            }
        }

        if let Some((ev, lvl, msg)) = app.tick_reminder() {
            let _ = (lvl, msg);
            emit(audio, Some(ev), app.muted);
        }

        while event::poll(Duration::from_millis(0))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('m') => {
                        app.muted = !app.muted;
                        if let Some(ref st) = app.audio_state {
                            st.set_muted(app.muted);
                        }
                        app.log(
                            app::Level::Info,
                            if app.muted { "sound muted" } else { "sound on" },
                        );
                    }
                    KeyCode::Char('r') => app.reset(),
                    KeyCode::Char('e') => match app.export_tsv() {
                        Ok(p) => app.log(app::Level::Good, format!("exported → {}", p)),
                        Err(e) => app.log(app::Level::Bad, format!("export failed: {}", e)),
                    },
                    KeyCode::Char('t') => {
                        let host = app
                            .primaries
                            .iter()
                            .find(|p| p.state == app::LinkState::Up)
                            .or_else(|| app.primaries.first())
                            .map(|p| p.host.clone())
                            .unwrap_or_else(|| "1.1.1.1".into());
                        app.log(app::Level::Info, format!("traceroute → {} …", host));
                        spawn_trace_request(
                            trace::TraceRequest {
                                target: host,
                                gateway: app.gateway_host(),
                                max_hops: 20,
                                timeout: Duration::from_secs(25),
                            },
                            tx.clone(),
                            None,
                        );
                    }
                    _ => {}
                }
            }
        }

        if last_draw.elapsed() >= draw_period {
            terminal.draw(|f| ui::draw(f, app))?;
            *last_draw = Instant::now();
        }
    }
}

fn emit(
    audio: Option<&mpsc::UnboundedSender<sound::SoundEvent>>,
    ev: Option<sound::SoundEvent>,
    muted: bool,
) {
    if muted {
        return;
    }
    if let (Some(a), Some(e)) = (audio, ev) {
        let _ = a.send(e);
    }
}

/// Collect all probe results from the JoinSet. Continues past JoinError
/// (panicked probe stays Missing; round continues).
async fn collect_round(set: &mut JoinSet<(usize, net::PingSample)>, n: usize) -> Vec<RoundResult> {
    let mut results = vec![RoundResult::Missing; n];
    while let Some(res) = set.join_next().await {
        if let Ok((idx, sample)) = res {
            results[idx] = RoundResult::Observed(sample);
        }
    }
    results
}

/// Probe the auto-gateway on a 5s cadence, reading the address from the
/// shared handle so re-detection retargets the probe without a respawn.
fn spawn_gateway_pinger(
    idx: usize,
    shared: Arc<std::sync::Mutex<(u64, String)>>,
    timeout_ms: u64,
    gw_cadence_ms: Arc<AtomicU64>,
    tx: mpsc::UnboundedSender<AppMsg>,
) {
    tokio::spawn(async move {
        loop {
            let ms = gw_cadence_ms
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(1000);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let (epoch, addr) = shared.lock().map(|g| g.clone()).unwrap_or_default();
            if addr.is_empty() {
                continue;
            }
            let pinger = net::TcpPinger {
                addr,
                port: 80,
                timeout_ms,
                alive_on_rejected: true,
            };
            let s = pinger.ping().await;
            let _ = tx.send(AppMsg::GatewayProbeResult(idx, epoch, s));
        }
    });
}

#[cfg(test)]
mod checks {
    use super::{
        close_succeeded, parse_primary_targets, reconcile_open_incident,
        should_auto_export_transition, should_start_automatic_trace, trace_error_record,
        trace_target_for_app,
    };
    use crate::app::{App, LinkState};
    use crate::checkpoint::{Checkpoint, StateStore};
    use crate::config::RunMode;
    use crate::incident::{Evidence, IncidentWriter, TransitionNotice};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn outage_start_notice() -> TransitionNotice {
        TransitionNotice {
            from: "up".into(),
            to: "down".into(),
            at: SystemTime::now(),
            duration_ms: 0,
            cause: "unknown".into(),
            evidence: Evidence::default(),
        }
    }

    fn tsv_contents(dir: &Path) -> String {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("tsv"))
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect()
    }

    #[test]
    fn msg_round_trip() {
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn target_parser_accepts_a_valid_explicit_list() {
        let targets = parse_primary_targets("ok:example.test:443").unwrap();
        assert_eq!(targets, vec![("ok".into(), "example.test".into(), 443)]);
    }

    #[test]
    fn target_parser_rejects_an_all_invalid_explicit_list() {
        assert!(parse_primary_targets("bad-entry").is_err());
    }

    #[test]
    fn target_parser_rejects_a_partially_invalid_explicit_list() {
        assert!(parse_primary_targets("ok:example.test:443,bad-entry").is_err());
    }

    #[test]
    fn target_parser_rejects_zero_port() {
        assert!(super::parse_target_piece("zero:example.test:0").is_err());
    }

    #[test]
    fn target_parser_rejects_option_like_hosts() {
        assert!(super::parse_target_piece("bad:-n:443").is_err());
        assert!(super::parse_target_piece("bad:[-n]:443").is_err());
    }

    #[test]
    fn target_parser_rejects_embedded_whitespace_and_control_in_hosts() {
        for host in [
            "example .test",
            "example\t.test",
            "example\n.test",
            "example\u{7f}.test",
        ] {
            assert!(
                super::parse_target_piece(&format!("bad:{host}:443")).is_err(),
                "host should be rejected: {host:?}"
            );
        }
    }

    #[test]
    fn extra_target_list_rejects_a_partially_invalid_entry() {
        assert!(super::parse_target_list("ok:example.test:443,bad-entry", "PM_EXTRAS").is_err());
    }

    #[test]
    fn absent_extra_target_list_is_valid_and_empty() {
        assert!(super::configured_extra_targets_from(None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn target_parser_accepts_bracketed_ipv6_targets() {
        let targets = parse_primary_targets("v6:[2001:db8::1]:443").unwrap();
        assert_eq!(targets, vec![("v6".into(), "2001:db8::1".into(), 443)]);
    }

    #[test]
    fn target_parser_still_accepts_ipv4_and_dns_hosts() {
        let targets = parse_primary_targets("ipv4:192.0.2.1:443,dns:example.test:80").unwrap();
        assert_eq!(
            targets,
            vec![
                ("ipv4".into(), "192.0.2.1".into(), 443),
                ("dns".into(), "example.test".into(), 80),
            ]
        );
    }

    #[test]
    fn failed_incident_close_is_not_a_clean_shutdown() {
        assert!(!close_succeeded(&Err(std::io::Error::other("disk full"))));
        assert!(close_succeeded(&Ok(())));
    }

    #[test]
    fn trace_errors_become_durable_unavailable_records() {
        let record = trace_error_record("no trace utility available");
        assert_eq!(record.classification, "unavailable");
        assert!(record.raw_output.contains("no trace utility"));
    }

    #[test]
    fn automatic_trace_prefers_an_affected_primary_over_first_healthy_primary() {
        let mut app = App::new(crate::app::Config::default());
        app.primaries.push(crate::app::PrimaryProbe::new(
            "healthy",
            "healthy.example",
            443,
        ));
        app.primaries.push(crate::app::PrimaryProbe::new(
            "affected",
            "affected.example",
            443,
        ));
        app.primaries[1].state = LinkState::Down;

        assert_eq!(trace_target_for_app(&app), Some("affected.example".into()));
    }

    #[test]
    fn interrupted_incident_cleanup_marks_checkpoint_clean() {
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ping-monitor-reconcile-{nonce}"));
        let log_dir = dir.join("logs");
        let state_path = dir.join("state");
        let mut writer = IncidentWriter::new(&log_dir).unwrap();
        let store = StateStore::new(&state_path);
        let checkpoint = Checkpoint {
            state: "down".into(),
            episode_kind: Some("outage".into()),
            incident_id: Some("incident-restarted".into()),
            started_at_ms: Some(1),
            config_fingerprint: "old".into(),
            evidence: Evidence::default(),
        };

        assert!(
            !reconcile_open_incident(&mut writer, Some(&store), Some(&checkpoint), "current",)
                .unwrap()
        );

        let saved = store
            .load()
            .unwrap()
            .expect("clean checkpoint should exist");
        assert_eq!(saved.state, "up");
        assert_eq!(saved.config_fingerprint, "current");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_down_episode_gets_a_trace_after_degraded_state() {
        assert!(should_start_automatic_trace(
            LinkState::Degraded,
            LinkState::Down,
            true
        ));
        assert!(!should_start_automatic_trace(
            LinkState::Down,
            LinkState::Degraded,
            true
        ));
    }

    #[test]
    fn automatic_session_export_is_scoped_to_tui_transitions() {
        assert!(should_auto_export_transition(RunMode::Tui, true));
        assert!(!should_auto_export_transition(RunMode::Headless, true));
        assert!(!should_auto_export_transition(RunMode::Tui, false));
    }

    #[test]
    fn open_start_without_checkpoint_gets_exactly_one_restart_end() {
        let dir = std::env::temp_dir().join(format!(
            "ping-monitor-open-start-no-checkpoint-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&outage_start_notice()).unwrap();

        let recovered = reconcile_open_incident(&mut writer, None, None, "test").unwrap();

        assert!(recovered);
        let contents = tsv_contents(&dir);
        assert_eq!(contents.matches("\tEND\t").count(), 1);
        assert!(contents.contains("monitor_restarted"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_start_with_clean_checkpoint_gets_exactly_one_restart_end() {
        let dir = std::env::temp_dir().join(format!(
            "ping-monitor-open-start-clean-checkpoint-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state_path = dir.join("state");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&outage_start_notice()).unwrap();
        let store = StateStore::new(&state_path);
        store
            .save(&Checkpoint {
                state: "up".into(),
                episode_kind: None,
                incident_id: None,
                started_at_ms: None,
                config_fingerprint: "test".into(),
                evidence: Evidence::default(),
            })
            .unwrap();

        let checkpoint = store.load().unwrap();
        let recovered =
            reconcile_open_incident(&mut writer, Some(&store), checkpoint.as_ref(), "test")
                .unwrap();

        assert!(recovered);
        let contents = tsv_contents(&dir);
        assert_eq!(contents.matches("\tEND\t").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn closed_episode_is_not_closed_again_during_recovery() {
        let dir = std::env::temp_dir().join(format!(
            "ping-monitor-closed-recovery-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&outage_start_notice()).unwrap();
        writer
            .close_with_evidence(
                SystemTime::now(),
                Duration::from_secs(1),
                "recovered",
                &Evidence::default(),
            )
            .unwrap();

        let recovered = reconcile_open_incident(&mut writer, None, None, "test").unwrap();

        assert!(!recovered);
        assert_eq!(tsv_contents(&dir).matches("\tEND\t").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_checkpoint_for_closed_episode_does_not_duplicate_end() {
        let dir = std::env::temp_dir().join(format!(
            "ping-monitor-stale-checkpoint-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state_path = dir.join("state");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&outage_start_notice()).unwrap();
        let metadata = writer.active_metadata().unwrap();
        writer
            .close_with_evidence(
                SystemTime::now(),
                Duration::from_secs(1),
                "recovered",
                &Evidence::default(),
            )
            .unwrap();
        let store = StateStore::new(&state_path);
        store
            .save(&Checkpoint {
                state: "down".into(),
                episode_kind: Some(metadata.episode_kind.clone()),
                incident_id: Some(metadata.id.clone()),
                started_at_ms: metadata
                    .started_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|duration| duration.as_millis() as u64),
                config_fingerprint: "stale".into(),
                evidence: Evidence::default(),
            })
            .unwrap();
        let checkpoint = store.load().unwrap();

        let recovered =
            reconcile_open_incident(&mut writer, Some(&store), checkpoint.as_ref(), "test")
                .unwrap();

        assert!(!recovered);
        assert_eq!(tsv_contents(&dir).matches("\tEND\t").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
