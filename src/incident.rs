use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
#[cfg(test)]
use std::io::Read;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HEADER: &str = "schema_version\trecord_type\tincident_id\tepisode_kind\tstarted_at\tended_at\tduration_ms\ttransition\tcause\tstate\tpacket_loss_pct\tlatest_rtt_ms\taverage_rtt_ms\tjitter_ms\tdns_state\tgateway_state\tend_reason\ttrace_classification\ttrace_last_responsive_hop\ttrace_failure_boundary\ttrace_reached_target\ttrace_raw_path\n";

#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub state: String,
    pub packet_loss_pct: Option<f64>,
    pub latest_rtt_ms: Option<f64>,
    pub average_rtt_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub dns_state: Option<String>,
    pub gateway_state: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransitionNotice {
    pub from: String,
    pub to: String,
    pub at: SystemTime,
    pub duration_ms: u64,
    pub cause: String,
    pub evidence: Evidence,
}

#[derive(Debug, Clone)]
pub struct TraceRecord {
    pub classification: String,
    pub last_responsive_hop: Option<String>,
    pub failure_boundary: Option<u8>,
    pub reached_target: bool,
    pub raw_output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpisodeKind {
    Degradation,
    Outage,
}

impl EpisodeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Degradation => "degradation",
            Self::Outage => "outage",
        }
    }
}

