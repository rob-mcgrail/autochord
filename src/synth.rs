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
    pub glide: f32,             // portamento time in seconds (0 = off)
    /// Stereo spread across a chord's notes (0..1). Applied by the app as a
    /// per-note pan; the synth just honours the pan it's handed.
    pub spread: f32,
    pub master: f32,
}

impl Patch {
    /// Linear blend of the continuous parameters from `a` to `b` at `t` in
    /// `0..1` (t=0 → `a`, t=1 → `b` exactly). Oscillator waves are discrete, so
    /// they snap to `b`. Used to glide between patches (preset switches, edits)
    /// over a beat instead of jumping.
    pub fn lerp(a: &Patch, b: &Patch, t: f32) -> Patch {
        let t = t.clamp(0.0, 1.0);
        let f = |x: f32, y: f32| x + (y - x) * t;
        let osc = |i: usize| Osc {
            wave: b.osc[i].wave,
            pitch: f(a.osc[i].pitch, b.osc[i].pitch),
            fine: f(a.osc[i].fine, b.osc[i].fine),
            level: f(a.osc[i].level, b.osc[i].level),
            pan: f(a.osc[i].pan, b.osc[i].pan),
        };
        let env = |x: &Adsr, y: &Adsr| Adsr {
            a: f(x.a, y.a),
            d: f(x.d, y.d),
            s: f(x.s, y.s),
            r: f(x.r, y.r),
        };
        Patch {
            osc: [osc(0), osc(1)],
            noise: f(a.noise, b.noise),
            amp: env(&a.amp, &b.amp),
            cutoff: f(a.cutoff, b.cutoff),
            resonance: f(a.resonance, b.resonance),
            filter_env: env(&a.filter_env, &b.filter_env),
            filter_env_amount: f(a.filter_env_amount, b.filter_env_amount),
            pitch_lfo_rate: f(a.pitch_lfo_rate, b.pitch_lfo_rate),
            pitch_lfo_depth: f(a.pitch_lfo_depth, b.pitch_lfo_depth),
            filter_lfo_rate: f(a.filter_lfo_rate, b.filter_lfo_rate),
            filter_lfo_depth: f(a.filter_lfo_depth, b.filter_lfo_depth),
            glide: f(a.glide, b.glide),
            spread: f(a.spread, b.spread),
            master: f(a.master, b.master),
        }
    }
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
            glide: 0.0,
            spread: 0.0,
            master: 0.16,
        }
    }
}

/// Number of built-in synth presets. PgUp/PgDn cycle them; the text interface
/// selects one with the `patch` key.
pub const PRESET_COUNT: usize = 24;

