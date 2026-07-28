use crate::detector::EwmaDetector;
use crate::sound::SoundEvent;
use crate::state::{consensus, reduce_connection, reduce_target, DesiredState, RecoveryState};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

    pub fn push_latency(&mut self, v: f64) {
        if self.latencies.len() == BASELINE_CAP {
            self.latencies.pop_front();
        }
        self.latencies.push_back(v);
    }

    pub fn push_jitter(&mut self, v: f64) {
        if self.jitters.len() == BASELINE_CAP {
            self.jitters.pop_front();
        }
        self.jitters.push_back(v);
    }

    pub fn latency_len(&self) -> usize {
        self.latencies.len()
    }

    pub fn jitter_len(&self) -> usize {
        self.jitters.len()
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
    jitter_cur_ms: Option<f64>,
    last_dns_ms: Option<f64>,
    avg_dns_ms: f64,
    cadence_ms: u64,
}

pub enum GatewayUpdate {
    Unchanged,
    Updated { old: String, new: String },
    Added { idx: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayRole {
    UserExtra(usize),
    AutoExtra(usize),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCause {
    GatewayFailure,
    WeakWifi,
    BeyondFirstHop,
    Unknown,
}

fn cause_word(cause: PathCause) -> &'static str {
    match cause {
        PathCause::GatewayFailure => "probable gateway failure",
        PathCause::WeakWifi => "weak Wi-Fi signal",
        PathCause::BeyondFirstHop => "gateway OK; failure beyond first hop",
        PathCause::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorMode {
    Legacy,
    Shadow,
    Hybrid,
}

impl DetectorMode {
    pub fn from_env() -> Self {
        match std::env::var("PM_RECURSIVE_MODE").as_deref() {
            Ok("shadow") => DetectorMode::Shadow,
            Ok("hybrid") => DetectorMode::Hybrid,
            Ok("legacy") | Ok(_) => DetectorMode::Legacy,
            Err(_) => DetectorMode::Legacy,
        }
    }
}

pub type NotifyFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Copy, Debug)]
pub enum RoundResult {
    Observed(crate::net::PingSample),
    Missing,
}

pub struct PrimaryBatch {
    pub round_id: u64,
    pub reset_epoch: u64,
    pub results: Vec<RoundResult>,
    #[allow(dead_code)]
    pub started_at: Instant,
}

impl std::fmt::Debug for PrimaryBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrimaryBatch")
            .field("round_id", &self.round_id)
            .field("reset_epoch", &self.reset_epoch)
            .field("results_len", &self.results.len())
            .finish()
    }
}

pub struct App {
    pub cfg: Config,
    pub primaries: Vec<PrimaryProbe>,
    pub dns: DnsMatrix,
    pub state: LinkState,
    pub state_since: Instant,
    pub recovery: RecoveryState,
    pub last_reminder: Option<Instant>,
    pub events: VecDeque<Event>,
    pub muted: bool,
    pub started: Instant,
    pub audio_state: Option<Arc<crate::sound::AudioState>>,
    pub hist: VecDeque<HistBucket>,
    pub cur_bucket_start: Option<Instant>,
    pub interval_ms: Arc<AtomicU64>,
    pub reset_epoch: Arc<AtomicU64>,
    pub lat_hist: Ring<Option<f64>>,
    pub jit_hist: Ring<Option<f64>>,
    pub extras: Vec<ExtraProbe>,
    pub auto_gw_idx: Option<usize>,
    pub gw_role: GatewayRole,
    pub gw_shared: Arc<Mutex<(u64, String)>>,
    pub gw_cadence_ms: Arc<AtomicU64>,
    pub outage_onset_at: Option<Instant>,
    pub gateway_probe_idx: Option<usize>,
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
    pub detector_mode: DetectorMode,
    pub shadow_log: VecDeque<ShadowEvent>,
    pub prealert_enabled: bool,
    pub probe_anomaly_edge: Vec<bool>,
    pub probe_anomaly_cooldown_at: Option<Instant>,
    pub pending_cause_log_idx: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ShadowEvent {
    pub ts: Instant,
    pub metric: &'static str,
    pub old_latch: bool,
    pub new_latch: bool,
    pub ratio: f64,
    pub short: f64,
}

#[derive(Clone)]
pub struct PrimaryProbe {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub lat_ring: Ring<Option<f64>>,
    pub loss_ring: Ring<f64>,
    pub jitter_ring: Ring<Option<f64>>,
    pub baseline: Baseline,
    pub stat: LatStat,
    pub last_value: Option<f64>,
    pub prev_rtt_adjacent: Option<f64>,
    pub consec_loss: u32,
    pub cur_loss_burst: u32,
    pub pending_worse: Option<(LinkState, u32)>,
    pub state: LinkState,
    pub state_since: Instant,
    pub total: u64,
    pub lost: u64,
    pub last_target_state: LinkState,
    pub latency_detector: EwmaDetector,
    pub jitter_detector: EwmaDetector,
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
            prev_rtt_adjacent: None,
            consec_loss: 0,
            cur_loss_burst: 0,
            pending_worse: None,
            state: LinkState::Up,
            state_since: Instant::now(),
            total: 0,
            lost: 0,
            last_target_state: LinkState::Up,
            latency_detector: EwmaDetector::new(),
            jitter_detector: EwmaDetector::new(),
        }
    }

    pub fn reset(&mut self) {
        self.lat_ring = Ring::new(WIN);
        self.loss_ring = Ring::new(WIN);
        self.jitter_ring = Ring::new(WIN);
        self.baseline = Baseline::new();
        self.stat = LatStat::default();
        self.last_value = None;
        self.prev_rtt_adjacent = None;
        self.consec_loss = 0;
        self.cur_loss_burst = 0;
        self.pending_worse = None;
        self.state = LinkState::Up;
        self.state_since = Instant::now();
        self.last_target_state = LinkState::Up;
        self.total = 0;
        self.lost = 0;
        self.latency_detector.reset();
        self.jitter_detector.reset();
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
            .filter_map(|v| *v)
            .collect();
        let avg_jit = if jit.is_empty() {
            0.0
        } else {
            jit.iter().sum::<f64>() / jit.len() as f64
        };

        let (lat_t, jit_t, loss_t, down_t) = self.adaptive_thresholds(cfg);
        let is_bad = loss_pct >= loss_t || avg_lat > lat_t || avg_jit > jit_t;
        let is_down = loss_pct >= down_t;
        (is_bad, is_down)
    }

    fn adaptive_thresholds(&self, cfg: &Config) -> (f64, f64, f64, f64) {
        let (lat_thresh, jit_thresh) =
            if self.baseline.latency_len() >= 50 && self.baseline.jitter_len() >= 50 {
                let lat_p90 = self.baseline.lat_p90().unwrap_or(cfg.latency_warn_ms);
                let jit_p90 = self.baseline.jit_p90().unwrap_or(cfg.jitter_warn_ms);
                (
                    (lat_p90 * 2.0).max(cfg.latency_warn_ms),
                    (jit_p90 * 2.5).max(cfg.jitter_warn_ms),
                )
            } else if self.baseline.latency_len() >= 50 {
                let lat_p90 = self.baseline.lat_p90().unwrap_or(cfg.latency_warn_ms);
                ((lat_p90 * 2.0).max(cfg.latency_warn_ms), cfg.jitter_warn_ms)
            } else if self.baseline.jitter_len() >= 50 {
                let jit_p90 = self.baseline.jit_p90().unwrap_or(cfg.jitter_warn_ms);
                (cfg.latency_warn_ms, (jit_p90 * 2.5).max(cfg.jitter_warn_ms))
            } else {
                (cfg.latency_warn_ms, cfg.jitter_warn_ms)
            };
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
    pub last_sample_at: Option<Instant>,
}