struct ActiveEpisode {
    id: String,
    kind: EpisodeKind,
    started_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct IncidentMetadata {
    pub id: String,
    pub episode_kind: String,
    pub started_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct RecoveredIncident {
    pub metadata: IncidentMetadata,
    pub evidence: Evidence,
}

struct RecordData<'a> {
    record_type: &'a str,
    ended_at: SystemTime,
    duration: Duration,
    transition: &'a str,
    cause: &'a str,
    evidence: &'a Evidence,
    end_reason: &'a str,
}

pub struct IncidentWriter {
    dir: PathBuf,
    active: Option<ActiveEpisode>,
    next_id: u64,
}

impl IncidentWriter {
    pub fn new(dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            active: None,
            next_id: 0,
        })
    }

    pub fn apply(&mut self, notice: &TransitionNotice) -> io::Result<()> {
        match (notice.from.as_str(), notice.to.as_str()) {
            ("up", "degraded") => self.start(EpisodeKind::Degradation, notice),
            ("degraded", "down") => {
                self.finish(
                    notice.at,
                    Duration::from_millis(notice.duration_ms),
                    "state_changed",
                    notice,
                )?;
                self.start(EpisodeKind::Outage, notice)
            }
            ("down", "degraded") => {
                self.finish(
                    notice.at,
                    Duration::from_millis(notice.duration_ms),
                    "state_changed",
                    notice,
                )?;
                self.start(EpisodeKind::Degradation, notice)
            }
            ("degraded", "up") | ("down", "up") => self.finish(
                notice.at,
                Duration::from_millis(notice.duration_ms),
                "recovered",
                notice,
            ),
            ("up", "down") => self.start(EpisodeKind::Outage, notice),
            _ => Ok(()),
        }
    }

    pub fn active_metadata(&self) -> Option<IncidentMetadata> {
        self.active.as_ref().map(|episode| IncidentMetadata {
            id: episode.id.clone(),
            episode_kind: episode.kind.as_str().to_string(),
            started_at: episode.started_at,
        })
    }

    pub fn recover_open_episode(&self) -> io::Result<Option<RecoveredIncident>> {
        let mut paths = fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("tsv"))
            .collect::<Vec<_>>();
        paths.sort();

        let mut open = HashMap::<String, (u64, RecoveredIncident)>::new();
        let mut closed = HashSet::<String>::new();
        let mut sequence = 0_u64;

        for path in paths {
            let contents = fs::read_to_string(path)?;
            let mut lines = contents.lines();
            let Some(header) = lines.next() else {
                continue;
            };
            if header.trim_end() != HEADER.trim_end() {
                continue;
            }
            for line in lines {
                let Some(record) = parse_recovery_record(line) else {
                    continue;
                };
                sequence += 1;
                match record {
                    RecoveryRecord::Start(incident) => {
                        if !closed.contains(&incident.metadata.id) {
                            open.insert(incident.metadata.id.clone(), (sequence, incident));
                        }
                    }
                    RecoveryRecord::End(id) => {
                        open.remove(&id);
                        closed.insert(id);
                    }
                    RecoveryRecord::Trace => {}
                }
            }
        }

        Ok(open
            .into_values()
            .max_by(|(left_sequence, left), (right_sequence, right)| {
                let left_time = left
                    .metadata
                    .started_at
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let right_time = right
                    .metadata
                    .started_at
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                left_time
                    .cmp(&right_time)
                    .then(left_sequence.cmp(right_sequence))
            })
            .map(|(_, incident)| incident))
    }

    pub fn resume(&mut self, incident_id: String, episode_kind: &str, started_at_ms: u64) {
        let kind = if episode_kind == "outage" {
            EpisodeKind::Outage
        } else {
            EpisodeKind::Degradation
        };
        self.active = Some(ActiveEpisode {
            id: incident_id,
            kind,
            started_at: UNIX_EPOCH + Duration::from_millis(started_at_ms),
        });
    }

    pub fn close_with_evidence(
        &mut self,
        at: SystemTime,
        duration: Duration,
        reason: &str,
        evidence: &Evidence,
    ) -> io::Result<()> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        self.write_record(
            &active,
            RecordData {
                record_type: "END",
                ended_at: at,
                duration,
                transition: "",
                cause: "",
                evidence,
                end_reason: reason,
            },
        )
    }

    pub fn record_trace_for(
        &self,
        context: &IncidentMetadata,
        at: SystemTime,
        trace: &TraceRecord,
    ) -> io::Result<()> {
        let traces_dir = self.dir.join("traces");
        let traces_dir_is_new = path_was_absent(&traces_dir);
        fs::create_dir_all(&traces_dir)?;
        if traces_dir_is_new {
            sync_parent_directory(&traces_dir)?;
        }
        let raw_path = traces_dir.join(format!("{}.txt", context.id));
        let raw_is_new = path_was_absent(&raw_path);
        let mut raw = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&raw_path)?;
        raw.write_all(trace.raw_output.as_bytes())?;
        raw.flush()?;
        raw.sync_all()?;
        if raw_is_new {
            sync_parent_directory(&raw_path)?;
        }

        let path = self
            .dir
            .join(format!("{}.tsv", date_utc(context.started_at)));
        let is_new = path_was_absent(&path);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if file.metadata()?.len() == 0 {
            file.write_all(HEADER.as_bytes())?;
        }
        let row = format!(
            "1\tTRACE\t{}\t{}\t{}\t{}\t\t\t\t\t\t\t\t\t\t\t\t{}\t{}\t{}\t{}\t{}\n",
            context.id,
            context.episode_kind,
            format_timestamp(context.started_at),
            format_timestamp(at),
            trace.classification,
            trace.last_responsive_hop.as_deref().unwrap_or(""),
            trace
                .failure_boundary
                .map(|ttl| ttl.to_string())
                .unwrap_or_default(),
            trace.reached_target,
            raw_path
                .strip_prefix(&self.dir)
                .unwrap_or(&raw_path)
                .display()
        );
        file.write_all(row.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        if is_new {
            sync_parent_directory(&path)?;
        }
        Ok(())
    }

    fn start(&mut self, kind: EpisodeKind, notice: &TransitionNotice) -> io::Result<()> {
        if self.active.is_some() {
            self.finish(
                notice.at,
                Duration::from_millis(notice.duration_ms),
                "state_changed",
                notice,
            )?;
        }
        self.next_id += 1;
        let active = ActiveEpisode {
            id: format!(
                "incident-{}-{}",
                notice
                    .at
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                self.next_id
            ),
            kind,
            started_at: notice.at,
        };
        let transition = format!("{}->{}", notice.from, notice.to);
        self.write_record(
            &active,
            RecordData {
                record_type: "START",
                ended_at: notice.at,
                duration: Duration::ZERO,
                transition: &transition,
                cause: &notice.cause,
                evidence: &notice.evidence,
                end_reason: "",
            },
        )?;
        self.active = Some(active);
        Ok(())
    }

    fn finish(
        &mut self,
        at: SystemTime,
        duration: Duration,
        reason: &str,
        notice: &TransitionNotice,
    ) -> io::Result<()> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let transition = format!("{}->{}", notice.from, notice.to);
        self.write_record(
            &active,
            RecordData {
                record_type: "END",
                ended_at: at,
                duration,
                transition: &transition,
                cause: &notice.cause,
                evidence: &notice.evidence,
                end_reason: reason,
            },
        )
    }

    fn write_record(&self, active: &ActiveEpisode, record: RecordData<'_>) -> io::Result<()> {
        let path = self
            .dir
            .join(format!("{}.tsv", date_utc(active.started_at)));
        let is_new = !path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if file.metadata()?.len() == 0 {
            file.write_all(HEADER.as_bytes())?;
        }
        let row = format!(
            "1\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\t\t\t\t\n",
            record.record_type,
            active.id,
            active.kind.as_str(),
            format_timestamp(active.started_at),
            if record.record_type == "END" {
                format_timestamp(record.ended_at)
            } else {
                String::new()
            },
            if record.record_type == "END" {
                record.duration.as_millis().to_string()
            } else {
                String::new()
            },
            record.transition,
            record.cause,
            record.evidence.state,
            format_option(record.evidence.packet_loss_pct),
            format_option(record.evidence.latest_rtt_ms),
            format_option(record.evidence.average_rtt_ms),
            format_option(record.evidence.jitter_ms),
            record.evidence.dns_state.as_deref().unwrap_or(""),
            record.evidence.gateway_state.as_deref().unwrap_or(""),
            record.end_reason
        );
        file.write_all(row.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        if is_new {
            sync_parent_directory(&path)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn read_all(&self) -> io::Result<String> {
        let mut output = String::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("tsv") {
                continue;
            }
            fs::File::open(path)?.read_to_string(&mut output)?;
        }
        Ok(output)
    }
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    sync_directory_if_supported(parent)
}

fn path_was_absent(path: &Path) -> bool {
    !path.exists()
}

fn is_directory_sync_unsupported(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
    )
}

