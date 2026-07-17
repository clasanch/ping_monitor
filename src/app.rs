use crate::sound::SoundEvent;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const WIN: usize = 120;
pub const HIST_BUCKET_SECS: u64 = 30;
pub const HIST_BUCKETS: usize = 120;
pub const BASELINE_CAP: usize = 300;
pub const POOL_HIST_CAP: usize = 360;

pub fn rssi_grade(rssi: i16) -> u8 {
    if rssi >= -55 {
        4
    } else if rssi >= -67 {
        3
    } else if rssi >= -75 {
        2
    } else if rssi >= -85 {
        1
    } else {
        0
    }
}

pub fn rssi_verdict_grade(g: u8) -> &'static str {
    match g {
        4 => "excellent",
        3 => "good",
        2 => "fair",
        1 => "weak",
        _ => "bad",
    }
}
pub const EVENTS_CAP: usize = 24;

#[derive(Clone)]
pub struct Baseline {
    pub latencies: VecDeque<f64>,
    pub jitters: VecDeque<f64>,
}

impl Baseline {
    pub fn new() -> Self {
        Self {
            latencies: VecDeque::with_capacity(BASELINE_CAP),
            jitters: VecDeque::with_capacity(BASELINE_CAP),
        }
    }

    pub fn push(&mut self, lat: Option<f64>, jit: f64) {
        if let Some(v) = lat {
            if self.latencies.len() == BASELINE_CAP {
                self.latencies.pop_front();
            }
            self.latencies.push_back(v);
        }
        if self.jitters.len() == BASELINE_CAP {
            self.jitters.pop_front();
        }
        self.jitters.push_back(jit);
    }

    fn percentile(v: &[f64], p: f64) -> Option<f64> {
        if v.is_empty() {
            return None;
        }
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((p / 100.0) * s.len() as f64).floor() as usize;
        Some(s[idx.min(s.len() - 1)])
    }

    pub fn lat_p90(&self) -> Option<f64> {
        Self::percentile(&self.latencies.iter().copied().collect::<Vec<_>>(), 90.0)
    }
    pub fn jit_p90(&self) -> Option<f64> {
        Self::percentile(&self.jitters.iter().copied().collect::<Vec<_>>(), 90.0)
    }
    pub fn len(&self) -> usize {
        self.latencies.len()
    }
}

#[derive(Clone, Default)]
pub struct HistBucket {
    pub peak_rtt: Option<f64>,
    pub peak_primary: Option<String>,
    pub count: u32,
    pub loss: u32,
}

impl HistBucket {
    pub fn push(&mut self, rtt: Option<f64>, label: &str) {
        self.count += 1;
        match rtt {
            Some(v) => {
                let new_peak = match self.peak_rtt {
                    Some(p) => v > p,
                    None => true,
                };
                if new_peak {
                    self.peak_rtt = Some(v);
                    self.peak_primary = Some(label.to_string());
                }
            }
            None => {
                self.loss += 1;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkState {
    Up,
    Degraded,
    Down,
}

fn push_spike(list: &mut Vec<(f64, u64)>, value: f64, cap: usize) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    list.push((value, ts));
    list.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    list.truncate(cap);
}

#[allow(clippy::too_many_arguments)]
fn step_dwell(
    state: LinkState,
    is_bad: bool,
    is_down: bool,
    bad_streak: u32,
    good_streak: u32,
    cfg: &Config,
    now: Instant,
    recover_at: Option<Instant>,
) -> (LinkState, Option<Instant>) {
    let dwell = cfg.recover_dwell;
    match state {
        LinkState::Up => {
            if bad_streak >= cfg.hysteresis_bad {
                let new_state = if is_down {
                    LinkState::Down
                } else {
                    LinkState::Degraded
                };
                (new_state, None)
            } else {
                (LinkState::Up, None)
            }
        }
        LinkState::Degraded => {
            if is_down && bad_streak >= cfg.hysteresis_bad {
                (LinkState::Down, None)
            } else if good_streak >= cfg.hysteresis_good {
                let started = recover_at.unwrap_or(now);
                if now.duration_since(started) >= dwell {
                    let new_state = if is_bad {
                        LinkState::Degraded
                    } else {
                        LinkState::Up
                    };
                    (new_state, None)
                } else {
                    (LinkState::Degraded, Some(started))
                }
            } else {
                (LinkState::Degraded, None)
            }
        }
        LinkState::Down => {
            if good_streak >= cfg.hysteresis_good {
                let started = recover_at.unwrap_or(now);
                if now.duration_since(started) >= dwell {
                    let new_state = if is_bad {
                        LinkState::Degraded
                    } else {
                        LinkState::Up
                    };
                    (new_state, None)
                } else {
                    (LinkState::Down, Some(started))
                }
            } else {
                (LinkState::Down, None)
            }
        }
    }
}

#[derive(Clone)]
pub struct Ring<T: Copy + Default> {
    pub buf: VecDeque<T>,
    pub cap: usize,
}

impl<T: Copy + Default> Ring<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }
    pub fn push(&mut self, v: T) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }
    pub fn as_vec(&self) -> Vec<T> {
        self.buf.iter().copied().collect()
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

#[derive(Default, Clone, Copy)]
pub struct LatStat {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub last: Option<f64>,
}

impl LatStat {
    pub fn add(&mut self, v: f64) {
        self.count += 1;
        self.sum += v;
        self.last = Some(v);
        if self.count == 1 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
    }
    pub fn avg(&self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.sum / self.count as f64)
        }
    }
}

pub struct Event {
    pub ts: std::time::SystemTime,
    pub level: Level,
    pub msg: String,
}

#[derive(Clone, Copy)]
pub enum Level {
    Info,
    Warn,
    Bad,
    Good,
}

#[derive(Clone)]
pub struct Config {
    pub ping_interval_ms: u64,
    pub dns_interval_ms: u64,
    pub timeout_ms: u64,
    pub latency_warn_ms: f64,
    pub latency_bad_ms: f64,
    pub jitter_warn_ms: f64,
    pub dns_warn_ms: f64,
    pub dns_bad_ms: f64,
    pub state_window: usize,
    pub degraded_loss_pct: f64,
    pub down_loss_pct: f64,
    pub hysteresis_good: u32,
    pub hysteresis_bad: u32,
    pub recover_dwell: Duration,
    pub reminder_interval: Duration,
}

pub const DEFAULT_DNS_RESOLVERS: &[(&str, Option<&str>)] = &[
    ("sys", None),
    ("cf", Some("1.1.1.1")),
    ("gg", Some("8.8.8.8")),
];

pub const DEFAULT_DNS_NAMES: &[&str] = &["www.google.com", "www.cloudflare.com", "www.amazon.com"];

impl Default for Config {
    fn default() -> Self {
        Self {
            ping_interval_ms: 1_000,
            dns_interval_ms: 5_000,
            timeout_ms: 1_500,
            latency_warn_ms: 200.0,
            latency_bad_ms: 500.0,
            jitter_warn_ms: 60.0,
            dns_warn_ms: 100.0,
            dns_bad_ms: 400.0,
            state_window: 20,
            degraded_loss_pct: 20.0,
            down_loss_pct: 60.0,
            hysteresis_good: 8,
            hysteresis_bad: 3,
            recover_dwell: Duration::from_secs(15),
            reminder_interval: Duration::from_secs(30),
        }
    }
}

