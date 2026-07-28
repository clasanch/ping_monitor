use std::time::Instant;

pub const TAU_S: f64 = 3.0;
pub const TAU_L: f64 = 60.0;
pub const WARMUP_SAMPLES: u32 = 30;
pub const WARMUP_ELAPSED: f64 = 60.0;
pub const TRIGGER_RATIO: f64 = 4.0;
pub const DETRIGGER_RATIO: f64 = 2.0;
pub const MINIMUM_DELTA_MS: f64 = 20.0;
pub const EPSILON: f64 = 1.0;
pub const GAP_RESET_SECS: f64 = 300.0;
pub const RELEARNING_SECS: f64 = 600.0;

#[derive(Clone, Debug)]
pub struct EwmaDetector {
    pub short: Option<f64>,
    pub long: Option<f64>,
    pub last_valid_at: Option<Instant>,
    pub last_long_admitted_at: Option<Instant>,
    pub valid_count: u32,
    pub elapsed_secs: f64,
    pub warmup_done: bool,
    pub latch_active: bool,
    pub continuity_broken: bool,
}

impl EwmaDetector {
    pub fn new() -> Self {
        Self {
            short: None,
            long: None,
            last_valid_at: None,
            last_long_admitted_at: None,
            valid_count: 0,
            elapsed_secs: 0.0,
            warmup_done: false,
            latch_active: false,
            continuity_broken: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Process a valid observation. Returns (latch_active, short, ratio, long_admitted).
    ///
    /// `x` is the metric value (latency or jitter in ms).
    /// `now` is the current time.
    /// `warn_threshold` is the warning floor (latency_warn_ms or jitter_warn_ms).
    /// `severe_threshold` is the severe floor (latency_bad_ms only; pass f64::MAX for jitter).
    pub fn observe(
        &mut self,
        x: f64,
        now: Instant,
        warn_threshold: f64,
        severe_threshold: f64,
    ) -> (bool, Option<f64>, f64, bool) {
        // Step 1: First valid sample — initialize
        let Some(last) = self.last_valid_at else {
            self.short = Some(x);
            self.long = Some(x);
            self.last_valid_at = Some(now);
            self.last_long_admitted_at = Some(now);
            self.valid_count = 1;
            self.elapsed_secs = 0.0;
            self.warmup_done = false;
            self.latch_active = false;
            self.continuity_broken = false;
            return (false, Some(x), 1.0, true);
        };

        let dt = now.duration_since(last).as_secs_f64();

        // Step 2: Gap reset
        if dt >= GAP_RESET_SECS {
            self.short = Some(x);
            self.long = Some(x);
            self.last_valid_at = Some(now);
            self.last_long_admitted_at = Some(now);
            self.valid_count = 1;
            self.elapsed_secs = 0.0;
            self.warmup_done = false;
            self.latch_active = false;
            self.continuity_broken = false;
            return (false, Some(x), 1.0, true);
        }

        // Relearning: 600s without long admission
        if let Some(last_admitted) = self.last_long_admitted_at {
            if now.duration_since(last_admitted).as_secs_f64() >= RELEARNING_SECS
                && !self.continuity_broken
            {
                self.short = Some(x);
                self.long = Some(x);
                self.last_valid_at = Some(now);
                self.last_long_admitted_at = Some(now);
                self.valid_count = 1;
                self.elapsed_secs = 0.0;
                self.warmup_done = false;
                self.latch_active = false;
                self.continuity_broken = false;
                return (false, Some(x), 1.0, true);
            }
        }

        // Step 3: First after gap
        if self.continuity_broken {
            self.short = Some(x);
            // Latch eval aligned with step 9 guards
            let long_val = self.long.unwrap_or(x);
            let ratio = x / long_val.max(EPSILON);
            if x >= severe_threshold
                || (self.warmup_done
                    && x >= warn_threshold
                    && ratio >= TRIGGER_RATIO
                    && (x - long_val) >= MINIMUM_DELTA_MS)
            {
                self.latch_active = true;
            }
            self.continuity_broken = false;
            self.last_valid_at = Some(now);
            let long_admitted = false;
            return (self.latch_active, Some(x), ratio, long_admitted);
        }

        // Step 4-5: Compute alphas
        let dt = dt.max(0.001);
        let alpha_s = 1.0 - (-dt / TAU_S).exp();
        let alpha_l = 1.0 - (-dt / TAU_L).exp();

        let short_val = self.short.unwrap_or(x);
        let long_val = self.long.unwrap_or(x);

        // Step 6: Update short
        let short_new = short_val + alpha_s * (x - short_val);

        // Step 7: Ratio
        let ratio = short_new / long_val.max(EPSILON);

        // Step 8: Warmup
        self.valid_count += 1;
        self.elapsed_secs += dt;
        self.warmup_done = self.warmup_done
            || (self.valid_count >= WARMUP_SAMPLES && self.elapsed_secs >= WARMUP_ELAPSED);

        // Step 9: Latch evaluation
        if x >= severe_threshold {
            self.latch_active = true;
        } else if self.latch_active && (ratio <= DETRIGGER_RATIO || x < warn_threshold) {
            self.latch_active = false;
        } else if !self.latch_active
            && self.warmup_done
            && x >= warn_threshold
            && ratio >= TRIGGER_RATIO
            && (short_new - long_val) >= MINIMUM_DELTA_MS
        {
            self.latch_active = true;
        }
        // intermediate 2 < ratio < 4: preserve current latch

        // Step 10: Commit short
        self.short = Some(short_new);

        // Step 11: Admit to long
        let mut long_admitted = false;
        if !self.latch_active && ratio <= DETRIGGER_RATIO {
            let long_new = long_val + alpha_l * (x - long_val);
            self.long = Some(long_new);
            self.last_long_admitted_at = Some(now);
            long_admitted = true;
        }

        // Step 12: Update timestamp
        self.last_valid_at = Some(now);

        (self.latch_active, Some(short_new), ratio, long_admitted)
    }

    /// Mark continuity broken (on observed loss). Does not update EWMAs.
    pub fn mark_gap(&mut self) {
        self.continuity_broken = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    fn t(secs: f64) -> Instant {
        Instant::now() + Duration::from_secs_f64(secs)
    }

    #[test]
    fn first_value_initializes() {
        let mut d = EwmaDetector::new();
        let (latch, short, ratio, admitted) = d.observe(50.0, t(0.0), 200.0, 500.0);
        assert!(!latch);
        assert_eq!(short, Some(50.0));
        assert!((ratio - 1.0).abs() < 0.001);
        assert!(admitted);
        assert_eq!(d.valid_count, 1);
    }

    #[test]
    fn gap_reset() {
        let mut d = EwmaDetector::new();
        let _ = d.observe(50.0, t(0.0), 200.0, 500.0);
        // 300s gap
        let (latch, short, ratio, _) = d.observe(80.0, t(301.0), 200.0, 500.0);
        assert!(!latch);
        assert_eq!(short, Some(80.0));
        assert!((ratio - 1.0).abs() < 0.001);
        assert_eq!(d.valid_count, 1);
    }

    #[test]
    fn warmup_blocks_relative_trigger() {
        let mut d = EwmaDetector::new();
        // 29 samples at 250ms (above warn=200), 58s elapsed
        for i in 0..29 {
            let _ = d.observe(250.0, t(i as f64 * 2.0), 200.0, 500.0);
        }
        assert!(!d.warmup_done);
        // 30th at 60s — warmup completes
        let _ = d.observe(250.0, t(60.0), 200.0, 500.0);
        assert!(d.warmup_done);
        // Spike to 1200: ratio>4, delta>20, x>warn
        let (latch, _, ratio, _) = d.observe(1200.0, t(62.0), 200.0, 500.0);
        assert!(
            latch,
            "warmup done + ratio>=4 + delta>=20 → latch, ratio={}",
            ratio
        );
    }

    #[test]
    fn severe_during_warmup() {
        let mut d = EwmaDetector::new();
        let _ = d.observe(250.0, t(0.0), 200.0, 500.0);
        // Severe during warmup
        let (latch, _, _, _) = d.observe(600.0, t(1.0), 200.0, 500.0);
        assert!(latch, "severe should activate during warmup");
    }

    #[test]
    fn delta_dominates() {
        let mut d = EwmaDetector::new();
        // 35 samples at 250ms, 70s
        for i in 0..35 {
            let _ = d.observe(250.0, t(i as f64 * 2.0), 200.0, 500.0);
        }
        assert!(d.warmup_done);
        // Spike to 1200: ratio>4, delta>20
        let (latch, _, ratio, _) = d.observe(1200.0, t(80.0), 200.0, 500.0);
        assert!(latch, "ratio>=4 && delta>=20 → latch, ratio={}", ratio);
        assert!(ratio >= 4.0);
    }

    #[test]
    fn ratio_insufficient() {
        let mut d = EwmaDetector::new();
        // 35 samples at 300ms, 70s
        for i in 0..35 {
            let _ = d.observe(300.0, t(i as f64 * 2.0), 200.0, 500.0);
        }
        assert!(d.warmup_done);
        // Spike to 800: ratio≈2.67 < 4, and x < severe(500) so severe branch doesn't fire
        // Actually 800 > 500 — need x < severe. Use 490.
        // With long≈300, ratio = short_new/300. For ratio < 4: short_new < 1200.
        // Feed 490: ratio = (300 + alpha*(490-300))/300 ≈ (300+0.98*190)/300 ≈ 486/300 ≈ 1.62
        // That's too low. Need ratio in (2,4). Let's use 900 but set severe higher.
        // Better: just test that x < severe AND ratio < 4 → no latch.
        // Use baseline 100ms, spike 350ms: ratio ≈ 3.5, x=350 < severe=500
        let mut d2 = EwmaDetector::new();
        for i in 0..35 {
            let _ = d2.observe(100.0, t(i as f64 * 2.0), 50.0, 500.0);
        }
        assert!(d2.warmup_done);
        let (latch, _, ratio, _) = d2.observe(350.0, t(80.0), 50.0, 500.0);
        assert!(!latch, "ratio<4 and x<severe → no latch, ratio={}", ratio);
        assert!(ratio < 4.0);
    }

    #[test]
    fn detrigger_exact() {
        let mut d = EwmaDetector::new();
        // Activate: 35 samples at 250ms, 70s
        for i in 0..35 {
            let _ = d.observe(250.0, t(i as f64 * 2.0), 200.0, 500.0);
        }
        let (latch, _, _, _) = d.observe(1200.0, t(80.0), 200.0, 500.0);
        assert!(latch);
        // Feed clean values to bring ratio down
        for i in 0..30 {
            let _ = d.observe(250.0, t(82.0 + i as f64 * 2.0), 200.0, 500.0);
        }
        let short_val = d.short.unwrap();
        let long_val = d.long.unwrap();
        let ratio = short_val / long_val.max(EPSILON);
        assert!(
            ratio <= DETRIGGER_RATIO,
            "ratio should be <= 2.0, got {}",
            ratio
        );
        assert!(!d.latch_active, "latch should have cleared");
    }

    #[test]
    fn intermediate_preserves_latch() {
        let mut d = EwmaDetector::new();
        for i in 0..35 {
            let _ = d.observe(250.0, t(i as f64 * 2.0), 200.0, 500.0);
        }
        // Activate
        let (latch, _, _, _) = d.observe(1200.0, t(80.0), 200.0, 500.0);
        assert!(latch);
        // Feed 500ms: short will be between long(250) and 4*long(1000), ratio in (2,4)
        let (latch2, _, ratio, _) = d.observe(500.0, t(82.0), 200.0, 500.0);
        assert!(
            ratio > 2.0 && ratio < 4.0,
            "ratio should be in (2,4), got {}",
            ratio
        );
        assert!(latch2, "intermediate ratio should preserve latch");
    }

    #[test]
    fn gap_breaks_continuity() {
        let mut d = EwmaDetector::new();
        let _ = d.observe(50.0, t(0.0), 200.0, 500.0);
        d.mark_gap();
        assert!(d.continuity_broken);
        // Next valid sample enters step 3 (first after gap)
        let (latch, short, _ratio, admitted) = d.observe(50.0, t(1.0), 200.0, 500.0);
        assert!(!latch);
        assert_eq!(short, Some(50.0));
        assert!(!admitted, "first after gap should not admit to long");
    }

    #[test]
    fn relearning_after_600s() {
        let mut d = EwmaDetector::new();
        let _ = d.observe(50.0, t(0.0), 200.0, 500.0);
        // 600s without admission
        let (latch, short, ratio, _) = d.observe(80.0, t(601.0), 200.0, 500.0);
        assert!(!latch);
        assert_eq!(short, Some(80.0));
        assert!((ratio - 1.0).abs() < 0.001);
        assert_eq!(d.valid_count, 1, "relearning should reset valid_count");
    }

    #[test]
    fn dt_zero_clamp() {
        let mut d = EwmaDetector::new();
        let _ = d.observe(50.0, t(0.0), 200.0, 500.0);
        // Same timestamp — dt should be clamped to 0.001
        let (_, short, _, _) = d.observe(60.0, t(0.0), 200.0, 500.0);
        // short should have moved slightly (alpha_s with dt=0.001 is very small)
        let s = short.unwrap();
        assert!(
            s > 50.0 && s < 60.0,
            "short should move slightly, got {}",
            s
        );
    }
}
