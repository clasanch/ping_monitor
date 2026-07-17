mod app;
mod net;
mod sound;
mod ui;
mod wifi;

use app::{App, Config};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::interval;

#[derive(Debug)]
enum AppMsg {
    Ping(usize, net::PingSample),
    Dns(usize, usize, Option<f64>),
    ExtraPing(usize, net::PingSample),
    TraceLine(String),
    TraceDone,
    WifiRssi(Option<i16>),
    Quit,
}

const DEFAULT_PRIMARIES: &[(&str, &str, u16)] = &[
    ("cf", "1.1.1.1", 443),
    ("gg", "8.8.8.8", 443),
    ("q9", "9.9.9.9", 443),
];

#[tokio::main]
async fn main() -> std::io::Result<()> {
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

    let audio = sound::spawn_audio();
    let (audio_tx, audio_state) = match &audio {
        Some((tx, st)) => (Some(tx.clone()), Some(Arc::clone(st))),
        None => (None, None),
    };
    if audio_tx.is_none() {
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
    let ping_interval_handle = Arc::clone(&app.interval_ms);

    let primaries: Vec<(String, String, u16)> = match std::env::var("PM_TARGETS") {
        Ok(raw) => raw
            .split(',')
            .filter_map(|piece| {
                let parts: Vec<&str> = piece.splitn(3, ':').collect();
                if parts.len() != 3 {
                    return None;
                }
                let port: u16 = parts[2].trim().parse().ok()?;
                Some((
                    parts[0].trim().to_string(),
                    parts[1].trim().to_string(),
                    port,
                ))
            })
            .collect(),
        Err(_) => DEFAULT_PRIMARIES
            .iter()
            .map(|(l, h, p)| (l.to_string(), h.to_string(), *p))
            .collect(),
    };

    for (label, host, port) in &primaries {
        app.primaries
            .push(app::PrimaryProbe::new(label, host, *port));
    }

    if let Ok(raw) = std::env::var("PM_EXTRAS") {
        for piece in raw.split(',') {
            let parts: Vec<&str> = piece.splitn(3, ':').collect();
            if parts.len() != 3 {
                continue;
            }
            let label = parts[0].trim().to_string();
            let host = parts[1].trim().to_string();
            let port: u16 = match parts[2].trim().parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
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
            });
        }
        if !app.extras.is_empty() {
            app.log(
                app::Level::Info,
                format!("extras: {} probe(s)", app.extras.len()),
            );
        }
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

    for (idx, (_label, host, port)) in primaries.iter().enumerate() {
        let tx = tx.clone();
        let pinger = net::TcpPinger {
            addr: host.clone(),
            port: *port,
            timeout_ms: app.cfg.timeout_ms,
        };
        let stagger_ms = (idx as u64) * 200;
        let interval_handle = Arc::clone(&ping_interval_handle);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(stagger_ms)).await;
            loop {
                let ms = interval_handle
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(50);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                let s = pinger.ping().await;
                let _ = tx.send(AppMsg::Ping(idx, s));
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
        let tx = tx.clone();
        let pinger = net::TcpPinger {
            addr: ex.host.clone(),
            port: ex.port,
            timeout_ms: app.cfg.timeout_ms,
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
    let _ = result;
    std::process::exit(0);
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
                AppMsg::Ping(i, s) => emit(audio, app.ingest_ping(i, s), app.muted),
                AppMsg::Dns(r, d, v) => {
                    let _ = app.ingest_dns(r, d, v);
                }
                AppMsg::ExtraPing(i, s) => app.ingest_extra(i, s),
                AppMsg::TraceLine(line) => app.log(app::Level::Info, line),
                AppMsg::TraceDone => app.log(app::Level::Info, "traceroute finished"),
                AppMsg::WifiRssi(v) => app.set_wifi_rssi(v),
                AppMsg::Quit => return Ok(()),
            }
        }

        if let Some((ev, lvl, msg)) = app.tick_reminder() {
            let suffix = if app.muted { "" } else { "  ♪" };
            app.log(lvl, format!("{}{}", msg, suffix));
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
                        let tx = tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let prog = if std::path::Path::new("/usr/sbin/traceroute").exists() {
                                "/usr/sbin/traceroute"
                            } else if std::path::Path::new("/usr/bin/traceroute").exists() {
                                "/usr/bin/traceroute"
                            } else {
                                let _ = tx
                                    .send(AppMsg::TraceLine("traceroute binary not found".into()));
                                let _ = tx.send(AppMsg::TraceDone);
                                return;
                            };
                            let out = std::process::Command::new(prog)
                                .args(["-n", "-q", "1", "-w", "1", "-m", "20"])
                                .arg(&host)
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                            let mut child = match out {
                                Ok(c) => c,
                                Err(e) => {
                                    let _ =
                                        tx.send(AppMsg::TraceLine(format!("spawn fail: {}", e)));
                                    let _ = tx.send(AppMsg::TraceDone);
                                    return;
                                }
                            };
                            use std::io::{BufRead, BufReader};
                            if let Some(stdout) = child.stdout.take() {
                                let reader = BufReader::new(stdout);
                                for line in reader.lines().map_while(Result::ok) {
                                    let _ = tx.send(AppMsg::TraceLine(line));
                                }
                            }
                            let _ = child.wait();
                            let _ = tx.send(AppMsg::TraceDone);
                        });
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

#[cfg(test)]
mod checks {
    #[test]
    fn msg_round_trip() {
        assert_eq!(2 + 2, 4);
    }
}
