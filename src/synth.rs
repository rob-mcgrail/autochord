//! Sound generation. Pure DSP — this module knows nothing about cpal, the
//! terminal, tuning, or how notes get triggered. It plays whatever frequencies
//! it is handed and turns note on/off requests into a stream of samples.

use std::f32::consts::TAU;
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

/// A single sounding voice: one sine oscillator plus a linear gate envelope so
/// notes fade in/out instead of clicking.
struct Voice {
    /// MIDI note this voice answers to, for note-on/off matching. Its actual
    /// pitch (`phase_inc`) may be detuned from 12-TET for just intonation.
    id: u8,
    /// Oscillator phase in turns (0.0..1.0). Using turns instead of radians
    /// keeps precision stable no matter how long a note is held.
    phase: f32,
    /// Turns advanced per sample = frequency / sample_rate.
    phase_inc: f32,
    /// Is the key still held? Drives the envelope toward 1.0 (on) or 0.0 (off).
    gate: bool,
    /// Current envelope level (0.0..1.0).
    level: f32,
}

impl Voice {
    fn new(id: u8, freq: f32, sample_rate: f32) -> Self {
        Self {
            id,
            phase: 0.0,
            phase_inc: freq / sample_rate,
            gate: true,
            level: 0.0,
        }
    }
}

/// A tiny polyphonic sine synth. One `Voice` per sounding tone.
pub struct Synth {
    sample_rate: f32,
    voices: Vec<Voice>,
    /// Master gain. Kept low so a stacked chord stays within headroom without
    /// any dynamic (click-inducing) level compensation.
    master: f32,
    /// Envelope increment per sample for the attack ramp.
    attack_per_sample: f32,
    /// Envelope decrement per sample for the release ramp.
    release_per_sample: f32,
    /// Shared, UI-readable view of which notes are gated on.
    monitor: Arc<VoiceMonitor>,
}

impl Synth {
    pub fn new(sample_rate: f32, monitor: Arc<VoiceMonitor>) -> Self {
        Self {
            sample_rate,
            voices: Vec::with_capacity(16),
            master: 0.15,
            // ~5 ms attack, ~60 ms release — enough to kill clicks.
            attack_per_sample: 1.0 / (0.005 * sample_rate),
            release_per_sample: 1.0 / (0.060 * sample_rate),
            monitor,
        }
    }

    /// Start (or retrigger) a tone with the given id, oscillating at `freq`.
    pub fn note_on(&mut self, id: u8, freq: f32) {
        if let Some(voice) = self.voices.iter_mut().find(|v| v.id == id) {
            voice.gate = true; // re-open a voice that was fading out
        } else {
            self.voices.push(Voice::new(id, freq, self.sample_rate));
        }
        self.monitor.set(id, true);
    }

    /// Release the tone with the given id (it fades out over the release ramp).
    pub fn note_off(&mut self, id: u8) {
        if let Some(voice) = self.voices.iter_mut().find(|v| v.id == id) {
            voice.gate = false;
        }
        self.monitor.set(id, false);
    }

    /// Produce one mono sample and advance every voice by one step.
    ///
    /// Called from the real-time audio callback, so it does no allocation and
    /// no blocking work.
    pub fn next_sample(&mut self) -> f32 {
        let mut mix = 0.0;
        for voice in &mut self.voices {
            let target = if voice.gate { 1.0 } else { 0.0 };
            if voice.level < target {
                voice.level = (voice.level + self.attack_per_sample).min(target);
            } else if voice.level > target {
                voice.level = (voice.level - self.release_per_sample).max(target);
            }

            mix += (voice.phase * TAU).sin() * voice.level;

            voice.phase += voice.phase_inc;
            if voice.phase >= 1.0 {
                voice.phase -= 1.0;
            }
        }

        // Reap voices that have finished releasing.
        self.voices.retain(|v| v.gate || v.level > 0.0001);

        // Fixed, linear gain. Normalising by live voice count would step the
        // level — and click — every time a tone starts or is reaped, so we
        // don't; a chord is just naturally a bit louder than one note. The
        // clamp is a last-ditch safety that shouldn't trigger in normal play.
        (mix * self.master).clamp(-1.0, 1.0)
    }
}