impl Config {
    pub fn validate(&mut self) -> Vec<&'static str> {
        let mut warns = Vec::new();
        let clamp =
            |v: &mut u64, lo: u64, hi: u64, name: &'static str, warns: &mut Vec<&'static str>| {
                if *v < lo {
                    *v = lo;
                    warns.push(name);
                } else if *v > hi {
                    *v = hi;
                    warns.push(name);
                }
            };
        clamp(
            &mut self.timeout_ms,
            50,
            10_000,
            "timeout_ms clamped to [50,10000]",
            &mut warns,
        );
        clamp(
            &mut self.ping_interval_ms,
            200,
            60_000,
            "ping_interval_ms clamped to [200,60000]",
            &mut warns,
        );
        clamp(
            &mut self.dns_interval_ms,
            1_000,
            60_000,
            "dns_interval_ms clamped to [1000,60000]",
            &mut warns,
        );
        let rd = self.recover_dwell.as_secs();
        if rd < 1 {
            self.recover_dwell = Duration::from_secs(1);
            warns.push("recover_dwell clamped to [1s,10min]");
        } else if rd > 600 {
            self.recover_dwell = Duration::from_secs(600);
            warns.push("recover_dwell clamped to [1s,10min]");
        }
        let ri = self.reminder_interval.as_secs();
        if ri < 5 {
            self.reminder_interval = Duration::from_secs(5);
            warns.push("reminder_interval clamped to [5s,1h]");
        } else if ri > 3600 {
            self.reminder_interval = Duration::from_secs(3600);
            warns.push("reminder_interval clamped to [5s,1h]");
        }
        if self.state_window < 5 {
            self.state_window = 5;
            warns.push("state_window clamped to [5,500]");
        } else if self.state_window > 500 {
            self.state_window = 500;
            warns.push("state_window clamped to [5,500]");
        }
        if self.hysteresis_good == 0 {
            self.hysteresis_good = 1;
            warns.push("hysteresis_good clamped to [1,50]");
        } else if self.hysteresis_good > 50 {
            self.hysteresis_good = 50;
            warns.push("hysteresis_good clamped to [1,50]");
        }
        if self.hysteresis_bad == 0 {
            self.hysteresis_bad = 1;
            warns.push("hysteresis_bad clamped to [1,50]");
        } else if self.hysteresis_bad > 50 {
            self.hysteresis_bad = 50;
            warns.push("hysteresis_bad clamped to [1,50]");
        }
        warns
    }
}

#[derive(Clone)]
pub struct DnsCell {
    pub ring: Ring<Option<f64>>,
    pub stat: LatStat,
    pub last: Option<f64>,
    pub state: LinkState,
}

impl DnsCell {
    pub fn new() -> Self {
        Self {
            ring: Ring::new(WIN),
            stat: LatStat::default(),
            last: None,
            state: LinkState::Up,
        }
    }
    pub fn reset(&mut self) {
        self.ring = Ring::new(WIN);
        self.stat = LatStat::default();
        self.last = None;
        self.state = LinkState::Up;
    }
}

pub struct DnsMatrix {
    pub resolvers: Vec<(String, Option<String>)>,
    pub names: Vec<String>,
    pub cells: Vec<Vec<DnsCell>>,
}

impl DnsMatrix {
    pub fn new(resolvers: Vec<(String, Option<String>)>, names: Vec<String>) -> Self {
        let cells = (0..resolvers.len())
            .map(|_| (0..names.len()).map(|_| DnsCell::new()).collect())
            .collect();
        Self {
            resolvers,
            names,
            cells,
        }
    }
}

struct ExportRow {
    epoch_s: u64,
    elapsed_s: u64,
    state: &'static str,
    score: u64,
    last_rtt_ms: Option<f64>,
    avg_rtt_ms: f64,
    min_rtt_ms: f64,
    max_rtt_ms: f64,
    total: u64,
    lost: u64,
    loss_pct: f64,
    jitter_cur_ms: f64,
    last_dns_ms: Option<f64>,
    avg_dns_ms: f64,
    cadence_ms: u64,
}

pub type NotifyFn = Arc<dyn Fn(&str) + Send + Sync>;

pub struct App {
    pub cfg: Config,
    pub primaries: Vec<PrimaryProbe>,
    pub dns: DnsMatrix,
    pub state: LinkState,
    pub state_since: Instant,
    pub recover_at: Option<Instant>,
    pub last_reminder: Option<Instant>,
    pub good_streak: u32,
    pub bad_streak: u32,
    pub events: VecDeque<Event>,
    pub muted: bool,
    pub started: Instant,
    pub audio_state: Option<Arc<crate::sound::AudioState>>,
    pub hist: VecDeque<HistBucket>,
    pub cur_bucket_start: Option<Instant>,
    pub interval_ms: Arc<AtomicU64>,
    pub lat_hist: Ring<Option<f64>>,
    pub jit_hist: Ring<f64>,
    pub extras: Vec<ExtraProbe>,
    pub last_export: Option<String>,
    pub best_uptime_secs: u64,
    pub worst_loss_burst: u32,
    pub peak_latency: f64,
    pub peak_jitter: f64,
    pub up_since: Option<Instant>,
    pub notify_fn: Option<NotifyFn>,
    pub wifi_rssi: Option<i16>,
    pub wifi_grade: Option<u8>,
    pub sess_up_ms: u64,
    pub sess_degraded_ms: u64,
    pub sess_down_ms: u64,
    pub last_accrual: Option<Instant>,
    pub bad_since: Option<Instant>,
    pub recoveries: u32,
    pub outages: u32,
    pub mttr_ms_total: u64,
    pub top_latency: Vec<(f64, u64)>,
    pub top_jitter: Vec<(f64, u64)>,
}

#[derive(Clone)]
pub struct PrimaryProbe {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub lat_ring: Ring<Option<f64>>,
    pub loss_ring: Ring<f64>,
    pub jitter_ring: Ring<f64>,
    pub baseline: Baseline,
    pub stat: LatStat,
    pub last_value: Option<f64>,
    pub consec_loss: u32,
    pub cur_loss_burst: u32,
    pub good_streak: u32,
    pub bad_streak: u32,
    pub state: LinkState,
    pub state_since: Instant,
    pub recover_at: Option<Instant>,
    pub total: u64,
    pub lost: u64,
    pub last_target_state: LinkState,
}

impl PrimaryProbe {
    pub fn new(label: &str, host: &str, port: u16) -> Self {
        Self {
            label: label.into(),
            host: host.into(),
            port,
            lat_ring: Ring::new(WIN),
            loss_ring: Ring::new(WIN),
            jitter_ring: Ring::new(WIN),
            baseline: Baseline::new(),
            stat: LatStat::default(),
            last_value: None,
            consec_loss: 0,
            cur_loss_burst: 0,
            good_streak: 0,
            bad_streak: 0,
            state: LinkState::Up,
            state_since: Instant::now(),
            recover_at: None,
            total: 0,
            lost: 0,
            last_target_state: LinkState::Up,
        }
    }

