use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Tui,
    Headless,
    CheckConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundBackend {
    Off,
    Audio,
    PcSpeaker,
    Both,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub mode: RunMode,
    pub sound: SoundBackend,
    pub log_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub auto_trace: bool,
    pub startup_grace: Duration,
    pub trace_timeout: Duration,
    pub trace_max_hops: u8,
    pub pc_speaker_device: Option<PathBuf>,
}

impl RuntimeConfig {
    pub fn from_values(
        args: &[&str],
        mode_env: Option<&str>,
        sound_env: Option<&str>,
    ) -> Result<Self, String> {
        if let Some(raw) = mode_env {
            if raw != "tui" && raw != "headless" {
                return Err("PM_MODE must be either tui or headless".into());
            }
        }
        let mode = if args.contains(&"--check-config") {
            RunMode::CheckConfig
        } else if args.contains(&"--headless") || mode_env == Some("headless") {
            RunMode::Headless
        } else {
            RunMode::Tui
        };
        let sound = match sound_env {
            Some(raw) => parse_sound(raw)?,
            None if mode == RunMode::Headless => SoundBackend::Off,
            None => SoundBackend::Audio,
        };
        let headless = matches!(mode, RunMode::Headless | RunMode::CheckConfig);
        Ok(Self {
            mode,
            sound,
            log_dir: headless.then(|| PathBuf::from("/var/log/ping-monitor")),
            state_dir: headless.then(|| PathBuf::from("/var/lib/ping-monitor")),
            auto_trace: headless,
            startup_grace: Duration::from_secs(30),
            trace_timeout: Duration::from_secs(25),
            trace_max_hops: 20,
            pc_speaker_device: None,
        })
    }

    pub fn from_process() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().collect();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut cfg = Self::from_values(
            &refs,
            std::env::var("PM_MODE").ok().as_deref(),
            std::env::var("PM_SOUND_BACKEND").ok().as_deref(),
        )?;
        if let Some(path) = std::env::var_os("PM_LOG_DIR") {
            cfg.log_dir = Some(PathBuf::from(path));
        }
        if let Some(path) = std::env::var_os("PM_STATE_DIR") {
            cfg.state_dir = Some(PathBuf::from(path));
        }
        if let Ok(raw) = std::env::var("PM_AUTO_TRACE") {
            cfg.auto_trace = parse_bool("PM_AUTO_TRACE", &raw)?;
        }
        cfg.startup_grace = parse_duration_env("PM_STARTUP_GRACE_S", cfg.startup_grace, 0, 3600)?;
        cfg.trace_timeout = parse_duration_env("PM_TRACE_TIMEOUT_S", cfg.trace_timeout, 1, 300)?;
        if let Ok(raw) = std::env::var("PM_TRACE_MAX_HOPS") {
            cfg.trace_max_hops = raw
                .parse::<u8>()
                .map_err(|_| "PM_TRACE_MAX_HOPS must be an integer".to_string())?;
            if cfg.trace_max_hops == 0 {
                return Err("PM_TRACE_MAX_HOPS must be greater than zero".into());
            }
        }
        cfg.pc_speaker_device = std::env::var_os("PM_PC_SPEAKER_DEVICE").map(PathBuf::from);
        Ok(cfg)
    }
}

fn parse_sound(raw: &str) -> Result<SoundBackend, String> {
    match raw {
        "off" => Ok(SoundBackend::Off),
        "audio" => Ok(SoundBackend::Audio),
        "pc-speaker" => Ok(SoundBackend::PcSpeaker),
        "both" => Ok(SoundBackend::Both),
        _ => Err(format!(
            "PM_SOUND_BACKEND must be one of: off, audio, pc-speaker, both (got {raw})"
        )),
    }
}

fn parse_bool(name: &str, raw: &str) -> Result<bool, String> {
    match raw {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(format!("{name} must be a boolean")),
    }
}

fn parse_duration_env(
    name: &str,
    default: Duration,
    min_secs: u64,
    max_secs: u64,
) -> Result<Duration, String> {
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let secs = raw
        .parse::<u64>()
        .map_err(|_| format!("{name} must be an integer number of seconds"))?;
    if !(min_secs..=max_secs).contains(&secs) {
        return Err(format!(
            "{name} must be between {min_secs} and {max_secs} seconds"
        ));
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headless_mode_without_desktop_dependencies() {
        let cfg = RuntimeConfig::from_values(&["ping_monitor", "--headless"], None, None)
            .expect("headless mode should parse");
        assert_eq!(cfg.mode, RunMode::Headless);
        assert_eq!(cfg.sound, SoundBackend::Off);
    }

    #[test]
    fn check_config_mode_is_explicit() {
        let cfg = RuntimeConfig::from_values(
            &["ping_monitor", "--check-config"],
            Some("headless"),
            Some("off"),
        )
        .expect("check-config should parse");
        assert_eq!(cfg.mode, RunMode::CheckConfig);
    }

    #[test]
    fn rejects_unknown_sound_backend() {
        let err = RuntimeConfig::from_values(&["ping_monitor"], None, Some("speaker"))
            .expect_err("unknown backend must fail validation");
        assert!(err.contains("PM_SOUND_BACKEND"));
    }

    #[test]
    fn rejects_unknown_run_mode() {
        let err = RuntimeConfig::from_values(&["ping_monitor"], Some("daemon"), None)
            .expect_err("unknown mode must fail validation");
        assert!(err.contains("PM_MODE"));
    }
}