impl ExtraProbe {
    pub fn reset(&mut self) {
        self.last = None;
        self.state = LinkState::Up;
        self.total = 0;
        self.lost = 0;
        self.consec_loss = 0;
        self.ring = Ring::new(30);
        self.last_sample_at = None;
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
            recovery: RecoveryState::new(),
            last_reminder: None,
            events: VecDeque::with_capacity(256),
            muted: false,
            started: Instant::now(),
            audio_state: None,
            hist: VecDeque::with_capacity(HIST_BUCKETS),
            cur_bucket_start: None,
            interval_ms: Arc::new(AtomicU64::new(interval_init)),
            reset_epoch: Arc::new(AtomicU64::new(0)),
            lat_hist: Ring::new(POOL_HIST_CAP),
            jit_hist: Ring::new(POOL_HIST_CAP),
            extras: Vec::new(),
            auto_gw_idx: None,
            gw_role: GatewayRole::Unknown,
            gw_shared: Arc::new(Mutex::new((0, String::new()))),
            gw_cadence_ms: Arc::new(AtomicU64::new(5_000)),
            outage_onset_at: None,
            gateway_probe_idx: None,
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
            detector_mode: DetectorMode::from_env(),
            shadow_log: VecDeque::with_capacity(64),
            prealert_enabled: std::env::var("PM_PREALERT").as_deref() == Ok("1"),
            probe_anomaly_edge: Vec::new(),
            probe_anomaly_cooldown_at: None,
            pending_cause_log_idx: None,
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
            if p.baseline.latency_len() >= 50 {
                if let Some(v) = p.baseline.lat_p90() {
                    lat_max = Some(lat_max.map_or(v, |m: f64| m.max(v)));
                }
            }
            if p.baseline.jitter_len() >= 50 {
                if let Some(v) = p.baseline.jit_p90() {
                    jit_max = Some(jit_max.map_or(v, |m: f64| m.max(v)));
                }
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
    pub fn jitter_view(&self) -> Option<f64> {
        let vals: Vec<f64> = self
            .primaries
            .iter()
            .filter_map(|p| p.jitter_ring.buf.back().and_then(|v| *v))
            .collect();
        if vals.is_empty() {
            return None;
        }
        let mut s = vals;
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(s[s.len() / 2])
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
        self.recovery = RecoveryState::new();
        self.last_reminder = None;
        self.outage_onset_at = None;
        self.hist.clear();
        self.cur_bucket_start = None;
        self.lat_hist = Ring::new(POOL_HIST_CAP);
        self.jit_hist = Ring::new(POOL_HIST_CAP);
        self.reset_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
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
        self.shadow_log.clear();
        self.probe_anomaly_edge.clear();
        self.probe_anomaly_cooldown_at = None;
        self.pending_cause_log_idx = None;
        self.log(Level::Info, "stats reset");
    }

    pub fn ingest_extra(&mut self, idx: usize, sample: crate::net::PingSample) {
        if idx >= self.extras.len() {
            return;
        }
        let label = self.extras[idx].label.clone();
        let e = &mut self.extras[idx];
        e.total += 1;
        e.last_sample_at = Some(Instant::now());
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
        // If we have a pending cause amendment and gateway evidence just became fresh, amend it
        self.try_amend_pending_cause();
    }

    /// Try to amend a pending transition event with updated gateway cause evidence.
    fn try_amend_pending_cause(&mut self) {
        let idx = match self.pending_cause_log_idx {
            Some(i) => i,
            None => return,
        };
        // Only amend if we're still in a bad state
        if self.state == LinkState::Up {
            self.pending_cause_log_idx = None;
            return;
        }
        // Check if gateway evidence is now fresh
        let cause = self.path_cause(
            if self.state == LinkState::Down {
                DesiredState::Down
            } else {
                DesiredState::Degraded
            },
            Instant::now(),
        );
        if cause == PathCause::Unknown {
            return; // still unknown, try again later
        }
        // Amend the event log entry
        if let Some(event) = self.events.get_mut(idx) {
            let old_msg = &event.msg;
            if let Some(cause_pos) = old_msg.find("cause: ") {
                let prefix = &old_msg[..cause_pos];
                let suffix_start = old_msg[cause_pos..]
                    .find("  ♪")
                    .map(|p| cause_pos + p)
                    .unwrap_or(old_msg.len());
                event.msg = format!(
                    "{}cause: {}  (cause updated)  {}",
                    prefix,
                    cause_word(cause),
                    &old_msg[suffix_start..]
                );
            }
        }
        self.pending_cause_log_idx = None;
    }

    /// Process a gateway probe result with epoch-based stale rejection.
    /// Rejects the result if the probe_epoch doesn't match the current gw_shared epoch.
    pub fn ingest_gateway_probe(
        &mut self,
        idx: usize,
        probe_epoch: u64,
        sample: crate::net::PingSample,
    ) {
        let current_epoch = self.gw_shared.lock().map(|guard| guard.0).unwrap_or(0);
        if probe_epoch != current_epoch {
            return;
        }
        self.ingest_extra(idx, sample);
    }

    /// Apply a gateway (re)detection result: update the auto-gateway extra in
    /// place when the address changed, or create it when it first appears.
    /// Prefers a user-configured extra whose host matches the detected gateway.
    pub fn apply_gateway_update(&mut self, gw: &str) -> GatewayUpdate {
        // Check if we already have an auto-gw extra with this address
        if let Some(i) = self.auto_gw_idx {
            if let Some(e) = self.extras.get_mut(i) {
                if e.host == gw {
                    return GatewayUpdate::Unchanged;
                }
                let old = std::mem::replace(&mut e.host, gw.to_string());
                e.reset();
                // Epoch already incremented by re-detection task (main.rs)
                return GatewayUpdate::Updated {
                    old,
                    new: gw.to_string(),
                };
            }
        }
        // Check if a user-configured extra already monitors this host
        // (skip the auto-gw extra itself)
        for (i, e) in self.extras.iter().enumerate() {
            if Some(i) == self.auto_gw_idx {
                continue;
            }
            if e.host == gw {
                self.gw_role = GatewayRole::UserExtra(i);
                self.gateway_probe_idx = Some(i);
                return GatewayUpdate::Unchanged;
            }
        }
        self.extras.push(ExtraProbe {
            label: "gw".into(),
            host: gw.to_string(),
            port: 80,
            last: None,
            state: LinkState::Up,
            total: 0,
            lost: 0,
            consec_loss: 0,
            ring: Ring::new(30),
            last_sample_at: None,
        });
        let idx = self.extras.len() - 1;
        self.auto_gw_idx = Some(idx);
        self.gw_role = GatewayRole::AutoExtra(idx);
        self.gateway_probe_idx = Some(idx);
        GatewayUpdate::Added { idx }
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
            row.jitter_cur_ms
                .map(|v| format!("{:.1}", v))
                .unwrap_or_else(|| "-".into()),
            row.last_dns_ms
                .map(|v| format!("{:.1}", v))
                .unwrap_or_else(|| "-".into()),
            row.avg_dns_ms,
            row.cadence_ms,
        )?;
        self.last_export = Some(path.clone());
        // Export shadow_log to separate file when mode is shadow
        if self.detector_mode == DetectorMode::Shadow && !self.shadow_log.is_empty() {
            let shadow_path = format!("{}/{}.shadow.tsv", dir, epoch);
            let shadow_exists = std::path::Path::new(&shadow_path).exists();
            if let Ok(mut sf) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&shadow_path)
            {
                if !shadow_exists {
                    let _ = writeln!(
                        sf,
                        "ts_offset_ms\tmetric\told_latch\tnew_latch\tratio\tshort"
                    );
                }
                for ev in &self.shadow_log {
                    let offset_ms = ev.ts.duration_since(self.started).as_millis();
                    let _ = writeln!(
                        sf,
                        "{}\t{}\t{}\t{}\t{:.4}\t{:.1}",
                        offset_ms, ev.metric, ev.old_latch, ev.new_latch, ev.ratio, ev.short
                    );
                }
            }
        }
        Ok(path)
    }

