use crate::app::LinkState;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Up,
    Degraded,
    Down,
    Inconclusive,
}

impl DesiredState {
    pub fn to_link_state(self) -> LinkState {
        match self {
            DesiredState::Up => LinkState::Up,
            DesiredState::Degraded => LinkState::Degraded,
            DesiredState::Down => LinkState::Down,
            DesiredState::Inconclusive => unreachable!("Inconclusive has no LinkState"),
        }
    }
}

pub fn consensus(states: &[LinkState]) -> DesiredState {
    let n = states.len();
    if n == 0 {
        return DesiredState::Inconclusive;
    }
    let quorum = n / 2 + 1;
    let down = states.iter().filter(|s| **s == LinkState::Down).count();
    let degraded = states.iter().filter(|s| **s == LinkState::Degraded).count();
    let up = states.iter().filter(|s| **s == LinkState::Up).count();

    if down >= quorum {
        DesiredState::Down
    } else if down + degraded >= quorum {
        DesiredState::Degraded
    } else if up >= quorum {
        DesiredState::Up
    } else {
        DesiredState::Inconclusive
    }
}

/// Pure target state reducer.
///
/// Returns (new_state, new_pending_worse).
/// candidate = None means Missing (hold everything).
pub fn reduce_target(
    current: LinkState,
    candidate: Option<LinkState>,
    pending_worse: Option<(LinkState, u32)>,
    hysteresis_bad: u32,
) -> (LinkState, Option<(LinkState, u32)>) {
    let Some(c) = candidate else {
        return (current, pending_worse);
    };

    // Better candidate: immediate transition, clear pending.
    if is_better(c, current) {
        return (c, None);
    }

    // Equal candidate: clear pending, hold state.
    if c == current {
        return (current, None);
    }

    // Worse candidate: confirm or start.
    match pending_worse {
        Some((ps, count)) if ps == c => {
            let new_count = count + 1;
            if new_count >= hysteresis_bad {
                (c, None) // transition clears pending
            } else {
                (current, Some((c, new_count)))
            }
        }
        _ => (current, Some((c, 1))),
    }
}

/// Returns true if `a` is strictly better (higher severity order) than `b`.
fn is_better(a: LinkState, b: LinkState) -> bool {
    severity(a) < severity(b)
}

fn severity(s: LinkState) -> u8 {
    match s {
        LinkState::Up => 0,
        LinkState::Degraded => 1,
        LinkState::Down => 2,
    }
}

#[derive(Clone, Debug)]
pub struct RecoveryState {
    pub accumulated: Duration,
    pub resumed_at: Option<Instant>,
}

impl RecoveryState {
    pub fn new() -> Self {
        Self {
            accumulated: Duration::ZERO,
            resumed_at: None,
        }
    }

    pub fn resume(&mut self, now: Instant) {
        if self.resumed_at.is_none() {
            self.resumed_at = Some(now);
        }
    }

    pub fn pause(&mut self, now: Instant) {
        if let Some(started) = self.resumed_at.take() {
            self.accumulated += now.saturating_duration_since(started);
        }
    }

    pub fn reset(&mut self) {
        self.accumulated = Duration::ZERO;
        self.resumed_at = None;
    }

    pub fn total(&self, now: Instant) -> Duration {
        let active = self
            .resumed_at
            .map(|s| now.saturating_duration_since(s))
            .unwrap_or(Duration::ZERO);
        self.accumulated + active
    }

    #[allow(dead_code)]
    pub fn remaining(&self, now: Instant, dwell: Duration) -> Option<Duration> {
        let t = self.total(now);
        if t >= dwell {
            None
        } else {
            Some(dwell - t)
        }
    }
}