/// The built-in preset bank, in cycle order. Index 0 is the startup patch.
///
/// Each patch is spelled out via struct-update over `Patch::default()`, so a
/// line only names what it changes. Waves are limited to sine/tri/square, so
/// character comes from detune, filter movement, envelope shape, and spread.
pub fn presets() -> [(&'static str, Patch); PRESET_COUNT] {
    fn osc(wave: Wave, pitch: f32, fine: f32, level: f32, pan: f32) -> Osc {
        Osc { wave, pitch, fine, level, pan }
    }
    fn adsr(a: f32, d: f32, s: f32, r: f32) -> Adsr {
        Adsr { a, d, s, r }
    }
    use Wave::{Sine, Square, Triangle};
    let base = Patch::default();
    [
        // 0 — warm pad/pluck hybrid: detuned width, resonant bloom per note.
        ("Warm Bloom", Patch {
            osc: [osc(Square, 0.0, -6.0, 0.50, -0.35), osc(Triangle, 0.0, 6.0, 0.55, 0.35)],
            noise: 0.03,
            amp: adsr(0.012, 0.45, 0.55, 0.45),
            cutoff: 900.0, resonance: 0.38, filter_env_amount: 0.65,
            filter_env: adsr(0.008, 0.40, 0.22, 0.40),
            filter_lfo_rate: 0.22, filter_lfo_depth: 0.18,
            spread: 0.45, master: 0.17,
            ..base
        }),
        // 1 — bright electric-piano: fast attack, decay to silence.
        ("Glass Keys", Patch {
            osc: [osc(Sine, 0.0, 0.0, 0.60, -0.15), osc(Triangle, 12.0, 0.0, 0.25, 0.15)],
            amp: adsr(0.004, 1.2, 0.0, 0.4),
            cutoff: 7000.0, resonance: 0.10, filter_env_amount: 0.20,
            filter_env: adsr(0.005, 0.5, 0.0, 0.3),
            spread: 0.2, master: 0.17,
            ..base
        }),
        // 2 — clean sine sub with a touch of glide.
        ("Deep Sub Bass", Patch {
            osc: [osc(Sine, 0.0, 0.0, 0.90, 0.0), osc(Sine, -12.0, 0.0, 0.30, 0.0)],
            amp: adsr(0.005, 0.2, 0.9, 0.15),
            cutoff: 300.0, resonance: 0.10, filter_env_amount: 0.20,
            filter_env: adsr(0.005, 0.15, 0.0, 0.15),
            glide: 0.04, master: 0.22,
            ..base
        }),
        // 3 — detuned squares, growly.
        ("Reese Bass", Patch {
            osc: [osc(Square, 0.0, -14.0, 0.50, 0.0), osc(Square, 0.0, 14.0, 0.50, 0.0)],
            amp: adsr(0.005, 0.25, 0.85, 0.2),
            cutoff: 500.0, resonance: 0.45, filter_env_amount: 0.30,
            filter_env: adsr(0.005, 0.25, 0.2, 0.2),
            glide: 0.03, master: 0.20,
            ..base
        }),
        // 4 — slow-attack ensemble with vibrato and filter drift.
        ("Analog Strings", Patch {
            osc: [osc(Square, 0.0, -8.0, 0.45, -0.4), osc(Square, 0.0, 8.0, 0.45, 0.4)],
            noise: 0.02,
            amp: adsr(0.35, 0.5, 0.8, 0.9),
            cutoff: 2200.0, resonance: 0.15, filter_env_amount: 0.20,
            filter_env: adsr(0.4, 0.6, 0.6, 0.8),
            pitch_lfo_rate: 4.5, pitch_lfo_depth: 0.06,
            filter_lfo_rate: 0.3, filter_lfo_depth: 0.12,
            spread: 0.5, master: 0.15,
            ..base
        }),
        // 5 — hollow wide pad.
        ("Hollow Pad", Patch {
            osc: [osc(Square, 0.0, -12.0, 0.40, -0.5), osc(Triangle, 0.0, 12.0, 0.50, 0.5)],
            amp: adsr(0.5, 0.6, 0.75, 1.0),
            cutoff: 1600.0, resonance: 0.10, filter_env_amount: 0.15,
            filter_env: adsr(0.6, 0.8, 0.5, 1.0),
            filter_lfo_rate: 0.15, filter_lfo_depth: 0.2,
            spread: 0.55, master: 0.15,
            ..base
        }),
        // 6 — snappy resonant pluck, great for arps.
        ("Pluck Stack", Patch {
            osc: [osc(Square, 0.0, -4.0, 0.50, -0.25), osc(Triangle, 0.0, 4.0, 0.50, 0.25)],
            amp: adsr(0.003, 0.25, 0.0, 0.25),
            cutoff: 700.0, resonance: 0.50, filter_env_amount: 0.70,
            filter_env: adsr(0.003, 0.22, 0.0, 0.2),
            spread: 0.4, master: 0.18,
            ..base
        }),
        // 7 — sine bell, long decay to silence.
        ("Bell Sine", Patch {
            osc: [osc(Sine, 0.0, 0.0, 0.60, -0.1), osc(Sine, 12.0, 2.0, 0.35, 0.1)],
            amp: adsr(0.002, 1.5, 0.0, 1.2),
            cutoff: 9000.0, resonance: 0.05, filter_env_amount: 0.0,
            filter_env: adsr(0.01, 1.0, 0.0, 1.0),
            spread: 0.25, master: 0.18,
            ..base
        }),
        // 8 — mono square lead with glide.
        ("Square Lead", Patch {
            osc: [osc(Square, 0.0, 0.0, 0.70, 0.0), osc(Square, 0.0, -5.0, 0.35, 0.0)],
            amp: adsr(0.006, 0.2, 0.8, 0.2),
            cutoff: 2500.0, resonance: 0.30, filter_env_amount: 0.35,
            filter_env: adsr(0.01, 0.2, 0.5, 0.2),
            pitch_lfo_rate: 5.5, pitch_lfo_depth: 0.05,
            glide: 0.06, spread: 0.1, master: 0.18,
            ..base
        }),
        // 9 — breathy flute, slow attack, vibrato.
        ("Soft Flute", Patch {
            osc: [osc(Sine, 0.0, 0.0, 0.70, 0.0), osc(Triangle, 0.0, 4.0, 0.25, 0.0)],
            noise: 0.06,
            amp: adsr(0.12, 0.3, 0.85, 0.3),
            cutoff: 3500.0, resonance: 0.10, filter_env_amount: 0.10,
            filter_env: adsr(0.1, 0.3, 0.6, 0.3),
            pitch_lfo_rate: 5.5, pitch_lfo_depth: 0.12,
            spread: 0.15, master: 0.16,
            ..base
        }),
        // 10 — expressive vibrato lead with glide.
        ("Vibrato Lead", Patch {
            osc: [osc(Triangle, 0.0, 0.0, 0.70, 0.0), osc(Sine, -12.0, 0.0, 0.30, 0.0)],
            amp: adsr(0.02, 0.25, 0.85, 0.25),
            cutoff: 3000.0, resonance: 0.20, filter_env_amount: 0.20,
            filter_env: adsr(0.02, 0.25, 0.6, 0.25),
            pitch_lfo_rate: 6.0, pitch_lfo_depth: 0.18,
            glide: 0.08, spread: 0.1, master: 0.17,
            ..base
        }),
        // 11 — heavily detuned warm pad.
        ("Detuned Pad", Patch {
            osc: [osc(Square, 0.0, -16.0, 0.45, -0.45), osc(Square, 0.0, 16.0, 0.45, 0.45)],
            noise: 0.02,
            amp: adsr(0.25, 0.5, 0.8, 0.8),
            cutoff: 1800.0, resonance: 0.20, filter_env_amount: 0.30,
            filter_env: adsr(0.3, 0.6, 0.5, 0.8),
            filter_lfo_rate: 0.2, filter_lfo_depth: 0.15,
            spread: 0.5, master: 0.15,
            ..base
        }),
        // 12 — square sub-bass with a filter pluck.
        ("Pulse Bass", Patch {
            osc: [osc(Square, 0.0, 0.0, 0.70, 0.0), osc(Square, -12.0, 0.0, 0.40, 0.0)],
            amp: adsr(0.004, 0.2, 0.7, 0.15),
            cutoff: 450.0, resonance: 0.35, filter_env_amount: 0.55,
            filter_env: adsr(0.004, 0.2, 0.0, 0.15),
            glide: 0.02, master: 0.20,
            ..base
        }),
        // 13 — slow low drone with deep filter sweep.
        ("Dark Drone", Patch {
            osc: [osc(Triangle, 0.0, -3.0, 0.50, -0.3), osc(Square, -12.0, 3.0, 0.40, 0.3)],
            noise: 0.02,
            amp: adsr(0.8, 1.0, 0.85, 1.5),
            cutoff: 700.0, resonance: 0.25, filter_env_amount: 0.15,
            filter_env: adsr(1.0, 1.5, 0.5, 1.5),
            filter_lfo_rate: 0.08, filter_lfo_depth: 0.25,
            spread: 0.4, master: 0.15,
            ..base
        }),
        // 14 — bright wide chime, sparkles on arps.
        ("Chime Arp", Patch {
            osc: [osc(Triangle, 0.0, 0.0, 0.50, -0.2), osc(Sine, 12.0, 3.0, 0.40, 0.2)],
            amp: adsr(0.002, 0.35, 0.1, 0.3),
            cutoff: 6000.0, resonance: 0.20, filter_env_amount: 0.40,
            filter_env: adsr(0.002, 0.3, 0.0, 0.25),
            spread: 0.6, master: 0.17,
            ..base
        }),
        // 15 — airy detuned choir, huge stereo.
        ("Wide Choir", Patch {
            osc: [osc(Sine, 0.0, -7.0, 0.50, -0.5), osc(Sine, 0.0, 7.0, 0.50, 0.5)],
            noise: 0.04,
            amp: adsr(0.3, 0.5, 0.85, 0.9),
            cutoff: 3000.0, resonance: 0.08, filter_env_amount: 0.10,
            filter_env: adsr(0.3, 0.5, 0.7, 0.8),
            pitch_lfo_rate: 4.0, pitch_lfo_depth: 0.08,
            filter_lfo_rate: 0.25, filter_lfo_depth: 0.1,
            spread: 0.6, master: 0.15,
            ..base
        }),
        // 16 — squelchy high-resonance 303-style.
        ("Acid Zap", Patch {
            osc: [osc(Square, 0.0, 0.0, 0.80, 0.0), osc(Square, 0.0, 0.0, 0.0, 0.0)],
            amp: adsr(0.004, 0.2, 0.6, 0.12),
            cutoff: 400.0, resonance: 0.70, filter_env_amount: 0.80,
            filter_env: adsr(0.003, 0.18, 0.0, 0.12),
            glide: 0.05, master: 0.18,
            ..base
        }),
        // 17 — soft muted triangle pluck.
        ("Muted Pluck", Patch {
            osc: [osc(Triangle, 0.0, -3.0, 0.60, -0.2), osc(Triangle, 0.0, 3.0, 0.50, 0.2)],
            amp: adsr(0.005, 0.3, 0.0, 0.25),
            cutoff: 1200.0, resonance: 0.15, filter_env_amount: 0.30,
            filter_env: adsr(0.005, 0.28, 0.0, 0.2),
            spread: 0.35, master: 0.18,
            ..base
        }),
        // 18 — punchy brass stab with a filter sweep.
        ("Brass Stab", Patch {
            osc: [osc(Square, 0.0, -5.0, 0.50, -0.2), osc(Square, 0.0, 5.0, 0.50, 0.2)],
            noise: 0.02,
            amp: adsr(0.03, 0.3, 0.75, 0.25),
            cutoff: 1200.0, resonance: 0.30, filter_env_amount: 0.60,
            filter_env: adsr(0.05, 0.3, 0.4, 0.25),
            pitch_lfo_rate: 5.0, pitch_lfo_depth: 0.04,
            spread: 0.3, master: 0.16,
            ..base
        }),
        // 19 — dead-clean sine bass.
        ("Sine Bass", Patch {
            osc: [osc(Sine, 0.0, 0.0, 0.95, 0.0), osc(Sine, 0.0, 0.0, 0.0, 0.0)],
            amp: adsr(0.004, 0.2, 0.9, 0.12),
            cutoff: 800.0, resonance: 0.05, filter_env_amount: 0.15,
            filter_env: adsr(0.004, 0.15, 0.0, 0.12),
            glide: 0.03, master: 0.22,
            ..base
        }),
        // 20 — lush pad with an octave-up layer.
        ("Octave Pad", Patch {
            osc: [osc(Square, 0.0, -5.0, 0.45, -0.35), osc(Triangle, 12.0, 5.0, 0.40, 0.35)],
            noise: 0.02,
            amp: adsr(0.2, 0.5, 0.8, 0.7),
            cutoff: 2500.0, resonance: 0.15, filter_env_amount: 0.25,
            filter_env: adsr(0.25, 0.5, 0.6, 0.7),
            filter_lfo_rate: 0.2, filter_lfo_depth: 0.12,
            spread: 0.5, master: 0.15,
            ..base
        }),
        // 21 — bright octave shimmer, very wide.
        ("Shimmer", Patch {
            osc: [osc(Sine, 0.0, 0.0, 0.50, -0.3), osc(Triangle, 12.0, 4.0, 0.45, 0.3)],
            noise: 0.02,
            amp: adsr(0.15, 0.6, 0.7, 1.0),
            cutoff: 7000.0, resonance: 0.10, filter_env_amount: 0.15,
            filter_env: adsr(0.2, 0.6, 0.6, 1.0),
            filter_lfo_rate: 0.18, filter_lfo_depth: 0.15,
            spread: 0.65, master: 0.15,
            ..base
        }),
        // 22 — moving growl bass (fast filter LFO).
        ("Growl Bass", Patch {
            osc: [osc(Square, 0.0, -10.0, 0.50, 0.0), osc(Square, 0.0, 10.0, 0.50, 0.0)],
            amp: adsr(0.006, 0.25, 0.85, 0.2),
            cutoff: 500.0, resonance: 0.40, filter_env_amount: 0.30,
            filter_env: adsr(0.006, 0.25, 0.3, 0.2),
            filter_lfo_rate: 3.5, filter_lfo_depth: 0.25,
            glide: 0.03, master: 0.20,
            ..base
        }),
        // 23 — huge slow ambient wash.
        ("Ambient Wash", Patch {
            osc: [osc(Sine, 0.0, -9.0, 0.45, -0.5), osc(Triangle, 0.0, 9.0, 0.45, 0.5)],
            noise: 0.05,
            amp: adsr(1.2, 1.5, 0.8, 2.0),
            cutoff: 2000.0, resonance: 0.12, filter_env_amount: 0.20,
            filter_env: adsr(1.5, 1.5, 0.6, 2.0),
            pitch_lfo_rate: 3.0, pitch_lfo_depth: 0.05,
            filter_lfo_rate: 0.06, filter_lfo_depth: 0.3,
            spread: 0.7, master: 0.14,
            ..base
        }),
    ]
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

    /// True while the note is held (pre-release) — what the piano display uses.
    fn gated(&self) -> bool {
        matches!(self.stage, Stage::Attack | Stage::Decay | Stage::Sustain)
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
    /// Voice key: high byte = source (0 live, 1..=N loop slots), low byte = MIDI
    /// note. Namespacing by source lets layered loops sound the same pitch on
    /// independent voices without stomping each other's note-offs or envelopes.
    id: u16,
    freq: f32,  // target base frequency
    glide: f32, // current base frequency (portamentos toward `freq`)
    spread: f32, // stereo-spread pan offset (-1..1), from note position in the chord
    phase1: f32,
    phase2: f32,
    amp: Env,
    filt: Env,
    left: Svf,
    right: Svf,
}

impl Voice {
    fn new(id: u16, freq: f32) -> Self {
        Self {
            id,
            freq,
            glide: freq,
            spread: 0.0,
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

        // Portamento: glide the base frequency toward the target in pitch space.
        if p.glide > 0.0 && (self.glide - self.freq).abs() > 0.01 {
            let coeff = 1.0 - (-1.0 / (p.glide * sr)).exp();
            self.glide *= (self.freq / self.glide).powf(coeff);
        } else {
            self.glide = self.freq;
        }

        let plfo = pitch_lfo * p.pitch_lfo_depth; // semitones of vibrato
        let o1 = run_osc(&p.osc[0], &mut self.phase1, plfo, self.glide, sr);
        let o2 = run_osc(&p.osc[1], &mut self.phase2, plfo, self.glide, sr);
        // Stereo-spread the whole voice on top of each oscillator's own pan.
        let (l1, r1) = pan_gains((p.osc[0].pan + self.spread).clamp(-1.0, 1.0));
        let (l2, r2) = pan_gains((p.osc[1].pan + self.spread).clamp(-1.0, 1.0));
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
// Drum voices — simple 808-style one-shots (kick, snare, hats, cowbell, tom,
// ride). Indexed by `inst`; the order must match `app::DrumInst`.
// ===========================================================================

/// Max lifetime (seconds) per drum instrument, after which the voice is reaped.
const DRUM_LIFE: [f32; 7] = [0.6, 0.4, 0.12, 0.5, 0.5, 0.6, 0.9];

#[derive(Clone, Copy)]
struct DrumVoice {
    inst: u8,
    age: f32, // seconds since the hit
    phase: f32,
    phase2: f32,
    lp: f32, // one-pole state, for shaping noise on snare/hats/ride
    rng: u32,
}

impl DrumVoice {
    fn new(inst: u8, rng: u32) -> Self {
        Self { inst, age: 0.0, phase: 0.0, phase2: 0.0, lp: 0.0, rng }
    }

    fn active(&self) -> bool {
        self.age < DRUM_LIFE[(self.inst as usize).min(6)]
    }

    /// One mono sample; advances the voice by one frame.
    fn render(&mut self, sr: f32) -> f32 {
        let a = self.age;
        let sq = |p: f32| if p < 0.5 { 1.0 } else { -1.0 };
        let s = match self.inst {
            0 => {
                // Kick: pitch drops 130→50 Hz, exp amp decay.
                let f = 50.0 + 80.0 * (-a / 0.03).exp();
                self.phase = (self.phase + f / sr).fract();
                (self.phase * TAU).sin() * (-a / 0.25).exp()
            }
            5 => {
                // Tom: like the kick, higher and a touch longer.
                let f = 90.0 + 100.0 * (-a / 0.06).exp();
                self.phase = (self.phase + f / sr).fract();
                (self.phase * TAU).sin() * (-a / 0.3).exp()
            }
            1 => {
                // Snare: a 180 Hz body plus high-passed noise.
                self.phase = (self.phase + 180.0 / sr).fract();
                let tone = (self.phase * TAU).sin() * (-a / 0.08).exp() * 0.5;
                let n = white(&mut self.rng);
                self.lp += 0.6 * (n - self.lp);
                (n - self.lp) * (-a / 0.2).exp() * 0.7 + tone
            }
            2 => {
                // Closed hat: short burst of high-passed noise.
                let n = white(&mut self.rng);
                self.lp += 0.75 * (n - self.lp);
                (n - self.lp) * (-a / 0.04).exp() * 0.7
            }
            3 => {
                // Open hat: longer high-passed noise.
                let n = white(&mut self.rng);
                self.lp += 0.75 * (n - self.lp);
                (n - self.lp) * (-a / 0.35).exp() * 0.5
            }
            4 => {
                // Cowbell: two detuned squares.
                self.phase = (self.phase + 540.0 / sr).fract();
                self.phase2 = (self.phase2 + 800.0 / sr).fract();
                (sq(self.phase) + sq(self.phase2)) * 0.25 * (-a / 0.2).exp()
            }
            6 => {
                // Ride: metallic — high tone plus bright noise, long decay.
                self.phase = (self.phase + 3200.0 / sr).fract();
                let tone = sq(self.phase) * 0.15;
                let n = white(&mut self.rng);
                self.lp += 0.85 * (n - self.lp);
                ((n - self.lp) * 0.4 + tone) * (-a / 0.6).exp()
            }
            _ => 0.0,
        };
        self.age += 1.0 / sr;
        s
    }
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
    last_freq: f32, // most recent note pitch — the glide origin for portamento
    rng: u32,
    monitor: Arc<VoiceMonitor>,
    /// Metronome click state: a short decaying sine blip, independent of the
    /// patch, used for the loop-recorder count-in.
    click_env: f32,
    click_phase: f32,
    click_freq: f32,
    /// Sounding 808-style drum one-shots.
    drum_voices: Vec<DrumVoice>,
}

impl Synth {
    pub fn new(sample_rate: f32, monitor: Arc<VoiceMonitor>) -> Self {
        Self {
            sample_rate,
            voices: Vec::with_capacity(16),
            patch: Patch::default(),
            pitch_lfo_phase: 0.0,
            filter_lfo_phase: 0.0,
            last_freq: 220.0,
            rng: 0x1234_5678,
            monitor,
            click_env: 0.0,
            click_phase: 0.0,
            click_freq: 1000.0,
            drum_voices: Vec::with_capacity(16),
        }
    }

    /// Trigger an 808-style drum one-shot (`inst` indexes the kit).
    pub fn drum_hit(&mut self, inst: u8) {
        // Advance the shared rng so each hit's noise differs.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.drum_voices.push(DrumVoice::new(inst, self.rng));
    }

    pub fn set_patch(&mut self, patch: Patch) {
        self.patch = patch;
    }

    /// Trigger a metronome click (count-in). Accented clicks (the downbeat) ring
    /// higher. It's a fixed short blip, not shaped by the patch.
    pub fn click(&mut self, accent: bool) {
        self.click_env = 1.0;
        self.click_phase = 0.0;
        self.click_freq = if accent { 1600.0 } else { 1000.0 };
    }

    /// Start (or retrigger) a tone with the given id at `freq`, panned by `pan`
    /// (stereo spread from its position in the chord).
    pub fn note_on(&mut self, id: u16, freq: f32, pan: f32) {
        if let Some(voice) = self.voices.iter_mut().find(|v| v.id == id) {
            voice.spread = pan;
            voice.gate_on(freq); // glide continues from the voice's current pitch
        } else {
            let mut voice = Voice::new(id, freq);
            voice.spread = pan;
            // With portamento on, glide in from the previous note's pitch.
            voice.glide = if self.patch.glide > 0.0 { self.last_freq } else { freq };
            voice.gate_on(freq);
            self.voices.push(voice);
        }
        self.last_freq = freq;
        self.refresh_monitor((id & 0xFF) as u8);
    }

    /// Release the tone with the given id (it fades out over its release stage).
    pub fn note_off(&mut self, id: u16) {
        if let Some(voice) = self.voices.iter_mut().find(|v| v.id == id) {
            voice.gate_off();
        }
        self.refresh_monitor((id & 0xFF) as u8);
    }

    /// Light the piano key for `note` iff some source still holds it (gated on),
    /// so a note-off from one source doesn't dark a key another source is
    /// playing at the same pitch.
    fn refresh_monitor(&self, note: u8) {
        let held = self
            .voices
            .iter()
            .any(|v| (v.id & 0xFF) as u8 == note && v.amp.gated());
        self.monitor.set(note, held);
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
        let mut l = l * m;
        let mut r = r * m;

        // Drum voices, mixed centered at a fixed level (independent of the
        // melodic patch/master).
        let mut drums = 0.0;
        for dv in &mut self.drum_voices {
            drums += dv.render(sr);
        }
        self.drum_voices.retain(|v| v.active());
        l += drums * 0.5;
        r += drums * 0.5;

        // Metronome click: a short decaying sine, mixed in at a fixed level
        // above the patch so the count-in is always audible.
        if self.click_env > 0.0005 {
            let s = (self.click_phase * TAU).sin() * self.click_env * 0.4;
            l += s;
            r += s;
            self.click_phase = (self.click_phase + self.click_freq / sr).fract();
            self.click_env *= (-30.0 / sr).exp(); // ~30ms decay
        } else {
            self.click_env = 0.0;
        }

        (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0))
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
        p.osc[1].fine = 7.0;
        p.noise = 0.4;
        p.resonance = 0.95;
        p.filter_env_amount = 1.0;
        p.pitch_lfo_depth = 2.0;
        p.filter_lfo_depth = 1.0;
        p.glide = 0.2;
        p.spread = 0.8;
        s.set_patch(p);

        s.note_on(60, 261.63, -0.8);
        s.note_on(64, 329.63, 0.0);
        s.note_on(67, 392.0, 0.8);
        run(&mut s, 48_000); // ~1s: attack/decay/sustain + glide
        s.note_off(60);
        s.note_off(64);
        s.note_off(67);
        run(&mut s, 48_000); // release + reap
    }

    #[test]
    fn drums_stay_finite_and_in_range() {
        let mut s = Synth::new(48_000.0, Arc::new(VoiceMonitor::new()));
        for inst in 0..7 {
            s.drum_hit(inst);
        }
        run(&mut s, 48_000); // ~1s: all voices decay and reap
        assert!(s.drum_voices.is_empty(), "drum voices should be reaped");
    }
}