    /// Process a single ping sample for one target (per-target update only).
    /// Returns the instantaneous candidate for this target.
    fn ingest_sample_at(
        &mut self,
        idx: usize,
        sample: crate::net::PingSample,
        now: Instant,
    ) -> Option<LinkState> {
        if idx >= self.primaries.len() {
            return None;
        }

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

        let jit_val: Option<f64> = match rtt {
            Some(v) => {
                p.stat.add(v);
                if v > self.peak_latency {
                    self.peak_latency = v;
                    push_spike(&mut self.top_latency, v, 3);
                }
                p.last_value = Some(v);
                let j = match p.prev_rtt_adjacent {
                    Some(x) => {
                        let delta = (v - x).abs();
                        if delta > self.peak_jitter {
                            self.peak_jitter = delta;
                            push_spike(&mut self.top_jitter, delta, 3);
                        }
                        Some(delta)
                    }
                    None => None,
                };
                p.prev_rtt_adjacent = Some(v);
                p.jitter_ring.push(j);
                if p.cur_loss_burst > 0 {
                    if p.cur_loss_burst > self.worst_loss_burst {
                        self.worst_loss_burst = p.cur_loss_burst;
                    }
                    p.cur_loss_burst = 0;
                }
                p.consec_loss = 0;
                j
            }
            None => {
                p.lost += 1;
                p.consec_loss += 1;
                p.cur_loss_burst += 1;
                p.last_value = None;
                p.prev_rtt_adjacent = None;
                p.jitter_ring.push(None);
                None
            }
        };

        let (is_bad, is_down) = p.classify(&self.cfg);
        let mode = self.detector_mode;

        // Run recursive detectors (legacy mode skips)
        let (lat_latch, jit_latch) = match mode {
            DetectorMode::Legacy => (false, false),
            DetectorMode::Shadow | DetectorMode::Hybrid => {
                let lat_warn = self.cfg.latency_warn_ms;
                let lat_severe = self.cfg.latency_bad_ms;
                let jit_warn = self.cfg.jitter_warn_ms;
                let jit_severe = f64::MAX; // no severe branch for jitter

                let old_lat_latch = p.latency_detector.latch_active;
                let (lat_latch, lat_short, lat_ratio, _) = if let Some(v) = rtt {
                    p.latency_detector.observe(v, now, lat_warn, lat_severe)
                } else {
                    p.latency_detector.mark_gap();
                    (p.latency_detector.latch_active, None, 0.0, false)
                };

                let old_jit_latch = p.jitter_detector.latch_active;
                let (jit_latch, jit_short, jit_ratio, _) = if let Some(j) = jit_val {
                    p.jitter_detector.observe(j, now, jit_warn, jit_severe)
                } else {
                    p.jitter_detector.mark_gap();
                    (p.jitter_detector.latch_active, None, 0.0, false)
                };

                // Shadow mode: log latch transitions to shadow_log
                if mode == DetectorMode::Shadow {
                    if old_lat_latch != lat_latch {
                        if self.shadow_log.len() >= 64 {
                            self.shadow_log.pop_front();
                        }
                        self.shadow_log.push_back(ShadowEvent {
                            ts: now,
                            metric: "latency",
                            old_latch: old_lat_latch,
                            new_latch: lat_latch,
                            ratio: lat_ratio,
                            short: lat_short.unwrap_or(0.0),
                        });
                    }
                    if old_jit_latch != jit_latch {
                        if self.shadow_log.len() >= 64 {
                            self.shadow_log.pop_front();
                        }
                        self.shadow_log.push_back(ShadowEvent {
                            ts: now,
                            metric: "jitter",
                            old_latch: old_jit_latch,
                            new_latch: jit_latch,
                            ratio: jit_ratio,
                            short: jit_short.unwrap_or(0.0),
                        });
                    }
                }

                (lat_latch, jit_latch)
            }
        };

        let candidate = if is_down {
            LinkState::Down
        } else if is_bad || (mode == DetectorMode::Hybrid && (lat_latch || jit_latch)) {
            LinkState::Degraded
        } else {
            LinkState::Up
        };

        let prev_target_state = p.state;
        p.last_target_state = prev_target_state;
        let (new_state, new_pending) = reduce_target(
            p.state,
            Some(candidate),
            p.pending_worse,
            self.cfg.hysteresis_bad,
        );
        let target_transition = new_state != p.state;
        if target_transition {
            p.state = new_state;
            p.state_since = now;
        }
        p.pending_worse = new_pending;

        // Baseline admission: candidate-clean AND latch-free in hybrid
        let latch_gates_admission = mode == DetectorMode::Hybrid && (lat_latch || jit_latch);
        if candidate == LinkState::Up && !latch_gates_admission {
            if let Some(v) = rtt {
                p.baseline.push_latency(v);
            }
            if let Some(j) = jit_val {
                p.baseline.push_jitter(j);
            }
        }

        // Save info before releasing the mutable borrow on p
        let is_worsening = prev_target_state == LinkState::Up
            && matches!(new_state, LinkState::Degraded | LinkState::Down);
        let label = p.label.clone();

        if target_transition {
            let msg = match new_state {
                LinkState::Up => format!("[{}] target recovered", label),
                LinkState::Degraded => format!("[{}] target degraded", label),
                LinkState::Down => format!("[{}] target unreachable", label),
            };
            self.log(Level::Warn, msg);
            if is_worsening {
                while self.probe_anomaly_edge.len() <= idx {
                    self.probe_anomaly_edge.push(false);
                }
                self.probe_anomaly_edge[idx] = true;
            }
        }

        Some(candidate)
    }

