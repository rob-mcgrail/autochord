//! Sound generation. Pure DSP — this module knows nothing about cpal, the
//! terminal, tuning, or how notes get triggered. It plays whatever frequencies
//! it is handed and turns note on/off requests into a stream of samples.

use std::f32::consts::{FRAC_1_SQRT_2, PI, TAU};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A lock-free, shared view of which MIDI notes the synth currently has gated
/// on. The audio thread writes it as notes start/stop; the UI reads it, so the
/// on-screen keys always reflect the synth's real state without the UI having
/// to track note events itself.
#[derive(Default)]
pub struct VoiceMonitor {
    bits: [AtomicU64; 2], // 128 MIDI notes
}

impl VoiceMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self, note: u8, on: bool) {
        let (word, bit) = (note as usize / 64, note % 64);
        let mask = 1u64 << bit;
        if on {
            self.bits[word].fetch_or(mask, Ordering::Relaxed);
        } else {
            self.bits[word].fetch_and(!mask, Ordering::Relaxed);
        }
    }

    /// MIDI notes currently gated on, ascending.
    pub fn active(&self) -> Vec<u8> {
        let mut notes = Vec::new();
        for (word, cell) in self.bits.iter().enumerate() {
            let mut bits = cell.load(Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                notes.push((word * 64 + bit) as u8);
                bits &= bits - 1; // clear lowest set bit
            }
        }
        notes
    }
}

// ===========================================================================
// Patch — the editable synth parameters (a single Copy struct, sent whole to
// the audio thread whenever the UI changes something).
// ===========================================================================

/// Oscillator waveform.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Wave {
    #[default]
    Sine,
    Triangle,
    Square,
}

impl Wave {
    pub fn label(self) -> &'static str {
        match self {
            Wave::Sine => "sine",
            Wave::Triangle => "tri",
            Wave::Square => "sqr",
        }
    }

    pub fn cycle(self, dir: i32) -> Wave {
        const ALL: [Wave; 3] = [Wave::Sine, Wave::Triangle, Wave::Square];
        let i = ALL.iter().position(|&w| w == self).unwrap_or(0) as i32;
        ALL[(i + dir).rem_euclid(3) as usize]
    }

    fn sample(self, phase: f32, dt: f32) -> f32 {
        match self {
            Wave::Sine => (phase * TAU).sin(),
            Wave::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
            Wave::Square => {
                // Naive square + PolyBLEP at both edges to tame aliasing.
                let mut s = if phase < 0.5 { 1.0 } else { -1.0 };
                s += poly_blep(phase, dt);
                s -= poly_blep((phase + 0.5).rem_euclid(1.0), dt);
                s
            }
        }
    }
}

