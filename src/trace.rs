use std::io;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceClassification {
    FailureAfterGateway,
    TargetReached,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceHop {
    pub ttl: u8,
    pub address: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TraceSummary {
    pub hops: Vec<TraceHop>,
    pub last_responsive_hop: Option<String>,
    pub failure_boundary: Option<u8>,
    pub reached_target: bool,
    pub classification: TraceClassification,
    pub raw_output: String,
}

pub struct TraceRequest {
    pub target: String,
    pub gateway: Option<String>,
    pub max_hops: u8,
    pub timeout: Duration,
}

pub fn parse_trace(output: &str, target: &str, gateway: Option<&str>) -> TraceSummary {
    let mut hops = Vec::new();
    let mut target_seen_on_hop = false;
    for line in output.lines() {
        let mut tokens = line.split_whitespace();
        let Some(ttl) = tokens
            .next()
            .and_then(|token| token.trim_end_matches([':', '?']).parse::<u8>().ok())
        else {
            continue;
        };
        let target_seen = line
            .split_whitespace()
            .any(|token| token.trim_matches(|c| c == '(' || c == ')') == target);
        target_seen_on_hop |= target_seen;
        let address = tokens
            .filter_map(|token| {
                token
                    .trim_matches(|c| c == '(' || c == ')')
                    .parse::<IpAddr>()
                    .ok()
            })
            .map(|ip| ip.to_string())
            .next();
        hops.push(TraceHop { ttl, address });
    }

    let reached_target = hops
        .iter()
        .filter_map(|hop| hop.address.as_deref())
        .any(|address| address == target)
        || target_seen_on_hop;
    let last_responsive_hop = hops.iter().rev().find_map(|hop| hop.address.clone());
    let gateway_responded = gateway
        .map(|expected| {
            hops.iter()
                .any(|hop| hop.address.as_deref() == Some(expected))
        })
        .unwrap_or_else(|| hops.first().is_some_and(|hop| hop.address.is_some()));

    let all_hops_timed_out = !hops.is_empty() && hops.iter().all(|hop| hop.address.is_none());
    let failure_boundary = if all_hops_timed_out {
        None
    } else {
        trailing_timeout_boundary(&hops)
    };
    let classification = if reached_target {
        TraceClassification::TargetReached
    } else if all_hops_timed_out {
        TraceClassification::Indeterminate
    } else if gateway_responded && failure_boundary.is_some() {
        TraceClassification::FailureAfterGateway
    } else {
        TraceClassification::Indeterminate
    };

    TraceSummary {
        hops,
        last_responsive_hop,
        failure_boundary,
        reached_target,
        classification,
        raw_output: output.to_string(),
    }
}

fn trailing_timeout_boundary(hops: &[TraceHop]) -> Option<u8> {
    let last_response = hops.iter().rposition(|hop| hop.address.is_some())?;
    let boundary = last_response + 1;
    if boundary < hops.len() && hops[boundary..].iter().all(|hop| hop.address.is_none()) {
        Some(hops[boundary].ttl)
    } else {
        None
    }
}

fn summarize_trace_process(
    success: bool,
    stdout: &str,
    stderr: &str,
    target: &str,
    gateway: Option<&str>,
) -> TraceSummary {
    let mut summary = parse_trace(stdout, target, gateway);
    let mut raw_output = stdout.to_string();
    if !stderr.trim().is_empty() {
        if !raw_output.is_empty() {
            raw_output.push('\n');
        }
        raw_output.push_str("[stderr]\n");
        raw_output.push_str(stderr);
    }
    summary.raw_output = raw_output;
    if !success || stdout.trim().is_empty() {
        summary.classification = TraceClassification::Indeterminate;
        summary.failure_boundary = None;
    }
    summary
}

fn trace_deadline(started: Instant, timeout: Duration) -> Instant {
    started.checked_add(timeout).unwrap_or(started)
}

fn trace_budget_remaining(deadline: Instant, now: Instant) -> Duration {
    deadline.checked_duration_since(now).unwrap_or_default()
}

fn validate_trace_target(target: &str) -> io::Result<()> {
    if target.is_empty()
        || target.starts_with('-')
        || target
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trace target must not be empty, option-like, or contain whitespace/control characters",
        ));
    }
    Ok(())
}

