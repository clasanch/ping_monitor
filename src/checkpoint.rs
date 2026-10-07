use crate::incident::Evidence;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub state: String,
    pub episode_kind: Option<String>,
    pub incident_id: Option<String>,
    pub started_at_ms: Option<u64>,
    pub config_fingerprint: String,
    pub evidence: Evidence,
}

pub struct StateStore {
    path: PathBuf,
}

fn atomic_replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        fs::rename(temporary, destination)
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(
                existing_file_name: *const u16,
                new_file_name: *const u16,
                flags: u32,
            ) -> i32;
        }

        let source: Vec<u16> = temporary
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
        let replaced = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn sync_parent_directory(destination: &Path) -> io::Result<()> {
    let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    #[cfg(unix)]
    {
        fs::File::open(parent)?.sync_all()
    }

    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

fn durable_replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    atomic_replace(temporary, destination)?;
    sync_parent_directory(destination)
}

impl StateStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub fn load(&self) -> io::Result<Option<Checkpoint>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&self.path)?;
        let mut state = None;
        let mut episode_kind = None;
        let mut incident_id = None;
        let mut started_at_ms = None;
        let mut config_fingerprint = String::new();
        let mut evidence = Evidence::default();
        for line in contents.lines() {
            let Some((key, value)) = line.split_once('=') else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "checkpoint line is missing '='",
                ));
            };
            match key {
                "state" => state = Some(value.to_string()),
                "episode_kind" => {
                    episode_kind = (!value.is_empty()).then(|| value.to_string());
                }
                "incident_id" => {
                    incident_id = (!value.is_empty()).then(|| value.to_string());
                }
                "started_at_ms" => {
                    started_at_ms = (!value.is_empty())
                        .then(|| value.parse::<u64>())
                        .transpose()
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "checkpoint started_at_ms is invalid",
                            )
                        })?;
                }
                "config_fingerprint" => config_fingerprint = value.to_string(),
                "evidence_state" => evidence.state = value.to_string(),
                "evidence_packet_loss_pct" => {
                    evidence.packet_loss_pct =
                        parse_optional_f64("evidence_packet_loss_pct", value)?
                }
                "evidence_latest_rtt_ms" => {
                    evidence.latest_rtt_ms = parse_optional_f64("evidence_latest_rtt_ms", value)?
                }
                "evidence_average_rtt_ms" => {
                    evidence.average_rtt_ms = parse_optional_f64("evidence_average_rtt_ms", value)?
                }
                "evidence_jitter_ms" => {
                    evidence.jitter_ms = parse_optional_f64("evidence_jitter_ms", value)?
                }
                "evidence_dns_state" => {
                    evidence.dns_state = (!value.is_empty()).then(|| value.to_string())
                }
                "evidence_gateway_state" => {
                    evidence.gateway_state = (!value.is_empty()).then(|| value.to_string())
                }
                _ => {}
            }
        }
        Ok(Some(Checkpoint {
            state: state.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "checkpoint state is missing")
            })?,
            episode_kind,
            incident_id,
            started_at_ms,
            config_fingerprint,
            evidence,
        }))
    }

    pub fn save(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = format!(
            "state={}\nepisode_kind={}\nincident_id={}\nstarted_at_ms={}\nconfig_fingerprint={}\nevidence_state={}\nevidence_packet_loss_pct={}\nevidence_latest_rtt_ms={}\nevidence_average_rtt_ms={}\nevidence_jitter_ms={}\nevidence_dns_state={}\nevidence_gateway_state={}\n",
            checkpoint.state,
            checkpoint.episode_kind.as_deref().unwrap_or(""),
            checkpoint.incident_id.as_deref().unwrap_or(""),
            checkpoint
                .started_at_ms
                .map(|value| value.to_string())
                .unwrap_or_default(),
            checkpoint.config_fingerprint,
            checkpoint.evidence.state,
            format_optional_f64(checkpoint.evidence.packet_loss_pct),
            format_optional_f64(checkpoint.evidence.latest_rtt_ms),
            format_optional_f64(checkpoint.evidence.average_rtt_ms),
            format_optional_f64(checkpoint.evidence.jitter_ms),
            checkpoint.evidence.dns_state.as_deref().unwrap_or(""),
            checkpoint.evidence.gateway_state.as_deref().unwrap_or("")
        );
        let temporary = self.path.with_extension("tmp");
        let mut file = fs::File::create(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        durable_replace(&temporary, &self.path)
    }
}