/// Band-limiting correction around a step discontinuity (PolyBLEP).
fn poly_blep(t: f32, dt: f32) -> f32 {
    if dt <= 0.0 {
        0.0
    } else if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// One oscillator.
#[derive(Clone, Copy)]
pub struct Osc {
    pub wave: Wave,
    pub pitch: f32, // semitone offset
    pub fine: f32,  // fine detune in cents (±100 = ±1 semitone)
    pub level: f32, // 0..1
    pub pan: f32,   // -1 (L) .. +1 (R)
}

/// Attack / Decay / Sustain / Release (times in seconds, sustain a 0..1 level).
#[derive(Clone, Copy)]
pub struct Adsr {
    pub a: f32,
    pub d: f32,
    pub s: f32,
    pub r: f32,
}

/// The full synth patch.
#[derive(Clone, Copy)]
pub struct Patch {
    pub osc: [Osc; 2],
    pub noise: f32, // 0..1
    pub amp: Adsr,
    pub cutoff: f32,            // Hz
    pub resonance: f32,         // 0..1
    pub filter_env: Adsr,       // modulates the cutoff
    pub filter_env_amount: f32, // 0..1 (octaves of upward sweep)
    pub pitch_lfo_rate: f32,    // Hz
    pub pitch_lfo_depth: f32,   // semitones
    pub filter_lfo_rate: f32,   // Hz
    pub filter_lfo_depth: f32,  // 0..1 (octaves of sweep)
    pub master: f32,
}

impl Default for Patch {
    fn default() -> Self {
        Self {
            osc: [
                Osc { wave: Wave::Triangle, pitch: 0.0, fine: 0.0, level: 0.6, pan: -0.25 },
                Osc { wave: Wave::Sine, pitch: 0.0, fine: 0.0, level: 0.5, pan: 0.25 },
            ],
            noise: 0.0,
            amp: Adsr { a: 0.01, d: 0.2, s: 0.8, r: 0.35 },
            cutoff: 6000.0,
            resonance: 0.12,
            filter_env: Adsr { a: 0.02, d: 0.3, s: 0.4, r: 0.3 },
            filter_env_amount: 0.0,
            pitch_lfo_rate: 5.0,
            pitch_lfo_depth: 0.0,
            filter_lfo_rate: 2.0,
            filter_lfo_depth: 0.0,
            master: 0.16,
        }
    }
}

// ===========================================================================
// Per-voice DSP
// ===========================================================================

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// An ADSR envelope generator.
#[derive(Clone, Copy)]
struct Env {
    stage: Stage,
    level: f32,
}

impl Env {
    fn new() -> Self {
        Self { stage: Stage::Idle, level: 0.0 }
    }

    fn gate_on(&mut self) {
        self.stage = Stage::Attack;
    }

    fn gate_off(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    fn active(&self) -> bool {
        self.stage != Stage::Idle
    }

    fn next(&mut self, adsr: &Adsr, sr: f32) -> f32 {
        let per = |t: f32| 1.0 / (t.max(0.0005) * sr);
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += per(adsr.a);
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level -= per(adsr.d) * (1.0 - adsr.s);
                if self.level <= adsr.s {
                    self.level = adsr.s;
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => self.level = adsr.s,
            Stage::Release => {
                self.level -= per(adsr.r);
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

/// One channel of a topology-preserving state-variable filter (Zavalishin).
#[derive(Clone, Copy, Default)]
struct Svf {
    ic1: f32,
    ic2: f32,
}

impl Svf {
    fn lowpass(&mut self, input: f32, a1: f32, a2: f32, a3: f32) -> f32 {
        let v3 = input - self.ic2;
        let v1 = a1 * self.ic1 + a2 * v3;
        let v2 = self.ic2 + a2 * self.ic1 + a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        v2
    }
}

/// SVF coefficients `(a1, a2, a3)` for a cutoff and resonance.
fn svf_coeffs(cutoff: f32, resonance: f32, sr: f32) -> (f32, f32, f32) {
    let g = (PI * cutoff / sr).tan();
    let q = 0.5 + resonance.clamp(0.0, 1.0) * 9.5;
    let k = 1.0 / q;
    let a1 = 1.0 / (1.0 + g * (g + k));
    let a2 = g * a1;
    let a3 = g * a2;
    (a1, a2, a3)
}

/// Equal-power pan gains for L/R.
fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * (0.25 * PI); // 0..π/2
    (angle.cos(), angle.sin())
}

/// One sounding note: two oscillators + noise, amp & filter envelopes, and a
/// stereo pair of filters (so per-oscillator panning survives filtering).
struct Voice {
    id: u8,
    freq: f32,
    phase1: f32,
    phase2: f32,
    amp: Env,
    filt: Env,
    left: Svf,
    right: Svf,
}

impl Voice {
    fn new(id: u8, freq: f32) -> Self {
        Self {
            id,
            freq,
            phase1: 0.0,
            phase2: 0.0,
            amp: Env::new(),
            filt: Env::new(),
            left: Svf::default(),
            right: Svf::default(),
        }
    }

    fn gate_on(&mut self, freq: f32) {
        self.freq = freq;
        self.amp.gate_on();
        self.filt.gate_on();
    }

    fn gate_off(&mut self) {
        self.amp.gate_off();
        self.filt.gate_off();
    }

    fn render(&mut self, p: &Patch, pitch_lfo: f32, filt_lfo: f32, noise: f32, sr: f32) -> (f32, f32) {
        let amp = self.amp.next(&p.amp, sr);
        let fenv = self.filt.next(&p.filter_env, sr);

        let plfo = pitch_lfo * p.pitch_lfo_depth; // semitones of vibrato
        let o1 = run_osc(&p.osc[0], &mut self.phase1, plfo, self.freq, sr);
        let o2 = run_osc(&p.osc[1], &mut self.phase2, plfo, self.freq, sr);
        let (l1, r1) = pan_gains(p.osc[0].pan);
        let (l2, r2) = pan_gains(p.osc[1].pan);
        let n = noise * p.noise * FRAC_1_SQRT_2;
        let mut l = o1 * l1 + o2 * l2 + n;
        let mut r = o1 * r1 + o2 * r2 + n;

        // Cutoff: base, swept up by the filter envelope, wobbled by its LFO.
        let cutoff = (p.cutoff
            * 2f32.powf(p.filter_env_amount * fenv * 4.0)
            * 2f32.powf(filt_lfo * p.filter_lfo_depth * 2.0))
        .clamp(20.0, sr * 0.45);
        let (a1, a2, a3) = svf_coeffs(cutoff, p.resonance, sr);
        l = self.left.lowpass(l, a1, a2, a3);
        r = self.right.lowpass(r, a1, a2, a3);

        (l * amp, r * amp)
    }
}

fn run_osc(osc: &Osc, phase: &mut f32, pitch_lfo: f32, base_freq: f32, sr: f32) -> f32 {
    let semis = osc.pitch + osc.fine / 100.0 + pitch_lfo;
    let freq = base_freq * 2f32.powf(semis / 12.0);
    let dt = freq / sr;
    let s = osc.wave.sample(*phase, dt) * osc.level;
    *phase = (*phase + dt).fract();
    s
}

fn white(rng: &mut u32) -> f32 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 17;
    *rng ^= *rng << 5;
    (*rng as f32 / u32::MAX as f32) * 2.0 - 1.0
}

// ===========================================================================
// Synth — the polyphonic engine
// ===========================================================================

pub struct Synth {
    sample_rate: f32,
    voices: Vec<Voice>,
    patch: Patch,
    pitch_lfo_phase: f32,
    filter_lfo_phase: f32,
    rng: u32,
    monitor: Arc<VoiceMonitor>,
}

impl Synth {
    pub fn new(sample_rate: f32, monitor: Arc<VoiceMonitor>) -> Self {
        Self {
            sample_rate,
            voices: Vec::with_capacity(16),
            patch: Patch::default(),
            pitch_lfo_phase: 0.0,
            filter_lfo_phase: 0.0,
            rng: 0x1234_5678,
            monitor,
        }
    }

    pub fn set_patch(&mut self, patch: Patch) {
        self.patch = patch;
    }

    /// Start (or retrigger) a tone with the given id at `freq`.
    pub fn note_on(&mut self, id: u8, freq: f32) {
        if let Some(voice) = self.voices.iter_mut().find(|v| v.id == id) {
            voice.gate_on(freq);
        } else {
            let mut voice = Voice::new(id, freq);
            voice.gate_on(freq);
            self.voices.push(voice);
        }
        self.monitor.set(id, true);
    }

    /// Release the tone with the given id (it fades out over its release stage).
    pub fn note_off(&mut self, id: u8) {
        if let Some(voice) = self.voices.iter_mut().find(|v| v.id == id) {
            voice.gate_off();
        }
        self.monitor.set(id, false);
    }

    /// Produce one stereo frame `(left, right)` and advance every voice.
    ///
    /// Called from the real-time audio callback, so it does no allocation and
    /// no blocking work.
    pub fn next_frame(&mut self) -> (f32, f32) {
        let patch = self.patch;
        let sr = self.sample_rate;

        let pitch_lfo = (self.pitch_lfo_phase * TAU).sin();
        let filt_lfo = (self.filter_lfo_phase * TAU).sin();
        self.pitch_lfo_phase = (self.pitch_lfo_phase + patch.pitch_lfo_rate / sr).fract();
        self.filter_lfo_phase = (self.filter_lfo_phase + patch.filter_lfo_rate / sr).fract();

        let mut rng = self.rng;
        let mut l = 0.0;
        let mut r = 0.0;
        for voice in &mut self.voices {
            let noise = white(&mut rng);
            let (vl, vr) = voice.render(&patch, pitch_lfo, filt_lfo, noise, sr);
            l += vl;
            r += vr;
        }
        self.rng = rng;

        // Reap voices whose amp envelope has finished.
        self.voices.retain(|v| v.amp.active());

        let m = patch.master;
        ((l * m).clamp(-1.0, 1.0), (r * m).clamp(-1.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut Synth, frames: usize) {
        for _ in 0..frames {
            let (l, r) = s.next_frame();
            assert!(l.is_finite() && r.is_finite(), "non-finite output");
            assert!(l.abs() <= 1.0 && r.abs() <= 1.0, "out of range: {l},{r}");
        }
    }

    #[test]
    fn engine_stays_finite_and_in_range() {
        let mut s = Synth::new(48_000.0, Arc::new(VoiceMonitor::new()));
        // A stress patch: square + noise, high resonance, filter + pitch LFOs.
        let mut p = Patch::default();
        p.osc[0].wave = Wave::Square;
        p.osc[1].wave = Wave::Triangle;
        p.noise = 0.4;
        p.resonance = 0.95;
        p.filter_env_amount = 1.0;
        p.pitch_lfo_depth = 2.0;
        p.filter_lfo_depth = 1.0;
        s.set_patch(p);

        s.note_on(60, 261.63);
        s.note_on(64, 329.63);
        s.note_on(67, 392.0);
        run(&mut s, 48_000); // ~1s: attack/decay/sustain
        s.note_off(60);
        s.note_off(64);
        s.note_off(67);
        run(&mut s, 48_000); // release + reap
    }
}