pub fn run_trace(request: &TraceRequest) -> io::Result<TraceSummary> {
    validate_trace_target(&request.target)?;
    let max_hops = request.max_hops.to_string();
    let numeric_target = request.target.parse::<IpAddr>().is_ok();
    let mut candidates = Vec::new();
    #[cfg(target_os = "windows")]
    {
        let mut args = vec![
            "-h",
            max_hops.as_str(),
            "-w",
            "1000",
            request.target.as_str(),
        ];
        if numeric_target {
            args.insert(0, "-d");
        }
        candidates.push(("tracert", args));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut tracepath_args = vec!["-m", max_hops.as_str(), request.target.as_str()];
        let mut traceroute_args = vec![
            "-q",
            "1",
            "-w",
            "1",
            "-m",
            max_hops.as_str(),
            request.target.as_str(),
        ];
        if numeric_target {
            tracepath_args.insert(0, "-n");
            traceroute_args.insert(0, "-n");
        }
        candidates.push(("tracepath", tracepath_args));
        candidates.push(("traceroute", traceroute_args));
    }
    let deadline = trace_deadline(Instant::now(), request.timeout);
    let mut last_error = None;
    'candidate: for (program, args) in candidates {
        if trace_budget_remaining(deadline, Instant::now()).is_zero() {
            break;
        }
        let mut child = match Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        loop {
            let now = Instant::now();
            let remaining = trace_budget_remaining(deadline, now);
            if remaining.is_zero() {
                let _ = child.kill();
                let output = child.wait_with_output()?;
                return Ok(summarize_trace_process(
                    false,
                    &String::from_utf8_lossy(&output.stdout),
                    &String::from_utf8_lossy(&output.stderr),
                    &request.target,
                    request.gateway.as_deref(),
                ));
            }
            if child.try_wait()?.is_some() {
                let output = child.wait_with_output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !output.status.success() {
                    let detail = if stderr.trim().is_empty() {
                        format!("{program} exited with status {}", output.status)
                    } else {
                        format!(
                            "{program} exited with status {}: {}",
                            output.status,
                            stderr.trim()
                        )
                    };
                    last_error = Some(io::Error::other(detail));
                    continue 'candidate;
                }
                return Ok(summarize_trace_process(
                    true,
                    &stdout,
                    &stderr,
                    &request.target,
                    request.gateway.as_deref(),
                ));
            }
            thread::sleep(Duration::from_millis(25).min(remaining));
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no trace utility available")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_completed_route() {
        let output = " 1 192.0.2.1 1.2 ms\n 2 203.0.113.7 4.5 ms\n 3 198.51.100.8 8.1 ms\n";
        let result = parse_trace(output, "198.51.100.8", Some("192.0.2.1"));
        assert_eq!(result.last_responsive_hop.as_deref(), Some("198.51.100.8"));
        assert!(result.reached_target);
        assert_eq!(result.classification, TraceClassification::TargetReached);
    }

    #[test]
    fn identifies_unreachable_gateway() {
        let output = " 1  * * *\n 2  * * *\n";
        let result = parse_trace(output, "198.51.100.8", Some("192.0.2.1"));
        assert_eq!(result.classification, TraceClassification::Indeterminate);
        assert_eq!(result.failure_boundary, None);
    }

    #[test]
    fn identifies_trailing_timeout_after_last_response() {
        let output = " 1 192.0.2.1 1.0 ms\n 2 203.0.113.7 4.0 ms\n 3  * * *\n 4  * * *\n";
        let result = parse_trace(output, "198.51.100.8", Some("192.0.2.1"));
        assert_eq!(
            result.classification,
            TraceClassification::FailureAfterGateway
        );
        assert_eq!(result.last_responsive_hop.as_deref(), Some("203.0.113.7"));
        assert_eq!(result.failure_boundary, Some(3));
    }

    #[test]
    fn isolated_timeout_is_not_a_failure_boundary() {
        let output = " 1 192.0.2.1 1.0 ms\n 2  * * *\n 3 203.0.113.7 4.0 ms\n";
        let result = parse_trace(output, "198.51.100.8", Some("192.0.2.1"));
        assert_eq!(result.classification, TraceClassification::Indeterminate);
        assert_eq!(result.failure_boundary, None);
    }

    #[test]
    fn parses_windows_tracert_format() {
        let output = "  1    <1 ms    <1 ms    <1 ms  192.0.2.1\n  2     4 ms     4 ms     4 ms  198.51.100.8\n";
        let result = parse_trace(output, "198.51.100.8", Some("192.0.2.1"));
        assert!(result.reached_target);
        assert_eq!(result.classification, TraceClassification::TargetReached);
    }

    #[test]
    fn recognizes_a_hostname_in_a_trace_hop() {
        let output = " 1 192.0.2.1 1.0 ms\n 2 example.test (198.51.100.8) 4.0 ms\n";
        let result = parse_trace(output, "example.test", Some("192.0.2.1"));
        assert!(result.reached_target);
        assert_eq!(result.classification, TraceClassification::TargetReached);
    }

    #[test]
    fn parses_tracepath_ttl_tokens_with_punctuation() {
        let output = " 1?: [LOCALHOST] pmtu 1500\n 1: 192.0.2.1 0.4ms\n 2: 198.51.100.8 4.0ms\n";
        let result = parse_trace(output, "198.51.100.8", Some("192.0.2.1"));
        assert!(result.reached_target);
        assert_eq!(result.last_responsive_hop.as_deref(), Some("198.51.100.8"));
    }

    #[test]
    fn failed_trace_process_is_indeterminate_and_preserves_diagnostics() {
        let result = summarize_trace_process(
            false,
            "",
            "permission denied",
            "198.51.100.8",
            Some("192.0.2.1"),
        );
        assert_eq!(result.classification, TraceClassification::Indeterminate);
        assert!(result.raw_output.contains("permission denied"));
    }

    #[test]
    fn trace_budget_is_shared_across_candidates() {
        let started = Instant::now();
        let deadline = trace_deadline(started, Duration::from_secs(5));

        assert_eq!(
            trace_budget_remaining(deadline, started + Duration::from_secs(2)),
            Duration::from_secs(3)
        );
        assert_eq!(
            trace_budget_remaining(deadline, started + Duration::from_secs(6)),
            Duration::ZERO
        );
    }

    #[test]
    fn trace_target_validation_rejects_option_like_and_embedded_whitespace_hosts() {
        for target in ["-n", "example .test", "example\t.test", "example\n.test"] {
            assert!(
                validate_trace_target(target).is_err(),
                "target should be rejected: {target:?}"
            );
        }
        assert!(validate_trace_target("example.test").is_ok());
        assert!(validate_trace_target("2001:db8::1").is_ok());
    }
}