    pub fn reset(&mut self) {
        self.lat_ring = Ring::new(WIN);
        self.loss_ring = Ring::new(WIN);
        self.jitter_ring = Ring::new(WIN);
        self.baseline = Baseline::new();
        self.stat = LatStat::default();
        self.last_value = None;
        self.consec_loss = 0;
        self.cur_loss_burst = 0;
        self.good_streak = 0;
        self.bad_streak = 0;
        self.state = LinkState::Up;
        self.state_since = Instant::now();
        self.recover_at = None;
        self.last_target_state = LinkState::Up;
        self.total = 0;
        self.lost = 0;
    }

    fn classify(&self, cfg: &Config) -> (bool, bool) {
        let win_n = self.lat_ring.buf.len().min(cfg.state_window);
        if win_n < 5 {
            return (false, false);
        }
        let samples: Vec<Option<f64>> = self.lat_ring.as_vec();
        let win: Vec<Option<f64>> = samples.iter().rev().take(win_n).copied().collect();
        let losses = win.iter().filter(|v| v.is_none()).count();
        let loss_pct = losses as f64 * 100.0 / win_n as f64;
        let lats: Vec<f64> = win.iter().filter_map(|v| *v).collect();
        let avg_lat = lats.iter().sum::<f64>() / lats.len().max(1) as f64;
        let jit: Vec<f64> = self
            .jitter_ring
            .as_vec()
            .iter()
            .rev()
            .take(win_n)
            .copied()
            .collect();
        let avg_jit = jit.iter().sum::<f64>() / jit.len().max(1) as f64;

        let (lat_t, jit_t, loss_t, down_t) = self.adaptive_thresholds(cfg);
        let is_bad = loss_pct >= loss_t || avg_lat > lat_t || avg_jit > jit_t;
        let is_down = loss_pct >= down_t;
        (is_bad, is_down)
    }

    fn adaptive_thresholds(&self, cfg: &Config) -> (f64, f64, f64, f64) {
        if self.baseline.len() < 50 {
            return (
                cfg.latency_warn_ms,
                cfg.jitter_warn_ms,
                cfg.degraded_loss_pct,
                cfg.down_loss_pct,
            );
        }
        let lat_p90 = self.baseline.lat_p90().unwrap_or(cfg.latency_warn_ms);
        let jit_p90 = self.baseline.jit_p90().unwrap_or(cfg.jitter_warn_ms);
        let lat_thresh = (lat_p90 * 2.0).max(cfg.latency_warn_ms);
        let jit_thresh = (jit_p90 * 2.5).max(cfg.jitter_warn_ms);
        (
            lat_thresh,
            jit_thresh,
            cfg.degraded_loss_pct,
            cfg.down_loss_pct,
        )
    }
}

#[derive(Clone)]
pub struct ExtraProbe {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub last: Option<f64>,
    pub state: LinkState,
    pub total: u64,
    pub lost: u64,
    pub ring: Ring<Option<f64>>,
    pub consec_loss: u32,
}

