//! Default gateway auto-detection (best-effort, stdlib + platform tools only).

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
pub fn default_gateway() -> Option<String> {
    let out = Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    parse_route_get(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
pub fn default_gateway() -> Option<String> {
    let s = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_proc_net_route(&s)
}

#[cfg(target_os = "windows")]
pub fn default_gateway() -> Option<String> {
    let out = std::process::Command::new("route")
        .args(["print", "-4"])
        .output()
        .ok()?;
    parse_route_print(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn default_gateway() -> Option<String> {
    None
}

fn valid_gw(tok: &str) -> Option<String> {
    match tok.parse::<std::net::Ipv4Addr>() {
        Ok(ip) if !ip.is_unspecified() => Some(ip.to_string()),
        _ => None,
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_route_get(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("gateway:") {
            if let Some(gw) = parts.next().and_then(valid_gw) {
                return Some(gw);
            }
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_net_route(s: &str) -> Option<String> {
    for line in s.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f[1] != "00000000" {
            continue;
        }
        let raw = u32::from_str_radix(f[2], 16).ok()?;
        let b = raw.to_le_bytes();
        let ip = std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]);
        if !ip.is_unspecified() {
            return Some(ip.to_string());
        }
    }
    None
}

#[cfg(any(target_os = "windows", test))]
fn parse_route_print(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 3 && f[0] == "0.0.0.0" && f[1] == "0.0.0.0" {
            if let Some(gw) = valid_gw(f[2]) {
                return Some(gw);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_route_get() {
        let s = "   route to: default\ndestination: default\n       mask: default\n    gateway: 192.168.1.1\n  interface: en0\n";
        assert_eq!(parse_route_get(s), Some("192.168.1.1".to_string()));
    }

    #[test]
    fn route_get_without_gateway_returns_none() {
        let s = "   route to: 10.0.0.0\n    gateway: link#4\n";
        assert_eq!(parse_route_get(s), None);
        assert_eq!(parse_route_get(""), None);
    }

    #[test]
    fn parses_proc_net_route() {
        let s =
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                 eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
                 eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_proc_net_route(s), Some("192.168.1.1".to_string()));
    }

    #[test]
    fn proc_net_route_without_default_returns_none() {
        let s = "Iface\tDestination\tGateway\tFlags\n\
                 eth0\t0001A8C0\t00000000\t0001\n";
        assert_eq!(parse_proc_net_route(s), None);
        assert_eq!(parse_proc_net_route(""), None);
    }

    #[test]
    fn parses_windows_route_print() {
        let s = "IPv4 Route Table\n\
                 ===========================================================================\n\
                 Active Routes:\n\
                 Network Destination        Netmask          Gateway       Interface  Metric\n\
                          0.0.0.0          0.0.0.0      192.168.1.1     192.168.1.50     25\n\
                        127.0.0.0        255.0.0.0         On-link         127.0.0.1    331\n";
        assert_eq!(parse_route_print(s), Some("192.168.1.1".to_string()));
    }

    #[test]
    fn route_print_on_link_gateway_returns_none() {
        let s = "          0.0.0.0          0.0.0.0         On-link      192.168.1.50    25\n";
        assert_eq!(parse_route_print(s), None);
        assert_eq!(parse_route_print(""), None);
    }
}