fn sync_directory_if_supported(directory: &Path) -> io::Result<()> {
    match sync_directory(directory) {
        Ok(()) => Ok(()),
        Err(error) if is_directory_sync_unsupported(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(directory)?.sync_all()
    }

    #[cfg(not(unix))]
    {
        let _ = directory;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory synchronization is not supported on this platform",
        ))
    }
}

enum RecoveryRecord {
    Start(RecoveredIncident),
    End(String),
    Trace,
}

fn parse_recovery_record(line: &str) -> Option<RecoveryRecord> {
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() < 22 || fields.first().copied() != Some("1") {
        return None;
    }
    let record_type = fields[1];
    let incident_id = fields[2];
    if incident_id.is_empty() {
        return None;
    }
    match record_type {
        "START" => {
            let episode_kind = match fields[3] {
                "degradation" | "outage" => fields[3].to_string(),
                _ => return None,
            };
            let started_at = parse_timestamp(fields[4])?;
            Some(RecoveryRecord::Start(RecoveredIncident {
                metadata: IncidentMetadata {
                    id: incident_id.to_string(),
                    episode_kind,
                    started_at,
                },
                evidence: parse_evidence(&fields)?,
            }))
        }
        "END" => Some(RecoveryRecord::End(incident_id.to_string())),
        "TRACE" => Some(RecoveryRecord::Trace),
        _ => None,
    }
}

fn parse_evidence(fields: &[&str]) -> Option<Evidence> {
    Some(Evidence {
        state: fields[9].to_string(),
        packet_loss_pct: parse_optional_field(fields[10])?,
        latest_rtt_ms: parse_optional_field(fields[11])?,
        average_rtt_ms: parse_optional_field(fields[12])?,
        jitter_ms: parse_optional_field(fields[13])?,
        dns_state: (!fields[14].is_empty()).then(|| fields[14].to_string()),
        gateway_state: (!fields[15].is_empty()).then(|| fields[15].to_string()),
    })
}

