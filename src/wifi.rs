#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Command;

#[cfg(target_os = "macos")]
pub fn poll_rssi() -> Option<i16> {
    let out = Command::new("system_profiler")
        .arg("SPAirPortDataType")
        .output()
        .ok()?;
    parse_rssi(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
pub fn poll_rssi() -> Option<i16> {
    let s = std::fs::read_to_string("/proc/net/wireless").ok()?;
    parse_proc_net_wireless(&s)
}

#[cfg(target_os = "windows")]
pub fn poll_rssi() -> Option<i16> {
    let out = Command::new("netsh")
        .args(["wlan", "show", "interfaces"])
        .output()
        .ok()?;
    parse_netsh_quality(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn poll_rssi() -> Option<i16> {
    None
}

#[cfg(any(target_os = "macos", test))]
fn parse_rssi(stdout: &str) -> Option<i16> {
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if !lower.contains("rssi") && !lower.contains("signal") {
            continue;
        }
        for tok in line.split(|c: char| c.is_whitespace() || c == ':') {
            let t = tok.trim();
            if t.is_empty() || t.len() < 2 {
                continue;
            }
            if let Ok(v) = t.parse::<i16>() {
                if v < 0 {
                    return Some(v);
                }
            }
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_net_wireless(s: &str) -> Option<i16> {
    s.lines()
        .nth(2)
        .and_then(|l| l.split_whitespace().nth(2))
        .and_then(|t| t.trim_end_matches('.').parse::<i16>().ok())
        .filter(|&v| v < 0)
}

#[cfg(any(target_os = "windows", test))]
fn parse_netsh_quality(stdout: &str) -> Option<i16> {
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if !lower.contains("signal") {
            continue;
        }
        let pct = line
            .split(':')
            .nth(1)
            .and_then(|t| t.trim().trim_end_matches('%').parse::<i16>().ok())
            .filter(|&v| (0..=100).contains(&v))?;
        return Some((pct / 2) - 100);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rssi_label() {
        let s = "      Signal Strength (RSSI): -54\n";
        assert_eq!(parse_rssi(s), Some(-54));
    }

    #[test]
    fn parses_signal_strength_label() {
        let s = "    Signal Strength: -42 dBm\n";
        assert_eq!(parse_rssi(s), Some(-42));
    }

    #[test]
    fn parses_signal_noise_combo() {
        let s = "        Signal / Noise (dBm): -54 / -95\n";
        assert_eq!(parse_rssi(s), Some(-54));
    }

    #[test]
    fn returns_none_when_no_wifi() {
        let s = "      Status Information: spairport_status_off\n";
        assert_eq!(parse_rssi(s), None);
    }

    #[test]
    fn returns_none_on_empty() {
        assert_eq!(parse_rssi(""), None);
    }

    #[test]
    fn ignores_positive_numbers() {
        let s = "      Channel: 6 dBm gain 23\n      RSSI: -68\n";
        assert_eq!(parse_rssi(s), Some(-68));
    }

    #[test]
    fn parses_proc_net_wireless() {
        let s =
            "Inter-| sta-|   Quality        |   Discarded packets               | Missed | We\n";
        let s = format!("{s} face | tus | link level noise |  nwid  crypto   frag  retry   misc | beacon |  we\n  wlan0:  42. -68.  -256        0       0        0      0        0        0\n");
        assert_eq!(parse_proc_net_wireless(&s), Some(-68));
    }

    #[test]
    fn proc_net_wireless_empty() {
        assert_eq!(parse_proc_net_wireless(""), None);
        assert_eq!(parse_proc_net_wireless("header\nheader\n"), None);
    }

    #[test]
    fn parses_netsh_quality() {
        let s = "There is 1 interface on the system:\n\n    Name                   : Wi-Fi\n    Description            : Intel Wireless\n    Signal                 : 84%\n";
        assert_eq!(parse_netsh_quality(s), Some(-58));
    }

    #[test]
    fn netsh_no_signal_returns_none() {
        let s = "There is 1 interface on the System:\n    Name : Wi-Fi\n    State : disconnected\n";
        assert_eq!(parse_netsh_quality(s), None);
    }

    #[test]
    fn netsh_quality_zero_means_100() {
        let s = "    Signal                 : 100%\n";
        assert_eq!(parse_netsh_quality(s), Some(-50));
    }
}