impl ExtraProbe {
    pub fn reset(&mut self) {
        self.last = None;
        self.state = LinkState::Up;
        self.total = 0;
        self.lost = 0;
        self.consec_loss = 0;
        self.ring = Ring::new(30);
    }
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let interval_init = cfg.ping_interval_ms;
        let resolvers: Vec<(String, Option<String>)> = DEFAULT_DNS_RESOLVERS
            .iter()
            .map(|(l, ip)| (l.to_string(), ip.map(|s| s.to_string())))
            .collect();
        let names: Vec<String> = DEFAULT_DNS_NAMES.iter().map(|s| s.to_string()).collect();
        Self {
            cfg,
            primaries: Vec::new(),
            dns: DnsMatrix::new(resolvers, names),
            state: LinkState::Up,
            state_since: Instant::now(),
            recover_at: None,
            last_reminder: None,
            good_streak: 0,
            bad_streak: 0,
            events: VecDeque::with_capacity(256),
            muted: false,
            started: Instant::now(),
            audio_state: None,
            hist: VecDeque::with_capacity(HIST_BUCKETS),
            cur_bucket_start: None,
            interval_ms: Arc::new(AtomicU64::new(interval_init)),
            lat_hist: Ring::new(POOL_HIST_CAP),
            jit_hist: Ring::new(POOL_HIST_CAP),
            extras: Vec::new(),
            last_export: None,
            best_uptime_secs: 0,
            worst_loss_burst: 0,
            peak_latency: 0.0,
            peak_jitter: 0.0,
            up_since: Some(Instant::now()),
            notify_fn: None,
            wifi_rssi: None,
            wifi_grade: None,
            sess_up_ms: 0,
            sess_degraded_ms: 0,
            sess_down_ms: 0,
            last_accrual: Some(Instant::now()),
            bad_since: None,
            recoveries: 0,
            outages: 0,
            mttr_ms_total: 0,
            top_latency: Vec::new(),
            top_jitter: Vec::new(),
        }
    }

    pub fn pooled_loss_pct(&self) -> f64 {
        let t: u64 = self.primaries.iter().map(|p| p.total).sum();
        let l: u64 = self.primaries.iter().map(|p| p.lost).sum();
        if t == 0 {
            0.0
        } else {
            l as f64 * 100.0 / t as f64
        }
    }

    fn pooled_p90s(&self) -> (Option<f64>, Option<f64>) {
        let mut lat_max: Option<f64> = None;
        let mut jit_max: Option<f64> = None;
        for p in &self.primaries {
            if p.baseline.len() < 50 {
                continue;
            }
            if let Some(v) = p.baseline.lat_p90() {
                lat_max = Some(lat_max.map_or(v, |m: f64| m.max(v)));
            }
            if let Some(v) = p.baseline.jit_p90() {
                jit_max = Some(jit_max.map_or(v, |m: f64| m.max(v)));
            }
        }
        (lat_max, jit_max)
    }

    pub fn lat_warn_ms(&self) -> f64 {
        let (lat, _) = self.pooled_p90s();
        lat.map_or(self.cfg.latency_warn_ms, |v| v * 2.0)
            .max(self.cfg.latency_warn_ms)
    }

    pub fn lat_bad_ms(&self) -> f64 {
        let (lat, _) = self.pooled_p90s();
        lat.map_or(self.cfg.latency_bad_ms, |v| v * 4.0)
            .max(self.cfg.latency_bad_ms)
    }

    pub fn jit_warn_ms(&self) -> f64 {
        let (_, jit) = self.pooled_p90s();
        jit.map_or(self.cfg.jitter_warn_ms, |v| v * 2.5)
            .max(self.cfg.jitter_warn_ms)
    }

    fn dns_p90_max(&self) -> Option<f64> {
        let mut max: Option<f64> = None;
        for row in &self.dns.cells {
            for cell in row {
                if cell.ring.len() < 50 {
                    continue;
                }
                let vals: Vec<f64> = cell.ring.as_vec().into_iter().flatten().collect();
                if vals.is_empty() {
                    continue;
                }
                if let Some(v) = Baseline::percentile(&vals, 90.0) {
                    max = Some(max.map_or(v, |m: f64| m.max(v)));
                }
            }
        }
        max
    }

    pub fn dns_warn_ms(&self) -> f64 {
        self.dns_p90_max()
            .map_or(self.cfg.dns_warn_ms, |v| v * 2.0)
            .max(self.cfg.dns_warn_ms)
    }

    pub fn dns_bad_ms(&self) -> f64 {
        self.dns_p90_max()
            .map_or(self.cfg.dns_bad_ms, |v| v * 4.0)
            .max(self.cfg.dns_bad_ms)
    }

    pub fn set_wifi_rssi(&mut self, rssi: Option<i16>) {
        let new_grade = rssi.map(rssi_grade);
        if new_grade == self.wifi_grade {
            self.wifi_rssi = rssi;
            return;
        }
        let (lvl, msg) = match (self.wifi_grade, new_grade) {
            (None, None) => {
                self.wifi_rssi = rssi;
                return;
            }
            (None, Some(g)) => {
                let lvl = if g <= 1 { Level::Warn } else { Level::Info };
                (
                    lvl,
                    format!("wifi up  {} dBm ({})", rssi.unwrap(), rssi_verdict_grade(g)),
                )
            }
            (Some(_), None) => (Level::Warn, "wifi down".to_string()),
            (Some(old), Some(new)) => {
                let lvl = if new > old { Level::Good } else { Level::Warn };
                (
                    lvl,
                    format!(
                        "wifi {} → {}  ({})",
                        rssi_verdict_grade(old),
                        rssi_verdict_grade(new),
                        rssi.unwrap(),
                    ),
                )
            }
        };
        self.wifi_grade = new_grade;
        self.wifi_rssi = rssi;
        self.log(lvl, msg);
    }

    pub fn pooled_total(&self) -> u64 {
        self.primaries.iter().map(|p| p.total).sum()
    }
    pub fn pooled_lost(&self) -> u64 {
        self.primaries.iter().map(|p| p.lost).sum()
    }
    pub fn last_value_view(&self) -> Option<f64> {
        let vals: Vec<f64> = self.primaries.iter().filter_map(|p| p.last_value).collect();
        if vals.is_empty() {
            return None;
        }
        let mut s = vals;
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(s[s.len() / 2])
    }
    pub fn jitter_view(&self) -> f64 {
        let vals: Vec<f64> = self
            .primaries
            .iter()
            .filter_map(|p| p.jitter_ring.buf.back().copied())
            .collect();
        if vals.is_empty() {
            return 0.0;
        }
        let mut s = vals;
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        s[s.len() / 2]
    }
    pub fn pooled_ping_stat(&self) -> LatStat {
        let mut out = LatStat::default();
        for p in &self.primaries {
            out.count += p.stat.count;
            out.sum += p.stat.sum;
            if out.count == p.stat.count {
                out.min = p.stat.min;
                out.max = p.stat.max;
            } else {
                out.min = out.min.min(p.stat.min);
                out.max = out.max.max(p.stat.max);
            }
        }
        out
    }

    pub fn log(&mut self, lvl: Level, msg: impl Into<String>) {
        if self.events.len() >= EVENTS_CAP {
            self.events.pop_front();
        }
        self.events.push_back(Event {
            ts: std::time::SystemTime::now(),
            level: lvl,
            msg: msg.into(),
        });
    }

    pub fn reset(&mut self) {
        for row in self.dns.cells.iter_mut() {
            for cell in row.iter_mut() {
                cell.reset();
            }
        }
        self.state = LinkState::Up;
        self.state_since = Instant::now();
        self.recover_at = None;
        self.last_reminder = None;
        self.good_streak = 0;
        self.bad_streak = 0;
        self.hist.clear();
        self.cur_bucket_start = None;
        self.lat_hist = Ring::new(POOL_HIST_CAP);
        self.jit_hist = Ring::new(POOL_HIST_CAP);
        for p in self.primaries.iter_mut() {
            p.reset();
        }
        for e in self.extras.iter_mut() {
            e.reset();
        }
        self.best_uptime_secs = 0;
        self.worst_loss_burst = 0;
        self.peak_latency = 0.0;
        self.peak_jitter = 0.0;
        self.up_since = Some(Instant::now());
        self.sess_up_ms = 0;
        self.sess_degraded_ms = 0;
        self.sess_down_ms = 0;
        self.last_accrual = Some(Instant::now());
        self.bad_since = None;
        self.recoveries = 0;
        self.outages = 0;
        self.mttr_ms_total = 0;
        self.top_latency.clear();
        self.top_jitter.clear();
        self.log(Level::Info, "stats reset");
    }

    pub fn ingest_extra(&mut self, idx: usize, sample: crate::net::PingSample) {
        if idx >= self.extras.len() {
            return;
        }
        let label = self.extras[idx].label.clone();
        let e = &mut self.extras[idx];
        e.total += 1;
        e.ring.push(sample.rtt_ms);
        let log_msg = match sample.rtt_ms {
            Some(v) => {
                e.last = Some(v);
                e.consec_loss = 0;
                if e.state == LinkState::Down {
                    e.state = LinkState::Up;
                    Some((Level::Good, format!("[{}] up  {:.0} ms", label, v)))
                } else {
                    None
                }
            }
            None => {
                e.lost += 1;
                e.consec_loss += 1;
                if e.consec_loss >= 2 && e.state != LinkState::Down {
                    e.state = LinkState::Down;
                    Some((Level::Bad, format!("[{}] down", label)))
                } else if e.consec_loss == 1 {
                    Some((Level::Warn, format!("[{}] loss", label)))
                } else {
                    None
                }
            }
        };
        if let Some((lvl, msg)) = log_msg {
            self.log(lvl, msg);
        }
    }

    fn export_row(&self) -> ExportRow {
        let stat = self.pooled_ping_stat();
        ExportRow {
            epoch_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            elapsed_s: self.started.elapsed().as_secs(),
            state: match self.state {
                LinkState::Up => "up",
                LinkState::Degraded => "degraded",
                LinkState::Down => "down",
            },
            score: self.score() as u64,
            last_rtt_ms: self.last_value_view(),
            avg_rtt_ms: stat.avg().unwrap_or(0.0),
            min_rtt_ms: stat.min,
            max_rtt_ms: stat.max,
            total: self.pooled_total(),
            lost: self.pooled_lost(),
            loss_pct: self.loss_pct(),
            jitter_cur_ms: self.jitter_view(),
            last_dns_ms: self.system_resolver_worst(),
            avg_dns_ms: self.dns_avg(),
            cadence_ms: self.interval_ms.load(Ordering::Relaxed),
        }
    }

    pub fn export_tsv(&mut self) -> std::io::Result<String> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let dir = format!("{}/.ping_monitor/sessions", home);
        std::fs::create_dir_all(&dir)?;
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = format!("{}/{}.tsv", dir, epoch);
        let exists = std::path::Path::new(&path).exists();
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        if !exists {
            writeln!(f, "epoch_s\telapsed_s\tstate\tscore\tlast_rtt_ms\tavg_rtt_ms\tmin_rtt_ms\tmax_rtt_ms\ttotal\tlost\tloss_pct\tjitter_cur_ms\tlast_dns_ms\tavg_dns_ms\tcadence_ms")?;
        }
        let row = self.export_row();
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{:.1}\t{}\t{}\t{:.2}\t{}\t{}\t{:.1}\t{}",
            row.epoch_s,
            row.elapsed_s,
            row.state,
            row.score,
            row.last_rtt_ms
                .map(|v| format!("{:.1}", v))
                .unwrap_or_else(|| "-".into()),
            row.avg_rtt_ms,
            row.min_rtt_ms,
            row.max_rtt_ms,
            row.total,
            row.lost,
            row.loss_pct,
            row.jitter_cur_ms,
            row.last_dns_ms
                .map(|v| format!("{:.1}", v))
                .unwrap_or_else(|| "-".into()),
            row.avg_dns_ms,
            row.cadence_ms,
        )?;
        self.last_export = Some(path.clone());
        Ok(path)
    }

    pub fn ingest_ping(
        &mut self,
        idx: usize,
        sample: crate::net::PingSample,
    ) -> Option<SoundEvent> {
        if idx >= self.primaries.len() {
            return None;
        }

        let now = Instant::now();
        let need_new = match self.cur_bucket_start {
            None => true,
            Some(s) => now.duration_since(s).as_secs() >= HIST_BUCKET_SECS,
        };
        if need_new {
            if self.hist.len() == HIST_BUCKETS {
                self.hist.pop_front();
            }
            self.hist.push_back(HistBucket::default());
            self.cur_bucket_start = Some(now);
        }

        let rtt = sample.rtt_ms;
        let label = self.primaries[idx].label.clone();
        if let Some(b) = self.hist.back_mut() {
            b.push(rtt, &label);
        }

        let p = &mut self.primaries[idx];
        p.total += 1;
        p.lat_ring.push(rtt);
        p.loss_ring.push(if rtt.is_none() { 1.0 } else { 0.0 });

        let mut jit_val = 0.0;
        match rtt {
            Some(v) => {
                p.stat.add(v);
                if v > self.peak_latency {
                    self.peak_latency = v;
                    push_spike(&mut self.top_latency, v, 3);
                }
                let prev = p.last_value.replace(v);
                jit_val = match prev {
                    Some(x) => (v - x).abs(),
                    None => 0.0,
                };
                p.jitter_ring.push(jit_val);
                if jit_val > self.peak_jitter {
                    self.peak_jitter = jit_val;
                    push_spike(&mut self.top_jitter, jit_val, 3);
                }
                if p.cur_loss_burst > 0 {
                    if p.cur_loss_burst > self.worst_loss_burst {
                        self.worst_loss_burst = p.cur_loss_burst;
                    }
                    p.cur_loss_burst = 0;
                }
                p.consec_loss = 0;
            }
            None => {
                p.lost += 1;
                p.consec_loss += 1;
                p.cur_loss_burst += 1;
                p.jitter_ring.push(0.0);
            }
        }

        let (is_bad, is_down) = p.classify(&self.cfg);
        if is_bad {
            p.bad_streak += 1;
            p.good_streak = 0;
        } else {
            p.good_streak += 1;
            p.bad_streak = 0;
        }
        let prev_target_state = p.state;
        p.last_target_state = prev_target_state;
        let now = Instant::now();
        let (new_state, new_recover_at) = step_dwell(
            p.state,
            is_bad,
            is_down,
            p.bad_streak,
            p.good_streak,
            &self.cfg,
            now,
            p.recover_at,
        );
        if new_state != p.state {
            p.state = new_state;
            p.state_since = now;
        }
        p.recover_at = new_recover_at;
        if p.state == LinkState::Up {
            p.baseline.push(rtt, jit_val);
        }
        if prev_target_state != p.state {
            let msg = match p.state {
                LinkState::Up => format!("[{}] target recovered", label),
                LinkState::Degraded => format!("[{}] target degraded", label),
                LinkState::Down => format!("[{}] target unreachable", label),
            };
            self.log(Level::Warn, msg);
        }

        self.lat_hist.push(self.last_value_view());
        self.jit_hist.push(self.jitter_view());

        self.recompute_connection_state()
    }

    fn recompute_connection_state(&mut self) -> Option<SoundEvent> {
        self.accrue_state_time();

        let n = self.primaries.len();
        if n == 0 {
            return None;
        }
        let mut down = 0;
        let mut bad = 0;
        let mut up = 0;
        for p in &self.primaries {
            match p.state {
                LinkState::Down => {
                    down += 1;
                    bad += 1;
                }
                LinkState::Degraded => {
                    bad += 1;
                }
                LinkState::Up => {
                    up += 1;
                }
            }
        }
        let majority = (n as f64 / 2.0).ceil() as usize;
        let majority_bad = bad >= majority;
        let majority_down = down >= majority;

        if majority_down {
            self.bad_streak += 1;
            self.good_streak = 0;
        } else if majority_bad {
            self.bad_streak += 1;
            self.good_streak = 1;
        } else {
            self.good_streak += 1;
            self.bad_streak = 1;
        }

        let prev = self.state;
        let now = Instant::now();
        let mut sound: Option<SoundEvent> = None;

        let (new_state, new_recover_at) = step_dwell(
            self.state,
            majority_bad,
            majority_down,
            self.bad_streak,
            self.good_streak,
            &self.cfg,
            now,
            self.recover_at,
        );
        if new_state != self.state {
            self.state = new_state;
            self.state_since = now;
        }
        self.recover_at = new_recover_at;

        if prev != self.state {
            if prev == LinkState::Up {
                if let Some(s) = self.up_since {
                    let secs = s.elapsed().as_secs();
                    if secs > self.best_uptime_secs {
                        self.best_uptime_secs = secs;
                    }
                }
                self.up_since = None;
            }
            if self.state == LinkState::Up {
                self.up_since = Some(Instant::now());
                if let Some(s) = self.bad_since.take() {
                    self.mttr_ms_total += s.elapsed().as_millis() as u64;
                    self.recoveries += 1;
                }
            }
            if matches!(self.state, LinkState::Degraded | LinkState::Down)
                && self.bad_since.is_none()
            {
                self.bad_since = Some(now);
            }
            if self.state == LinkState::Down {
                self.outages += 1;
            }
            let total: u64 = self.primaries.iter().map(|p| p.total).sum();
            let lost: u64 = self.primaries.iter().map(|p| p.lost).sum();
            let loss_pct = if total == 0 {
                0.0
            } else {
                lost as f64 * 100.0 / total as f64
            };
            let up_n = up;
            let bad_n = bad;
            let down_n = down;
            let consensus = format!(
                "targets {}/{} up  loss {:.0}%  ({}/{}/{})",
                up_n,
                n,
                loss_pct,
                up_n,
                bad_n - down_n,
                down_n
            );
            match self.state {
                LinkState::Up => {
                    self.log(
                        Level::Good,
                        format!("connection recovered  {}  ♪ recover", consensus),
                    );
                    self.notify("connection recovered");
                    sound = Some(SoundEvent::Recover);
                }
                LinkState::Degraded => {
                    if prev == LinkState::Up {
                        self.log(
                            Level::Warn,
                            format!("connection degraded  {}  ♪ degraded", consensus),
                        );
                        self.notify("connection degraded");
                        sound = Some(SoundEvent::Loss);
                    } else {
                        self.log(Level::Warn, format!("still bad → degraded  {}", consensus));
                    }
                }
                LinkState::Down => {
                    self.log(
                        Level::Bad,
                        format!("connection DOWN  {}  ♪ down", consensus),
                    );
                    self.notify("connection DOWN");
                    sound = Some(SoundEvent::Down);
                }
            }
            self.last_reminder = Some(Instant::now());
            let _ = self.export_tsv();
        }

        let ms = match self.state {
            LinkState::Up => 1_000,
            LinkState::Degraded | LinkState::Down => 500,
        };
        self.interval_ms.store(ms, Ordering::Relaxed);
        sound
    }

    pub fn score(&self) -> f32 {
        if self.state == LinkState::Down {
            return 0.0;
        }
        let mut s: f32 = 100.0;
        let lat = self.last_value_view().unwrap_or(0.0) as f32;
        let loss = self.pooled_loss_pct() as f32;
        let dns = self.system_resolver_worst().unwrap_or(0.0) as f32;
        let jit = self.jitter_view() as f32;
        let lat_w = self.lat_warn_ms() as f32;
        let jit_w = self.jit_warn_ms() as f32;
        let dns_w = self.dns_warn_ms() as f32;

        if lat > lat_w {
            s -= ((lat - lat_w) / 10.0).min(30.0);
        }
        if jit > jit_w {
            s -= ((jit - jit_w) / 5.0).min(20.0);
        }
        s -= loss.min(40.0);
        if dns > dns_w {
            s -= ((dns - dns_w) / 10.0).min(10.0);
        }
        if self.state == LinkState::Degraded {
            s = s.min(55.0);
        }
        s.clamp(0.0, 100.0)
    }

    pub fn tick_reminder(&mut self) -> Option<(SoundEvent, Level, &'static str)> {
        match self.state {
            LinkState::Down => {
                let due = match self.last_reminder {
                    None => true,
                    Some(t) => t.elapsed() >= self.cfg.reminder_interval,
                };
                if due {
                    self.last_reminder = Some(Instant::now());
                    return Some((SoundEvent::Down, Level::Bad, "still down"));
                }
            }
            LinkState::Degraded => {
                let due = match self.last_reminder {
                    None => true,
                    Some(t) => t.elapsed() >= self.cfg.reminder_interval,
                };
                if due {
                    self.last_reminder = Some(Instant::now());
                    return Some((SoundEvent::Shimmer, Level::Warn, "still degraded"));
                }
            }
            LinkState::Up => {}
        }
        None
    }

    pub fn loss_pct(&self) -> f64 {
        self.pooled_loss_pct()
    }

    pub fn cur_uptime_secs(&self) -> u64 {
        self.up_since.map(|s| s.elapsed().as_secs()).unwrap_or(0)
    }

    pub fn accrue_state_time(&mut self) {
        let now = Instant::now();
        let ms = match self.last_accrual {
            Some(t) => now.duration_since(t).as_millis() as u64,
            None => 0,
        };
        match self.state {
            LinkState::Up => self.sess_up_ms += ms,
            LinkState::Degraded => self.sess_degraded_ms += ms,
            LinkState::Down => self.sess_down_ms += ms,
        }
        self.last_accrual = Some(now);
    }

    pub fn mttr_ms(&self) -> u64 {
        if self.recoveries == 0 {
            0
        } else {
            self.mttr_ms_total / self.recoveries as u64
        }
    }

    pub fn uptime_pct(&self) -> f64 {
        let total = self.sess_up_ms + self.sess_degraded_ms + self.sess_down_ms;
        if total == 0 {
            100.0
        } else {
            self.sess_up_ms as f64 * 100.0 / total as f64
        }
    }
    pub fn degraded_pct(&self) -> f64 {
        let total = self.sess_up_ms + self.sess_degraded_ms + self.sess_down_ms;
        if total == 0 {
            0.0
        } else {
            self.sess_degraded_ms as f64 * 100.0 / total as f64
        }
    }
    pub fn down_pct(&self) -> f64 {
        let total = self.sess_up_ms + self.sess_degraded_ms + self.sess_down_ms;
        if total == 0 {
            0.0
        } else {
            self.sess_down_ms as f64 * 100.0 / total as f64
        }
    }

    pub fn ingest_dns(
        &mut self,
        r_idx: usize,
        d_idx: usize,
        ms: Option<f64>,
    ) -> Option<SoundEvent> {
        if r_idx >= self.dns.cells.len() || d_idx >= self.dns.cells[r_idx].len() {
            return None;
        }
        let r_label = self.dns.resolvers[r_idx].0.clone();
        let d_name = self.dns.names[d_idx].clone();
        let dns_warn = self.dns_warn_ms();
        let dns_bad = self.dns_bad_ms();
        let cell = &mut self.dns.cells[r_idx][d_idx];
        cell.ring.push(ms);
        cell.last = ms;
        match ms {
            Some(v) => {
                cell.stat.add(v);
                if v > dns_bad {
                    cell.state = LinkState::Down;
                    self.log(
                        Level::Bad,
                        format!("[DNS {}→{}] slow: {:.0} ms", r_label, d_name, v),
                    );
                } else if v > dns_warn {
                    cell.state = LinkState::Degraded;
                    self.log(
                        Level::Warn,
                        format!("[DNS {}→{}] high: {:.0} ms", r_label, d_name, v),
                    );
                } else {
                    cell.state = LinkState::Up;
                }
            }
            None => {
                cell.state = LinkState::Down;
                self.log(Level::Bad, format!("[DNS {}→{}] failed", r_label, d_name));
            }
        }
        None
    }

    pub fn system_resolver_worst(&self) -> Option<f64> {
        let row = self.dns.cells.first()?;
        row.iter()
            .filter_map(|c| c.last)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |m| m.max(v)))
            })
    }

    pub fn dns_avg(&self) -> f64 {
        let mut sum = 0.0;
        let mut n = 0;
        for row in &self.dns.cells {
            for c in row {
                if let Some(avg) = c.stat.avg() {
                    sum += avg;
                    n += 1;
                }
            }
        }
        if n == 0 {
            0.0
        } else {
            sum / n as f64
        }
    }

    fn notify(&self, msg: &str) {
        if let Some(ref f) = self.notify_fn {
            f(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::PingSample;

    fn app_with_n_probes(n: usize) -> App {
        let mut a = App::new(Config::default());
        for i in 0..n {
            a.primaries
                .push(PrimaryProbe::new(&format!("t{}", i), "127.0.0.1", 80));
        }
        a
    }

    #[test]
    fn connection_up_with_healthy_majority() {
        let mut a = app_with_n_probes(3);
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        assert_eq!(a.state, LinkState::Up);
        for _ in 0..15 {
            a.ingest_ping(0, PingSample { rtt_ms: None });
            a.ingest_ping(1, PingSample { rtt_ms: Some(20.0) });
            a.ingest_ping(2, PingSample { rtt_ms: Some(20.0) });
        }
        assert_eq!(a.state, LinkState::Up);
    }

    #[test]
    fn connection_down_when_majority_fails() {
        let mut a = app_with_n_probes(3);
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        assert_eq!(a.state, LinkState::Up);
        for _ in 0..25 {
            a.ingest_ping(0, PingSample { rtt_ms: None });
            a.ingest_ping(1, PingSample { rtt_ms: None });
            a.ingest_ping(2, PingSample { rtt_ms: Some(20.0) });
        }
        assert_eq!(a.state, LinkState::Down);
    }

    #[test]
    fn loss_pct_pooled_across_primaries() {
        let mut a = app_with_n_probes(2);
        a.ingest_ping(0, PingSample { rtt_ms: Some(10.0) });
        a.ingest_ping(0, PingSample { rtt_ms: None });
        a.ingest_ping(1, PingSample { rtt_ms: Some(10.0) });
        a.ingest_ping(1, PingSample { rtt_ms: Some(10.0) });
        assert_eq!(a.loss_pct(), 25.0);
    }

    #[test]
    fn anti_flapping_holds_recovery_for_dwell() {
        let mut a = app_with_n_probes(3);
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        assert_eq!(a.state, LinkState::Up);
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: None });
            }
        }
        assert_ne!(a.state, LinkState::Up);
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        let old = std::time::Instant::now() - std::time::Duration::from_secs(20);
        for p in a.primaries.iter_mut() {
            p.recover_at = Some(old);
        }
        for _ in 0..3 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        assert!(
            a.recover_at.is_some(),
            "recover_at should be set once per-target dwell clears"
        );
        assert_ne!(
            a.state,
            LinkState::Up,
            "recovery should still be held by connection dwell"
        );
        let old = std::time::Instant::now() - std::time::Duration::from_secs(20);
        a.recover_at = Some(old);
        for i in 0..3 {
            a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
        }
        assert_eq!(
            a.state,
            LinkState::Up,
            "recovery should fire after dwell elapses"
        );
    }

    #[test]
    fn jitter_view_is_median_across_primaries() {
        let mut a = app_with_n_probes(3);
        for _ in 0..30 {
            a.primaries[0].jitter_ring.push(5.0);
            a.primaries[1].jitter_ring.push(5.0);
            a.primaries[2].jitter_ring.push(80.0);
        }
        assert!(
            (a.jitter_view() - 5.0).abs() < 0.001,
            "jitter_view should be median (5.0), got {}",
            a.jitter_view()
        );
    }

    #[test]
    fn baseline_skipped_when_target_not_up() {
        let mut a = app_with_n_probes(1);
        for _ in 0..30 {
            a.ingest_ping(0, PingSample { rtt_ms: None });
        }
        assert_ne!(
            a.primaries[0].state,
            LinkState::Up,
            "target should be in a bad state after sustained loss"
        );
        assert!(
            a.primaries[0].baseline.len() == 0,
            "baseline should not have grown while target was down, got len {}",
            a.primaries[0].baseline.len()
        );
    }

    #[test]
    fn peak_jitter_tracks_maximum() {
        let mut a = app_with_n_probes(1);
        a.ingest_ping(0, PingSample { rtt_ms: Some(10.0) });
        a.ingest_ping(0, PingSample { rtt_ms: Some(50.0) });
        a.ingest_ping(0, PingSample { rtt_ms: Some(20.0) });
        assert!(
            (a.peak_jitter - 40.0).abs() < 0.001,
            "peak_jitter should be 40.0 (the max), got {}",
            a.peak_jitter
        );
    }

    #[test]
    fn pooled_stat_last_is_none() {
        let mut a = app_with_n_probes(3);
        a.ingest_ping(0, PingSample { rtt_ms: Some(10.0) });
        a.ingest_ping(1, PingSample { rtt_ms: Some(20.0) });
        a.ingest_ping(2, PingSample { rtt_ms: Some(30.0) });
        let pooled = a.pooled_ping_stat();
        assert!(
            pooled.last.is_none(),
            "pooled_ping_stat.last must be None, got {:?}",
            pooled.last
        );
        assert_eq!(pooled.count, 3);
    }

    #[test]
    fn config_clamps_invalid_values() {
        let mut cfg = Config {
            timeout_ms: 0,
            ping_interval_ms: 10,
            dns_interval_ms: 50,
            recover_dwell: Duration::from_secs(0),
            reminder_interval: Duration::from_secs(1),
            state_window: 1,
            hysteresis_good: 0,
            hysteresis_bad: 0,
            ..Default::default()
        };
        let warns = cfg.validate();
        assert!(
            !warns.is_empty(),
            "validate should warn on out-of-range values"
        );
        assert!(
            cfg.timeout_ms >= 50,
            "timeout clamped to >=50, got {}",
            cfg.timeout_ms
        );
        assert!(
            cfg.ping_interval_ms >= 200,
            "ping_interval clamped to >=200, got {}",
            cfg.ping_interval_ms
        );
        assert!(
            cfg.dns_interval_ms >= 1000,
            "dns_interval clamped to >=1000, got {}",
            cfg.dns_interval_ms
        );
        assert!(
            cfg.recover_dwell.as_secs() >= 1,
            "recover_dwell clamped to >=1s"
        );
        assert!(
            cfg.reminder_interval.as_secs() >= 5,
            "reminder_interval clamped to >=5s"
        );
        assert!(cfg.state_window >= 5, "state_window clamped to >=5");
        assert!(cfg.hysteresis_good >= 1, "hysteresis_good clamped to >=1");
        assert!(cfg.hysteresis_bad >= 1, "hysteresis_bad clamped to >=1");
    }

    #[test]
    fn step_dwell_up_to_degraded_on_bad_streak() {
        let cfg = Config {
            hysteresis_bad: 3,
            ..Default::default()
        };
        let now = Instant::now();
        let (new, recover) = step_dwell(LinkState::Up, true, false, 3, 0, &cfg, now, None);
        assert_eq!(new, LinkState::Degraded);
        assert!(recover.is_none(), "recover_at must reset on transition");
    }

    #[test]
    fn step_dwell_recovery_held_by_dwell() {
        let cfg = Config {
            hysteresis_good: 2,
            recover_dwell: Duration::from_secs(10),
            ..Default::default()
        };
        let now = Instant::now();
        let started = now - Duration::from_secs(2);
        let (new, recover) = step_dwell(
            LinkState::Down,
            false,
            false,
            0,
            2,
            &cfg,
            now,
            Some(started),
        );
        assert_eq!(
            new,
            LinkState::Down,
            "must stay Down while dwell not elapsed"
        );
        assert_eq!(
            recover,
            Some(started),
            "recover_at must persist while waiting"
        );
        let started_old = now - Duration::from_secs(20);
        let (new2, recover2) = step_dwell(
            LinkState::Down,
            false,
            false,
            0,
            2,
            &cfg,
            now,
            Some(started_old),
        );
        assert_eq!(new2, LinkState::Up, "must recover after dwell elapsed");
        assert!(recover2.is_none(), "recover_at must clear on transition");
    }

    #[test]
    fn tick_reminder_returns_factual_message() {
        let mut a = app_with_n_probes(3);
        for _ in 0..30 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: None });
            }
        }
        assert_ne!(a.state, LinkState::Up);
        a.last_reminder = Some(Instant::now() - Duration::from_secs(999));
        let result = a.tick_reminder();
        assert!(result.is_some(), "reminder should be due");
        let (_, _, msg) = result.unwrap();
        assert!(!msg.contains("♪"), "message must be factual, got '{}'", msg);
        assert!(
            msg.contains("degraded") || msg.contains("down"),
            "factual state, got '{}'",
            msg
        );
    }

    #[test]
    fn peak_jitter_resets_on_reset() {
        let mut a = app_with_n_probes(1);
        a.ingest_ping(0, PingSample { rtt_ms: Some(10.0) });
        a.ingest_ping(0, PingSample { rtt_ms: Some(80.0) });
        assert!(a.peak_jitter > 0.0);
        a.reset();
        assert_eq!(a.peak_jitter, 0.0, "peak_jitter must reset to 0");
        assert_eq!(a.peak_latency, 0.0, "peak_latency must reset to 0");
        assert_eq!(a.worst_loss_burst, 0, "worst_loss_burst must reset to 0");
    }

    #[test]
    fn push_spike_keeps_top_n_sorted_desc() {
        let mut list: Vec<(f64, u64)> = Vec::new();
        for v in [10.0, 5.0, 30.0, 20.0, 25.0, 15.0] {
            push_spike(&mut list, v, 3);
        }
        assert_eq!(list.len(), 3, "must keep top-3 only");
        assert_eq!(list[0].0, 30.0, "top must be largest seen (30.0)");
        assert_eq!(list[1].0, 25.0, "second must be 25.0");
        assert_eq!(list[2].0, 20.0, "third must be 20.0 (15/10/5 truncated)");
    }

    #[test]
    fn accrue_state_time_grows_up_bucket_when_up() {
        let mut a = app_with_n_probes(1);
        a.ingest_ping(0, PingSample { rtt_ms: Some(10.0) });
        assert_eq!(a.state, LinkState::Up, "precondition: connection Up");
        std::thread::sleep(std::time::Duration::from_millis(2));
        a.accrue_state_time();
        assert!(
            a.sess_up_ms > 0,
            "sess_up_ms must grow after sleep+accrue, got {}",
            a.sess_up_ms
        );
        assert_eq!(
            a.sess_degraded_ms, 0,
            "degraded bucket must stay 0 while Up"
        );
        assert_eq!(a.sess_down_ms, 0, "down bucket must stay 0 while Up");
        assert!(
            (a.uptime_pct() - 100.0).abs() < 0.01,
            "uptime_pct must be 100 while all Up, got {}",
            a.uptime_pct()
        );
    }

    #[test]
    fn bad_since_set_on_first_bad_state_and_cleared_on_up_transition() {
        let mut a = app_with_n_probes(3);
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        assert!(a.bad_since.is_none(), "bad_since must be None while Up");
        for _ in 0..25 {
            a.ingest_ping(0, PingSample { rtt_ms: None });
            a.ingest_ping(1, PingSample { rtt_ms: None });
            a.ingest_ping(2, PingSample { rtt_ms: Some(20.0) });
        }
        assert_eq!(a.state, LinkState::Down);
        assert!(
            a.bad_since.is_some(),
            "bad_since must be captured on entry into bad state"
        );
        assert!(
            a.outages >= 1,
            "outages must increment on entry to Down, got {}",
            a.outages
        );
    }

    #[test]
    fn reset_clears_session_resumes() {
        let mut a = app_with_n_probes(1);
        a.ingest_ping(0, PingSample { rtt_ms: Some(10.0) });
        a.ingest_ping(0, PingSample { rtt_ms: Some(80.0) });
        a.accrue_state_time();
        a.outages = 5;
        a.recoveries = 3;
        a.mttr_ms_total = 12_000;
        a.sess_up_ms = 1_500;
        a.bad_since = Some(Instant::now());
        a.reset();
        assert!(a.top_latency.is_empty(), "top_latency must clear on reset");
        assert!(a.top_jitter.is_empty(), "top_jitter must clear on reset");
        assert_eq!(a.outages, 0, "outages must reset to 0");
        assert_eq!(a.recoveries, 0, "recoveries must reset to 0");
        assert_eq!(a.mttr_ms_total, 0, "mttr_ms_total must reset to 0");
        assert_eq!(a.sess_up_ms, 0, "sess_up_ms must reset to 0");
        assert!(a.bad_since.is_none(), "bad_since must clear on reset");
        assert_eq!(a.mttr_ms(), 0, "mttr_ms() must be 0 with no recoveries");
    }

    #[test]
    fn top_latency_and_jitter_track_session_extremes() {
        let mut a = app_with_n_probes(1);
        for v in [10.0, 20.0, 30.0, 50.0, 40.0, 60.0] {
            a.ingest_ping(0, PingSample { rtt_ms: Some(v) });
        }
        assert_eq!(a.peak_latency, 60.0, "peak must match global max");
        assert_eq!(a.top_latency.len(), 3, "must have top-3 latency spikes");
        assert_eq!(
            a.top_latency[0].0, 60.0,
            "top spike must be 60 (the global peak)"
        );
        assert!(
            (a.peak_jitter - 20.0).abs() < 0.01,
            "peak_jitter must be 20.0, got {}",
            a.peak_jitter
        );
        assert!(!a.top_jitter.is_empty(), "top_jitter must be populated");
    }

    #[test]
    fn adaptive_thresholds_track_baseline_p90() {
        let mut a = app_with_n_probes(2);
        let floor_warn = a.cfg.latency_warn_ms;
        let floor_bad = a.cfg.latency_bad_ms;
        assert_eq!(a.lat_warn_ms(), floor_warn);
        assert_eq!(a.lat_bad_ms(), floor_bad);
        for _ in 0..60 {
            for i in 0..2 {
                a.ingest_ping(
                    i,
                    PingSample {
                        rtt_ms: Some(150.0),
                    },
                );
            }
        }
        assert!(
            a.lat_warn_ms() > floor_warn,
            "warn should rise above floor after baseline warmup, got {}",
            a.lat_warn_ms()
        );
        assert!(
            a.lat_bad_ms() > floor_bad,
            "bad should rise above floor after baseline warmup, got {}",
            a.lat_bad_ms()
        );
        assert!(
            a.lat_warn_ms() <= 350.0,
            "warn should be ~p90*2 (300), got {}",
            a.lat_warn_ms()
        );
        assert!(
            a.lat_bad_ms() <= 650.0,
            "bad should be ~p90*4 (600), got {}",
            a.lat_bad_ms()
        );
    }

    #[test]
    fn wifi_rssi_logs_only_on_grade_change() {
        let mut a = app_with_n_probes(1);
        assert_eq!(a.wifi_grade, None);
        a.set_wifi_rssi(Some(-50));
        assert_eq!(a.wifi_grade, Some(4));
        let events_before = a.events.len();
        a.set_wifi_rssi(Some(-52));
        assert_eq!(a.wifi_grade, Some(4), "same grade, no transition");
        assert_eq!(a.events.len(), events_before, "no log when grade unchanged");
        a.set_wifi_rssi(Some(-80));
        assert_eq!(a.wifi_grade, Some(1));
        assert_eq!(a.events.len(), events_before + 1, "logged on downgrade");
        a.set_wifi_rssi(None);
        assert_eq!(a.wifi_grade, None);
        assert!(a.events.len() >= events_before + 2, "logged wifi down");
    }
}