/// Pure connection state reducer.
///
/// Returns the new connection state. Mutates `recovery` in place.
pub fn reduce_connection(
    current: LinkState,
    desired: DesiredState,
    recovery: &mut RecoveryState,
    recover_dwell: Duration,
    now: Instant,
) -> LinkState {
    match current {
        LinkState::Up => {
            recovery.reset();
            match desired {
                DesiredState::Down | DesiredState::Degraded => desired.to_link_state(),
                DesiredState::Up | DesiredState::Inconclusive => LinkState::Up,
            }
        }
        LinkState::Degraded => match desired {
            DesiredState::Down => {
                recovery.reset();
                LinkState::Down
            }
            DesiredState::Up => {
                recovery.resume(now);
                if recovery.total(now) >= recover_dwell {
                    recovery.reset();
                    LinkState::Up
                } else {
                    LinkState::Degraded
                }
            }
            DesiredState::Degraded => {
                recovery.reset();
                LinkState::Degraded
            }
            DesiredState::Inconclusive => {
                recovery.pause(now);
                LinkState::Degraded
            }
        },
        LinkState::Down => match desired {
            DesiredState::Down => {
                recovery.reset();
                LinkState::Down
            }
            DesiredState::Degraded | DesiredState::Up => {
                recovery.reset();
                LinkState::Degraded
            }
            DesiredState::Inconclusive => {
                // Defensive no-op (timer cleared on Down entry).
                LinkState::Down
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── consensus ──────────────────────────────────────────────────────

    #[test]
    fn consensus_n1_up() {
        assert_eq!(consensus(&[LinkState::Up]), DesiredState::Up);
    }

    #[test]
    fn consensus_n1_down() {
        assert_eq!(consensus(&[LinkState::Down]), DesiredState::Down);
    }

    #[test]
    fn consensus_n1_degraded() {
        assert_eq!(consensus(&[LinkState::Degraded]), DesiredState::Degraded);
    }

    #[test]
    fn consensus_n2_up_up() {
        assert_eq!(consensus(&[LinkState::Up, LinkState::Up]), DesiredState::Up);
    }

    #[test]
    fn consensus_n2_down_up() {
        assert_eq!(
            consensus(&[LinkState::Down, LinkState::Up]),
            DesiredState::Inconclusive
        );
    }

    #[test]
    fn consensus_n2_down_degraded() {
        assert_eq!(
            consensus(&[LinkState::Down, LinkState::Degraded]),
            DesiredState::Degraded
        );
    }

    #[test]
    fn consensus_n2_degraded_degraded() {
        assert_eq!(
            consensus(&[LinkState::Degraded, LinkState::Degraded]),
            DesiredState::Degraded
        );
    }

    #[test]
    fn consensus_n3_two_down() {
        assert_eq!(
            consensus(&[LinkState::Down, LinkState::Down, LinkState::Up]),
            DesiredState::Down
        );
    }

    #[test]
    fn consensus_n3_two_degraded() {
        assert_eq!(
            consensus(&[LinkState::Degraded, LinkState::Degraded, LinkState::Up]),
            DesiredState::Degraded
        );
    }

    #[test]
    fn consensus_n3_two_up() {
        assert_eq!(
            consensus(&[LinkState::Down, LinkState::Up, LinkState::Up]),
            DesiredState::Up
        );
    }

    #[test]
    fn consensus_n4_2down_2degraded() {
        assert_eq!(
            consensus(&[
                LinkState::Down,
                LinkState::Down,
                LinkState::Degraded,
                LinkState::Degraded
            ]),
            DesiredState::Degraded
        );
    }

    #[test]
    fn consensus_n4_2down_2up() {
        assert_eq!(
            consensus(&[
                LinkState::Down,
                LinkState::Down,
                LinkState::Up,
                LinkState::Up
            ]),
            DesiredState::Inconclusive
        );
    }

    #[test]
    fn consensus_n4_3down() {
        assert_eq!(
            consensus(&[
                LinkState::Down,
                LinkState::Down,
                LinkState::Down,
                LinkState::Up
            ]),
            DesiredState::Down
        );
    }

    #[test]
    fn consensus_n5_all_down() {
        assert_eq!(
            consensus(&[
                LinkState::Down,
                LinkState::Down,
                LinkState::Down,
                LinkState::Down,
                LinkState::Down
            ]),
            DesiredState::Down
        );
    }

    #[test]
    fn consensus_empty() {
        assert_eq!(consensus(&[]), DesiredState::Inconclusive);
    }

    // ── target reducer ─────────────────────────────────────────────────

    #[test]
    fn target_missing_holds() {
        let (s, p) = reduce_target(LinkState::Up, None, None, 3);
        assert_eq!(s, LinkState::Up);
        assert!(p.is_none());
    }

    #[test]
    fn target_up_to_up_clears_pending() {
        let (s, p) = reduce_target(
            LinkState::Up,
            Some(LinkState::Up),
            Some((LinkState::Degraded, 2)),
            3,
        );
        assert_eq!(s, LinkState::Up);
        assert!(p.is_none());
    }

    #[test]
    fn target_up_to_degraded_starts_pending() {
        let (s, p) = reduce_target(LinkState::Up, Some(LinkState::Degraded), None, 3);
        assert_eq!(s, LinkState::Up);
        assert_eq!(p, Some((LinkState::Degraded, 1)));
    }

    #[test]
    fn target_up_to_degraded_confirms_at_hysteresis() {
        let (s, p) = reduce_target(
            LinkState::Up,
            Some(LinkState::Degraded),
            Some((LinkState::Degraded, 2)),
            3,
        );
        assert_eq!(s, LinkState::Degraded);
        assert!(p.is_none());
    }

    #[test]
    fn target_up_to_down_replaces_pending() {
        let (s, p) = reduce_target(
            LinkState::Up,
            Some(LinkState::Down),
            Some((LinkState::Degraded, 1)),
            3,
        );
        assert_eq!(s, LinkState::Up);
        assert_eq!(p, Some((LinkState::Down, 1)));
    }

    #[test]
    fn target_degraded_to_up_immediate() {
        let (s, p) = reduce_target(
            LinkState::Degraded,
            Some(LinkState::Up),
            Some((LinkState::Down, 2)),
            3,
        );
        assert_eq!(s, LinkState::Up);
        assert!(p.is_none());
    }

    #[test]
    fn target_degraded_to_degraded_clears() {
        let (s, p) = reduce_target(
            LinkState::Degraded,
            Some(LinkState::Degraded),
            Some((LinkState::Down, 1)),
            3,
        );
        assert_eq!(s, LinkState::Degraded);
        assert!(p.is_none());
    }

    #[test]
    fn target_degraded_to_down_confirms() {
        let (s, p) = reduce_target(
            LinkState::Degraded,
            Some(LinkState::Down),
            Some((LinkState::Down, 2)),
            3,
        );
        assert_eq!(s, LinkState::Down);
        assert!(p.is_none());
    }

    #[test]
    fn target_down_to_up_immediate() {
        let (s, p) = reduce_target(LinkState::Down, Some(LinkState::Up), None, 3);
        assert_eq!(s, LinkState::Up);
        assert!(p.is_none());
    }

    #[test]
    fn target_down_to_degraded_immediate() {
        let (s, p) = reduce_target(LinkState::Down, Some(LinkState::Degraded), None, 3);
        assert_eq!(s, LinkState::Degraded);
        assert!(p.is_none());
    }

    #[test]
    fn target_bad_bad_good_no_confirm() {
        let h = 3;
        let (s, p) = reduce_target(LinkState::Up, Some(LinkState::Degraded), None, h);
        assert_eq!(s, LinkState::Up);
        let (s, p) = reduce_target(s, Some(LinkState::Degraded), p, h);
        assert_eq!(s, LinkState::Up);
        let (s, p) = reduce_target(s, Some(LinkState::Up), p, h);
        assert_eq!(s, LinkState::Up);
        assert!(p.is_none());
    }

    // ── connection reducer ─────────────────────────────────────────────

    #[test]
    fn conn_up_to_down_immediate() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let s = reduce_connection(
            LinkState::Up,
            DesiredState::Down,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Down);
    }

    #[test]
    fn conn_up_to_degraded_immediate() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let s = reduce_connection(
            LinkState::Up,
            DesiredState::Degraded,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Degraded);
    }

    #[test]
    fn conn_up_to_inconclusive_holds() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let s = reduce_connection(
            LinkState::Up,
            DesiredState::Inconclusive,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Up);
    }

    #[test]
    fn conn_degraded_to_down() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Down,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Down);
    }

    #[test]
    fn conn_degraded_up_recovery_dwell() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        // Start recovery
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Degraded);
        // Not enough time yet
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(10),
        );
        assert_eq!(s, LinkState::Degraded);
        // Enough time
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(16),
        );
        assert_eq!(s, LinkState::Up);
    }

    #[test]
    fn conn_degraded_equal_cancels_recovery() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        // Equal cancels
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Degraded,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(5),
        );
        // Should not complete recovery even with enough total time
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(20),
        );
        // Since recovery was cancelled and restarted, need another 15s
        assert_eq!(s, LinkState::Degraded);
    }

    #[test]
    fn conn_degraded_inconclusive_pauses() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        // Start recovery
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        // Accumulate 10s
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(10),
        );
        assert_eq!(
            r.total(now + Duration::from_secs(10)),
            Duration::from_secs(10)
        );
        // Pause
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Inconclusive,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(10),
        );
        // Wait 60s — accumulated should NOT advance
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Inconclusive,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(70),
        );
        assert_eq!(
            r.total(now + Duration::from_secs(70)),
            Duration::from_secs(10),
            "paused recovery should not advance"
        );
        // Resume
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(70),
        );
        assert_eq!(s, LinkState::Degraded);
        // 6 more seconds → total 16s >= 15s
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(76),
        );
        assert_eq!(s, LinkState::Up, "true pause: 10s + 6s = 16s >= 15s");
    }

    #[test]
    fn conn_down_to_degraded_immediate() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let s = reduce_connection(
            LinkState::Down,
            DesiredState::Degraded,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Degraded);
    }

    #[test]
    fn conn_down_to_up_goes_to_degraded() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let s = reduce_connection(
            LinkState::Down,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Degraded, "Down+Up desired → Degraded");
    }

    #[test]
    fn conn_down_inconclusive_holds() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        let s = reduce_connection(
            LinkState::Down,
            DesiredState::Inconclusive,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        assert_eq!(s, LinkState::Down);
    }

    #[test]
    fn conn_minority_blip_doesnt_reset_recovery() {
        let mut r = RecoveryState::new();
        let now = Instant::now();
        // Degraded, recovering toward Up
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now,
        );
        // Accumulate 10s
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(10),
        );
        // Inconclusive (minority blip) — pauses, doesn't cancel
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Inconclusive,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(10),
        );
        // Back to Up — resume
        let _ = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(12),
        );
        // 10s accumulated + 1s active = 11s < 15s — still Degraded
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(13),
        );
        assert_eq!(s, LinkState::Degraded);
        // 10s accumulated + 5s active = 15s → Up
        let s = reduce_connection(
            LinkState::Degraded,
            DesiredState::Up,
            &mut r,
            Duration::from_secs(15),
            now + Duration::from_secs(17),
        );
        assert_eq!(s, LinkState::Up);
    }
}