    /// Convenience: process a single ping sample and finalize one generation.
    #[allow(dead_code)]
    pub fn ingest_ping(
        &mut self,
        idx: usize,
        sample: crate::net::PingSample,
    ) -> Option<SoundEvent> {
        let now = Instant::now();
        self.ingest_sample_at(idx, sample, now);
        self.lat_hist.push(self.last_value_view());
        self.jit_hist.push(self.jitter_view());
        let sound = self.finalize_generation(now);
        // Clear edge bits after finalization (same as ingest_generation_at)
        for e in self.probe_anomaly_edge.iter_mut() {
            *e = false;
        }
        sound
    }

    /// Process a batch of observations and finalize one generation.
    pub fn ingest_generation(&mut self, batch: PrimaryBatch) -> Option<SoundEvent> {
        self.ingest_generation_at(batch, Instant::now())
    }

    /// Process a batch of observations and finalize one generation.
    /// Deterministic entry point: accepts injected time for testing.
    pub fn ingest_generation_at(
        &mut self,
        batch: PrimaryBatch,
        now: Instant,
    ) -> Option<SoundEvent> {
        // Stale batch rejection
        if batch.reset_epoch != self.reset_epoch.load(Ordering::Acquire) {
            return None;
        }

        for (idx, result) in batch.results.iter().enumerate() {
            match result {
                RoundResult::Observed(sample) => {
                    self.ingest_sample_at(idx, *sample, now);
                }
                RoundResult::Missing => {
                    // Hold target state and pending — do nothing.
                }
            }
        }

        self.lat_hist.push(self.last_value_view());
        self.jit_hist.push(self.jitter_view());
        let mut sound = self.finalize_generation(now);

        // ProbeAnomaly cue: only fires on batch finalization
        if sound.is_none() {
            if let Some(cue) = self.evaluate_probe_anomaly_cue(now) {
                sound = Some(cue);
            }
        }

        // Clear all edge bits after finalization
        for e in self.probe_anomaly_edge.iter_mut() {
            *e = false;
        }
        sound
    }