fn parse_optional_field(value: &str) -> Option<Option<f64>> {
    if value.is_empty() {
        Some(None)
    } else {
        value.parse::<f64>().ok().map(Some)
    }
}

fn parse_timestamp(raw: &str) -> Option<SystemTime> {
    let (date, time) = raw.split_once('T')?;
    let time = time.strip_suffix('Z')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i64>().ok()?;
    let month = date_parts.next()?.parse::<u32>().ok()?;
    let day = date_parts.next()?.parse::<u32>().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > max_day {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<u64>().ok()?;
    let minute = time_parts.next()?.parse::<u64>().ok()?;
    let second = time_parts.next()?.parse::<u64>().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let clock_seconds = hour.checked_mul(3_600)? + minute.checked_mul(60)? + second;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(clock_seconds as i64)?;
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs((-seconds) as u64))
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_prime = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn format_option(value: Option<f64>) -> String {
    value.map(|v| format!("{v:.2}")).unwrap_or_default()
}

fn date_utc(time: SystemTime) -> String {
    let days = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn format_timestamp(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let days = duration.as_secs() / 86_400;
    let seconds = duration.as_secs() % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = seconds / 3600;
    let minute = (seconds % 3600) / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::{is_directory_sync_unsupported, path_was_absent, sync_directory_if_supported, *};
    use std::io;
    use std::time::{Duration, SystemTime};

    fn test_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ping-monitor-{name}-{nonce}"))
    }

    fn notice(from: &str, to: &str, ms: u64) -> TransitionNotice {
        TransitionNotice {
            from: from.into(),
            to: to.into(),
            at: SystemTime::now(),
            duration_ms: ms,
            cause: "unknown".into(),
            evidence: Evidence::default(),
        }
    }

    #[test]
    fn continuous_degradation_writes_one_start_record() {
        let dir = test_dir("continuous-degradation");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&notice("up", "degraded", 0)).unwrap();
        writer.apply(&notice("degraded", "degraded", 1)).unwrap();
        let contents = writer.read_all().unwrap();
        assert_eq!(contents.matches("	START	").count(), 1);
        assert_eq!(contents.matches("	END	").count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transitions_close_and_open_the_expected_episodes() {
        let dir = test_dir("transition-sequence");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        for (from, to, duration) in [
            ("up", "degraded", 100),
            ("degraded", "down", 200),
            ("down", "degraded", 300),
            ("degraded", "up", 400),
        ] {
            writer.apply(&notice(from, to, duration)).unwrap();
        }
        let contents = writer.read_all().unwrap();
        assert_eq!(contents.matches("	START	").count(), 3);
        assert_eq!(contents.matches("	END	").count(), 3);
        assert!(contents.contains("	degradation	"));
        assert!(contents.contains("	outage	"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn start_record_contains_evidence_and_is_flushed() {
        let dir = test_dir("evidence");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        let mut n = notice("up", "degraded", 0);
        n.evidence.packet_loss_pct = Some(25.0);
        n.evidence.latest_rtt_ms = Some(230.0);
        writer.apply(&n).unwrap();
        let contents = writer.read_all().unwrap();
        assert!(contents.contains("packet_loss_pct"));
        assert!(contents.contains("25.00"));
        assert!(contents.contains("230.00"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn closing_active_episode_uses_explicit_reason() {
        let dir = test_dir("shutdown");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&notice("up", "down", 0)).unwrap();
        writer
            .close_with_evidence(
                SystemTime::now(),
                Duration::from_secs(4),
                "monitor_stopped",
                &Evidence::default(),
            )
            .unwrap();
        let contents = writer.read_all().unwrap();
        assert!(contents.contains("monitor_stopped"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_record_is_linked_to_active_incident_and_raw_output_is_separate() {
        let dir = test_dir("trace");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&notice("up", "down", 0)).unwrap();
        let context = writer.active_metadata().expect("active incident context");
        writer
            .record_trace_for(
                &context,
                SystemTime::now(),
                &TraceRecord {
                    classification: "failure_after_gateway".into(),
                    last_responsive_hop: Some("203.0.113.7".into()),
                    failure_boundary: Some(3),
                    reached_target: false,
                    raw_output: "1 192.0.2.1\n2 * * *\n".into(),
                },
            )
            .unwrap();
        let contents = writer.read_all().unwrap();
        assert!(contents.contains("\tTRACE\t"));
        assert!(contents.contains("failure_after_gateway"));
        let traces = dir.join("traces");
        assert_eq!(
            std::fs::read_dir(traces).unwrap().count(),
            1,
            "raw trace must be written separately"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_record_can_be_written_after_episode_is_closed() {
        let dir = test_dir("trace-after-close");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&notice("up", "down", 0)).unwrap();
        let context = writer.active_metadata().expect("active incident context");
        writer
            .close_with_evidence(
                SystemTime::now(),
                Duration::from_secs(1),
                "recovered",
                &Evidence::default(),
            )
            .unwrap();
        writer
            .record_trace_for(
                &context,
                SystemTime::now(),
                &TraceRecord {
                    classification: "failure_after_gateway".into(),
                    last_responsive_hop: Some("203.0.113.7".into()),
                    failure_boundary: Some(3),
                    reached_target: false,
                    raw_output: "1 192.0.2.1\n2 * * *\n".into(),
                },
            )
            .unwrap();
        let contents = writer.read_all().unwrap();
        assert!(contents.contains("\tTRACE\t"));
        assert_eq!(std::fs::read_dir(dir.join("traces")).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_artifacts_finish_with_a_valid_raw_reference_and_no_orphan_files() {
        let dir = test_dir("trace-artifacts-final-state");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        writer.apply(&notice("up", "down", 0)).unwrap();
        let context = writer.active_metadata().expect("active incident context");
        writer
            .record_trace_for(
                &context,
                SystemTime::now(),
                &TraceRecord {
                    classification: "unavailable".into(),
                    last_responsive_hop: None,
                    failure_boundary: None,
                    reached_target: false,
                    raw_output: "trace evidence".into(),
                },
            )
            .unwrap();

        let contents = writer.read_all().unwrap();
        let trace_line = contents
            .lines()
            .find(|line| line.contains("\tTRACE\t"))
            .expect("TRACE row should exist");
        let fields = trace_line.split('\t').collect::<Vec<_>>();
        let raw_relative = fields.get(21).expect("TRACE raw path column");
        let raw_path = dir.join(raw_relative);
        assert!(
            raw_path.is_file(),
            "TRACE must reference an existing raw file"
        );
        assert_eq!(std::fs::read_to_string(raw_path).unwrap(), "trace evidence");
        assert_eq!(std::fs::read_dir(dir.join("traces")).unwrap().count(), 1);
        assert!(!std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("tmp")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn creation_detection_runs_before_open_and_directory_sync_is_supported_or_explicitly_skipped() {
        let dir = test_dir("trace-creation-detection");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("new.tsv");
        assert!(path_was_absent(&path));
        std::fs::File::create(&path).unwrap();
        assert!(!path_was_absent(&path));
        sync_directory_if_supported(&dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_sync_classifies_invalid_input_like_unsupported() {
        assert!(is_directory_sync_unsupported(&io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported",
        )));
        assert!(is_directory_sync_unsupported(&io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory fsync is unavailable",
        )));
        assert!(!is_directory_sync_unsupported(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission denied",
        )));
        assert!(!is_directory_sync_unsupported(&io::Error::other(
            "I/O failure",
        )));
    }

    #[test]
    fn shutdown_end_record_keeps_final_evidence() {
        let dir = test_dir("shutdown-evidence");
        let mut writer = IncidentWriter::new(&dir).unwrap();
        let mut start = notice("up", "down", 0);
        start.evidence.state = "down".into();
        start.evidence.packet_loss_pct = Some(100.0);
        writer.apply(&start).unwrap();
        writer
            .close_with_evidence(
                SystemTime::now(),
                Duration::from_secs(2),
                "monitor_stopped",
                &start.evidence,
            )
            .unwrap();
        let contents = writer.read_all().unwrap();
        let end = contents
            .lines()
            .rfind(|line| line.contains("\tEND\t"))
            .expect("END record should exist");
        assert!(end.contains("\tdown\t"));
        assert!(end.contains("\t100.00\t"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
