use rodio::Source;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const SR: u32 = 48000;
const TWO_PI: f64 = std::f64::consts::TAU;

#[derive(Default)]
pub struct AudioState {
    pub muted: AtomicBool,
}

impl AudioState {
    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundEvent {
    Loss,
    Down,
    Recover,
    Shimmer,
}

#[derive(Clone)]
pub struct NoteSpec {
    pub freq: f64,
    pub dur_samples: u64,
    pub partials: Vec<(f64, f32)>,
    pub attack_samples: u64,
    pub decay_samples: u64,
    pub sustain_gain: f32,
    pub release_samples: u64,
}

impl NoteSpec {
    pub fn bell(freq: f64, dur_ms: u64) -> Self {
        let dur = (dur_ms as f64 * SR as f64 / 1000.0) as u64;
        let atk = (6.0 * SR as f64 / 1000.0) as u64;
        let rel = (dur as f64 * 0.6) as u64;
        Self {
            freq,
            dur_samples: dur,
            partials: vec![
                (1.0, 1.0),
                (2.0, 0.5),
                (2.4, 0.32),
                (3.5, 0.20),
                (4.5, 0.12),
            ],
            attack_samples: atk,
            decay_samples: (dur as f64 * 0.25) as u64,
            sustain_gain: 0.35,
            release_samples: rel,
        }
    }

    pub fn soft(freq: f64, dur_ms: u64) -> Self {
        let dur = (dur_ms as f64 * SR as f64 / 1000.0) as u64;
        Self {
            freq,
            dur_samples: dur,
            partials: vec![(1.0, 1.0), (2.0, 0.30), (3.0, 0.12)],
            attack_samples: (10.0 * SR as f64 / 1000.0) as u64,
            decay_samples: (dur as f64 * 0.3) as u64,
            sustain_gain: 0.5,
            release_samples: (dur as f64 * 0.5) as u64,
        }
    }

    pub fn pad(freq: f64, dur_ms: u64) -> Self {
        let dur = (dur_ms as f64 * SR as f64 / 1000.0) as u64;
        Self {
            freq,
            dur_samples: dur,
            partials: vec![(1.0, 1.0), (2.0, 0.45), (3.0, 0.18), (4.0, 0.08)],
            attack_samples: (40.0 * SR as f64 / 1000.0) as u64,
            decay_samples: (dur as f64 * 0.4) as u64,
            sustain_gain: 0.55,
            release_samples: (dur as f64 * 0.5) as u64,
        }
    }
}

pub struct NoteSource {
    spec: NoteSpec,
    t: u64,
    phases: Vec<f64>,
}

impl NoteSource {
    pub fn new(spec: NoteSpec) -> Self {
        let phases = vec![0.0; spec.partials.len()];
        Self { spec, t: 0, phases }
    }
}

impl Iterator for NoteSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.t >= self.spec.dur_samples {
            return None;
        }
        let sr = SR as f64;
        let t = self.t;

        let a = self.spec.attack_samples;
        let d = self.spec.decay_samples;
        let s = self.spec.sustain_gain;
        let r = self.spec.release_samples;
        let dur = self.spec.dur_samples;
        let env: f32 = if t < a {
            (t as f32 / a.max(1) as f32).min(1.0)
        } else if t < a + d {
            let p = (t - a) as f32 / d.max(1) as f32;
            1.0 - (1.0 - s) * p
        } else if t < dur.saturating_sub(r) {
            s
        } else {
            let remaining = (dur - t) as f32;
            s * (remaining / r.max(1) as f32).max(0.0)
        };

        let mut sample: f32 = 0.0;
        for (i, (mult, gain)) in self.spec.partials.iter().enumerate() {
            let ph = self.phases[i];
            sample += (ph * TWO_PI).sin() as f32 * gain * env;
            self.phases[i] = ph + self.spec.freq * mult / sr;
            if self.phases[i] >= 1.0 {
                self.phases[i] -= 1.0;
            }
        }

        let out = sample.tanh() * 0.35;
        self.t += 1;
        Some(out)
    }
}

impl Source for NoteSource {
    fn current_frame_len(&self) -> Option<usize> {
        if self.t >= self.spec.dur_samples {
            Some(0)
        } else {
            Some((self.spec.dur_samples - self.t) as usize)
        }
    }
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate(&self) -> u32 {
        SR
    }
    fn total_duration(&self) -> Option<Duration> {
        Some(Duration::from_secs_f64(
            self.spec.dur_samples as f64 / SR as f64,
        ))
    }
}

fn delayed(spec: NoteSpec, offset_ms: u64, gain: f32) -> impl Source<Item = f32> {
    NoteSource::new(spec)
        .delay(Duration::from_millis(offset_ms))
        .amplify(gain)
}

pub fn chime_loss() -> impl Source<Item = f32> {
    let n1 = delayed(NoteSpec::soft(392.00, 130), 0, 0.9);
    let n2 = delayed(NoteSpec::soft(311.13, 180), 120, 0.9);
    n1.mix(n2)
}

pub fn chime_recover() -> impl Source<Item = f32> {
    let n1 = delayed(NoteSpec::bell(523.25, 260), 0, 0.9);
    let n2 = delayed(NoteSpec::bell(659.25, 260), 100, 0.9);
    let n3 = delayed(NoteSpec::bell(783.99, 280), 200, 0.9);
    let n4 = delayed(NoteSpec::bell(1046.50, 360), 300, 0.9);
    n1.mix(n2).mix(n3).mix(n4)
}

pub fn chime_down() -> impl Source<Item = f32> {
    let n1 = delayed(NoteSpec::bell(196.00, 500), 0, 0.9);
    let n2 = delayed(NoteSpec::bell(233.08, 500), 0, 0.9);
    n1.mix(n2)
}

pub fn chime_shimmer() -> impl Source<Item = f32> {
    delayed(NoteSpec::pad(1318.51, 320), 0, 0.10)
}

pub fn spawn_audio() -> Option<(mpsc::UnboundedSender<SoundEvent>, Arc<AudioState>)> {
    use rodio::{OutputStream, Sink};
    let (stream, handle) = OutputStream::try_default().ok()?;
    std::mem::forget(stream);

    let state = Arc::new(AudioState::default());

    let (tx, mut rx) = mpsc::unbounded_channel::<SoundEvent>();
    let _h = Arc::clone(&state);
    tokio::task::spawn_blocking(move || {
        while let Some(ev) = rx.blocking_recv() {
            let sink = match Sink::try_new(&handle) {
                Ok(s) => s,
                Err(_) => continue,
            };
            match ev {
                SoundEvent::Recover => sink.append(chime_recover()),
                SoundEvent::Loss => sink.append(chime_loss()),
                SoundEvent::Down => sink.append(chime_down()),
                SoundEvent::Shimmer => sink.append(chime_shimmer()),
            }
            sink.detach();
        }
    });
    Some((tx, state))
}