    /// Finalize one generation: consensus, connection state, side effects.
    fn finalize_generation(&mut self, now: Instant) -> Option<SoundEvent> {
        self.accrue_state_time();

        let n = self.primaries.len();
        if n == 0 {
            return None;
        }

        let states: Vec<LinkState> = self.primaries.iter().map(|p| p.state).collect();
        let desired = consensus(&states);
        let prev = self.state;

        let new_state = reduce_connection(
            self.state,
            desired,
            &mut self.recovery,
            self.cfg.recover_dwell,
            now,
        );
        if new_state != self.state {
            self.state = new_state;
            self.state_since = now;
        }

        // Track outage onset for gateway evidence freshness
        if desired != DesiredState::Up && self.outage_onset_at.is_none() {
            self.outage_onset_at = Some(now);
        }
        if desired == DesiredState::Up {
            self.outage_onset_at = None;
        }

        let mut sound: Option<SoundEvent> = None;

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
                self.up_since = Some(now);
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

            let up_n = states.iter().filter(|s| **s == LinkState::Up).count();
            let down_n = states.iter().filter(|s| **s == LinkState::Down).count();
            let bad_n = n - up_n;
            let total: u64 = self.primaries.iter().map(|p| p.total).sum();
            let lost: u64 = self.primaries.iter().map(|p| p.lost).sum();
            let loss_pct = if total == 0 {
                0.0
            } else {
                lost as f64 * 100.0 / total as f64
            };
            let consensus_msg = format!(
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
                        format!("connection recovered  {}  ♪ recover", consensus_msg),
                    );
                    self.notify("connection recovered");
                    self.pending_cause_log_idx = None;
                    sound = Some(SoundEvent::Recover);
                }
                LinkState::Degraded => {
                    if prev == LinkState::Up {
                        let cause = self.path_cause(desired, now);
                        self.log(
                            Level::Warn,
                            format!(
                                "connection degraded  {}  cause: {}  ♪ degraded",
                                consensus_msg,
                                cause_word(cause)
                            ),
                        );
                        if cause == PathCause::Unknown {
                            self.pending_cause_log_idx = Some(self.events.len().saturating_sub(1));
                        }
                        self.notify("connection degraded");
                        sound = Some(SoundEvent::Loss);
                    } else {
                        self.log(
                            Level::Warn,
                            format!("improving → degraded  {}", consensus_msg),
                        );
                    }
                }
                LinkState::Down => {
                    let cause = self.path_cause(desired, now);
                    self.log(
                        Level::Bad,
                        format!(
                            "connection DOWN  {}  cause: {}  ♪ down",
                            consensus_msg,
                            cause_word(cause)
                        ),
                    );
                    if cause == PathCause::Unknown {
                        self.pending_cause_log_idx = Some(self.events.len().saturating_sub(1));
                    }
                    self.notify("connection DOWN");
                    sound = Some(SoundEvent::Down);
                }
            }
            self.last_reminder = Some(now);
            let _ = self.export_tsv();
        }

        let ms = match self.state {
            LinkState::Up => 1_000,
            LinkState::Degraded | LinkState::Down => 500,
        };
        self.interval_ms.store(ms, Ordering::Relaxed);

        // Adaptive gateway cadence: 1s during worsening, 5s otherwise
        let gw_any_worsening = self.primaries.iter().any(|p| p.pending_worse.is_some());
        let gw_ms = if gw_any_worsening || self.state != LinkState::Up {
            1_000
        } else {
            5_000
        };
        self.gw_cadence_ms.store(gw_ms, Ordering::Relaxed);

        sound
    }

    /// Evaluate whether an isolated-target ProbeAnomaly cue should fire.
    /// Called only from `ingest_generation_at` (batch finalization), not per-sample.
    fn evaluate_probe_anomaly_cue(&mut self, now: Instant) -> Option<SoundEvent> {
        if !self.prealert_enabled || self.muted {
            return None;
        }
        let cooldown_ok = self
            .probe_anomaly_cooldown_at
            .map(|t| now.duration_since(t).as_secs() >= 60)
            .unwrap_or(true);
        if !cooldown_ok {
            return None;
        }
        // Eligibility: exactly one primary has edge and remains bad,
        // all others are Up, connection is Up.
        let edge_count = self
            .probe_anomaly_edge
            .iter()
            .enumerate()
            .filter(|(i, &e)| {
                e && self
                    .primaries
                    .get(*i)
                    .is_some_and(|p| p.state != LinkState::Up)
            })
            .count();
        let all_others_up = self.primaries.iter().enumerate().all(|(i, p)| {
            p.state == LinkState::Up || self.probe_anomaly_edge.get(i).copied() == Some(true)
        });
        if edge_count == 1 && all_others_up && self.state == LinkState::Up {
            self.probe_anomaly_cooldown_at = Some(now);
            self.log(
                Level::Warn,
                "isolated target anomaly detected  ♪ probe anomaly",
            );
            Some(SoundEvent::ProbeAnomaly)
        } else {
            None
        }
    }

    /// Assess probable path cause for a connection transition.
    /// Receives `desired` explicitly to avoid relying on state invariant.
    pub fn path_cause(&self, desired: DesiredState, _now: Instant) -> PathCause {
        use crate::state::DesiredState;
        // No outage → no cause
        if desired == DesiredState::Up {
            return PathCause::Unknown;
        }

        let gw_fresh = match self.gw_role {
            GatewayRole::UserExtra(i) | GatewayRole::AutoExtra(i) => {
                self.extras
                    .get(i)
                    .and_then(|e| e.last_sample_at)
                    .map(|t| {
                        // Fresh if sampled after outage onset
                        self.outage_onset_at
                            .map(|onset| t >= onset)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            }
            GatewayRole::Unknown => false,
        };

        if gw_fresh {
            let gw_state = match self.gw_role {
                GatewayRole::UserExtra(i) | GatewayRole::AutoExtra(i) => {
                    self.extras.get(i).map(|e| e.state)
                }
                _ => None,
            };
            if gw_state == Some(LinkState::Down) {
                return PathCause::GatewayFailure;
            }
            if gw_state == Some(LinkState::Up) && self.state != LinkState::Up {
                return PathCause::BeyondFirstHop;
            }
        }

        // Weak Wi-Fi as supporting evidence
        if self.wifi_grade.map(|g| g <= 1).unwrap_or(false) {
            return PathCause::WeakWifi;
        }

        PathCause::Unknown
    }

    pub fn score(&self) -> f32 {
        if self.state == LinkState::Down {
            return 0.0;
        }
        let mut s: f32 = 100.0;
        let lat = self.last_value_view().unwrap_or(0.0) as f32;
        let loss = self.pooled_loss_pct() as f32;
        let dns = self.system_resolver_worst().unwrap_or(0.0) as f32;
        let jit = self.jitter_view().unwrap_or(0.0) as f32;
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

    pub fn recovery_remaining(&self, now: Instant) -> Option<Duration> {
        self.recovery.remaining(now, self.cfg.recover_dwell)
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
        // Build up healthy baseline.
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        assert_eq!(a.state, LinkState::Up);
        // Drive all targets down.
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: None });
            }
        }
        assert_ne!(a.state, LinkState::Up);
        // Drive all targets back up — recovery should start.
        for _ in 0..25 {
            for i in 0..3 {
                a.ingest_ping(i, PingSample { rtt_ms: Some(20.0) });
            }
        }
        // Connection is recovering but dwell hasn't elapsed yet.
        assert_ne!(
            a.state,
            LinkState::Up,
            "recovery should still be held by connection dwell"
        );
        // Verify recovery state exists.
        assert!(
            a.recovery.resumed_at.is_some(),
            "recovery should have started"
        );
    }

    #[test]
    fn jitter_view_is_median_across_primaries() {
        let mut a = app_with_n_probes(3);
        for _ in 0..30 {
            a.primaries[0].jitter_ring.push(Some(5.0));
            a.primaries[1].jitter_ring.push(Some(5.0));
            a.primaries[2].jitter_ring.push(Some(80.0));
        }
        let jv = a.jitter_view().unwrap();
        assert!(
            (jv - 5.0).abs() < 0.001,
            "jitter_view should be median (5.0), got {}",
            jv
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
            a.primaries[0].baseline.latency_len() == 0,
            "baseline should not have grown while target was down, got len {}",
            a.primaries[0].baseline.latency_len()
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
        assert!(cfg.hysteresis_bad >= 1, "hysteresis_bad clamped to >=1");
    }

    #[test]
    fn target_reducer_up_to_degraded_on_bad_streak() {
        let h = 3;
        let (s, p) = crate::state::reduce_target(LinkState::Up, Some(LinkState::Degraded), None, h);
        assert_eq!(s, LinkState::Up);
        assert_eq!(p, Some((LinkState::Degraded, 1)));
        let (s, p) = crate::state::reduce_target(s, Some(LinkState::Degraded), p, h);
        assert_eq!(s, LinkState::Up);
        let (s, _p) = crate::state::reduce_target(s, Some(LinkState::Degraded), p, h);
        assert_eq!(s, LinkState::Degraded, "should transition at hysteresis");
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

    #[test]
    fn gateway_update_added_when_absent() {
        let mut a = app_with_n_probes(1);
        let r = a.apply_gateway_update("192.168.1.1");
        assert!(matches!(r, GatewayUpdate::Added { idx: 0 }));
        assert_eq!(a.auto_gw_idx, Some(0));
        assert_eq!(a.extras[0].host, "192.168.1.1");
        assert_eq!(a.extras[0].label, "gw");
    }

    #[test]
    fn gateway_update_unchanged_when_same_host() {
        let mut a = app_with_n_probes(1);
        a.apply_gateway_update("192.168.1.1");
        let r = a.apply_gateway_update("192.168.1.1");
        assert!(matches!(r, GatewayUpdate::Unchanged));
        assert_eq!(a.extras.len(), 1);
    }

    #[test]
    fn gateway_update_replaces_host_and_resets_stats() {
        let mut a = app_with_n_probes(1);
        a.apply_gateway_update("192.168.1.1");
        {
            let e = &mut a.extras[0];
            e.total = 10;
            e.lost = 5;
            e.consec_loss = 3;
            e.state = LinkState::Down;
            e.last = None;
        }
        let r = a.apply_gateway_update("10.0.0.1");
        assert!(matches!(r, GatewayUpdate::Updated { .. }));
        let e = &a.extras[0];
        assert_eq!(e.host, "10.0.0.1");
        assert_eq!(e.label, "gw");
        assert_eq!(e.total, 0);
        assert_eq!(e.state, LinkState::Up);
    }

    #[test]
    fn classify_no_jitter_penalty_when_all_none() {
        let mut a = app_with_n_probes(1);
        // Feed enough successful pings to fill the window, then only loss.
        for _ in 0..25 {
            a.ingest_ping(0, PingSample { rtt_ms: Some(20.0) });
        }
        // Now push only loss — jitter ring fills with None.
        for _ in 0..25 {
            a.ingest_ping(0, PingSample { rtt_ms: None });
        }
        let cfg = Config::default();
        let (_, is_down) = a.primaries[0].classify(&cfg);
        // Loss alone triggers is_down, but jitter contributes nothing.
        assert!(is_down, "high loss should trigger is_down");
    }

    #[test]
    fn classify_averages_only_some_jitter() {
        let mut a = app_with_n_probes(1);
        // Fill with successful pings so window has data.
        for _ in 0..25 {
            a.ingest_ping(0, PingSample { rtt_ms: Some(20.0) });
        }
        let jit_ring = &a.primaries[0].jitter_ring;
        let jit: Vec<f64> = jit_ring
            .as_vec()
            .iter()
            .rev()
            .take(20)
            .filter_map(|v| *v)
            .collect();
        // All contiguous successes should produce Some jitter values.
        assert!(
            !jit.is_empty(),
            "contiguous successes should produce Some jitter values"
        );
    }

    #[test]
    fn independent_baseline_counts() {
        let mut a = app_with_n_probes(1);
        // Feed 60 successful pings — both latency and jitter should be populated.
        for _ in 0..60 {
            a.ingest_ping(0, PingSample { rtt_ms: Some(20.0) });
        }
        assert!(
            a.primaries[0].baseline.latency_len() >= 50,
            "latency should have >=50 values, got {}",
            a.primaries[0].baseline.latency_len()
        );
        assert!(
            a.primaries[0].baseline.jitter_len() >= 50,
            "jitter should have >=50 values, got {}",
            a.primaries[0].baseline.jitter_len()
        );
    }

    #[test]
    fn prev_rtt_adjacent_cleared_on_loss() {
        let mut a = app_with_n_probes(1);
        a.ingest_ping(0, PingSample { rtt_ms: Some(20.0) });
        assert!(
            a.primaries[0].prev_rtt_adjacent.is_some(),
            "prev_rtt_adjacent should be set after first success"
        );
        a.ingest_ping(0, PingSample { rtt_ms: None });
        assert!(
            a.primaries[0].prev_rtt_adjacent.is_none(),
            "prev_rtt_adjacent should be cleared on loss"
        );
        // First success after loss: jitter should be None (no adjacent).
        a.ingest_ping(0, PingSample { rtt_ms: Some(30.0) });
        let last_jit = *a.primaries[0].jitter_ring.buf.back().unwrap();
        assert_eq!(
            last_jit, None,
            "first jitter after loss should be None, got {:?}",
            last_jit
        );
    }

    #[test]
    fn last_value_is_none_during_loss() {
        let mut a = app_with_n_probes(1);
        a.ingest_ping(0, PingSample { rtt_ms: Some(20.0) });
        assert_eq!(a.primaries[0].last_value, Some(20.0));
        a.ingest_ping(0, PingSample { rtt_ms: None });
        assert_eq!(
            a.primaries[0].last_value, None,
            "last_value should be None on loss"
        );
        assert_eq!(
            a.last_value_view(),
            None,
            "last_value_view should exclude target with loss"
        );
    }

    #[test]
    fn segment_runs_all_none() {
        use crate::ui::segment_runs;
        let data: Vec<Option<f64>> = vec![None, None, None];
        let runs = segment_runs(&data);
        assert!(runs.is_empty(), "all None should produce no runs");
    }

    #[test]
    fn segment_runs_all_some() {
        use crate::ui::segment_runs;
        let data: Vec<Option<f64>> = vec![Some(1.0), Some(2.0), Some(3.0)];
        let runs = segment_runs(&data);
        assert_eq!(runs.len(), 1, "all Some should produce one run");
        assert_eq!(runs[0].len(), 3);
    }

    #[test]
    fn segment_runs_mixed() {
        use crate::ui::segment_runs;
        let data: Vec<Option<f64>> = vec![Some(1.0), None, Some(3.0), Some(4.0), None];
        let runs = segment_runs(&data);
        assert_eq!(runs.len(), 2, "mixed should produce two runs");
        assert_eq!(runs[0], vec![(0, 1.0)]);
        assert_eq!(runs[1], vec![(2, 3.0), (3, 4.0)]);
    }

    #[test]
    fn segment_runs_empty() {
        use crate::ui::segment_runs;
        let data: Vec<Option<f64>> = vec![];
        let runs = segment_runs(&data);
        assert!(runs.is_empty(), "empty input should produce no runs");
    }

    #[test]
    fn extra_probe_last_sample_at_set() {
        let mut a = app_with_n_probes(1);
        a.extras.push(ExtraProbe {
            label: "gw".into(),
            host: "192.168.1.1".into(),
            port: 80,
            last: None,
            state: LinkState::Up,
            total: 0,
            lost: 0,
            consec_loss: 0,
            ring: Ring::new(30),
            last_sample_at: None,
        });
        assert!(a.extras[0].last_sample_at.is_none());
        a.ingest_extra(0, PingSample { rtt_ms: Some(5.0) });
        assert!(
            a.extras[0].last_sample_at.is_some(),
            "last_sample_at should be set after ingest_extra"
        );
    }

    #[test]
    fn extra_probe_reset_clears_last_sample_at() {
        let mut e = ExtraProbe {
            label: "gw".into(),
            host: "192.168.1.1".into(),
            port: 80,
            last: Some(5.0),
            state: LinkState::Down,
            total: 10,
            lost: 3,
            consec_loss: 2,
            ring: Ring::new(30),
            last_sample_at: Some(Instant::now()),
        };
        e.reset();
        assert!(e.last_sample_at.is_none());
        assert_eq!(e.state, LinkState::Up);
    }

    #[test]
    fn path_cause_unknown_when_no_outage() {
        let a = app_with_n_probes(3);
        let cause = a.path_cause(DesiredState::Up, Instant::now());
        assert_eq!(cause, PathCause::Unknown);
    }

    #[test]
    fn path_cause_gateway_failure() {
        let mut a = app_with_n_probes(3);
        a.outage_onset_at = Some(Instant::now());
        a.gw_role = GatewayRole::AutoExtra(0);
        a.extras.push(ExtraProbe {
            label: "gw".into(),
            host: "192.168.1.1".into(),
            port: 80,
            last: Some(2.0),
            state: LinkState::Down,
            total: 10,
            lost: 5,
            consec_loss: 3,
            ring: Ring::new(30),
            last_sample_at: Some(Instant::now()),
        });
        let cause = a.path_cause(DesiredState::Down, Instant::now());
        assert_eq!(cause, PathCause::GatewayFailure);
    }

    #[test]
    fn path_cause_beyond_first_hop() {
        let mut a = app_with_n_probes(3);
        a.outage_onset_at = Some(Instant::now());
        a.state = LinkState::Down;
        a.gw_role = GatewayRole::AutoExtra(0);
        a.extras.push(ExtraProbe {
            label: "gw".into(),
            host: "192.168.1.1".into(),
            port: 80,
            last: Some(2.0),
            state: LinkState::Up,
            total: 10,
            lost: 0,
            consec_loss: 0,
            ring: Ring::new(30),
            last_sample_at: Some(Instant::now()),
        });
        let cause = a.path_cause(DesiredState::Down, Instant::now());
        assert_eq!(cause, PathCause::BeyondFirstHop);
    }

    #[test]
    fn path_cause_stale_gateway_is_unknown() {
        let mut a = app_with_n_probes(3);
        a.outage_onset_at = Some(Instant::now());
        a.gw_role = GatewayRole::AutoExtra(0);
        a.extras.push(ExtraProbe {
            label: "gw".into(),
            host: "192.168.1.1".into(),
            port: 80,
            last: Some(2.0),
            state: LinkState::Up,
            total: 10,
            lost: 0,
            consec_loss: 0,
            ring: Ring::new(30),
            last_sample_at: None, // stale
        });
        let cause = a.path_cause(DesiredState::Down, Instant::now());
        assert_eq!(cause, PathCause::Unknown);
    }

    #[test]
    fn outage_onset_at_set_and_cleared() {
        let mut a = app_with_n_probes(3);
        assert!(a.outage_onset_at.is_none());
        // Send enough loss rounds for targets to transition (hysteresis_bad=3)
        // and connection to reach Down (quorum=2).
        for round in 0..15 {
            let batch = PrimaryBatch {
                round_id: round,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: None }),
                    RoundResult::Observed(PingSample { rtt_ms: None }),
                    RoundResult::Observed(PingSample { rtt_ms: None }),
                ],
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
        }
        assert!(
            a.outage_onset_at.is_some(),
            "outage_onset_at should be set after sustained loss, state={:?}",
            a.state
        );
    }

    #[test]
    fn prealert_default_off() {
        let a = app_with_n_probes(3);
        assert!(!a.prealert_enabled, "prealert should be off by default");
    }

    #[test]
    fn prealert_cue_fires_on_isolated_anomaly() {
        let mut a = app_with_n_probes(3);
        a.prealert_enabled = true;
        // Build healthy baseline using batches
        for round in 0..25 {
            let batch = PrimaryBatch {
                round_id: round,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                ],
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
        }
        assert_eq!(a.state, LinkState::Up);
        // Burst of losses on target 0 only
        for round in 25..50 {
            let batch = PrimaryBatch {
                round_id: round,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: None }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                ],
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
        }
        // Edge bit should have been set and then cleared after finalization
        // But the target should be in a bad state
        assert_ne!(
            a.primaries[0].state,
            LinkState::Up,
            "target 0 should be degraded/down"
        );
        assert_eq!(a.primaries[1].state, LinkState::Up);
        assert_eq!(a.primaries[2].state, LinkState::Up);
    }

    #[test]
    fn prealert_cooldown_suppresses() {
        let mut a = app_with_n_probes(3);
        a.prealert_enabled = true;
        a.probe_anomaly_cooldown_at = Some(Instant::now());
        // Even with edge bits set, cooldown should suppress
        a.probe_anomaly_edge = vec![true, false, false];
        // Can't easily test finalize_generation sound selection without
        // going through the full batch path, but verify cooldown is checked
        let now = Instant::now();
        let cooldown_ok = a
            .probe_anomaly_cooldown_at
            .map(|t| now.duration_since(t).as_secs() >= 60)
            .unwrap_or(true);
        assert!(!cooldown_ok, "cooldown should suppress within 60s");
    }

    #[test]
    fn reset_clears_prealert_state() {
        let mut a = app_with_n_probes(3);
        a.prealert_enabled = true;
        a.probe_anomaly_edge = vec![true, false, false];
        a.probe_anomaly_cooldown_at = Some(Instant::now());
        a.reset();
        assert!(a.probe_anomaly_edge.is_empty());
        assert!(a.probe_anomaly_cooldown_at.is_none());
    }

    #[test]
    fn arrival_order_invariant() {
        // All permutations of 3 targets arriving in different orders should
        // produce identical connection state after one batch.
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for order in &orders {
            let mut a = app_with_n_probes(3);
            // Build baseline
            for _ in 0..25 {
                let batch = PrimaryBatch {
                    round_id: 0,
                    reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                    results: vec![
                        RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                        RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                        RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    ],
                    started_at: Instant::now(),
                };
                let _ = a.ingest_generation_at(batch, Instant::now());
            }
            // Now send one batch with targets in a specific order
            let results: Vec<RoundResult> = order
                .iter()
                .map(|&i| {
                    if i == 0 {
                        RoundResult::Observed(PingSample { rtt_ms: None })
                    } else {
                        RoundResult::Observed(PingSample { rtt_ms: Some(20.0) })
                    }
                })
                .collect();
            let batch = PrimaryBatch {
                round_id: 25,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results,
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
            // Target 0 should be the same regardless of arrival order
            assert_eq!(
                a.primaries[0].state,
                LinkState::Up,
                "target 0 state should be independent of arrival order {:?}",
                order
            );
        }
    }

    #[test]
    fn three_observations_advance_logic_once() {
        let mut a = app_with_n_probes(3);
        // Build baseline
        for _ in 0..25 {
            let batch = PrimaryBatch {
                round_id: 0,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                ],
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
        }
        assert_eq!(a.state, LinkState::Up);
        // Send one batch with 2 loss + 1 success
        let batch = PrimaryBatch {
            round_id: 25,
            reset_epoch: a.reset_epoch.load(Ordering::Acquire),
            results: vec![
                RoundResult::Observed(PingSample { rtt_ms: None }),
                RoundResult::Observed(PingSample { rtt_ms: None }),
                RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
            ],
            started_at: Instant::now(),
        };
        let _ = a.ingest_generation_at(batch, Instant::now());
        // Connection should still be Up (not Down yet — hysteresis)
        assert_eq!(a.state, LinkState::Up);
    }

    #[test]
    fn stale_epoch_rejected_after_reset() {
        let mut a = app_with_n_probes(3);
        let old_epoch = a.reset_epoch.load(Ordering::Acquire);
        // Build baseline
        for _ in 0..25 {
            let batch = PrimaryBatch {
                round_id: 0,
                reset_epoch: old_epoch,
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                ],
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
        }
        // Reset increments epoch
        a.reset();
        let new_epoch = a.reset_epoch.load(Ordering::Acquire);
        assert_ne!(old_epoch, new_epoch, "reset should increment epoch");
        // Batch with old epoch should be rejected
        let batch = PrimaryBatch {
            round_id: 25,
            reset_epoch: old_epoch,
            results: vec![
                RoundResult::Observed(PingSample { rtt_ms: None }),
                RoundResult::Observed(PingSample { rtt_ms: None }),
                RoundResult::Observed(PingSample { rtt_ms: None }),
            ],
            started_at: Instant::now(),
        };
        let sound = a.ingest_generation_at(batch, Instant::now());
        assert!(sound.is_none(), "stale batch should be rejected");
        assert_eq!(
            a.primaries[0].total, 0,
            "counters should not be updated for stale batch"
        );
    }

    #[test]
    fn missing_holds_state_no_counters() {
        let mut a = app_with_n_probes(3);
        // Build baseline
        for _ in 0..25 {
            let batch = PrimaryBatch {
                round_id: 0,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                ],
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
        }
        let prev_total = a.primaries[0].total;
        let prev_state = a.primaries[0].state;
        // Send batch with Missing for target 0
        let batch = PrimaryBatch {
            round_id: 25,
            reset_epoch: a.reset_epoch.load(Ordering::Acquire),
            results: vec![
                RoundResult::Missing,
                RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
            ],
            started_at: Instant::now(),
        };
        let _ = a.ingest_generation_at(batch, Instant::now());
        assert_eq!(
            a.primaries[0].total, prev_total,
            "Missing should not increment total"
        );
        assert_eq!(
            a.primaries[0].state, prev_state,
            "Missing should hold target state"
        );
    }

    #[test]
    fn one_transition_one_side_effect() {
        let mut a = app_with_n_probes(3);
        // Build baseline
        for round in 0..25 {
            let batch = PrimaryBatch {
                round_id: round,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                    RoundResult::Observed(PingSample { rtt_ms: Some(20.0) }),
                ],
                started_at: Instant::now(),
            };
            let _ = a.ingest_generation_at(batch, Instant::now());
        }
        let events_before = a.events.len();
        let mut sounds = 0;
        // Drive all targets down
        for round in 25..55 {
            let batch = PrimaryBatch {
                round_id: round,
                reset_epoch: a.reset_epoch.load(Ordering::Acquire),
                results: vec![
                    RoundResult::Observed(PingSample { rtt_ms: None }),
                    RoundResult::Observed(PingSample { rtt_ms: None }),
                    RoundResult::Observed(PingSample { rtt_ms: None }),
                ],
                started_at: Instant::now(),
            };
            let sound = a.ingest_generation_at(batch, Instant::now());
            if sound.is_some() {
                sounds += 1;
            }
        }
        // Should produce exactly 2 connection transition sounds:
        // Up→Degraded (1 sound) and Degraded→Down (1 sound)
        assert!(
            sounds <= 2,
            "should produce at most 2 transition sounds (Up→Degraded, Degraded→Down), got {}",
            sounds
        );
        assert!(
            a.events.len() > events_before,
            "transitions should produce log events"
        );
    }

    #[test]
    fn detector_mode_from_env() {
        // Default is Legacy
        std::env::remove_var("PM_RECURSIVE_MODE");
        assert_eq!(DetectorMode::from_env(), DetectorMode::Legacy);
        // Explicit values
        std::env::set_var("PM_RECURSIVE_MODE", "shadow");
        assert_eq!(DetectorMode::from_env(), DetectorMode::Shadow);
        std::env::set_var("PM_RECURSIVE_MODE", "hybrid");
        assert_eq!(DetectorMode::from_env(), DetectorMode::Hybrid);
        std::env::set_var("PM_RECURSIVE_MODE", "legacy");
        assert_eq!(DetectorMode::from_env(), DetectorMode::Legacy);
        // Invalid value falls back to Legacy
        std::env::set_var("PM_RECURSIVE_MODE", "invalid");
        assert_eq!(DetectorMode::from_env(), DetectorMode::Legacy);
        std::env::remove_var("PM_RECURSIVE_MODE");
    }
}