fn parse_optional_f64(name: &str, value: &str) -> io::Result<Option<f64>> {
    if value.is_empty() {
        return Ok(None);
    }
    value.parse::<f64>().map(Some).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("checkpoint {name} is invalid"),
        )
    })
}

fn format_optional_f64(value: Option<f64>) -> String {
    value
        .map(|number| format!("{number:.2}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{atomic_replace, durable_replace, Checkpoint, StateStore};
    use crate::incident::Evidence;
    use std::time::SystemTime;

    #[test]
    fn checkpoint_round_trips_without_external_dependencies() {
        let path = std::env::temp_dir().join(format!(
            "ping-monitor-checkpoint-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = StateStore::new(&path);
        let expected = Checkpoint {
            state: "down".into(),
            episode_kind: Some("outage".into()),
            incident_id: Some("incident-test".into()),
            started_at_ms: Some(1234),
            config_fingerprint: "test".into(),
            evidence: Evidence {
                state: "down".into(),
                packet_loss_pct: Some(100.0),
                ..Evidence::default()
            },
        };
        store.save(&expected).unwrap();
        let actual = store.load().unwrap().expect("checkpoint should load");
        assert_eq!(actual.state, expected.state);
        assert_eq!(actual.episode_kind, expected.episode_kind);
        assert_eq!(actual.incident_id, expected.incident_id);
        assert_eq!(actual.started_at_ms, expected.started_at_ms);
        assert_eq!(actual.config_fingerprint, expected.config_fingerprint);
        assert_eq!(actual.evidence.state, expected.evidence.state);
        assert_eq!(
            actual.evidence.packet_loss_pct,
            expected.evidence.packet_loss_pct
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_checkpoint_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "ping-monitor-bad-checkpoint-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "state=down\nstarted_at_ms=not-a-number\n").unwrap();
        let store = StateStore::new(&path);
        assert!(store.load().is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_replace_overwrites_an_existing_checkpoint() {
        let path = std::env::temp_dir().join(format!(
            "ping-monitor-replace-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let temporary = path.with_extension("tmp");
        std::fs::write(&path, "old").unwrap();
        std::fs::write(&temporary, "new").unwrap();

        atomic_replace(&temporary, &path).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn durable_replace_replaces_existing_file_and_consumes_temp_file() {
        let path = std::env::temp_dir().join(format!(
            "ping-monitor-durable-replace-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let temporary = path.with_extension("tmp");
        std::fs::write(&path, "old").unwrap();
        std::fs::write(&temporary, "new").unwrap();

        durable_replace(&temporary, &path).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!temporary.exists(), "temporary file must be consumed");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn save_replaces_existing_checkpoint_and_leaves_no_temp_file() {
        let path = std::env::temp_dir().join(format!(
            "ping-monitor-durable-save-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = StateStore::new(&path);
        let mut checkpoint = Checkpoint {
            state: "down".into(),
            episode_kind: Some("outage".into()),
            incident_id: Some("first".into()),
            started_at_ms: Some(1),
            config_fingerprint: "test".into(),
            evidence: Evidence::default(),
        };
        store.save(&checkpoint).unwrap();
        checkpoint.incident_id = Some("second".into());
        store.save(&checkpoint).unwrap();

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.incident_id.as_deref(), Some("second"));
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_file(path).unwrap();
    }
}
