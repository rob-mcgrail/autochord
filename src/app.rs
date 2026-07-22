//! Application state, keyboard handling, and TUI rendering.

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::audio::{AudioInfo, SynthEvent};
use crate::notes::{
    addition_for_key, chord_notes, chord_symbol, note_for_key, note_name, pitch_class_name,
    quality_for_key, tone_frequency, voice_chord, Addition, Quality, ADDITIONS, QUALITIES,
};
use crate::control::Control;
use crate::notes::parse_note;
use crate::synth::{FilterMode, Patch, VoiceMonitor, Wave};
use crate::transport::Transport;

/// Clamp range for the Chord Voicing dial (clicks either side of neutral).
const VOICING_RANGE: i32 = 24;
/// Highest bass offset above the root (kept within the octave below the chord).
const BASS_MAX: i32 = 11;
/// White-key steps the window can be transposed either side of home.
const WINDOW_RANGE: i32 = 14;

/// Number of loop-recorder slots on the play page (navigable, layered).
const LOOP_SLOTS: usize = 4;
/// Selectable time signatures, as beats-per-bar (denominator fixed at /4).
const TIME_SIGS: [u32; 2] = [4, 3];

/// Per-loop playback-length divisions: the fraction of the recorded loop that
/// plays before repeating (full, half, quarter, eighth).
const LOOP_DIVISIONS: [f64; 4] = [1.0, 0.5, 0.25, 0.125];
const LOOP_DIVISION_LABELS: [&str; 4] = ["1/1", "1/2", "1/4", "1/8"];
/// Per-loop speed (playback-rate) increment and its step bounds (0.25×–4×).
const LOOP_SPEED_STEP: f64 = 0.25;
const LOOP_SPEED_MIN_STEPS: i32 = -3;
const LOOP_SPEED_MAX_STEPS: i32 = 12;
/// Per-loop transpose bound in semitones, either direction.
const LOOP_TRANSPOSE_RANGE: i32 = 24;
/// Cells in a recorded loop lane: loop, quantize, mute, solo, undo, div, speed,
/// transpose, reset (columns 0..9).
const LOOP_CELLS: usize = 9;

/// Per-loop playback quantize grids `(label, grid-in-beats)`. `None` is "free"
/// — play the notes at their exact recorded timing. Quantize is applied only at
/// playback (never baked), so switching back to `free` restores the original
/// feel. Includes straight and triplet grids.
const LOOP_QUANTIZE: [(&str, Option<f64>); 7] = [
    ("free", None),
    ("1/4", Some(1.0)),
    ("1/8", Some(0.5)),
    ("1/8T", Some(1.0 / 3.0)),
    ("1/16", Some(0.25)),
    ("1/16T", Some(1.0 / 6.0)),
    ("1/32", Some(0.125)),
];
/// Default quantize index (1/16).
const LOOP_QUANTIZE_DEFAULT: usize = 4;

/// The live keyboard is voice source 0; loop slot `i` is source `i + 1`. See
/// `synth::Voice::id` for how the source namespaces voices.
const LIVE_SOURCE: u8 = 0;

/// Compose a synth voice id: high byte = source, low byte = MIDI note.
fn voice_id(source: u8, note: u8) -> u16 {
    ((source as u16) << 8) | note as u16
}

// ---------------------------------------------------------------------------
// Loop recorder
// ---------------------------------------------------------------------------

/// One recorded note event, timed in beats from the loop's start.
#[derive(Clone, Copy)]
struct LoopEvent {
    beat: f64,
    on: bool,
    note: u8,
    freq: f32,
    pan: f32,
}

/// The lifecycle state of a loop slot. Mute and solo are orthogonal flags.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum LoopState {
    #[default]
    Empty,
    /// Record requested; waiting for the next bar line to start capturing.
    Armed,
    Recording,
    Playing,
}

/// One loop slot: a stack of recorded layers (overdubs) plus playback state.
/// The first pass sets the loop length (a whole number of bars) and its phase
/// anchor; later passes overdub onto it. Undo pops the last layer. Phase-locked
/// to the shared transport so all slots and the arp stay in sync.
#[derive(Clone)]
struct LoopSlot {
    state: LoopState,
    muted: bool,
    solo: bool,
    /// One entry per recorded pass; playback merges them all.
    layers: Vec<Vec<LoopEvent>>,
    /// Loop length in beats (a whole number of bars), set by the first pass.
    len_beats: f64,
    /// Transport beat the loop is phase-locked to (its beat-0).
    anchor_beat: f64,
    /// Loop position (beats) we've fired playback up to, for edge detection.
    played_to: f64,
    /// Notes currently sounding from this slot (for clean wrap / mute / clear).
    sounding: Vec<u8>,
    /// Playback modifiers (adjusted with `+`/`-` on the lane's cells).
    quantize_idx: usize, // playback grid the notes snap to (non-destructive)
    division_idx: usize, // fraction of the loop that plays before repeating
    speed_steps: i32,    // playback-rate increments from 1×
    transpose: i32,      // semitone shift applied to played notes
}

impl Default for LoopSlot {
    fn default() -> Self {
        Self {
            state: LoopState::default(),
            muted: false,
            solo: false,
            layers: Vec::new(),
            len_beats: 0.0,
            anchor_beat: 0.0,
            played_to: 0.0,
            sounding: Vec::new(),
            quantize_idx: LOOP_QUANTIZE_DEFAULT,
            division_idx: 0,
            speed_steps: 0,
            transpose: 0,
        }
    }
}

impl LoopSlot {
    fn has_content(&self) -> bool {
        !self.layers.is_empty()
    }

    /// Playback quantize grid in beats, or `None` for free (as recorded).
    fn quantize_grid(&self) -> Option<f64> {
        LOOP_QUANTIZE[self.quantize_idx.min(LOOP_QUANTIZE.len() - 1)].1
    }

    fn quantize_label(&self) -> &'static str {
        LOOP_QUANTIZE[self.quantize_idx.min(LOOP_QUANTIZE.len() - 1)].0
    }

    /// Fraction of the recorded loop that plays before repeating.
    fn division(&self) -> f64 {
        LOOP_DIVISIONS[self.division_idx.min(LOOP_DIVISIONS.len() - 1)]
    }

    fn division_label(&self) -> &'static str {
        LOOP_DIVISION_LABELS[self.division_idx.min(LOOP_DIVISION_LABELS.len() - 1)]
    }

    /// Playback-rate multiplier (1× is nominal, phase-locked to the transport).
    fn speed(&self) -> f64 {
        1.0 + self.speed_steps as f64 * LOOP_SPEED_STEP
    }

    /// Effective loop length in beats after the division.
    fn span(&self) -> f64 {
        self.len_beats * self.division()
    }
}

/// An in-progress recording pass.
struct Recording {
    slot: usize,
    /// The loop-defining first pass (bar-aligned start/stop) vs an overdub
    /// (records straight onto the already-cycling loop).
    defining: bool,
    /// Bar-aligned start (defining pass only).
    start_beat: f64,
    /// Bar-aligned stop, set on the second Space (defining pass only).
    stop_beat: Option<f64>,
    events: Vec<LoopEvent>,
    /// Pending count-in clicks `(beat, accent)`, drained as the transport
    /// reaches them (first-ever recording only). Accent marks the downbeat.
    count_in: Vec<(f64, bool)>,
}

// ---------------------------------------------------------------------------
// Drum machine (808-style step sequencer)
// ---------------------------------------------------------------------------

/// Number of sequencer tracks (rows) and steps per track.
const DRUM_TRACKS: usize = 8;
const DRUM_STEPS: usize = 16;

/// The drum kit. The order is the canonical instrument index shared with
/// `synth::DrumVoice` — do not reorder without matching the synth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DrumInst {
    Kick,
    Snare,
    Hihat,
    OpenHat,
    Cowbell,
    Tom,
    Ride,
    Clap,
    Rim,
    Clave,
    Maracas,
    Conga,
    Crash,
}

impl DrumInst {
    /// In canonical index order.
    const ALL: [DrumInst; 13] = [
        DrumInst::Kick,
        DrumInst::Snare,
        DrumInst::Hihat,
        DrumInst::OpenHat,
        DrumInst::Cowbell,
        DrumInst::Tom,
        DrumInst::Ride,
        DrumInst::Clap,
        DrumInst::Rim,
        DrumInst::Clave,
        DrumInst::Maracas,
        DrumInst::Conga,
        DrumInst::Crash,
    ];

    fn index(self) -> u8 {
        DrumInst::ALL.iter().position(|&d| d == self).unwrap_or(0) as u8
    }

    fn label(self) -> &'static str {
        match self {
            DrumInst::Kick => "kick",
            DrumInst::Snare => "snare",
            DrumInst::Hihat => "hihat",
            DrumInst::OpenHat => "openhat",
            DrumInst::Cowbell => "cowbell",
            DrumInst::Tom => "tom",
            DrumInst::Ride => "ride",
            DrumInst::Clap => "clap",
            DrumInst::Rim => "rim",
            DrumInst::Clave => "clave",
            DrumInst::Maracas => "maracas",
            DrumInst::Conga => "conga",
            DrumInst::Crash => "crash",
        }
    }

    fn from_label(s: &str) -> Option<DrumInst> {
        DrumInst::ALL.iter().copied().find(|d| d.label() == s)
    }
}

/// Number of per-track control cells (Left/Right selects, `+`/`-` adjusts):
/// instrument, release, pitch, level, pan, solo, mute, divide, speed.
const DRUM_CELLS: usize = 9;

/// One drum track: an instrument, a 16-step pattern, and per-track voice and
/// clock controls.
#[derive(Clone)]
struct DrumTrack {
    inst: DrumInst,
    steps: [bool; DRUM_STEPS],
    release: f32, // decay multiplier (0.25..4)
    pitch: i32,   // tune in semitones (-24..24)
    level: f32,   // volume (0..1.5)
    pan: f32,     // -1 (L) .. +1 (R)
    solo: bool,
    mute: bool,
    division_idx: usize, // fraction of the 16 steps that plays (see LOOP_DIVISIONS)
    speed_steps: i32,    // playback-rate increments from 1×
    last_step: i64,      // sequencer edge-detection cursor
}

impl DrumTrack {
    fn new(inst: DrumInst) -> Self {
        Self {
            inst,
            steps: [false; DRUM_STEPS],
            release: 1.0,
            pitch: 0,
            level: 1.0,
            pan: 0.0,
            solo: false,
            mute: false,
            division_idx: 0,
            speed_steps: 0,
            last_step: 0,
        }
    }

    fn pitch_mul(&self) -> f32 {
        2f32.powf(self.pitch as f32 / 12.0)
    }

    fn speed(&self) -> f64 {
        1.0 + self.speed_steps as f64 * LOOP_SPEED_STEP
    }

    fn division(&self) -> f64 {
        LOOP_DIVISIONS[self.division_idx.min(LOOP_DIVISIONS.len() - 1)]
    }

    fn division_label(&self) -> &'static str {
        LOOP_DIVISION_LABELS[self.division_idx.min(LOOP_DIVISION_LABELS.len() - 1)]
    }
}

/// Default instrument per track (reassignable with `,`/`.`): the seven-piece
/// kit, with the eighth track a second kick.
const DRUM_DEFAULTS: [DrumInst; DRUM_TRACKS] = [
    DrumInst::Kick,
    DrumInst::Snare,
    DrumInst::Hihat,
    DrumInst::OpenHat,
    DrumInst::Cowbell,
    DrumInst::Tom,
    DrumInst::Ride,
    DrumInst::Kick,
];

/// Keys that toggle the 16 steps of the selected track: `q`..`i`, then `a`..`k`.
const DRUM_STEP_KEYS: [char; DRUM_STEPS] = [
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'a', 's', 'd', 'f', 'g', 'h', 'j', 'k',
];
/// Keys that live-trigger the seven instruments: `z`..`m`.
const DRUM_TRIGGER_KEYS: [char; 7] = ['z', 'x', 'c', 'v', 'b', 'n', 'm'];

/// Ignore repeated presses of the same chord button within this window, so OS
/// key-repeat can't rapidly flip a latch on and off.
const BUTTON_DEBOUNCE: Duration = Duration::from_millis(250);

/// In the release-fallback, a single note (no chord selected) plays as a brief
/// one-shot of this length rather than latching — good for basslines/leads.
const LEAD_GATE: Duration = Duration::from_millis(160);

/// The active synth engine's name. Its parameters are exposed to the text
/// control interface namespaced under this (`subtractive.filter.cutoff`, …).
/// The subtractive engine is the first of what may be several; a future engine
/// gets its own name here and its params fall under that prefix, with the
/// `engine` state key naming whichever is live.
const SYNTH_ENGINE: &str = "subtractive";

/// Tempo bounds (BPM) and the arpeggiator's steps-per-beat: 16ths straight,
/// 16th-triplets in triplet mode.
const TEMPO_MIN: u32 = 20;
const TEMPO_MAX: u32 = 400;
const ARP_SUBDIV: u32 = 4;
const ARP_SUBDIV_TRIPLET: u32 = 6;
/// Arp phrase multipliers: each note's length as a factor of the base 16th.
/// Below 1 goes faster (32nd/64th/128th), above goes slower (8th, quarter, …).
const ARP_LENGTHS: [f32; 11] = [
    0.125, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0,
];
/// Index of the ×1 default in `ARP_LENGTHS`.
const ARP_LEN_DEFAULT: usize = 3;

/// Arpeggiator note order.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum ArpPattern {
    #[default]
    Up,
    Down,
    UpDown,
    Random,
}

impl ArpPattern {
    const ALL: [ArpPattern; 4] = [
        ArpPattern::Up,
        ArpPattern::Down,
        ArpPattern::UpDown,
        ArpPattern::Random,
    ];

    fn label(self) -> &'static str {
        match self {
            ArpPattern::Up => "up",
            ArpPattern::Down => "down",
            ArpPattern::UpDown => "up-down",
            ArpPattern::Random => "random",
        }
    }

    /// The order of pool indices this pattern steps through (Random is handled
    /// separately, so it just returns the plain order here).
    fn sequence(self, n: usize) -> Vec<usize> {
        match self {
            ArpPattern::Up | ArpPattern::Random => (0..n).collect(),
            ArpPattern::Down => (0..n).rev().collect(),
            ArpPattern::UpDown if n > 2 => (0..n).chain((1..n - 1).rev()).collect(),
            ArpPattern::UpDown => (0..n).collect(),
        }
    }
}

/// The chord currently sounding (monophonic at the chord level).
struct Held {
    /// Keyboard key that triggered it — the stable identity (survives
    /// transposition) used for locks and re-pitching.
    key: char,
    /// Absolute sounding root (the key's note at the current window).
    root: u8,
    notes: Vec<u8>,
}

/// Which screen is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Play,
    Synth,
    Drum,
}

/// An editable synth parameter. `usize` selects oscillator 0 or 1.
#[derive(Clone, Copy)]
enum Param {
    OscWave(usize),
    OscPitch(usize),
    OscFine(usize),
    OscLevel(usize),
    OscPan(usize),
    OscPw(usize),
    Sub,
    Ring,
    Fm,
    Sync,
    Pwm,
    Noise,
    AmpA,
    AmpD,
    AmpS,
    AmpR,
    Cutoff,
    Resonance,
    FiltEnvAmt,
    FMode,
    FSlope,
    FKey,
    FiltA,
    FiltD,
    FiltS,
    FiltR,
    PitchLfoRate,
    PitchLfoDepth,
    FiltLfoRate,
    FiltLfoDepth,
    Glide,
    Spread,
    Drift,
    Drive,
    Unison,
    Detune,
    Master,
}

/// Max unison voices (mirrors `synth::UNISON_MAX`).
const UNISON_MAX: i32 = 4;

impl Param {
    /// Nudge the parameter in `dir` (±1) within its range and step.
    fn adjust(self, p: &mut Patch, dir: i32) {
        let d = dir as f32;
        let bump = |v: &mut f32, step: f32, lo: f32, hi: f32| *v = (*v + step * d).clamp(lo, hi);
        match self {
            Param::OscWave(i) => p.osc[i].wave = p.osc[i].wave.cycle(dir),
            Param::OscPitch(i) => bump(&mut p.osc[i].pitch, 1.0, -24.0, 24.0),
            Param::OscFine(i) => bump(&mut p.osc[i].fine, 1.0, -100.0, 100.0),
            Param::OscLevel(i) => bump(&mut p.osc[i].level, 0.05, 0.0, 1.0),
            Param::OscPan(i) => bump(&mut p.osc[i].pan, 0.1, -1.0, 1.0),
            Param::OscPw(i) => bump(&mut p.osc[i].pw, 0.02, 0.02, 0.98),
            Param::Sub => bump(&mut p.sub, 0.05, 0.0, 1.0),
            Param::Ring => bump(&mut p.ring, 0.05, 0.0, 1.0),
            Param::Fm => bump(&mut p.fm, 0.05, 0.0, 1.0),
            Param::Sync => p.sync = dir > 0,
            Param::Pwm => bump(&mut p.pwm, 0.05, 0.0, 1.0),
            Param::Noise => bump(&mut p.noise, 0.05, 0.0, 1.0),
            Param::AmpA => bump(&mut p.amp.a, 0.01, 0.001, 4.0),
            Param::AmpD => bump(&mut p.amp.d, 0.01, 0.001, 4.0),
            Param::AmpS => bump(&mut p.amp.s, 0.05, 0.0, 1.0),
            Param::AmpR => bump(&mut p.amp.r, 0.01, 0.001, 4.0),
            Param::Cutoff => p.cutoff = (p.cutoff * 1.12f32.powi(dir)).clamp(20.0, 18000.0),
            Param::Resonance => bump(&mut p.resonance, 0.05, 0.0, 1.0),
            Param::FiltEnvAmt => bump(&mut p.filter_env_amount, 0.05, 0.0, 1.0),
            Param::FMode => p.filter_mode = p.filter_mode.cycle(dir),
            Param::FSlope => p.filter_slope = if dir > 0 { 24 } else { 12 },
            Param::FKey => bump(&mut p.filter_keytrack, 0.1, 0.0, 1.0),
            Param::FiltA => bump(&mut p.filter_env.a, 0.01, 0.001, 4.0),
            Param::FiltD => bump(&mut p.filter_env.d, 0.01, 0.001, 4.0),
            Param::FiltS => bump(&mut p.filter_env.s, 0.05, 0.0, 1.0),
            Param::FiltR => bump(&mut p.filter_env.r, 0.01, 0.001, 4.0),
            // Multiplicative so sub-1 Hz rates get very fine steps.
            Param::PitchLfoRate => {
                p.pitch_lfo_rate = (p.pitch_lfo_rate * 1.1f32.powi(dir)).clamp(0.02, 20.0)
            }
            Param::PitchLfoDepth => bump(&mut p.pitch_lfo_depth, 0.1, 0.0, 12.0),
            Param::FiltLfoRate => {
                p.filter_lfo_rate = (p.filter_lfo_rate * 1.1f32.powi(dir)).clamp(0.02, 20.0)
            }
            Param::FiltLfoDepth => bump(&mut p.filter_lfo_depth, 0.05, 0.0, 1.0),
            Param::Glide => bump(&mut p.glide, 0.01, 0.0, 1.0),
            Param::Spread => bump(&mut p.spread, 0.05, 0.0, 1.0),
            Param::Drift => bump(&mut p.drift, 0.05, 0.0, 1.0),
            Param::Drive => bump(&mut p.drive, 0.05, 0.0, 1.0),
            Param::Unison => p.unison = (p.unison as i32 + dir).clamp(1, UNISON_MAX) as u8,
            Param::Detune => bump(&mut p.detune, 1.0, 0.0, 50.0),
            Param::Master => bump(&mut p.master, 0.02, 0.0, 0.6),
        }
    }

    /// The parameter's current value, formatted for display.
    fn value(self, p: &Patch) -> String {
        match self {
            Param::OscWave(i) => p.osc[i].wave.label().to_string(),
            Param::OscPitch(i) => format!("{:+}st", p.osc[i].pitch as i32),
            Param::OscFine(i) => format!("{:+}c", p.osc[i].fine as i32),
            Param::OscLevel(i) => pct(p.osc[i].level),
            Param::OscPan(i) => pan_str(p.osc[i].pan),
            Param::OscPw(i) => pct(p.osc[i].pw),
            Param::Sub => pct(p.sub),
            Param::Ring => pct(p.ring),
            Param::Fm => pct(p.fm),
            Param::Sync => onoff(p.sync).to_string(),
            Param::Pwm => pct(p.pwm),
            Param::Noise => pct(p.noise),
            Param::AmpA => secs(p.amp.a),
            Param::AmpD => secs(p.amp.d),
            Param::AmpS => pct(p.amp.s),
            Param::AmpR => secs(p.amp.r),
            Param::Cutoff => hz(p.cutoff),
            Param::Resonance => pct(p.resonance),
            Param::FiltEnvAmt => pct(p.filter_env_amount),
            Param::FMode => p.filter_mode.label().to_string(),
            Param::FSlope => format!("{}dB", p.filter_slope),
            Param::FKey => pct(p.filter_keytrack),
            Param::FiltA => secs(p.filter_env.a),
            Param::FiltD => secs(p.filter_env.d),
            Param::FiltS => pct(p.filter_env.s),
            Param::FiltR => secs(p.filter_env.r),
            Param::PitchLfoRate => lfo_hz(p.pitch_lfo_rate),
            Param::PitchLfoDepth => format!("{:.1}st", p.pitch_lfo_depth),
            Param::FiltLfoRate => lfo_hz(p.filter_lfo_rate),
            Param::FiltLfoDepth => pct(p.filter_lfo_depth),
            Param::Glide => {
                if p.glide < 0.005 {
                    "off".to_string()
                } else {
                    secs(p.glide)
                }
            }
            Param::Spread => {
                if p.spread <= 0.0 {
                    "off".to_string()
                } else {
                    pct(p.spread)
                }
            }
            Param::Drift => pct(p.drift),
            Param::Drive => {
                if p.drive <= 0.0 {
                    "off".to_string()
                } else {
                    pct(p.drive)
                }
            }
            Param::Unison => format!("{}", p.unison),
            Param::Detune => format!("{}c", p.detune as i32),
            Param::Master => pct(p.master / 0.6),
        }
    }

    /// Stable key used in the text control interface (state + commands),
    /// namespaced under the active synth engine.
    fn key(self) -> String {
        format!("{SYNTH_ENGINE}.{}", self.field())
    }

    /// The engine-local field name (unprefixed).
    fn field(self) -> String {
        match self {
            Param::OscWave(i) => format!("osc{}.wave", i + 1),
            Param::OscPitch(i) => format!("osc{}.pitch", i + 1),
            Param::OscFine(i) => format!("osc{}.fine", i + 1),
            Param::OscLevel(i) => format!("osc{}.level", i + 1),
            Param::OscPan(i) => format!("osc{}.pan", i + 1),
            Param::OscPw(i) => format!("osc{}.pw", i + 1),
            Param::Sub => "sub".into(),
            Param::Ring => "ring".into(),
            Param::Fm => "fm".into(),
            Param::Sync => "sync".into(),
            Param::Pwm => "pwm".into(),
            Param::Noise => "noise".into(),
            Param::AmpA => "amp.attack".into(),
            Param::AmpD => "amp.decay".into(),
            Param::AmpS => "amp.sustain".into(),
            Param::AmpR => "amp.release".into(),
            Param::Cutoff => "filter.cutoff".into(),
            Param::Resonance => "filter.reso".into(),
            Param::FiltEnvAmt => "filter.env".into(),
            Param::FMode => "filter.mode".into(),
            Param::FSlope => "filter.slope".into(),
            Param::FKey => "filter.keytrack".into(),
            Param::FiltA => "filterenv.attack".into(),
            Param::FiltD => "filterenv.decay".into(),
            Param::FiltS => "filterenv.sustain".into(),
            Param::FiltR => "filterenv.release".into(),
            Param::PitchLfoRate => "pitchlfo.rate".into(),
            Param::PitchLfoDepth => "pitchlfo.depth".into(),
            Param::FiltLfoRate => "filterlfo.rate".into(),
            Param::FiltLfoDepth => "filterlfo.depth".into(),
            Param::Glide => "glide".into(),
            Param::Spread => "spread".into(),
            Param::Drift => "drift".into(),
            Param::Drive => "drive".into(),
            Param::Unison => "unison".into(),
            Param::Detune => "detune".into(),
            Param::Master => "master".into(),
        }
    }

    /// Raw (machine-parseable) value for the control interface.
    fn raw(self, p: &Patch) -> String {
        match self {
            Param::OscWave(i) => p.osc[i].wave.label().to_string(),
            Param::OscPitch(i) => format!("{}", p.osc[i].pitch as i32),
            Param::OscFine(i) => format!("{}", p.osc[i].fine as i32),
            Param::OscLevel(i) => format!("{:.2}", p.osc[i].level),
            Param::OscPan(i) => format!("{:.2}", p.osc[i].pan),
            Param::OscPw(i) => format!("{:.2}", p.osc[i].pw),
            Param::Sub => format!("{:.2}", p.sub),
            Param::Ring => format!("{:.2}", p.ring),
            Param::Fm => format!("{:.2}", p.fm),
            Param::Sync => onoff(p.sync).to_string(),
            Param::Pwm => format!("{:.2}", p.pwm),
            Param::Noise => format!("{:.2}", p.noise),
            Param::AmpA => format!("{:.3}", p.amp.a),
            Param::AmpD => format!("{:.3}", p.amp.d),
            Param::AmpS => format!("{:.2}", p.amp.s),
            Param::AmpR => format!("{:.3}", p.amp.r),
            Param::Cutoff => format!("{:.0}", p.cutoff),
            Param::Resonance => format!("{:.2}", p.resonance),
            Param::FiltEnvAmt => format!("{:.2}", p.filter_env_amount),
            Param::FMode => p.filter_mode.label().to_string(),
            Param::FSlope => format!("{}", p.filter_slope),
            Param::FKey => format!("{:.2}", p.filter_keytrack),
            Param::FiltA => format!("{:.3}", p.filter_env.a),
            Param::FiltD => format!("{:.3}", p.filter_env.d),
            Param::FiltS => format!("{:.2}", p.filter_env.s),
            Param::FiltR => format!("{:.3}", p.filter_env.r),
            Param::PitchLfoRate => format!("{:.3}", p.pitch_lfo_rate),
            Param::PitchLfoDepth => format!("{:.2}", p.pitch_lfo_depth),
            Param::FiltLfoRate => format!("{:.3}", p.filter_lfo_rate),
            Param::FiltLfoDepth => format!("{:.2}", p.filter_lfo_depth),
            Param::Glide => format!("{:.3}", p.glide),
            Param::Spread => format!("{:.2}", p.spread),
            Param::Drift => format!("{:.2}", p.drift),
            Param::Drive => format!("{:.2}", p.drive),
            Param::Unison => format!("{}", p.unison),
            Param::Detune => format!("{:.0}", p.detune),
            Param::Master => format!("{:.2}", p.master),
        }
    }

    /// Set from a raw string (control interface); returns false if unparseable.
    fn set_raw(self, p: &mut Patch, v: &str) -> bool {
        // Discrete params take words, not numbers — handle them first.
        match self {
            Param::OscWave(i) => {
                p.osc[i].wave = match v {
                    "sine" => Wave::Sine,
                    "tri" | "triangle" => Wave::Triangle,
                    "sqr" | "square" => Wave::Square,
                    _ => return false,
                };
                return true;
            }
            Param::Sync => {
                p.sync = matches!(v, "on" | "true" | "1");
                return true;
            }
            Param::FMode => {
                p.filter_mode = match v {
                    "lp" => FilterMode::Lp,
                    "hp" => FilterMode::Hp,
                    "bp" => FilterMode::Bp,
                    _ => return false,
                };
                return true;
            }
            _ => {}
        }
        let Ok(x) = v.parse::<f32>() else {
            return false;
        };
        match self {
            Param::OscWave(_) | Param::Sync | Param::FMode => {}
            Param::OscPitch(i) => p.osc[i].pitch = x.clamp(-24.0, 24.0),
            Param::OscFine(i) => p.osc[i].fine = x.clamp(-100.0, 100.0),
            Param::OscLevel(i) => p.osc[i].level = x.clamp(0.0, 1.0),
            Param::OscPan(i) => p.osc[i].pan = x.clamp(-1.0, 1.0),
            Param::OscPw(i) => p.osc[i].pw = x.clamp(0.02, 0.98),
            Param::Sub => p.sub = x.clamp(0.0, 1.0),
            Param::Ring => p.ring = x.clamp(0.0, 1.0),
            Param::Fm => p.fm = x.clamp(0.0, 1.0),
            Param::Pwm => p.pwm = x.clamp(0.0, 1.0),
            Param::Noise => p.noise = x.clamp(0.0, 1.0),
            Param::AmpA => p.amp.a = x.clamp(0.001, 4.0),
            Param::AmpD => p.amp.d = x.clamp(0.001, 4.0),
            Param::AmpS => p.amp.s = x.clamp(0.0, 1.0),
            Param::AmpR => p.amp.r = x.clamp(0.001, 4.0),
            Param::Cutoff => p.cutoff = x.clamp(20.0, 18000.0),
            Param::Resonance => p.resonance = x.clamp(0.0, 1.0),
            Param::FiltEnvAmt => p.filter_env_amount = x.clamp(0.0, 1.0),
            Param::FSlope => p.filter_slope = if x >= 18.0 { 24 } else { 12 },
            Param::FKey => p.filter_keytrack = x.clamp(0.0, 1.0),
            Param::FiltA => p.filter_env.a = x.clamp(0.001, 4.0),
            Param::FiltD => p.filter_env.d = x.clamp(0.001, 4.0),
            Param::FiltS => p.filter_env.s = x.clamp(0.0, 1.0),
            Param::FiltR => p.filter_env.r = x.clamp(0.001, 4.0),
            Param::PitchLfoRate => p.pitch_lfo_rate = x.clamp(0.02, 20.0),
            Param::PitchLfoDepth => p.pitch_lfo_depth = x.clamp(0.0, 12.0),
            Param::FiltLfoRate => p.filter_lfo_rate = x.clamp(0.02, 20.0),
            Param::FiltLfoDepth => p.filter_lfo_depth = x.clamp(0.0, 1.0),
            Param::Glide => p.glide = x.clamp(0.0, 1.0),
            Param::Spread => p.spread = x.clamp(0.0, 1.0),
            Param::Drift => p.drift = x.clamp(0.0, 1.0),
            Param::Drive => p.drive = x.clamp(0.0, 1.0),
            Param::Unison => p.unison = (x.round() as i32).clamp(1, UNISON_MAX) as u8,
            Param::Detune => p.detune = x.clamp(0.0, 50.0),
            Param::Master => p.master = x.clamp(0.0, 0.6),
        }
        true
    }
}

/// Every editable synth parameter, in editor order.
fn all_params() -> Vec<Param> {
    synth_columns()
        .iter()
        .flat_map(|col| column_params(col))
        .collect()
}

fn param_by_key(key: &str) -> Option<Param> {
    all_params().into_iter().find(|p| p.key() == key)
}

fn pct(v: f32) -> String {
    format!("{}%", (v * 100.0).round() as i32)
}

fn secs(s: f32) -> String {
    if s < 1.0 {
        format!("{}ms", (s * 1000.0).round() as i32)
    } else {
        format!("{:.2}s", s)
    }
}

fn hz(v: f32) -> String {
    if v >= 1000.0 {
        format!("{:.1}kHz", v / 1000.0)
    } else {
        format!("{}Hz", v.round() as i32)
    }
}

/// LFO rate — extra precision below 1 Hz so fine steps are visible.
fn lfo_hz(v: f32) -> String {
    if v < 1.0 {
        format!("{:.3}Hz", v)
    } else {
        format!("{:.2}Hz", v)
    }
}

fn pan_str(p: f32) -> String {
    if p.abs() < 0.05 {
        "C".to_string()
    } else if p < 0.0 {
        format!("L{}", (-p * 100.0).round() as i32)
    } else {
        format!("R{}", (p * 100.0).round() as i32)
    }
}

/// One line in the synth editor: a section heading or a labelled parameter.
enum Item {
    Head(&'static str),
    P(&'static str, Param),
}

/// The synth editor laid out in four columns.
fn synth_columns() -> [Vec<Item>; 4] {
    use Param::*;
    [
        vec![
            Item::Head("OSC 1"),
            Item::P("wave", OscWave(0)),
            Item::P("pitch", OscPitch(0)),
            Item::P("fine", OscFine(0)),
            Item::P("width", OscPw(0)),
            Item::P("level", OscLevel(0)),
            Item::P("pan", OscPan(0)),
            Item::Head("OSC 2"),
            Item::P("wave", OscWave(1)),
            Item::P("pitch", OscPitch(1)),
            Item::P("fine", OscFine(1)),
            Item::P("width", OscPw(1)),
            Item::P("level", OscLevel(1)),
            Item::P("pan", OscPan(1)),
        ],
        vec![
            Item::Head("MIX / MOD"),
            Item::P("sub", Sub),
            Item::P("ring", Ring),
            Item::P("fm", Fm),
            Item::P("sync", Sync),
            Item::P("pwm", Pwm),
            Item::P("noise", Noise),
            Item::Head("AMP ENV"),
            Item::P("attack", AmpA),
            Item::P("decay", AmpD),
            Item::P("sustain", AmpS),
            Item::P("release", AmpR),
        ],
        vec![
            Item::Head("FILTER"),
            Item::P("cutoff", Cutoff),
            Item::P("reso", Resonance),
            Item::P("env amt", FiltEnvAmt),
            Item::P("mode", FMode),
            Item::P("slope", FSlope),
            Item::P("keytrk", FKey),
            Item::Head("FILTER ENV"),
            Item::P("attack", FiltA),
            Item::P("decay", FiltD),
            Item::P("sustain", FiltS),
            Item::P("release", FiltR),
        ],
        vec![
            Item::Head("PITCH LFO"),
            Item::P("rate", PitchLfoRate),
            Item::P("depth", PitchLfoDepth),
            Item::Head("FILTER LFO"),
            Item::P("rate", FiltLfoRate),
            Item::P("depth", FiltLfoDepth),
            Item::Head("GLOBAL"),
            Item::P("glide", Glide),
            Item::P("spread", Spread),
            Item::P("drift", Drift),
            Item::P("drive", Drive),
            Item::P("unison", Unison),
            Item::P("detune", Detune),
            Item::P("volume", Master),
        ],
    ]
}

fn column_params(items: &[Item]) -> Vec<Param> {
    items
        .iter()
        .filter_map(|it| match it {
            Item::P(_, p) => Some(*p),
            Item::Head(_) => None,
        })
        .collect()
}

/// A snapshot of the chord-shaping options. Used both for the persistent
/// "working" brush and for the frozen configs locked to notes with backtick.
#[derive(Clone)]
struct ChordOptions {
    quality: Option<Quality>,
    additions: Vec<Addition>,
    voicing: i32,
    bass: Option<i32>,
    arp_on: bool,
    arp_pattern: ArpPattern,
    arp_len: usize, // index into ARP_LENGTHS
    arp_triplet: bool,
}

impl Default for ChordOptions {
    fn default() -> Self {
        Self {
            quality: None,
            additions: Vec::new(),
            voicing: 0,
            bass: None,
            arp_on: false,
            arp_pattern: ArpPattern::Up,
            arp_len: ARP_LEN_DEFAULT,
            arp_triplet: false,
        }
    }
}

pub struct App {
    tx: Sender<SynthEvent>,
    audio: AudioInfo,
    /// Live view of the synth's sounding voices, for the on-screen piano.
    monitor: Arc<VoiceMonitor>,
    /// True when the terminal reports key-release events (Kitty protocol).
    enhanced: bool,
    /// Tune chord tones to just ratios above the root (vs plain 12-TET).
    just: bool,
    /// Latch mode: a played chord keeps ringing after the key is released.
    /// User-toggleable with `q` in Kitty mode; always on in fallback (where
    /// key-release can't be detected). Default on.
    latch: bool,
    /// Latched chord quality; `None` plays single notes.
    quality: Option<Quality>,
    /// Latched additions stacked on top of the chord.
    additions: Vec<Addition>,
    /// Chord Voicing dial (inversion cascade): net clicks from neutral.
    voicing: i32,
    /// Bass dial: a separate bass note as a semitone offset above the root,
    /// placed an octave below the chord. `None` = bass engine off.
    bass: Option<i32>,
    /// Chord configs locked to keyboard keys with backtick. Playing a locked
    /// key recalls its frozen config.
    locked: BTreeMap<char, ChordOptions>,
    /// Your persistent "working" config — what a non-locked key plays.
    /// Playing a locked key doesn't disturb it, so the next non-locked key
    /// resumes whatever you had before.
    working: ChordOptions,
    /// Whether the key currently sounding was locked when it was played (so
    /// edits don't leak into `working`).
    current_locked: bool,
    /// Window offset in white-key steps (`<` / `>`): how far the keyboard's
    /// seven-key window has slid along the piano's white keys.
    window: i32,
    /// Arpeggiator on/off (`/`) — locked per note.
    arp_on: bool,
    /// Arpeggiator pattern (`1`/`2`) — locked per note.
    arp_pattern: ArpPattern,
    /// Arp phrase length as an index into `ARP_LENGTHS` (`3`/`4`).
    arp_len: usize,
    /// Triplet feel (`5`): 16th-triplet grid instead of straight 16ths.
    arp_triplet: bool,
    /// Time-signature numerator (beats per bar; denominator fixed at /4). Sets
    /// the bar length the loop recorder locks to. One of `TIME_SIGS`.
    beats_per_bar: u32,
    /// Play-page selection grid. Row 0 is the transport (Tempo · Time · Keys);
    /// rows 1..=LOOP_SLOTS are the loop lanes. `sel_col` indexes cells within
    /// the row. Arrows move it; `+`/`-` adjust the transport row; Space presses
    /// the selected loop button.
    sel_row: usize,
    sel_col: usize,
    /// The four loop slots (baked note-tapes, layered, phase-locked).
    loops: [LoopSlot; LOOP_SLOTS],
    /// The recording pass in progress, if any (only one at a time).
    rec: Option<Recording>,
    /// Drum machine: eight tracks of a 16-step sequencer (Drum view).
    drum_tracks: Vec<DrumTrack>,
    /// The selected drum track (number keys 1-8; step edits act on it).
    drum_sel: usize,
    /// Selected per-track control cell (Left/Right; `+`/`-` adjusts). See
    /// `DRUM_CELLS`: 0 inst, 1 release, 2 pitch, 3 level, 4 pan, 5 solo,
    /// 6 mute, 7 divide, 8 speed.
    drum_col: usize,
    /// Drum sequencer enabled (plays on the shared 16th grid).
    drums_on: bool,
    /// Tap-record armed (Space): drum triggers write quantized into the grid.
    drum_tap: bool,
    /// Shared clock: tempo + beat grid, synced across instances.
    transport: Transport,
    /// Arpeggiator runtime: pattern position, the note currently sounding, and
    /// the last global grid step we fired on. `rng` seeds the Random pattern.
    arp_pos: usize,
    arp_sounding: Option<u8>,
    last_step: i64,
    rng: u32,
    /// Live notes we've sent NoteOn for and not yet NoteOff'd, as
    /// `(note, freq, pan)` — lets us silence cleanly when switching chords or
    /// arp mode, and snapshot what's sounding into a starting recording.
    sent: Vec<(u8, f32, f32)>,
    /// The synth patch *target* — what the editor, presets, and text commands
    /// set, and what `state_text` reports. The sounding patch (`patch_live`)
    /// glides toward this over a beat, so switches don't jump.
    patch: Patch,
    /// The currently-sounding patch, pushed to the audio thread. Interpolated
    /// from `patch_from` toward `patch` while `patch_gliding`.
    patch_live: Patch,
    /// Snapshot of the sounding patch when the current glide began.
    patch_from: Patch,
    /// When the current patch glide started.
    patch_glide_start: Instant,
    /// True while `patch_live` is still catching up to `patch`.
    patch_gliding: bool,
    /// Currently-selected preset index (PgUp/PgDn cycle; `patch` key selects).
    patch_index: usize,
    /// This instance's editable copy of all presets. Seeded from the factory
    /// bank at startup; edits are written back to the current slot, so
    /// switching away and back recalls *your* modified version (per-pid).
    patch_bank: [Patch; crate::synth::PRESET_COUNT],
    /// Text control interface for agents (state files + command inbox).
    control: Control,
    /// Which screen is showing, and the synth-editor cursor `(column, row)`.
    view: View,
    synth_col: usize,
    synth_row: usize,
    /// Fallback lead/bass one-shot: when set, the current (single, un-chorded)
    /// note auto-releases at this time instead of latching.
    lead_off: Option<Instant>,
    /// Key physically held right now (Kitty only); `None` when nothing is held
    /// or a chord is only ringing via latch.
    held: Option<char>,
    /// The sounding chord, if any.
    current: Option<Held>,
    /// Debounce guard for the chord buttons: `(key, when)`.
    last_button: Option<(char, Instant)>,
    pub should_quit: bool,
}

impl App {
    pub fn new(
        tx: Sender<SynthEvent>,
        audio: AudioInfo,
        enhanced: bool,
        just: bool,
        monitor: Arc<VoiceMonitor>,
        transport: Transport,
    ) -> Self {
        // Seed this instance's mutable config slots from the factory presets.
        let patch_bank: [Patch; crate::synth::PRESET_COUNT] =
            crate::synth::presets().map(|(_, p)| p);
        let patch = patch_bank[0]; // startup patch = first slot
        let _ = tx.send(SynthEvent::SetPatch(patch)); // sync the audio thread
        Self {
            tx,
            audio,
            monitor,
            enhanced,
            just,
            latch: true,
            quality: None,
            additions: Vec::new(),
            voicing: 0,
            bass: None,
            locked: BTreeMap::new(),
            working: ChordOptions::default(),
            current_locked: false,
            window: 0,
            arp_on: false,
            arp_pattern: ArpPattern::Up,
            arp_len: ARP_LEN_DEFAULT,
            arp_triplet: false,
            beats_per_bar: TIME_SIGS[0],
            sel_row: 0,
            sel_col: 0,
            loops: std::array::from_fn(|_| LoopSlot::default()),
            rec: None,
            drum_tracks: DRUM_DEFAULTS.iter().map(|&inst| DrumTrack::new(inst)).collect(),
            drum_sel: 0,
            drum_col: 0,
            drums_on: true,
            drum_tap: false,
            transport,
            arp_pos: 0,
            arp_sounding: None,
            last_step: 0,
            rng: 0x9E3779B9,
            sent: Vec::new(),
            patch,
            patch_live: patch,
            patch_from: patch,
            patch_glide_start: Instant::now(),
            patch_gliding: false,
            patch_index: 0,
            patch_bank,
            control: Control::new(),
            view: View::Play,
            synth_col: 0,
            synth_row: 0,
            lead_off: None,
            held: None,
            current: None,
            last_button: None,
            should_quit: false,
        }
    }

    /// Handle one key event from the terminal.
    pub fn on_key(&mut self, key: KeyEvent) {
        // Quit: Esc or Ctrl-C only (both views).
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if key.code == KeyCode::Esc || ctrl_c {
                self.should_quit = true;
                return;
            }
        }

        // Tab cycles the views: Play → Synth → Drum → Play.
        if key.code == KeyCode::Tab {
            if key.kind == KeyEventKind::Press {
                self.view = match self.view {
                    View::Play => View::Synth,
                    View::Synth => View::Drum,
                    View::Drum => View::Play,
                };
            }
            return;
        }

        // PgUp / PgDn cycle the preset bank (both views); wraps around.
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            if key.kind == KeyEventKind::Press {
                let n = crate::synth::PRESET_COUNT;
                // PgUp = previous, PgDn = next.
                let step = if key.code == KeyCode::PageUp { n - 1 } else { 1 };
                self.load_preset((self.patch_index + step) % n);
            }
            return;
        }

        let c = char_of(key.code);

        // The Drum view reclaims the whole keyboard (z-m are drum pads, q-i/a-k
        // are step buttons), so the piano isn't live there.
        if self.view == View::Drum {
            self.drum_key(key, c);
            return;
        }

        // Piano trigger keys work in the Play and Synth views ("piano keys stay
        // piano keys").
        if let Some(ch) = c {
            if let Some(root) = note_for_key(ch, self.window) {
                match key.kind {
                    KeyEventKind::Press => self.press(ch, root),
                    KeyEventKind::Repeat => {}
                    KeyEventKind::Release => self.release(ch),
                }
                return;
            }
        }

        // Everything else is view-specific.
        match self.view {
            View::Play => self.play_key(key, c),
            View::Synth => self.synth_key(key, c),
            View::Drum => {}
        }
    }

    /// Play-view controls: grid navigation, loop buttons, latch, lock, dials,
    /// chords, arp.
    fn play_key(&mut self, key: KeyEvent, c: Option<char>) {
        // Arrows walk the selection grid: row 0 is the transport (Tempo · Time ·
        // Keys); rows below are the loop lanes and their action buttons.
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            match key.code {
                KeyCode::Left => return self.move_sel(0, -1),
                KeyCode::Right => return self.move_sel(0, 1),
                KeyCode::Up => return self.move_sel(-1, 0),
                KeyCode::Down => return self.move_sel(1, 0),
                _ => {}
            }
        }

        let Some(c) = c else {
            return;
        };

        // Space: press the selected loop button (record/overdub, mute, solo,
        // undo, reset). Debounced so key-repeat doesn't double-fire.
        if c == ' ' {
            if key.kind == KeyEventKind::Press && self.button_debounced(' ') {
                self.press_loop_button();
            }
            return;
        }

        // `/`: toggle the arpeggiator.
        if c == '/' {
            if key.kind == KeyEventKind::Press {
                self.toggle_arp();
            }
            return;
        }

        // `1` / `2`: cycle the arpeggiator pattern (debounced).
        if matches!(c, '1' | '2') {
            if key.kind == KeyEventKind::Press && self.button_debounced(c) {
                self.cycle_pattern(if c == '1' { -1 } else { 1 });
            }
            return;
        }

        // `3` / `4`: shrink / extend the arp phrase (note-length multiplier).
        if matches!(c, '3' | '4') {
            if key.kind == KeyEventKind::Press && self.button_debounced(c) {
                self.adjust_arp_length(if c == '3' { -1 } else { 1 });
            }
            return;
        }

        // `5`: toggle triplet feel.
        if c == '5' {
            if key.kind == KeyEventKind::Press && self.button_debounced(c) {
                self.toggle_triplet();
            }
            return;
        }

        // `q`: toggle latch mode (Kitty) / cancel the sounding chord (fallback).
        if c == 'q' {
            if key.kind == KeyEventKind::Press {
                self.handle_q();
            }
            return;
        }

        // Backtick: lock/unlock the current chord config to the current note.
        // No debounce — an unlock-then-relock is a deliberate quick double-tap,
        // and a tap never key-repeats anyway.
        if c == '`' {
            if key.kind == KeyEventKind::Press {
                self.toggle_lock();
            }
            return;
        }

        // Voicing (`;`/`'`) and bass (`[`/`]`) dials. Act on press AND repeat so
        // holding a key sweeps continuously, like turning a knob.
        if matches!(c, ';' | '\'' | '[' | ']') {
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                self.turn_dial(c);
            }
            return;
        }

        // Adjust the selected field: `-`/`<`/`,` down, `=`/`+`/`>`/`.` up. Act on
        // press AND repeat so holding sweeps (e.g. tempo, transpose).
        if matches!(c, '-' | '<' | ',' | '=' | '+' | '>' | '.') {
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                let dir = if matches!(c, '-' | '<' | ',') { -1 } else { 1 };
                self.adjust_field(dir);
            }
            return;
        }

        // Chord buttons: quality (7 8 9 0) and additions (u i o p). Toggle on
        // the press edge only, debounced so key-repeat can't flip them.
        if (quality_for_key(c).is_some() || addition_for_key(c).is_some())
            && key.kind == KeyEventKind::Press
            && self.button_debounced(c)
        {
            self.toggle_button(c);
        }
    }

    /// Drum-view controls: number keys select a track, `q`-`i`/`a`-`k` toggle
    /// its 16 steps, `z`-`m` live-trigger the kit, `,`/`.` (or `-`/`+`) change
    /// the selected track's instrument, and Space arms tap-record.
    fn drum_key(&mut self, key: KeyEvent, c: Option<char>) {
        // Arrows: Up/Down move the selected track, Left/Right the control cell.
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            match key.code {
                KeyCode::Up => {
                    self.drum_sel = self.drum_sel.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    self.drum_sel = (self.drum_sel + 1).min(DRUM_TRACKS - 1);
                    return;
                }
                KeyCode::Left => {
                    self.drum_col = self.drum_col.saturating_sub(1);
                    return;
                }
                KeyCode::Right => {
                    self.drum_col = (self.drum_col + 1).min(DRUM_CELLS - 1);
                    return;
                }
                _ => {}
            }
        }
        let Some(c) = c else {
            return;
        };

        // Space: arm/disarm tap-record.
        if c == ' ' {
            if key.kind == KeyEventKind::Press && self.button_debounced(' ') {
                self.drum_tap = !self.drum_tap;
            }
            return;
        }

        // Number keys 1-8: select the track (keeps the step layout in place).
        if let Some(d) = c.to_digit(10) {
            if (1..=DRUM_TRACKS as u32).contains(&d) {
                if key.kind == KeyEventKind::Press {
                    self.drum_sel = (d - 1) as usize;
                }
                return;
            }
        }

        // `,`/`.` (or `-`/`+`) adjust the selected control cell. Continuous
        // cells sweep on press+repeat; the solo/mute toggles fire once (press
        // edge, debounced so key-repeat can't flap them).
        if matches!(c, ',' | '<' | '.' | '>' | '-' | '=' | '+') {
            let toggle_cell = matches!(self.drum_col, 5 | 6);
            let act = if toggle_cell {
                key.kind == KeyEventKind::Press && self.button_debounced(c)
            } else {
                matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            };
            if act {
                let dir = if matches!(c, ',' | '<' | '-') { -1 } else { 1 };
                self.drum_adjust(dir);
            }
            return;
        }

        // `z`-`m`: live-trigger tracks 1-7 (and tap into the grid if armed).
        if let Some(t) = DRUM_TRIGGER_KEYS.iter().position(|&k| k == c) {
            if key.kind == KeyEventKind::Press {
                self.drum_trigger(t);
            }
            return;
        }

        // `q`-`i` / `a`-`k`: toggle the selected track's steps.
        if let Some(step) = DRUM_STEP_KEYS.iter().position(|&k| k == c) {
            if key.kind == KeyEventKind::Press && self.button_debounced(c) {
                let t = self.drum_sel;
                self.drum_tracks[t].steps[step] ^= true;
            }
        }
    }

    /// Adjust the selected drum control cell of the selected track by `dir`.
    fn drum_adjust(&mut self, dir: i32) {
        let t = self.drum_sel;
        let raw = self.transport.step_position(ARP_SUBDIV); // for divide/speed resync
        let tr = &mut self.drum_tracks[t];
        match self.drum_col {
            0 => {
                let cur = DrumInst::ALL.iter().position(|&d| d == tr.inst).unwrap_or(0) as i32;
                let n = DrumInst::ALL.len() as i32;
                tr.inst = DrumInst::ALL[(cur + dir).rem_euclid(n) as usize];
            }
            1 => tr.release = (tr.release + dir as f32 * 0.25).clamp(0.25, 4.0),
            2 => tr.pitch = (tr.pitch + dir).clamp(-24, 24),
            3 => tr.level = (tr.level + dir as f32 * 0.1).clamp(0.0, 1.5),
            4 => tr.pan = (tr.pan + dir as f32 * 0.2).clamp(-1.0, 1.0),
            5 => tr.solo = !tr.solo, // +/- both toggle
            6 => tr.mute = !tr.mute,
            7 => {
                let n = LOOP_DIVISIONS.len() as i32;
                tr.division_idx = (tr.division_idx as i32 + dir).clamp(0, n - 1) as usize;
                tr.last_step = (raw * tr.speed()).floor() as i64; // resync, no burst
            }
            8 => {
                tr.speed_steps =
                    (tr.speed_steps + dir).clamp(LOOP_SPEED_MIN_STEPS, LOOP_SPEED_MAX_STEPS);
                tr.last_step = (raw * tr.speed()).floor() as i64;
            }
            _ => {}
        }
    }

    /// Set a drum track's fields from the text interface.
    fn drum_set(&mut self, slot: usize, field: &str, value: &str) {
        let tr = &mut self.drum_tracks[slot];
        match field {
            "inst" => {
                if let Some(inst) = DrumInst::from_label(value) {
                    tr.inst = inst;
                }
            }
            "steps" => {
                let mut steps = [false; DRUM_STEPS];
                for (i, ch) in value.chars().take(DRUM_STEPS).enumerate() {
                    steps[i] = matches!(ch, 'x' | 'X' | '1' | '#' | '*');
                }
                tr.steps = steps;
            }
            "release" => {
                if let Ok(v) = value.parse::<f32>() {
                    tr.release = v.clamp(0.25, 4.0);
                }
            }
            "pitch" => {
                if let Ok(v) = value.parse::<i32>() {
                    tr.pitch = v.clamp(-24, 24);
                }
            }
            "level" => {
                if let Ok(v) = value.parse::<f32>() {
                    tr.level = v.clamp(0.0, 1.5);
                }
            }
            "pan" => {
                if let Ok(v) = value.parse::<f32>() {
                    tr.pan = v.clamp(-1.0, 1.0);
                }
            }
            "solo" => tr.solo = value == "on",
            "mute" => tr.mute = value == "on",
            "div" | "division" => {
                if let Some(i) = LOOP_DIVISION_LABELS.iter().position(|&l| l == value) {
                    tr.division_idx = i;
                }
            }
            "speed" => {
                if let Ok(mult) = value.trim_end_matches('x').parse::<f64>() {
                    let steps = ((mult - 1.0) / LOOP_SPEED_STEP).round() as i32;
                    tr.speed_steps = steps.clamp(LOOP_SPEED_MIN_STEPS, LOOP_SPEED_MAX_STEPS);
                }
            }
            _ => {}
        }
    }

    /// The DrumHit params (inst, pitch-mul, release, level, pan) for a track.
    fn drum_hit_event(tr: &DrumTrack) -> SynthEvent {
        SynthEvent::DrumHit {
            inst: tr.inst.index(),
            pitch: tr.pitch_mul(),
            release: tr.release,
            level: tr.level,
            pan: tr.pan,
        }
    }

    /// Live-trigger a track's drum; tap it into the grid at the nearest step if
    /// tap-record is armed.
    fn drum_trigger(&mut self, track: usize) {
        if track >= self.drum_tracks.len() {
            return;
        }
        let _ = self.tx.send(Self::drum_hit_event(&self.drum_tracks[track]));
        if self.drum_tap {
            let step = self.nearest_drum_step();
            self.drum_tracks[track].steps[step] = true;
        }
    }

    /// The nearest 16th step index (for quantized tap-record).
    fn nearest_drum_step(&self) -> usize {
        let pos = self.transport.step_position(ARP_SUBDIV);
        (pos.round() as i64).rem_euclid(DRUM_STEPS as i64) as usize
    }

    /// Advance the drum sequencer, firing each track's active step at its own
    /// speed and divide. Runs every frame (regardless of view) so a groove
    /// keeps playing across tabs.
    fn tick_drums(&mut self) {
        let raw = self.transport.step_position(ARP_SUBDIV);
        if !self.drums_on {
            for tr in &mut self.drum_tracks {
                tr.last_step = (raw * tr.speed()).floor() as i64;
            }
            return;
        }
        let any_solo = self.drum_tracks.iter().any(|t| t.solo);
        let mut hits: Vec<SynthEvent> = Vec::new();
        for tr in &mut self.drum_tracks {
            let step = (raw * tr.speed()).floor() as i64;
            if step <= tr.last_step {
                tr.last_step = step; // handle backward jumps (tempo/speed change)
                continue;
            }
            tr.last_step = step;
            if tr.mute || (any_solo && !tr.solo) {
                continue;
            }
            let len = (DRUM_STEPS as f64 * tr.division()).round() as i64;
            if len <= 0 {
                continue;
            }
            let idx = step.rem_euclid(len) as usize;
            if idx < DRUM_STEPS && tr.steps[idx] {
                hits.push(Self::drum_hit_event(tr));
            }
        }
        for h in hits {
            let _ = self.tx.send(h);
        }
    }

    /// Synth-view controls: arrows navigate the parameter grid, `-`/`+` adjust
    /// (with `<`/`,` and `>`/`.` as equivalents, matching the play panel).
    fn synth_key(&mut self, key: KeyEvent, c: Option<char>) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        match key.code {
            KeyCode::Up => return self.synth_nav(0, -1),
            KeyCode::Down => return self.synth_nav(0, 1),
            KeyCode::Left => return self.synth_nav(-1, 0),
            KeyCode::Right => return self.synth_nav(1, 0),
            _ => {}
        }
        match c {
            Some('-') | Some('<') | Some(',') => self.synth_adjust(-1),
            Some('+') | Some('=') | Some('>') | Some('.') => self.synth_adjust(1),
            _ => {}
        }
    }

    /// Move the synth-editor cursor within the parameter grid.
    fn synth_nav(&mut self, dcol: i32, drow: i32) {
        let cols = synth_columns();
        let col = (self.synth_col as i32 + dcol).clamp(0, cols.len() as i32 - 1) as usize;
        let n = column_params(&cols[col]).len() as i32;
        let row = (self.synth_row as i32 + drow).clamp(0, n.max(1) - 1) as usize;
        self.synth_col = col;
        self.synth_row = row;
    }

    /// Adjust the selected parameter and push the new patch to the synth.
    fn synth_adjust(&mut self, dir: i32) {
        let cols = synth_columns();
        let params = column_params(&cols[self.synth_col]);
        if let Some(param) = params.get(self.synth_row.min(params.len().saturating_sub(1))) {
            param.adjust(&mut self.patch, dir);
            self.retarget_patch();
        }
    }

    /// Switch to config slot `index` (wrapping), gliding into it over a beat.
    /// The current slot's edits are saved first, so returning to it later
    /// recalls your modified version (slots are mutable and per-instance).
    fn load_preset(&mut self, index: usize) {
        self.patch_bank[self.patch_index] = self.patch; // remember current edits
        self.patch_index = index % self.patch_bank.len();
        self.patch = self.patch_bank[self.patch_index];
        self.retarget_patch();
    }

    /// Begin (or restart) a glide of the sounding patch toward `self.patch`,
    /// starting from wherever the sound currently is.
    fn retarget_patch(&mut self) {
        self.patch_from = self.patch_live;
        self.patch_glide_start = Instant::now();
        self.patch_gliding = true;
    }

    /// How long a patch glide takes: one beat, kept fast (0.12–0.6 s) so it's
    /// gradual but never sluggish at slow tempos.
    fn patch_glide_secs(&self) -> f32 {
        (60.0 / self.transport.tempo() as f32).clamp(0.12, 0.6)
    }

    /// Advance the sounding patch toward the target; push it to the synth.
    /// Called every frame from `tick`.
    fn update_patch_glide(&mut self) {
        if !self.patch_gliding {
            return;
        }
        let dur = self.patch_glide_secs();
        let t = (self.patch_glide_start.elapsed().as_secs_f32() / dur).min(1.0);
        self.patch_live = Patch::lerp(&self.patch_from, &self.patch, t);
        let _ = self.tx.send(SynthEvent::SetPatch(self.patch_live));
        if t >= 1.0 {
            self.patch_gliding = false;
        }
    }

    fn handle_q(&mut self) {
        if self.enhanced {
            self.latch = !self.latch;
            // Leaving latch mode with nothing held means silence.
            if !self.latch {
                self.stop_current();
            }
        } else {
            // Fallback: chords ring forever, so `q` is the panic / cancel.
            self.stop_current();
        }
    }

    fn press(&mut self, key: char, root: u8) {
        // Fallback lead/bass mode (no chord selected): every hit is a fresh
        // brief note, so skip the re-press short-circuit and always re-play.
        let lead_mode = !self.enhanced && self.quality.is_none();

        // Re-pressing the key that's already sounding *at the same pitch* — a
        // key-repeat or redundant press. (If transpose has since moved this key
        // to a new pitch, fall through and re-trigger at that pitch.)
        if !lead_mode && matches!(&self.current, Some(h) if h.key == key && h.root == root) {
            self.held = Some(key);
            // A locked note snaps back to its saved config, undoing any
            // ephemeral edits (revoice is a no-op if nothing changed).
            if let Some(lock) = self.locked.get(&key).cloned() {
                self.apply(lock);
                self.revoice();
            }
            if self.arp_on {
                self.arp_pos = 0; // re-click restarts the arp pattern (on the grid)
            } else if self.enhanced {
                // Re-hitting a ringing (latched/held) chord re-strikes it — the
                // synth re-gates its envelopes. Kitty only, so fallback
                // key-repeat can't machine-gun it.
                self.retrigger();
            }
            return;
        }
        self.held = Some(key);
        // A locked key recalls its frozen config; any other key plays the
        // working brush (which locked keys never disturb).
        let lock = self.locked.get(&key).cloned();
        let locked = lock.is_some();
        let config = lock.unwrap_or_else(|| self.working.clone());
        let was_arping = self.arp_on;
        self.current_locked = locked;
        self.apply(config);

        if self.arp_on && was_arping && self.current.is_some() {
            // Arp → arp chord change: swap the chord in place but leave the
            // clock alone, so the new chord takes over on the next step and the
            // pulse never drifts. The pattern restarts from the bottom.
            let notes = self.voiced(root);
            self.current = Some(Held { key, root, notes });
            self.arp_pos = 0;
        } else {
            // Chord mode, or (re)starting the arp from a strum/silence.
            self.play(key, root); // silences the previous chord itself
        }

        // In the fallback with no chord selected, a note is a brief one-shot
        // (lead/bass) rather than a latched chord.
        self.lead_off = if !self.enhanced && !self.arp_on && self.quality.is_none() {
            Some(Instant::now() + LEAD_GATE)
        } else {
            None
        };
    }

    /// Lock the current chord config to the current key (keyed by the physical
    /// key, so it survives transposition), or unlock it if already locked. The
    /// lock is a frozen snapshot — later edits don't change it, only re-locking.
    fn toggle_lock(&mut self) {
        let Some(key) = self.current.as_ref().map(|h| h.key) else {
            return; // nothing playing to lock
        };
        if self.locked.remove(&key).is_none() {
            self.locked.insert(key, self.live_config());
        }
    }

    /// Number of selectable cells in play-page row `row` (0 = transport;
    /// 1..=LOOP_SLOTS = loop lanes, whose cells appear once recorded).
    fn row_width(&self, row: usize) -> usize {
        if row == 0 {
            3 // tempo, time-sig, keyboard
        } else if self.loops[row - 1].has_content() {
            LOOP_CELLS // loop, mute, solo, undo, div, speed, xpose, reset
        } else {
            1 // just the (empty) loop cell
        }
    }

    /// Move the selection in the play-page grid, clamped; clamp the column to
    /// the destination row's width.
    fn move_sel(&mut self, drow: i32, dcol: i32) {
        let rows = 1 + LOOP_SLOTS as i32; // transport + loop lanes
        self.sel_row = (self.sel_row as i32 + drow).clamp(0, rows - 1) as usize;
        let w = self.row_width(self.sel_row) as i32;
        self.sel_col = (self.sel_col as i32 + dcol).clamp(0, w - 1) as usize;
        // Landing on a new row via up/down can leave the column past its end.
        self.sel_col = self.sel_col.min(w as usize - 1);
    }

    /// Adjust the selected field by `dir` (±1): the transport row's tempo /
    /// time-sig / keyboard, or a loop lane's division / speed / transpose.
    fn adjust_field(&mut self, dir: i32) {
        if self.sel_row == 0 {
            match self.sel_col {
                0 => {
                    // Tempo, ±1 BPM (shared across instances via the transport).
                    let t = (self.transport.tempo() as i32 + dir)
                        .clamp(TEMPO_MIN as i32, TEMPO_MAX as i32) as u32;
                    self.transport.set_tempo(t);
                }
                1 => self.cycle_time_sig(dir),
                2 => self.transpose_by(dir),
                _ => {}
            }
            return;
        }
        // A loop lane: quantize (1), div (5), speed (6), transpose (7) are
        // +/- controlled; the rest are Space buttons.
        let slot = self.sel_row - 1;
        if !self.loops[slot].has_content() {
            return;
        }
        match self.sel_col {
            1 => {
                let n = LOOP_QUANTIZE.len() as i32;
                self.loops[slot].quantize_idx =
                    (self.loops[slot].quantize_idx as i32 + dir).clamp(0, n - 1) as usize;
                // No resync: quantize is applied per-event at playback.
            }
            5 => {
                let n = LOOP_DIVISIONS.len() as i32;
                self.loops[slot].division_idx =
                    (self.loops[slot].division_idx as i32 + dir).clamp(0, n - 1) as usize;
                self.resync_slot(slot);
            }
            6 => {
                self.loops[slot].speed_steps = (self.loops[slot].speed_steps + dir)
                    .clamp(LOOP_SPEED_MIN_STEPS, LOOP_SPEED_MAX_STEPS);
                self.resync_slot(slot);
            }
            7 => {
                self.loops[slot].transpose = (self.loops[slot].transpose + dir)
                    .clamp(-LOOP_TRANSPOSE_RANGE, LOOP_TRANSPOSE_RANGE);
                self.resync_slot(slot);
            }
            _ => {}
        }
    }

    /// Re-align a slot's playback after a division/speed/transpose change:
    /// silence its notes and reset the play cursor to the current position so
    /// the change doesn't burst events or hang notes.
    fn resync_slot(&mut self, slot: usize) {
        self.force_off_slot(slot);
        let now = self.now_beats();
        let s = &self.loops[slot];
        let span = s.span();
        self.loops[slot].played_to = if span > 0.0 {
            ((now - s.anchor_beat) * s.speed()).rem_euclid(span)
        } else {
            0.0
        };
    }

    /// Cycle the time signature through `TIME_SIGS` by `dir`.
    fn cycle_time_sig(&mut self, dir: i32) {
        let i = TIME_SIGS
            .iter()
            .position(|&b| b == self.beats_per_bar)
            .unwrap_or(0) as i32;
        let n = TIME_SIGS.len() as i32;
        self.beats_per_bar = TIME_SIGS[(i + dir).rem_euclid(n) as usize];
    }

    /// The stable name of the currently-selected cell (for the `field` state
    /// key): `tempo`/`timesig`/`keyboard`, or `loopN[.button]`.
    fn selected_field_name(&self) -> String {
        match self.sel_row {
            0 => ["tempo", "timesig", "keyboard"][self.sel_col.min(2)].to_string(),
            r => {
                let btn = ["", ".quantize", ".mute", ".solo", ".undo", ".div", ".speed",
                    ".transpose", ".reset"][self.sel_col.min(LOOP_CELLS - 1)];
                format!("loop{}{}", r, btn)
            }
        }
    }

    /// Point the selection at a named transport field or loop cell.
    fn select_field(&mut self, name: &str) {
        match name {
            "tempo" => (self.sel_row, self.sel_col) = (0, 0),
            "timesig" => (self.sel_row, self.sel_col) = (0, 1),
            "keyboard" => (self.sel_row, self.sel_col) = (0, 2),
            _ => {
                if let Some(rest) = name.strip_prefix("loop") {
                    let mut it = rest.splitn(2, '.');
                    if let Some(n) = it.next().and_then(|s| s.parse::<usize>().ok()) {
                        if (1..=LOOP_SLOTS).contains(&n) {
                            let col = match it.next() {
                                Some("quantize") => 1,
                                Some("mute") => 2,
                                Some("solo") => 3,
                                Some("undo") => 4,
                                Some("div") => 5,
                                Some("speed") => 6,
                                Some("transpose") => 7,
                                Some("reset") => 8,
                                _ => 0,
                            };
                            self.sel_row = n;
                            self.sel_col = col.min(self.row_width(n) - 1);
                        }
                    }
                }
            }
        }
    }

    fn transpose_by(&mut self, delta: i32) {
        self.window = (self.window + delta).clamp(-WINDOW_RANGE, WINDOW_RANGE);
        if self.enhanced {
            if let Some(key) = self.held {
                match note_for_key(key, self.window) {
                    Some(root) => self.play(key, root),
                    None => self.stop_current(), // held key fell into a gap
                }
            }
        }
    }

    /// Load a config into the live chord-shaping fields.
    fn apply(&mut self, cfg: ChordOptions) {
        self.quality = cfg.quality;
        self.additions = cfg.additions;
        self.voicing = cfg.voicing;
        self.bass = cfg.bass;
        self.arp_on = cfg.arp_on;
        self.arp_pattern = cfg.arp_pattern;
        self.arp_len = cfg.arp_len;
        self.arp_triplet = cfg.arp_triplet;
    }

    /// Snapshot the live chord-shaping fields.
    fn live_config(&self) -> ChordOptions {
        ChordOptions {
            quality: self.quality,
            additions: self.additions.clone(),
            voicing: self.voicing,
            bass: self.bass,
            arp_on: self.arp_on,
            arp_pattern: self.arp_pattern,
            arp_len: self.arp_len,
            arp_triplet: self.arp_triplet,
        }
    }

    /// Persist an edit into the working brush — unless we're playing a locked
    /// note, whose edits are ephemeral (they don't touch the brush or the lock).
    fn sync_working(&mut self) {
        if !(self.current.is_some() && self.current_locked) {
            self.working = self.live_config();
        }
    }

    fn release(&mut self, key: char) {
        if self.held == Some(key) {
            self.held = None;
        }
        // Only "key-press defined" mode (Kitty, latch off) stops on release.
        if self.enhanced
            && !self.latch
            && matches!(&self.current, Some(h) if h.key == key)
        {
            self.stop_current();
        }
    }

    /// Returns true if a chord-button press should act (not a debounced repeat).
    fn button_debounced(&mut self, key: char) -> bool {
        let now = Instant::now();
        if let Some((last, when)) = self.last_button {
            if last == key && now.duration_since(when) < BUTTON_DEBOUNCE {
                return false;
            }
        }
        self.last_button = Some((key, now));
        true
    }

    /// Flip the quality/addition a button controls, then re-voice any chord.
    fn toggle_button(&mut self, c: char) {
        if let Some(quality) = quality_for_key(c) {
            self.quality = if self.quality == Some(quality) {
                None // press the lit button again to turn it off
            } else {
                Some(quality)
            };
        } else if let Some(addition) = addition_for_key(c) {
            if let Some(pos) = self.additions.iter().position(|a| *a == addition) {
                self.additions.remove(pos);
            } else {
                self.additions.push(addition);
            }
        }
        self.sync_working();
        self.revoice();
    }

    /// Update the sounding chord to the current quality/additions without
    /// retriggering tones it shares with the previous voicing.
    fn revoice(&mut self) {
        let (root, old) = match &self.current {
            Some(h) => (h.root, h.notes.clone()),
            None => return,
        };
        let new = self.voiced(root);

        if !self.arp_on {
            // Chord mode: move only the tones that changed.
            for note in &old {
                if !new.contains(note) {
                    self.send_off(*note);
                }
            }
            let count = new.len();
            for (i, note) in new.iter().enumerate() {
                if !old.contains(note) {
                    self.send_on(root, *note, self.spread_pan(i, count));
                }
            }
        }
        // Arp mode: just swap the pool; the clock keeps stepping over the new
        // notes on its own.

        if let Some(h) = &mut self.current {
            h.notes = new; // same root
        }
    }

    /// Turn one of the voicing dials, then re-voice any sounding chord.
    fn turn_dial(&mut self, c: char) {
        match c {
            // Chord Voicing: ; lowers (highest note down), ' raises (lowest up).
            ';' => self.voicing = (self.voicing - 1).max(-VOICING_RANGE),
            '\'' => self.voicing = (self.voicing + 1).min(VOICING_RANGE),
            // Bass: ] engages/raises, [ lowers and switches off below the root.
            ']' => {
                self.bass = Some(match self.bass {
                    None => 0,
                    Some(o) => (o + 1).min(BASS_MAX),
                });
            }
            '[' => {
                self.bass = match self.bass {
                    Some(o) if o > 0 => Some(o - 1),
                    _ => None, // at/below root-in-bass → bass off
                };
            }
            _ => {}
        }
        self.sync_working();
        self.revoice();
    }

    /// The full set of MIDI notes to sound: the chord run through the voicing
    /// cascade, plus the bass note if the bass engine is on.
    fn voiced(&self, root: u8) -> Vec<u8> {
        let base = chord_notes(root, self.quality, &self.additions);
        let mut notes = voice_chord(&base, self.voicing);
        if let Some(offset) = self.bass {
            let bass = (root as i32 - 12 + offset).clamp(0, 127) as u8;
            if !notes.contains(&bass) {
                notes.push(bass);
            }
        }
        notes.sort_unstable();
        notes.dedup();
        notes
    }

    /// (Re)play a chord for `key` at `root`, replacing whatever's sounding. In
    /// chord mode every voiced note sounds at once; in arp mode the clock steps
    /// through them one at a time.
    fn play(&mut self, key: char, root: u8) {
        self.silence();
        let notes = self.voiced(root);
        if self.arp_on {
            self.arp_pos = 0;
            self.arp_sounding = None;
            // Catch up to the shared grid so the first note lands on the next
            // step in lockstep with any other instances.
            self.last_step = self.arp_note_step();
        } else {
            let count = notes.len();
            for (i, &note) in notes.iter().enumerate() {
                self.send_on(root, note, self.spread_pan(i, count));
            }
        }
        self.current = Some(Held { key, root, notes });
    }

    fn stop_current(&mut self) {
        self.silence();
        self.current = None;
        self.arp_sounding = None;
        self.lead_off = None;
    }

    /// Re-strike the currently sounding chord: re-send NoteOn for each tone so
    /// the synth restarts its amp/filter envelopes (no note-off, so no gap).
    fn retrigger(&mut self) {
        let (root, notes) = match self.current.as_ref() {
            Some(h) => (h.root, h.notes.clone()),
            None => return,
        };
        let count = notes.len();
        for (i, &id) in notes.iter().enumerate() {
            let freq = tone_frequency(root, id, self.just);
            let pan = self.spread_pan(i, count);
            let _ = self.tx.send(SynthEvent::NoteOn { id: voice_id(LIVE_SOURCE, id), freq, pan });
        }
    }

    /// Note-off everything the live keyboard is sounding.
    fn silence(&mut self) {
        for (id, _, _) in std::mem::take(&mut self.sent) {
            let _ = self.tx.send(SynthEvent::NoteOff { id: voice_id(LIVE_SOURCE, id) });
            self.capture(id, false, 0.0, 0.0);
        }
    }

    /// Start tone `id`, tuned relative to `root`, panned by `pan` (stereo spread).
    fn send_on(&mut self, root: u8, id: u8, pan: f32) {
        if !self.sent.iter().any(|&(n, _, _)| n == id) {
            let freq = tone_frequency(root, id, self.just);
            let _ = self.tx.send(SynthEvent::NoteOn { id: voice_id(LIVE_SOURCE, id), freq, pan });
            self.sent.push((id, freq, pan));
            self.capture(id, true, freq, pan);
        }
    }

    /// Stereo-spread pan for the note at `index` of a chord of `count` notes:
    /// symmetric around center so the chord never leans to one side, and
    /// re-centered per chord (independent of where it sits on the keyboard).
    fn spread_pan(&self, index: usize, count: usize) -> f32 {
        if count <= 1 || self.patch.spread <= 0.0 {
            0.0
        } else {
            self.patch.spread * (2.0 * index as f32 / (count - 1) as f32 - 1.0)
        }
    }

    fn send_off(&mut self, id: u8) {
        if let Some(pos) = self.sent.iter().position(|&(n, _, _)| n == id) {
            self.sent.remove(pos);
            let _ = self.tx.send(SynthEvent::NoteOff { id: voice_id(LIVE_SOURCE, id) });
            self.capture(id, false, 0.0, 0.0);
        }
    }

    /// Write the notes the live keyboard is currently sounding into the running
    /// recording as note-ons, so a chord or arp already latched when recording
    /// begins is captured too (not only changes made inside the window).
    fn snapshot_live(&mut self) {
        for (note, freq, pan) in self.sent.clone() {
            self.capture(note, true, freq, pan);
        }
    }

    // --- Loop recorder ------------------------------------------------------

    /// Transport position in beats since the shared epoch.
    fn now_beats(&self) -> f64 {
        self.transport.step_position(1)
    }

    /// Bar length in beats (the time-signature numerator).
    fn bar_beats(&self) -> f64 {
        self.beats_per_bar as f64
    }

    /// The next bar downbeat at or after `from` (strictly after, so a fresh
    /// bar always follows).
    fn next_bar(&self, from: f64) -> f64 {
        let b = self.bar_beats();
        (from / b).floor() * b + b
    }

    /// If a recording is running, append this live note event to it, timed to
    /// the loop. Called from `send_on`/`send_off`/`silence`, so a loop captures
    /// exactly the notes that sounded — chords, arps, voicing/bass/addition
    /// changes and all — baked in.
    fn capture(&mut self, note: u8, on: bool, freq: f32, pan: f32) {
        if self.rec.is_none() {
            return;
        }
        let now = self.now_beats();
        let (slot, defining, start) = {
            let r = self.rec.as_ref().unwrap();
            (r.slot, r.defining, r.start_beat)
        };
        let beat = if defining {
            if now < start {
                return; // still armed — capture only once past the bar line
            }
            now - start
        } else {
            let s = &self.loops[slot];
            (now - s.anchor_beat).rem_euclid(s.len_beats)
        };
        self.rec
            .as_mut()
            .unwrap()
            .events
            .push(LoopEvent { beat, on, note, freq, pan });
    }

    /// Space on the selected grid cell.
    fn press_loop_button(&mut self) {
        if self.sel_row == 0 {
            return; // transport row — Space does nothing
        }
        let slot = self.sel_row - 1;
        self.sel_col = self.sel_col.min(self.row_width(self.sel_row) - 1);
        match self.sel_col {
            0 => self.loop_record_button(slot),
            2 => self.loops[slot].muted = !self.loops[slot].muted,
            3 => self.loops[slot].solo = !self.loops[slot].solo,
            4 => self.loop_undo(slot),
            8 => self.loop_reset(slot),
            // 1 quantize, 5 div, 6 speed, 7 transpose are +/- controlled.
            _ => {}
        }
    }

    /// The loop cell (col 0): start recording, stop the pass, or start an
    /// overdub — depending on what's happening for this slot.
    fn loop_record_button(&mut self, slot: usize) {
        match &self.rec {
            Some(r) if r.slot == slot => self.stop_recording(),
            Some(_) => {} // another slot is recording — ignore
            None => self.start_recording(slot),
        }
    }

    fn start_recording(&mut self, slot: usize) {
        let now = self.now_beats();
        if self.loops[slot].has_content() {
            // Overdub straight onto the cycling loop (no bar wait); make sure
            // it's audible so you hear what you're layering over.
            self.loops[slot].muted = false;
            self.rec = Some(Recording {
                slot,
                defining: false,
                start_beat: 0.0,
                stop_beat: None,
                events: Vec::new(),
                count_in: Vec::new(),
            });
            self.snapshot_live(); // capture any chord/arp already held
        } else {
            // Loop-defining pass: arm and start on the next bar line. When it's
            // the first-ever loop (nothing else to play against), give a full
            // bar of quarter-note count-in clicks leading in.
            let bar = self.bar_beats();
            let first_ever = !self.loops.iter().any(|s| s.has_content());
            let mut start = self.next_bar(now);
            if first_ever && start - now < bar {
                start += bar; // ensure a full, clean count-in bar before start
            }
            let count_in = if first_ever {
                (0..self.beats_per_bar)
                    .map(|k| (start - bar + k as f64, k == 0))
                    .filter(|&(b, _)| b >= now)
                    .collect()
            } else {
                Vec::new()
            };
            self.rec = Some(Recording {
                slot,
                defining: true,
                start_beat: start,
                stop_beat: None,
                events: Vec::new(),
                count_in,
            });
            self.loops[slot].state = LoopState::Armed;
        }
    }

    /// Second Space on the recording slot: for the defining pass, mark a
    /// bar-aligned stop (finalised in `tick`); for an overdub, commit now.
    fn stop_recording(&mut self) {
        let Some(rec) = self.rec.as_ref() else { return };
        let slot = rec.slot;
        if rec.defining {
            let now = self.now_beats();
            let stop = self.next_bar(now).max(rec.start_beat + self.bar_beats());
            self.rec.as_mut().unwrap().stop_beat = Some(stop);
        } else {
            let events = self.rec.take().unwrap().events;
            if !events.is_empty() {
                self.loops[slot].layers.push(events);
            }
        }
    }

    /// Finalise a defining pass once the transport reaches its stop bar.
    fn finalize_defining(&mut self) {
        let rec = self.rec.take().unwrap();
        let len = rec.stop_beat.unwrap() - rec.start_beat;
        let s = &mut self.loops[rec.slot];
        s.layers = vec![rec.events];
        s.len_beats = len;
        s.anchor_beat = rec.start_beat;
        s.played_to = 0.0;
        s.state = LoopState::Playing;
    }

    /// Undo (pop) the most recent layer of a slot; empties the slot if it was
    /// the only one. Ignored while that slot is recording.
    fn loop_undo(&mut self, slot: usize) {
        if matches!(&self.rec, Some(r) if r.slot == slot) {
            return;
        }
        self.force_off_slot(slot);
        self.loops[slot].layers.pop();
        if self.loops[slot].layers.is_empty() {
            self.loops[slot] = LoopSlot::default();
        }
        self.clamp_sel();
    }

    /// Reset a slot completely back to empty.
    fn loop_reset(&mut self, slot: usize) {
        if matches!(&self.rec, Some(r) if r.slot == slot) {
            self.rec = None;
        }
        self.force_off_slot(slot);
        self.loops[slot] = LoopSlot::default();
        self.clamp_sel();
    }

    /// Clamp the selected column into the current row's width (state changes can
    /// shrink a row from under a stationary cursor).
    fn clamp_sel(&mut self) {
        let w = self.row_width(self.sel_row);
        self.sel_col = self.sel_col.min(w - 1);
    }

    /// Advance recording state and all loop playback. Called every frame.
    fn tick_loops(&mut self) {
        let now = self.now_beats();
        // Count-in: fire any clicks the transport has reached.
        if let Some(rec) = self.rec.as_mut() {
            while rec.count_in.first().is_some_and(|&(b, _)| now >= b) {
                let (_, accent) = rec.count_in.remove(0);
                let _ = self.tx.send(SynthEvent::Click { accent });
            }
        }
        // Recording state transitions (defining pass only).
        if let Some((slot, start, stop)) = self
            .rec
            .as_ref()
            .filter(|r| r.defining)
            .map(|r| (r.slot, r.start_beat, r.stop_beat))
        {
            if self.loops[slot].state == LoopState::Armed && now >= start {
                self.loops[slot].state = LoopState::Recording;
                self.snapshot_live(); // bake in a chord/arp already sounding
            }
            if stop.is_some_and(|s| now >= s) {
                self.finalize_defining();
            }
        }
        // Playback. Solo wins: if any slot is soloed, only soloed slots sound.
        let any_solo = self.loops.iter().any(|s| s.solo && s.has_content());
        for i in 0..LOOP_SLOTS {
            self.play_loop_slot(i, now, any_solo);
        }
    }

    /// Fire a slot's recorded events for the beats crossed this frame. Position
    /// respects the loop's division (effective length) and speed; only events
    /// within the effective span play, so a shortened loop repeats sooner.
    fn play_loop_slot(&mut self, i: usize, now: f64, any_solo: bool) {
        let s = &self.loops[i];
        let span = s.span();
        if s.state != LoopState::Playing || !s.has_content() || span <= 0.0 {
            return;
        }
        let audible = !s.muted && (!any_solo || s.solo);
        let pos = ((now - s.anchor_beat) * s.speed()).rem_euclid(span);
        let from = s.played_to;

        if !audible {
            if !self.loops[i].sounding.is_empty() {
                self.force_off_slot(i);
            }
            self.loops[i].played_to = pos;
            return;
        }

        if pos >= from {
            self.fire_range(i, from, pos, true);
        } else {
            // Wrapped the loop: finish the tail, silence at the boundary for a
            // clean restart, then play the new cycle's head.
            self.fire_range(i, from, span, false);
            self.force_off_slot(i);
            self.fire_range(i, 0.0, pos, true);
        }
        self.loops[i].played_to = pos;
    }

    /// Fire slot `i`'s events with beat in `(lo, hi]` (or `[0, hi]` at the head
    /// of a cycle when `inclusive_lo`), in time order.
    fn fire_range(&mut self, i: usize, lo: f64, hi: f64, inclusive_lo: bool) {
        // Quantize is applied here, at playback — the stored beats stay exact,
        // so switching back to "free" restores the original timing.
        let grid = self.loops[i].quantize_grid();
        let q = |b: f64| match grid {
            Some(g) => (b / g).round() * g,
            None => b,
        };
        let mut fired: Vec<(f64, LoopEvent)> = Vec::new();
        for layer in &self.loops[i].layers {
            for &e in layer {
                let beat = q(e.beat);
                let after = if inclusive_lo && lo == 0.0 {
                    beat >= lo
                } else {
                    beat > lo
                };
                if after && beat <= hi {
                    fired.push((beat, e));
                }
            }
        }
        fired.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        for (_, e) in fired {
            if e.on {
                self.send_loop_on(i, e);
            } else {
                self.send_loop_off(i, e.note);
            }
        }
    }

    fn send_loop_on(&mut self, i: usize, e: LoopEvent) {
        let t = self.loops[i].transpose;
        let note = (e.note as i32 + t).clamp(0, 127) as u8;
        let freq = e.freq * 2f32.powf(t as f32 / 12.0);
        let id = voice_id(i as u8 + 1, note);
        let _ = self.tx.send(SynthEvent::NoteOn { id, freq, pan: e.pan });
        if !self.loops[i].sounding.contains(&note) {
            self.loops[i].sounding.push(note);
        }
    }

    fn send_loop_off(&mut self, i: usize, note: u8) {
        // Match the shift used when the note was started (see `send_loop_on`).
        let t = self.loops[i].transpose;
        let note = (note as i32 + t).clamp(0, 127) as u8;
        if let Some(pos) = self.loops[i].sounding.iter().position(|&n| n == note) {
            self.loops[i].sounding.remove(pos);
            let _ = self.tx.send(SynthEvent::NoteOff { id: voice_id(i as u8 + 1, note) });
        }
    }

    /// Silence every note a slot currently has sounding.
    fn force_off_slot(&mut self, i: usize) {
        for note in std::mem::take(&mut self.loops[i].sounding) {
            let _ = self.tx.send(SynthEvent::NoteOff { id: voice_id(i as u8 + 1, note) });
        }
    }

    /// Toggle the arpeggiator, re-playing the current chord in the new mode.
    fn toggle_arp(&mut self) {
        self.arp_on = !self.arp_on;
        self.sync_working();
        if let Some((key, root)) = self.current.as_ref().map(|h| (h.key, h.root)) {
            self.play(key, root);
        }
    }

    /// Step the arpeggiator pattern by `delta` (with `1`/`2`).
    fn cycle_pattern(&mut self, delta: i32) {
        let i = ArpPattern::ALL
            .iter()
            .position(|&p| p == self.arp_pattern)
            .unwrap_or(0) as i32;
        let n = ArpPattern::ALL.len() as i32;
        self.arp_pattern = ArpPattern::ALL[(i + delta).rem_euclid(n) as usize];
        self.sync_working();
    }

    /// Grid subdivisions per beat for the current feel.
    fn arp_subdiv(&self) -> u32 {
        if self.arp_triplet {
            ARP_SUBDIV_TRIPLET
        } else {
            ARP_SUBDIV
        }
    }

    fn arp_length(&self) -> f32 {
        ARP_LENGTHS[self.arp_len.min(ARP_LENGTHS.len() - 1)]
    }

    /// The current arp note index on the shared grid: one note every
    /// `arp_length` subdivisions. Below 1 fires faster than the grid (32nd…),
    /// above fires slower (8th, quarter, …).
    fn arp_note_step(&self) -> i64 {
        let pos = self.transport.step_position(self.arp_subdiv());
        (pos / self.arp_length() as f64).floor() as i64
    }

    /// Move the phrase length `delta` steps through `ARP_LENGTHS` (`3` faster,
    /// `4` slower); re-anchor the clock so it doesn't burst, and persist.
    fn adjust_arp_length(&mut self, delta: i32) {
        self.arp_len =
            (self.arp_len as i32 + delta).clamp(0, ARP_LENGTHS.len() as i32 - 1) as usize;
        self.last_step = self.arp_note_step();
        self.sync_working();
    }

    /// Toggle triplet feel; re-anchor the clock and persist.
    fn toggle_triplet(&mut self) {
        self.arp_triplet = !self.arp_triplet;
        self.last_step = self.arp_note_step();
        self.sync_working();
    }

    /// Advance the arpeggiator against the shared transport grid; called every
    /// UI frame. Steps are keyed to a machine-wide wall-clock grid, so every
    /// instance fires on the same beats at the same tempo.
    pub fn tick(&mut self) {
        self.transport.sync(); // pick up tempo/epoch changes from other instances
        self.update_patch_glide(); // ease the sounding patch toward its target
        self.tick_loops(); // advance loop recording/playback
        self.tick_drums(); // advance the drum sequencer

        // Fallback lead/bass: release the brief one-shot note when its gate lapses.
        if let Some(t) = self.lead_off {
            if Instant::now() >= t {
                self.stop_current();
            }
        }

        let step = self.arp_note_step();
        if !self.arp_on || self.current.is_none() {
            self.last_step = step; // stay caught up so the arp starts on the grid
            return;
        }
        if step > self.last_step {
            self.last_step = step; // fire once per grid step (no catch-up bursts)
            self.arp_step();
        }
    }

    /// Move the sounding arp note one step along the pattern.
    fn arp_step(&mut self) {
        let (root, pool) = match &self.current {
            Some(h) if !h.notes.is_empty() => (h.root, h.notes.clone()),
            _ => return,
        };
        if let Some(note) = self.arp_sounding.take() {
            self.send_off(note);
        }
        let idx = if self.arp_pattern == ArpPattern::Random {
            self.rng_next() % pool.len()
        } else {
            let seq = self.arp_pattern.sequence(pool.len());
            seq[self.arp_pos % seq.len()]
        };
        self.arp_pos = self.arp_pos.wrapping_add(1);
        let note = pool[idx];
        // Spread by the note's position in the chord, so the arp sweeps across
        // the stereo field as it climbs/descends.
        self.send_on(root, note, self.spread_pan(idx, pool.len()));
        self.arp_sounding = Some(note);
    }

    fn rng_next(&mut self) -> usize {
        // xorshift32 — deterministic but plenty random-feeling for note order.
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x as usize
    }

    /// MIDI notes the synth is actually sounding, read straight from the shared
    /// voice monitor — no UI-side bookkeeping to keep in sync.
    fn active_notes(&self) -> Vec<u8> {
        self.monitor.active()
    }

    /// The single note to mark as "the root you played" on the piano: the
    /// lowest sounding note at the root's pitch class that isn't the bass note.
    /// (Highlighting every root-pitch-class note gets noisy in spread voicings.)
    fn root_note(&self) -> Option<u8> {
        let held = self.current.as_ref()?;
        let root_pc = held.root % 12;
        let bass = self
            .bass
            .map(|o| (held.root as i32 - 12 + o).clamp(0, 127) as u8);
        held
            .notes
            .iter()
            .copied()
            .filter(|&n| Some(n) != bass)
            .find(|&n| n % 12 == root_pc)
    }

    // --- Text control interface (agents/scripts) ---------------------------

    /// Service the control files: apply queued commands, refresh the state file.
    pub fn serve(&mut self) {
        let commands = self.control.take_commands();
        let had = !commands.is_empty();
        for cmd in commands {
            self.apply_command(&cmd);
        }
        let due = self.control.due();
        if had || due {
            let state = self.state_text();
            self.control.publish(&state);
        }
    }

    /// Remove this instance's control files on exit.
    pub fn shutdown(&self) {
        self.control.cleanup();
    }

    /// The full instance state as `key value` lines (the read-only view; every
    /// key here is also a valid command).
    fn state_text(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "# autochord instance {}", std::process::id());
        let _ = writeln!(s, "tempo {}", self.transport.tempo());
        let _ = writeln!(s, "view {}", match self.view {
            View::Play => "play",
            View::Synth => "synth",
            View::Drum => "drum",
        });
        let _ = writeln!(s, "latch {}", onoff(self.latch));
        let _ = writeln!(s, "tuning {}", if self.just { "just" } else { "et" });
        let _ = writeln!(s, "transpose {}", self.window);
        let _ = writeln!(s, "quality {}", quality_name(self.quality));
        let adds: Vec<&str> = self.additions.iter().map(|a| addition_name(*a)).collect();
        let _ = writeln!(s, "additions {}", if adds.is_empty() { "-".into() } else { adds.join(" ") });
        let _ = writeln!(s, "voicing {}", self.voicing);
        let _ = writeln!(s, "bass {}", match self.bass {
            None => "off".to_string(),
            Some(o) => o.to_string(),
        });
        let _ = writeln!(s, "arp {}", onoff(self.arp_on));
        let _ = writeln!(s, "pattern {}", self.arp_pattern.label());
        let _ = writeln!(s, "phrase {}", self.arp_length());
        let _ = writeln!(s, "triplet {}", onoff(self.arp_triplet));
        let _ = writeln!(s, "timesig {}/4", self.beats_per_bar);
        let _ = writeln!(s, "field {}", self.selected_field_name());
        let _ = writeln!(s, "chord {}", {
            let name = chord_description(self);
            if name.is_empty() { "-".to_string() } else { name }
        });
        let notes: Vec<String> = self.active_notes().iter().map(|&n| note_name(n)).collect();
        let _ = writeln!(s, "notes {}", if notes.is_empty() { "-".into() } else { notes.join(" ") });
        // Loop slots: `loopN state bars layers [div .. speed .. transpose ..]
        // [muted] [solo]`.
        for (i, slot) in self.loops.iter().enumerate() {
            let bars = if slot.len_beats > 0.0 {
                (slot.len_beats / self.bar_beats()).round() as u32
            } else {
                0
            };
            let mut line = format!(
                "loop{} {} {}bars {}layers",
                i + 1,
                loop_state_name(slot.state),
                bars,
                slot.layers.len()
            );
            if slot.has_content() {
                let _ = write!(
                    line,
                    " quantize {} div {} speed {:.2}x transpose {}",
                    slot.quantize_label(),
                    slot.division_label(),
                    slot.speed(),
                    slot.transpose
                );
            }
            if slot.muted {
                line.push_str(" muted");
            }
            if slot.solo {
                line.push_str(" solo");
            }
            let _ = writeln!(s, "{line}");
            // Each layer's notes as `note@beat:dur`, mirroring the write format.
            for (k, layer) in slot.layers.iter().enumerate() {
                let _ = writeln!(
                    s,
                    "loop{}.layer{} {}",
                    i + 1,
                    k + 1,
                    self.layer_tokens(layer, slot.len_beats)
                );
            }
        }
        // Drum machine: selection/enable/tap, then one line per track with its
        // instrument and 16-step pattern (`x` = hit, `.` = rest).
        let _ = writeln!(s, "drums.track {}", self.drum_sel + 1);
        let _ = writeln!(s, "drums.on {}", onoff(self.drums_on));
        let _ = writeln!(s, "drums.tap {}", onoff(self.drum_tap));
        for (i, tr) in self.drum_tracks.iter().enumerate() {
            let n = i + 1;
            let pat: String = tr.steps.iter().map(|&on| if on { 'x' } else { '.' }).collect();
            let _ = writeln!(s, "drum{n}.inst {}", tr.inst.label());
            let _ = writeln!(s, "drum{n}.steps {pat}");
            let _ = writeln!(s, "drum{n}.release {:.2}", tr.release);
            let _ = writeln!(s, "drum{n}.pitch {}", tr.pitch);
            let _ = writeln!(s, "drum{n}.level {:.2}", tr.level);
            let _ = writeln!(s, "drum{n}.pan {:.2}", tr.pan);
            let _ = writeln!(s, "drum{n}.solo {}", onoff(tr.solo));
            let _ = writeln!(s, "drum{n}.mute {}", onoff(tr.mute));
            let _ = writeln!(s, "drum{n}.div {}", tr.division_label());
            let _ = writeln!(s, "drum{n}.speed {:.2}x", tr.speed());
        }
        // Selected preset (PgUp/PgDn or the `patch` command).
        let presets = crate::synth::presets();
        let pi = self.patch_index.min(presets.len() - 1);
        let _ = writeln!(s, "patch {pi}");
        let _ = writeln!(s, "patch.name {}", presets[pi].0);
        // Synth-engine params, namespaced under the active engine's name.
        let _ = writeln!(s, "engine {SYNTH_ENGINE}");
        for p in all_params() {
            let _ = writeln!(s, "{} {}", p.key(), p.raw(&self.patch));
        }
        s
    }

    /// Apply one `key value` command line from the inbox.
    fn apply_command(&mut self, line: &str) {
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else {
            return;
        };
        let arg = it.next().unwrap_or("");
        let rest: Vec<&str> = std::iter::once(arg).chain(it).filter(|s| !s.is_empty()).collect();
        match key {
            "tempo" => {
                if let Ok(b) = arg.parse::<u32>() {
                    self.transport.set_tempo(b.clamp(TEMPO_MIN, TEMPO_MAX));
                }
            }
            "latch" => {
                self.latch = arg == "on";
                if !self.latch {
                    self.stop_current();
                }
            }
            "tuning" => self.just = arg == "just",
            "transpose" => {
                if let Ok(w) = arg.parse::<i32>() {
                    self.window = w.clamp(-WINDOW_RANGE, WINDOW_RANGE);
                }
            }
            "quality" => {
                self.quality = parse_quality(arg);
                self.sync_working();
                self.revoice();
            }
            "additions" => {
                self.additions = if arg == "-" || arg == "none" {
                    Vec::new()
                } else {
                    rest.iter().filter_map(|s| addition_by_name(s)).collect()
                };
                self.sync_working();
                self.revoice();
            }
            "voicing" => {
                if let Ok(v) = arg.parse::<i32>() {
                    self.voicing = v.clamp(-VOICING_RANGE, VOICING_RANGE);
                    self.sync_working();
                    self.revoice();
                }
            }
            "bass" => {
                self.bass = if arg == "off" {
                    None
                } else {
                    arg.parse::<i32>().ok().map(|o| o.clamp(0, BASS_MAX))
                };
                self.sync_working();
                self.revoice();
            }
            "arp" => self.set_arp(arg == "on"),
            "pattern" => {
                if let Some(p) = parse_pattern(arg) {
                    self.arp_pattern = p;
                    self.sync_working();
                }
            }
            "phrase" => {
                if let Ok(mult) = arg.parse::<f32>() {
                    // snap to the nearest available phrase multiplier
                    let idx = ARP_LENGTHS
                        .iter()
                        .enumerate()
                        .min_by(|a, b| {
                            (a.1 - mult).abs().partial_cmp(&(b.1 - mult).abs()).unwrap()
                        })
                        .map(|(i, _)| i)
                        .unwrap_or(ARP_LEN_DEFAULT);
                    self.arp_len = idx;
                    self.last_step = self.arp_note_step();
                    self.sync_working();
                }
            }
            "triplet" => {
                self.arp_triplet = arg == "on";
                self.last_step = self.arp_note_step();
                self.sync_working();
            }
            "timesig" => {
                // Accept "4/4" or bare "4"; snap to a supported signature.
                let beats: Option<u32> = arg.split('/').next().and_then(|n| n.parse().ok());
                if let Some(b) = beats {
                    if TIME_SIGS.contains(&b) {
                        self.beats_per_bar = b;
                    }
                }
            }
            "field" => self.select_field(arg),
            "drums.track" => {
                if let Ok(n) = arg.parse::<usize>() {
                    if (1..=DRUM_TRACKS).contains(&n) {
                        self.drum_sel = n - 1;
                    }
                }
            }
            "drums.on" => self.drums_on = arg == "on",
            "drums.tap" => self.drum_tap = arg == "on",
            "drums.hit" => {
                if let Some(inst) = DrumInst::from_label(arg) {
                    // Audition with the first matching track's tuning, if any.
                    let ev = self
                        .drum_tracks
                        .iter()
                        .find(|t| t.inst == inst)
                        .map(Self::drum_hit_event)
                        .unwrap_or(SynthEvent::DrumHit {
                            inst: inst.index(),
                            pitch: 1.0,
                            release: 1.0,
                            level: 1.0,
                            pan: 0.0,
                        });
                    let _ = self.tx.send(ev);
                }
            }
            "play" => {
                if let Some(root) = parse_note(arg) {
                    self.current_locked = false;
                    self.play('*', root);
                }
            }
            "stop" => self.stop_current(),
            "patch" => {
                // Select a preset by index (`patch 3`) or name (`patch Reese Bass`).
                let presets = crate::synth::presets();
                let index = if let Ok(i) = arg.parse::<usize>() {
                    Some(i % presets.len())
                } else {
                    let name = rest.join(" ");
                    presets.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name))
                };
                if let Some(i) = index {
                    self.load_preset(i);
                }
            }
            other => {
                if let Some(slot) =
                    other.strip_prefix("loop").and_then(|n| n.parse::<usize>().ok())
                {
                    if (1..=LOOP_SLOTS).contains(&slot) {
                        match arg {
                            "quantize" | "quant" | "div" | "division" | "speed" | "transpose" => {
                                self.loop_set(slot - 1, arg, rest.get(1).copied().unwrap_or(""));
                            }
                            "define" => self.loop_define(slot - 1, &rest[1..]),
                            "layer" => self.loop_layer(slot - 1, &rest[1..]),
                            _ => self.loop_command(slot - 1, arg),
                        }
                    }
                } else if let Some(spec) = other.strip_prefix("drum") {
                    // drumN.inst / drumN.steps
                    let mut it2 = spec.splitn(2, '.');
                    if let (Some(n), Some(field)) = (
                        it2.next().and_then(|s| s.parse::<usize>().ok()),
                        it2.next(),
                    ) {
                        if (1..=DRUM_TRACKS).contains(&n) {
                            self.drum_set(n - 1, field, arg);
                        }
                    }
                } else if let Some(p) = param_by_key(other) {
                    if p.set_raw(&mut self.patch, arg) {
                        self.retarget_patch();
                    }
                }
            }
        }
    }

    /// A loop action from the text interface: `loopN <action>`.
    fn loop_command(&mut self, slot: usize, action: &str) {
        match action {
            "record" | "rec" | "overdub" | "press" | "toggle" | "" => {
                self.loop_record_button(slot)
            }
            "stop" => {
                if matches!(&self.rec, Some(r) if r.slot == slot) {
                    self.stop_recording();
                }
            }
            "mute" => self.loops[slot].muted = true,
            "unmute" => self.loops[slot].muted = false,
            "solo" => self.loops[slot].solo = true,
            "unsolo" => self.loops[slot].solo = false,
            "undo" => self.loop_undo(slot),
            "reset" | "clear" => self.loop_reset(slot),
            _ => {}
        }
    }

    /// A loop playback-modifier from the text interface: `loopN <field> <value>`
    /// where field is `div`/`speed`/`transpose`.
    fn loop_set(&mut self, slot: usize, field: &str, value: &str) {
        match field {
            "quantize" | "quant" => {
                if let Some(i) = LOOP_QUANTIZE.iter().position(|&(l, _)| l == value) {
                    self.loops[slot].quantize_idx = i;
                }
            }
            "div" | "division" => {
                if let Some(i) = LOOP_DIVISION_LABELS.iter().position(|&l| l == value) {
                    self.loops[slot].division_idx = i;
                    self.resync_slot(slot);
                }
            }
            "speed" => {
                if let Ok(mult) = value.trim_end_matches('x').parse::<f64>() {
                    let steps = ((mult - 1.0) / LOOP_SPEED_STEP).round() as i32;
                    self.loops[slot].speed_steps =
                        steps.clamp(LOOP_SPEED_MIN_STEPS, LOOP_SPEED_MAX_STEPS);
                    self.resync_slot(slot);
                }
            }
            "transpose" => {
                if let Ok(t) = value.parse::<i32>() {
                    self.loops[slot].transpose = t.clamp(-LOOP_TRANSPOSE_RANGE, LOOP_TRANSPOSE_RANGE);
                    self.resync_slot(slot);
                }
            }
            _ => {}
        }
    }

    /// Author a loop directly (for agents/scripts): `loopN define <bars>
    /// <note@beat:dur> ...`. Replaces the slot with one baked layer, `bars`
    /// bars long, phase-locked to the current bar. Defaults to `free` quantize
    /// so the supplied timing plays exactly.
    fn loop_define(&mut self, slot: usize, args: &[&str]) {
        let Some(bars) = args.first().and_then(|s| s.parse::<u32>().ok()) else {
            return;
        };
        if bars == 0 {
            return;
        }
        let len = bars as f64 * self.bar_beats();
        let events = self.parse_loop_events(&args[1..], len);
        let now = self.now_beats();
        let bar = self.bar_beats();
        let anchor = (now / bar).floor() * bar; // most recent bar line
        self.force_off_slot(slot);
        self.loops[slot] = LoopSlot {
            state: LoopState::Playing,
            layers: vec![events],
            len_beats: len,
            anchor_beat: anchor,
            played_to: (now - anchor).rem_euclid(len),
            quantize_idx: 0, // agent supplies exact timing → free
            ..LoopSlot::default()
        };
        self.clamp_sel();
    }

    /// Overdub an authored layer onto an existing loop: `loopN layer
    /// <note@beat:dur> ...` (beats within the loop's existing length).
    fn loop_layer(&mut self, slot: usize, args: &[&str]) {
        if !self.loops[slot].has_content() {
            return;
        }
        let len = self.loops[slot].len_beats;
        let events = self.parse_loop_events(args, len);
        if !events.is_empty() {
            self.loops[slot].layers.push(events);
        }
    }

    /// Parse `note@beat:dur` tokens into on/off loop events (beats wrapped into
    /// `len`). `note` may be a name (`C4`, `F#3`) or raw MIDI number.
    fn parse_loop_events(&self, tokens: &[&str], len: f64) -> Vec<LoopEvent> {
        let mut events = Vec::new();
        for tok in tokens {
            let Some((note_s, rest)) = tok.split_once('@') else {
                continue;
            };
            let Some((beat_s, dur_s)) = rest.split_once(':') else {
                continue;
            };
            let (Some(note), Ok(beat), Ok(dur)) = (
                parse_note(note_s),
                beat_s.parse::<f64>(),
                dur_s.parse::<f64>(),
            ) else {
                continue;
            };
            let beat = beat.rem_euclid(len);
            let freq = tone_frequency(note, note, self.just);
            events.push(LoopEvent { beat, on: true, note, freq, pan: 0.0 });
            events.push(LoopEvent { beat: beat + dur.max(0.0), on: false, note, freq, pan: 0.0 });
        }
        events
    }

    /// A layer's events as readable `note@beat:dur` tokens — the inverse of the
    /// `define`/`layer` format, so a loop round-trips through the interface.
    fn layer_tokens(&self, layer: &[LoopEvent], len: f64) -> String {
        let mut out: Vec<String> = Vec::new();
        for on in layer.iter().filter(|e| e.on) {
            // Pair with the nearest following off of the same note.
            let off = layer
                .iter()
                .filter(|e| !e.on && e.note == on.note && e.beat > on.beat)
                .map(|e| e.beat)
                .min_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap_or(len);
            out.push(format!(
                "{}@{}:{}",
                note_name(on.note),
                fmt_beat(on.beat),
                fmt_beat(off - on.beat)
            ));
        }
        out.join(" ")
    }

    /// Set the arpeggiator on/off (command interface), re-playing in the new mode.
    fn set_arp(&mut self, on: bool) {
        if self.arp_on == on {
            return;
        }
        self.arp_on = on;
        self.sync_working();
        if let Some((key, root)) = self.current.as_ref().map(|h| (h.key, h.root)) {
            self.play(key, root);
        }
    }
}

/// Compact beat formatting: whole numbers as integers, else up to 4 decimals
/// with trailing zeros trimmed (so 0.25 stays "0.25", 1.0 becomes "1").
fn fmt_beat(x: f64) -> String {
    if (x - x.round()).abs() < 1e-6 {
        format!("{}", x.round() as i64)
    } else {
        format!("{x:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn loop_state_name(s: LoopState) -> &'static str {
    match s {
        LoopState::Empty => "empty",
        LoopState::Armed => "armed",
        LoopState::Recording => "rec",
        LoopState::Playing => "playing",
    }
}

fn onoff(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

fn quality_name(q: Option<Quality>) -> &'static str {
    match q {
        None => "none",
        Some(Quality::Power) => "power",
        Some(Quality::Dim) => "dim",
        Some(Quality::Min) => "min",
        Some(Quality::Maj) => "maj",
        Some(Quality::Sus) => "sus",
    }
}

fn parse_quality(s: &str) -> Option<Quality> {
    match s {
        "power" | "5" => Some(Quality::Power),
        "dim" => Some(Quality::Dim),
        "min" | "m" => Some(Quality::Min),
        "maj" => Some(Quality::Maj),
        "sus" | "sus4" => Some(Quality::Sus),
        _ => None, // "none" and anything else = no chord
    }
}

fn addition_name(a: Addition) -> &'static str {
    ADDITIONS
        .iter()
        .find(|(_, add, _)| *add == a)
        .map(|(_, _, label)| *label)
        .unwrap_or("?")
}

fn addition_by_name(s: &str) -> Option<Addition> {
    // Exact match — `m7` and `M7` differ only by case.
    ADDITIONS
        .iter()
        .find(|(_, _, label)| *label == s)
        .map(|(_, a, _)| *a)
}

fn parse_pattern(s: &str) -> Option<ArpPattern> {
    match s {
        "up" => Some(ArpPattern::Up),
        "down" => Some(ArpPattern::Down),
        "updown" | "up-down" => Some(ArpPattern::UpDown),
        "random" => Some(ArpPattern::Random),
        _ => None,
    }
}

fn char_of(code: KeyCode) -> Option<char> {
    match code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// Draw the whole UI: a slim status line, the control readout, then the chord
/// name and piano at the bottom.
pub fn render(app: &App, frame: &mut Frame) {
    // The Drum view is its own full-height screen (no melodic piano).
    if app.view == View::Drum {
        let chunks = Layout::vertical([
            Constraint::Length(1), // status + transport readout
            Constraint::Length(1), // padding
            Constraint::Min(1),    // drum grid
        ])
        .split(frame.area());
        let top =
            Layout::horizontal([Constraint::Min(20), Constraint::Length(34)]).split(chunks[0]);
        frame.render_widget(status(app), top[0]);
        frame.render_widget(transport_readout(app), top[1]);
        render_drums(app, frame, chunks[2]);
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Length(1), // status + transport readout
        Constraint::Length(1), // padding
        Constraint::Min(8),    // middle panel (controls or synth editor)
        Constraint::Length(1), // chord name
        Constraint::Length(3), // piano
    ])
    .split(frame.area());

    // Top line: status on the left, the transport (tempo · time · keys) right.
    let top = Layout::horizontal([Constraint::Min(20), Constraint::Length(34)]).split(chunks[0]);
    frame.render_widget(status(app), top[0]);
    frame.render_widget(transport_readout(app), top[1]);

    match app.view {
        View::Play => frame.render_widget(controls(app), chunks[2]),
        View::Synth => render_synth(app, frame, chunks[2]),
        View::Drum => {}
    }
    frame.render_widget(chord_name(app), chunks[3]);
    render_piano(app, frame, chunks[4]);
}

/// The drum machine: eight tracks × 16 steps, the selected track highlighted,
/// the current step lit as a playhead.
fn render_drums(app: &App, frame: &mut Frame, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header / hints
        Constraint::Length(1), // step-key hints
        Constraint::Min(1),    // tracks
    ])
    .split(area);

    let tap = if app.drum_tap {
        Span::styled(
            " TAP REC ",
            Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("tap: space", Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  1-8 ", Style::default().fg(Color::DarkGray)),
            Span::styled("track ", Style::default().fg(Color::Yellow)),
            Span::styled(
                "· ←→ ctrl +/- adjust · q-i/a-k steps · z-m play   ",
                Style::default().fg(Color::DarkGray),
            ),
            tap,
        ])),
        rows[0],
    );

    // Step-key hint row, aligned under the grid columns.
    let prefix = "             "; // matches the track-row label width
    let mut hint = vec![Span::raw(prefix.to_string())];
    for (s, &k) in DRUM_STEP_KEYS.iter().enumerate() {
        if s % 4 == 0 {
            hint.push(Span::raw(" "));
        }
        hint.push(Span::styled(format!(" {k}"), Style::default().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(Line::from(hint)), rows[1]);

    frame.render_widget(Paragraph::new(drum_lines(app)), rows[2]);
}

/// Compact pan label: `C`, or `L50` / `R60`.
fn pan_label(p: f32) -> String {
    if p.abs() < 0.05 {
        "C".to_string()
    } else {
        format!("{}{}", if p < 0.0 { "L" } else { "R" }, (p.abs() * 100.0).round() as i32)
    }
}

fn drum_lines(app: &App) -> Vec<Line<'static>> {
    let raw = app.transport.step_position(ARP_SUBDIV);
    let any_solo = app.drum_tracks.iter().any(|t| t.solo);
    let mut lines = Vec::new();
    for (i, tr) in app.drum_tracks.iter().enumerate() {
        let row_sel = i == app.drum_sel;
        let cell_style = |col: usize, active: bool| {
            if row_sel && app.drum_col == col {
                Style::default().fg(Color::Black).bg(ROOT_COLOR).add_modifier(Modifier::BOLD)
            } else if active {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            }
        };

        // Marker + track number, then the instrument (control cell 0).
        let mut spans = vec![Span::styled(
            format!("  {} {} ", if row_sel { "▸" } else { " " }, i + 1),
            if row_sel {
                Style::default().fg(ROOT_COLOR).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        )];
        spans.push(Span::styled(format!("{:<8}", tr.inst.label()), cell_style(0, false)));

        // 16 steps; those beyond the division are inactive (dimmed).
        let active_len = (DRUM_STEPS as f64 * tr.division()).round() as usize;
        let step_now = if app.drums_on && !tr.mute && (!any_solo || tr.solo) && active_len > 0 {
            Some(((raw * tr.speed()).floor() as i64).rem_euclid(active_len as i64) as usize)
        } else {
            None
        };
        for s in 0..DRUM_STEPS {
            if s % 4 == 0 {
                spans.push(Span::raw(" "));
            }
            let inactive = s >= active_len;
            let on = tr.steps[s];
            let playhead = step_now == Some(s);
            let glyph = if on { "●" } else { "·" };
            let style = if playhead {
                Style::default()
                    .fg(Color::Black)
                    .bg(if on { ROOT_COLOR } else { Color::DarkGray })
                    .add_modifier(Modifier::BOLD)
            } else if inactive {
                Style::default().fg(Color::Rgb(70, 70, 70))
            } else if on {
                Style::default().fg(TONE_COLOR).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(format!(" {glyph}"), style));
        }

        // Control cells 1..8 to the right of the steps.
        let cells: [(usize, String, bool); 8] = [
            (1, format!("R{:.2}", tr.release), false),
            (2, format!("P{:+}", tr.pitch), false),
            (3, format!("V{:.1}", tr.level), false),
            (4, pan_label(tr.pan), false),
            (5, "S".to_string(), tr.solo),
            (6, "M".to_string(), tr.mute),
            (7, tr.division_label().to_string(), false),
            (8, format!("{:.2}x", tr.speed()), false),
        ];
        spans.push(Span::raw("  "));
        for (col, text, active) in cells {
            spans.push(Span::styled(format!(" {text} "), cell_style(col, active)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// The top-right transport readout: Tempo · Time-Sig · Keyboard. The selected
/// transport cell (Left/Right on row 0) is highlighted; `+`/`-` adjust it.
fn transport_readout(app: &App) -> Paragraph<'static> {
    let z = note_for_key('z', app.window).map(note_name).unwrap_or_default();
    let keys = if app.window == 0 {
        format!("z:{z}")
    } else {
        format!("z:{z}{:+}", app.window)
    };
    let cells = [
        format!("{}bpm", app.transport.tempo()),
        format!("{}/4", app.beats_per_bar),
        keys,
    ];
    let mut spans = Vec::new();
    for (i, cell) in cells.into_iter().enumerate() {
        let selected = app.view == View::Play && app.sel_row == 0 && app.sel_col == i;
        let style = if selected {
            Style::default().fg(Color::Black).bg(ROOT_COLOR).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {cell} "), style));
        spans.push(Span::raw(" "));
    }
    Paragraph::new(Line::from(spans)).alignment(Alignment::Right)
}

/// The synth editor: three columns of parameters, arrow-navigated, `-`/`+` to
/// adjust. The piano keys still play, so you hear edits live.
fn render_synth(app: &App, frame: &mut Frame, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(1), // preset header
        Constraint::Length(1), // padding
        Constraint::Min(1),    // parameter columns
    ])
    .split(area);
    frame.render_widget(patch_header(app), rows[0]);
    let cols = synth_columns();
    let areas = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(rows[2]);
    for (ci, items) in cols.iter().enumerate() {
        frame.render_widget(Paragraph::new(synth_column_lines(app, ci, items)), areas[ci]);
    }
}

/// The synth-view header: the selected preset and the keys that cycle it.
fn patch_header(app: &App) -> Paragraph<'static> {
    let presets = crate::synth::presets();
    let i = app.patch_index.min(presets.len() - 1);
    let hint = Style::default().fg(Color::DarkGray);
    Paragraph::new(Line::from(vec![
        Span::styled("  PgUp/PgDn ", hint),
        Span::styled("patch ", Style::default().fg(Color::Gray)),
        Span::styled(
            presets[i].0.to_string(),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  ({}/{})", i + 1, presets.len()), hint),
    ]))
}

fn synth_column_lines(app: &App, ci: usize, items: &[Item]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut prow = 0usize; // running parameter index within this column
    for it in items {
        match it {
            Item::Head(h) => lines.push(Line::from(Span::styled(
                format!("  {h}"),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))),
            Item::P(label, param) => {
                let selected = ci == app.synth_col && prow == app.synth_row;
                let (marker, label_style, value_style) = if selected {
                    (
                        "▸",
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        Style::default().fg(ROOT_COLOR).add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        " ",
                        Style::default().fg(Color::Gray),
                        Style::default().fg(Color::Cyan),
                    )
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {marker} "), Style::default().fg(ROOT_COLOR)),
                    Span::styled(format!("{label:<8}"), label_style),
                    Span::styled(param.value(&app.patch), value_style),
                ]));
                prow += 1;
            }
        }
    }
    lines
}

/// The top control readout: chord types, additions, and the voicing/bass
/// sliders — options shown, latched ones highlighted.
fn controls(app: &App) -> Paragraph<'static> {
    let chord = option_row(
        "6-0",
        "chord",
        QUALITIES
            .iter()
            .map(|&(_, q, label)| (label.to_string(), app.quality == Some(q)))
            .collect(),
    );
    let adds = option_row(
        "t-p",
        "add",
        ADDITIONS
            .iter()
            .map(|&(_, a, label)| (label.to_string(), app.additions.contains(&a)))
            .collect(),
    );
    let voicing = slider_line(
        ";/'",
        "voicing",
        voicing_slider(app.voicing),
        format!("{:+}", app.voicing),
    );
    let bass = slider_line(
        "[/]",
        "bass",
        bass_slider(app.bass),
        match app.bass {
            None => "off".to_string(),
            Some(o) => format!("root+{o}"),
        },
    );
    let mut lines = vec![
        chord,
        adds,
        voicing,
        bass,
        arp_line(app),
        phrase_line(app),
        locked_row(app),
        Line::from(""),
        Line::from(Span::styled(
            "  loops  ←→ move · ↑↓ lane · space: press",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    lines.extend(loop_lanes(app));
    Paragraph::new(lines)
}

/// One full-width lane per loop slot: state, bars, a playhead, and the
/// mute/solo/undo/reset buttons (which appear once a slot has content). The
/// selected cell is highlighted.
fn loop_lanes(app: &App) -> Vec<Line<'static>> {
    const BAR_W: usize = 14; // playhead track width
    let mut lines = Vec::new();
    for (i, slot) in app.loops.iter().enumerate() {
        let row = i + 1;
        let selected_here = app.sel_row == row;
        let mut spans = Vec::new();

        // Lane label (highlighted when the loop cell itself is selected).
        let loop_sel = selected_here && app.sel_col == 0;
        spans.push(Span::styled(
            format!("  L{} ", i + 1),
            if loop_sel {
                Style::default().fg(Color::Black).bg(ROOT_COLOR).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            },
        ));

        // State glyph + label. An active overdub keeps the slot Playing, so
        // check the recorder to show REC while overdubbing.
        let overdubbing = matches!(&app.rec, Some(r) if r.slot == i && !r.defining);
        let bars = if slot.len_beats > 0.0 {
            (slot.len_beats / app.bar_beats()).round() as u32
        } else {
            0
        };
        // Count-in clicks remaining (first-ever recording) → show a countdown.
        let counting = match &app.rec {
            Some(r) if r.slot == i && !r.count_in.is_empty() => Some(r.count_in.len()),
            _ => None,
        };
        // Stop pressed, but recording out the rest of the bar until the loop
        // closes on the next bar line.
        let ending = matches!(
            &app.rec,
            Some(r) if r.slot == i && r.defining && r.stop_beat.is_some()
        );
        let (glyph, label, color) = if ending {
            ("◌", "ending".to_string(), Color::Yellow)
        } else if overdubbing {
            ("●", format!("REC {bars}b"), Color::Red)
        } else if let Some(n) = counting {
            ("◌", format!("count {n}"), Color::Yellow)
        } else {
            match slot.state {
                LoopState::Empty => ("·", "empty".to_string(), Color::DarkGray),
                LoopState::Armed => ("◌", "armed".to_string(), Color::Yellow),
                LoopState::Recording => ("●", "REC".to_string(), Color::Red),
                LoopState::Playing if slot.muted => ("○", format!("mute {bars}b"), Color::DarkGray),
                LoopState::Playing => ("▸", format!("play {bars}b"), Color::Green),
            }
        };
        spans.push(Span::styled(format!("{glyph} {label:<9}"), Style::default().fg(color)));

        // Playhead track (spans the effective, division-scaled length).
        let recording = overdubbing || slot.state == LoopState::Recording;
        let track = if slot.has_content() && slot.span() > 0.0 {
            let pos = (((app.now_beats() - slot.anchor_beat) * slot.speed()).rem_euclid(slot.span())
                / slot.span()
                * BAR_W as f64) as usize;
            let pos = pos.min(BAR_W - 1);
            let mut t = "▓".repeat(pos);
            t.push('█');
            t.push_str(&"░".repeat(BAR_W - 1 - pos));
            t
        } else {
            "░".repeat(BAR_W)
        };
        let track_color = if ending || counting.is_some() {
            Color::Yellow
        } else if recording {
            Color::Red
        } else if slot.state == LoopState::Playing && !slot.muted {
            Color::Green
        } else {
            Color::DarkGray
        };
        spans.push(Span::styled(format!(" {track} "), Style::default().fg(track_color)));

        // Layer count, action buttons, and +/- value cells (once recorded).
        if slot.has_content() {
            spans.push(Span::styled(
                format!("{}L ", slot.layers.len()),
                Style::default().fg(Color::Gray),
            ));
            let cells: [(usize, String, bool); 8] = [
                (1, slot.quantize_label().to_string(), false),
                (2, "mute".to_string(), slot.muted),
                (3, "solo".to_string(), slot.solo),
                (4, "undo".to_string(), false),
                (5, slot.division_label().to_string(), false),
                (6, format!("{:.2}x", slot.speed()), false),
                (7, format!("{:+}st", slot.transpose), false),
                (8, "reset".to_string(), false),
            ];
            for (col, text, active) in cells {
                let sel = selected_here && app.sel_col == col;
                let style = if sel {
                    Style::default().fg(Color::Black).bg(ROOT_COLOR).add_modifier(Modifier::BOLD)
                } else if active {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Gray)
                };
                spans.push(Span::styled(format!(" {text} "), style));
                spans.push(Span::raw(" "));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// The arp phrase controls that sit under the arp row: length and triplet feel.
fn phrase_line(app: &App) -> Line<'static> {
    let mut spans = row_prefix("3 4", "phrase");
    let on = app.arp_on;
    let val = |lit: bool| {
        if on && lit {
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)
        } else if on {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    };
    let mult = app.arp_length();
    let mult_label = if mult < 1.0 {
        format!("÷{}", (1.0 / mult).round() as i32)
    } else {
        format!("×{}", mult as i32)
    };
    spans.push(Span::styled(mult_label, val((mult - 1.0).abs() > f32::EPSILON)));
    spans.push(Span::styled("     5 ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled("triplet ", Style::default().fg(Color::Yellow)));
    spans.push(Span::styled(
        if app.arp_triplet { "on" } else { "off" },
        val(app.arp_triplet),
    ));
    Line::from(spans)
}

/// Grey key-hint + yellow label prefix shared by every control row, at a fixed
/// width so the options line up.
fn row_prefix(hint: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("  {hint:>5} "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{label:<8}"), Style::default().fg(Color::Yellow)),
    ]
}

/// The arpeggiator row: on/off, the pattern options, and the tempo.
fn arp_line(app: &App) -> Line<'static> {
    let mut spans = row_prefix("/ 1 2", "arp");
    let hot = Style::default()
        .fg(Color::Black)
        .bg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    spans.push(Span::styled(
        if app.arp_on { " on " } else { " off " },
        if app.arp_on { hot } else { Style::default().fg(Color::Gray) },
    ));
    spans.push(Span::raw("  "));
    for p in ArpPattern::ALL {
        let style = if !app.arp_on {
            Style::default().fg(Color::DarkGray)
        } else if p == app.arp_pattern {
            hot
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {} ", p.label()), style));
    }
    spans.push(Span::styled(
        format!("   {} bpm", app.transport.tempo()),
        Style::default().fg(if app.arp_on { Color::Gray } else { Color::DarkGray }),
    ));
    Line::from(spans)
}

/// The keys with locked chord configs (backtick). The one matching the current
/// key is highlighted — that's the one backtick will unlock.
fn locked_row(app: &App) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = row_prefix("`", "locked");
    if app.locked.is_empty() {
        spans.push(Span::styled("—", dim));
        return Line::from(spans);
    }
    let current_key = app.current.as_ref().map(|h| h.key);
    for (i, (&key, opts)) in app.locked.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", dim));
        }
        // Show at the pitch the key plays now; a key sitting in a black-key gap
        // at this window has no note, so show the key itself.
        let name = match note_for_key(key, app.window) {
            Some(root) => format!(
                "{}{}",
                pitch_class_name(root),
                chord_symbol(opts.quality, &opts.additions)
            ),
            None => format!("({key})"),
        };
        let style = if Some(key) == current_key {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        spans.push(Span::styled(name, style));
    }
    Line::from(spans)
}

/// A row of options with a key-hint + label prefix; active options highlighted.
fn option_row(hint: &str, label: &str, items: Vec<(String, bool)>) -> Line<'static> {
    let mut spans = row_prefix(hint, label);
    for (text, active) in items {
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {text} "), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

/// A slider row: key-hint + label prefix, track with knob, then the value.
fn slider_line(hint: &str, label: &str, track: Vec<Span<'static>>, value: String) -> Line<'static> {
    let mut spans = row_prefix(hint, label);
    spans.extend(track);
    spans.push(Span::styled(
        format!("  {value}"),
        Style::default().fg(Color::Gray),
    ));
    Line::from(spans)
}

fn voicing_slider(value: i32) -> Vec<Span<'static>> {
    const W: usize = 25;
    let span = (2 * VOICING_RANGE) as f32;
    let pos = (((value + VOICING_RANGE) as f32 / span) * (W as f32 - 1.0)).round() as i32;
    slider_track(pos.clamp(0, W as i32 - 1) as usize, W)
}

fn bass_slider(bass: Option<i32>) -> Vec<Span<'static>> {
    const W: usize = 13; // "off" at index 0, then offsets 0..=11
    let pos = bass.map_or(0, |o| (o + 1) as usize);
    slider_track(pos, W)
}

fn slider_track(pos: usize, width: usize) -> Vec<Span<'static>> {
    let pos = pos.min(width.saturating_sub(1));
    vec![
        Span::styled("─".repeat(pos), Style::default().fg(Color::DarkGray)),
        Span::styled(
            "●",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "─".repeat(width - 1 - pos),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

fn status(app: &App) -> Paragraph<'static> {
    let tab = match app.view {
        View::Play => "tab:synth",
        View::Synth => "SYNTH · tab:drum",
        View::Drum => "DRUM · tab:play",
    };
    let release = if app.enhanced { "release on" } else { "release fallback" };
    let latch = if app.enhanced && !app.latch { "latch off" } else { "latch on" };
    let tuning = if app.just { "just" } else { "12-TET" };
    let presets = crate::synth::presets();
    let patch = presets[app.patch_index.min(presets.len() - 1)].0;
    let text = format!(
        "autochord · {tab} · {release} · {latch} (q) · {tuning} · patch:{patch} (PgUp/Dn) · {} {}Hz",
        app.audio.device, app.audio.sample_rate
    );
    Paragraph::new(text)
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray))
}

fn chord_name(app: &App) -> Paragraph<'static> {
    let name = chord_description(app);
    let text = if name.is_empty() { "—".to_string() } else { name };
    Paragraph::new(text)
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
}

/// The most musically correct name for the sounding chord, with a slash bass
/// when the lowest note isn't the root.
fn chord_description(app: &App) -> String {
    let Some(held) = &app.current else {
        return String::new();
    };
    let root_pc = held.root % 12;
    let base = if app.quality.is_none() && app.additions.is_empty() {
        note_name(held.root) // a single note, not a chord
    } else {
        format!(
            "{}{}",
            pitch_class_name(root_pc),
            chord_symbol(app.quality, &app.additions)
        )
    };
    match held.notes.iter().min() {
        Some(&bass) if bass % 12 != root_pc => format!("{base}/{}", pitch_class_name(bass)),
        _ => base,
    }
}

/// The root you played lights up amber; other chord tones stay magenta.
const ROOT_COLOR: Color = Color::Rgb(255, 176, 0);
const TONE_COLOR: Color = Color::Magenta;

fn render_piano(app: &App, frame: &mut Frame, area: Rect) {
    let active = app.active_notes();
    let root = app.root_note();
    let (lo, hi) = piano_range(&active);
    let (mut lines, width) = piano(lo, hi, &active, root);
    let pad = (area.width as usize).saturating_sub(width) / 2;
    if pad > 0 {
        for line in &mut lines {
            line.spans.insert(0, Span::raw(" ".repeat(pad)));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Piano MIDI range: at least C3..C6, widened (snapped to octaves) to cover any
/// sounding notes.
fn piano_range(active: &[u8]) -> (u8, u8) {
    let mut lo = 48u8; // C3
    let mut hi = 84u8; // C6
    if let (Some(&mn), Some(&mx)) = (active.iter().min(), active.iter().max()) {
        lo = lo.min(mn - mn % 12).max(24);
        let ceil = if mx % 12 == 0 { mx } else { mx + (12 - mx % 12) };
        hi = hi.max(ceil).min(108);
    }
    (lo, hi)
}

/// Highlight colour for a note: amber if it's the played root, magenta if it's
/// another sounding tone, `None` if it isn't sounding.
fn key_color(note: u8, active: &[u8], root: Option<u8>) -> Option<Color> {
    if !active.contains(&note) {
        None
    } else if root == Some(note) {
        Some(ROOT_COLOR)
    } else {
        Some(TONE_COLOR)
    }
}

/// Build the piano: a black-key row, a white-key row, and a note-letter row.
/// Sounding notes are highlighted (root amber, other tones magenta). Returns
/// the lines and their column width.
fn piano(lo: u8, hi: u8, active: &[u8], root: Option<u8>) -> (Vec<Line<'static>>, usize) {
    const W: usize = 2; // columns per white key (border + one content cell)
    let is_white = |pc: u8| matches!(pc % 12, 0 | 2 | 4 | 5 | 7 | 9 | 11);
    let is_black = |pc: u8| matches!(pc % 12, 1 | 3 | 6 | 8 | 10);
    let whites: Vec<u8> = (lo..=hi).filter(|&n| is_white(n)).collect();
    let width = whites.len() * W + 1;
    let border = Style::default().fg(Color::DarkGray);

    // White-key row.
    let mut white_spans: Vec<Span> = Vec::new();
    for &wn in &whites {
        white_spans.push(Span::styled("│", border));
        let style = match key_color(wn, active, root) {
            Some(c) => Style::default().bg(c),
            None => Style::default(),
        };
        white_spans.push(Span::styled(" ", style));
    }
    white_spans.push(Span::styled("│", border));

    // Black keys sit just past the white key they follow.
    let mut black: Vec<(char, Style)> = vec![(' ', Style::default()); width];
    for (i, &wn) in whites.iter().enumerate() {
        let sharp = wn + 1;
        if is_black(sharp) && sharp <= hi {
            let style = match key_color(sharp, active, root) {
                Some(c) => Style::default().fg(c).add_modifier(Modifier::BOLD),
                None => Style::default().fg(Color::DarkGray),
            };
            let c = W * i + 2;
            if c < width {
                black[c] = ('█', style);
            }
        }
    }

    // Note letters under the white keys (C's a touch brighter to anchor octaves).
    let mut label: Vec<(char, Style)> = vec![(' ', Style::default()); width];
    for (i, &wn) in whites.iter().enumerate() {
        let letter = pitch_class_name(wn).chars().next().unwrap_or(' ');
        let style = Style::default().fg(if wn % 12 == 0 { Color::Gray } else { Color::DarkGray });
        let c = W * i + 1;
        if c < width {
            label[c] = (letter, style);
        }
    }

    let lines = vec![
        Line::from(coalesce(&black)),
        Line::from(white_spans),
        Line::from(coalesce(&label)),
    ];
    (lines, width)
}

/// Merge a per-column char/style buffer into the fewest spans.
fn coalesce(buf: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut text = String::new();
    let mut style: Option<Style> = None;
    for &(ch, st) in buf {
        if Some(st) != style {
            if let Some(s) = style.take() {
                spans.push(Span::styled(std::mem::take(&mut text), s));
            }
            style = Some(st);
        }
        text.push(ch);
    }
    if let Some(s) = style {
        spans.push(Span::styled(text, s));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn app() -> App {
        let (tx, _rx) = std::sync::mpsc::channel();
        let monitor = Arc::new(VoiceMonitor::new());
        let audio = AudioInfo { device: "test".into(), sample_rate: 48000 };
        let transport = Transport::disconnected();
        App::new(tx, audio, /*enhanced*/ true, /*just*/ true, monitor, transport)
    }

    fn tap(a: &mut App, c: char) {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    fn release(a: &mut App, c: char) {
        a.on_key(KeyEvent::new_with_kind(
            KeyCode::Char(c),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
    }

    fn root(a: &App) -> u8 {
        a.current.as_ref().unwrap().root
    }

    // play c + maj, backtick, play a + min, play c again -> maj comes back
    #[test]
    fn locked_note_recalls_its_config() {
        let mut a = app();
        tap(&mut a, '9'); // latch maj
        tap(&mut a, 'z'); // play C
        assert_eq!(a.quality, Some(Quality::Maj));
        tap(&mut a, '`'); // lock C = maj
        tap(&mut a, '8'); // switch to min
        tap(&mut a, 'n'); // play A (min)
        assert_eq!(a.quality, Some(Quality::Min));
        tap(&mut a, 'z'); // play C again -> recalls maj
        assert_eq!(a.quality, Some(Quality::Maj));
    }

    // editing a locked note while it plays must NOT update the lock
    #[test]
    fn edits_after_lock_are_frozen() {
        let mut a = app();
        tap(&mut a, '9'); // maj
        tap(&mut a, 'z'); // C
        tap(&mut a, '`'); // lock C = maj
        tap(&mut a, '8'); // edit live to min (no relock)
        assert_eq!(a.quality, Some(Quality::Min));
        tap(&mut a, 'n'); // play A
        tap(&mut a, 'z'); // back to C -> still the frozen maj
        assert_eq!(a.quality, Some(Quality::Maj));
    }

    // passing through a locked note must not clobber the working brush: the
    // next non-locked note resumes what you had before.
    #[test]
    fn working_config_survives_a_locked_note() {
        let mut a = app();
        tap(&mut a, '9'); // maj -> working maj
        tap(&mut a, 'z'); // C maj
        tap(&mut a, '`'); // lock C = maj
        tap(&mut a, '8'); // brush -> min
        tap(&mut a, 'n'); // A min (working brush)
        assert_eq!(a.quality, Some(Quality::Min));
        tap(&mut a, 'z'); // C recalls its locked maj (ephemeral)
        assert_eq!(a.quality, Some(Quality::Maj));
        tap(&mut a, 'n'); // A again -> working brush still min
        assert_eq!(a.quality, Some(Quality::Min));
    }

    // backtick again on a locked note unlocks it
    #[test]
    fn backtick_toggles_lock() {
        let mut a = app();
        tap(&mut a, '9');
        tap(&mut a, 'z');
        tap(&mut a, '`');
        assert!(a.locked.contains_key(&'z'));
        tap(&mut a, '`');
        assert!(!a.locked.contains_key(&'z'));
    }

    // re-pressing a locked note snaps its edited options back to the lock
    #[test]
    fn repressing_locked_note_resets_edits() {
        let mut a = app();
        tap(&mut a, '9'); // maj
        tap(&mut a, 'z'); // C maj
        tap(&mut a, '`'); // lock C = maj
        tap(&mut a, 'n'); // A
        tap(&mut a, 'z'); // back to C (locked, live=maj)
        tap(&mut a, '8'); // edit C -> min (ephemeral)
        assert_eq!(a.quality, Some(Quality::Min));
        tap(&mut a, 'z'); // re-press C -> snaps back to locked maj
        assert_eq!(a.quality, Some(Quality::Maj));
    }

    // the bottom row is always white keys, at any window
    #[test]
    fn bottom_row_is_always_white() {
        for w in -3..=3 {
            for &k in &['z', 'x', 'c', 'v', 'b', 'n', 'm'] {
                let n = note_for_key(k, w).unwrap();
                assert!(
                    matches!(n % 12, 0 | 2 | 4 | 5 | 7 | 9 | 11),
                    "key {k} at window {w} -> {n} (not white)"
                );
            }
        }
    }

    // black-key triggers shift with the window: `d` is a black key at one
    // position and nothing at the next; `f` is the reverse
    #[test]
    fn black_key_triggers_shift_with_window() {
        assert_eq!(note_for_key('d', 0), Some(63)); // above D -> D#
        assert_eq!(note_for_key('d', 1), None); // above E -> no black key
        assert_eq!(note_for_key('f', 0), None); // above E -> no black key
        assert_eq!(note_for_key('f', 1), Some(66)); // above F -> F#
    }

    // Select the "keyboard" field so `+`/`-`/`<`/`>` transpose the window.
    fn select_keyboard(a: &mut App) {
        a.apply_command("field keyboard");
    }

    // > slides the window a white key at a time (with keyboard field selected)
    #[test]
    fn transpose_slides_the_window() {
        let mut a = app();
        select_keyboard(&mut a);
        tap(&mut a, '>'); // one white key up: window starts on D
        tap(&mut a, 'z');
        assert_eq!(root(&a), 62); // z -> D
        tap(&mut a, 'x');
        assert_eq!(root(&a), 64); // x -> E
        tap(&mut a, 's');
        assert_eq!(root(&a), 63); // s -> D# (sharp above z=D)
    }

    // a held key re-pitches on transpose; a latched (ringing) one doesn't
    #[test]
    fn transpose_repitches_held_not_latched() {
        // held: Kitty hold mode (latch off)
        let mut held = app();
        select_keyboard(&mut held);
        tap(&mut held, 'q'); // latch off
        tap(&mut held, 'z'); // hold C
        select_keyboard(&mut held); // playing doesn't move the cursor, but be explicit
        tap(&mut held, '>'); // window up while held -> z now D
        assert_eq!(root(&held), 62);

        // latched: press then release, chord rings on
        let mut latched = app();
        select_keyboard(&mut latched);
        tap(&mut latched, 'z'); // press C (latch on)
        release(&mut latched, 'z'); // release -> only ringing now
        tap(&mut latched, '>'); // transpose -> must NOT move it
        assert_eq!(root(&latched), 60);
    }

    // after transposing a ringing chord, re-hitting the key plays the new pitch
    #[test]
    fn repress_after_transpose_uses_new_pitch() {
        let mut a = app(); // latch on
        select_keyboard(&mut a);
        tap(&mut a, 'z'); // C (60), latched
        release(&mut a, 'z'); // ringing, not held
        tap(&mut a, '>'); // window up — ringing chord stays put
        assert_eq!(root(&a), 60);
        tap(&mut a, 'z'); // re-hit z -> now D (62)
        assert_eq!(root(&a), 62);
    }

    // Field navigation: arrows move the cursor; +/- adjust the selected field.
    #[test]
    fn field_navigation_and_adjust() {
        let mut a = app();
        // Cursor starts on tempo; +/- change BPM by 1.
        let t0 = a.transport.tempo();
        tap(&mut a, '+');
        assert_eq!(a.transport.tempo(), t0 + 1);
        tap(&mut a, '-');
        assert_eq!(a.transport.tempo(), t0);

        // Right to time-sig; adjust toggles 4/4 <-> 3/4.
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.beats_per_bar, 4);
        tap(&mut a, '+');
        assert_eq!(a.beats_per_bar, 3);

        // Right again to keyboard; > transposes.
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        tap(&mut a, '>');
        assert_eq!(a.window, 1);

        // Voicing moved to ; / '
        let v0 = a.voicing;
        tap(&mut a, '\'');
        assert_eq!(a.voicing, v0 + 1);
        tap(&mut a, ';');
        assert_eq!(a.voicing, v0);
    }

    // Time signature round-trips through the text interface.
    #[test]
    fn timesig_command_round_trip() {
        let mut a = app();
        a.apply_command("timesig 3/4");
        assert_eq!(a.beats_per_bar, 3);
        assert!(a.state_text().contains("timesig 3/4"));
        a.apply_command("timesig 4"); // bare numerator also accepted
        assert_eq!(a.beats_per_bar, 4);
        a.apply_command("timesig 7"); // unsupported -> ignored
        assert_eq!(a.beats_per_bar, 4);
    }

    // `/` toggles the arpeggiator
    #[test]
    fn slash_toggles_arp() {
        let mut a = app();
        assert!(!a.arp_on);
        tap(&mut a, '/');
        assert!(a.arp_on);
        tap(&mut a, '/');
        assert!(!a.arp_on);
    }

    // an arp chord change swaps in place and restarts the pattern from the
    // bottom (the shared grid clock is untouched)
    #[test]
    fn arp_chord_change_swaps_in_place() {
        let mut a = app();
        tap(&mut a, '/'); // arp on
        tap(&mut a, 'z'); // play C
        a.arp_pos = 5; // pretend the pattern has advanced
        tap(&mut a, 'n'); // change chord while arping
        assert_eq!(root(&a), 69); // swapped in place (A)
        assert_eq!(a.arp_pos, 0); // pattern restarted for the new chord
    }

    // re-clicking the current chord in arp mode restarts the pattern
    #[test]
    fn arp_reclick_restarts_pattern() {
        let mut a = app();
        tap(&mut a, '/');
        tap(&mut a, 'z');
        a.arp_pos = 5;
        tap(&mut a, 'z'); // re-click the same chord
        assert_eq!(a.arp_pos, 0); // restarted
    }

    // spread pans are symmetric around center (never lopsided), single = center
    #[test]
    fn spread_pan_is_centered_and_symmetric() {
        let mut a = app();
        a.patch.spread = 1.0;
        assert_eq!(a.spread_pan(0, 1), 0.0); // lone note dead center
        assert_eq!(a.spread_pan(0, 3), -1.0); // lowest hard left
        assert_eq!(a.spread_pan(2, 3), 1.0); // highest hard right
        assert_eq!(a.spread_pan(1, 3), 0.0); // middle center
        let sum: f32 = (0..5).map(|i| a.spread_pan(i, 5)).sum();
        assert!(sum.abs() < 1e-6, "spread should balance to center");
    }

    // fallback: a single note is a brief one-shot; a chord latches
    #[test]
    fn fallback_single_note_is_brief_chord_latches() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let monitor = Arc::new(VoiceMonitor::new());
        let audio = AudioInfo { device: "test".into(), sample_rate: 48000 };
        // enhanced = false -> release fallback
        let mut a = App::new(tx, audio, false, true, monitor, Transport::disconnected());
        tap(&mut a, 'z'); // single note, no chord -> brief
        assert!(a.lead_off.is_some());
        tap(&mut a, '9'); // select maj
        tap(&mut a, 'x'); // play a chord -> latches
        assert!(a.lead_off.is_none());
    }

    // re-hitting a latched chord (Kitty) re-strikes it: NoteOn is re-sent
    #[test]
    fn latch_rehit_retriggers() {
        let (tx, rx) = std::sync::mpsc::channel();
        let monitor = Arc::new(VoiceMonitor::new());
        let audio = AudioInfo { device: "test".into(), sample_rate: 48000 };
        let mut a = App::new(tx, audio, true, true, monitor, Transport::disconnected());
        tap(&mut a, 'z'); // play C (latched)
        while rx.try_recv().is_ok() {} // drain the initial NoteOns
        tap(&mut a, 'z'); // re-hit the same chord
        let note_ons = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, SynthEvent::NoteOn { .. }))
            .count();
        assert!(note_ons >= 1, "re-hit should re-send NoteOn to retrigger");
    }

    // 3/4 shrink/extend the phrase (clamped at 1), 5 toggles triplet
    #[test]
    fn arp_phrase_controls() {
        let mut a = app();
        assert_eq!(a.arp_length(), 1.0); // default ×1
        a.adjust_arp_length(1);
        assert_eq!(a.arp_length(), 2.0); // ×2 (slower)
        a.adjust_arp_length(-1);
        assert_eq!(a.arp_length(), 1.0);
        a.adjust_arp_length(-1);
        assert_eq!(a.arp_length(), 0.5); // ÷2 (faster)
        a.adjust_arp_length(-10);
        assert_eq!(a.arp_length(), 0.125); // clamped to ÷8
        a.adjust_arp_length(100);
        assert_eq!(a.arp_length(), 8.0); // clamped to ×8
        assert!(!a.arp_triplet);
        a.toggle_triplet();
        assert!(a.arp_triplet);
    }

    // phrase length + triplet ride along with the per-note lock
    #[test]
    fn arp_phrase_locks_with_backtick() {
        let mut a = app();
        tap(&mut a, '/'); // arp on
        a.adjust_arp_length(1); // ×2
        a.toggle_triplet(); // triplet on
        tap(&mut a, 'z'); // play C with ×2/triplet
        tap(&mut a, '`'); // lock C
        a.adjust_arp_length(1); // brush -> ×3
        a.toggle_triplet(); // brush -> straight
        tap(&mut a, 'n'); // play A (brush)
        assert_eq!(a.arp_length(), 3.0);
        assert!(!a.arp_triplet);
        tap(&mut a, 'z'); // recall locked C
        assert_eq!(a.arp_length(), 2.0);
        assert!(a.arp_triplet);
    }

    // arp on/off and pattern are captured by the per-note lock
    #[test]
    fn arp_state_locks_with_backtick() {
        let mut a = app();
        tap(&mut a, '/'); // arp on
        tap(&mut a, '2'); // pattern: up -> down
        assert_eq!(a.arp_pattern, ArpPattern::Down);
        tap(&mut a, 'z'); // play C (arp on, down)
        tap(&mut a, '`'); // lock C with that arp config
        tap(&mut a, '/'); // arp off (working brush)
        tap(&mut a, 'n'); // play A (no arp)
        assert!(!a.arp_on);
        tap(&mut a, 'z'); // recall locked C
        assert!(a.arp_on);
        assert_eq!(a.arp_pattern, ArpPattern::Down);
    }

    // Text commands mutate the same state the piano keys do, and the
    // republished state round-trips through the same keys.
    #[test]
    fn control_commands_apply_and_round_trip() {
        let mut a = app();

        a.apply_command("tempo 96");
        assert_eq!(a.transport.tempo(), 96);

        a.apply_command("quality min");
        assert_eq!(a.quality, Some(Quality::Min));

        a.apply_command("additions m7 9");
        assert_eq!(a.additions.len(), 2);
        // `m7` and `M7` differ only by case — must not collide.
        a.apply_command("additions M7");
        assert_eq!(a.additions, vec![Addition::Maj7]);

        a.apply_command("arp on");
        assert!(a.arp_on);
        a.apply_command("pattern down");
        assert_eq!(a.arp_pattern, ArpPattern::Down);

        a.apply_command("subtractive.filter.cutoff 3000");
        assert!((a.patch.cutoff - 3000.0).abs() < 1.0);
        a.apply_command("subtractive.osc1.wave sqr");
        assert_eq!(a.patch.osc[0].wave, Wave::Square);

        // New character params round-trip (including the discrete ones).
        a.apply_command("subtractive.sync on");
        assert!(a.patch.sync);
        a.apply_command("subtractive.filter.mode hp");
        assert_eq!(a.patch.filter_mode, FilterMode::Hp);
        a.apply_command("subtractive.filter.slope 24");
        assert_eq!(a.patch.filter_slope, 24);
        a.apply_command("subtractive.unison 3");
        assert_eq!(a.patch.unison, 3);
        a.apply_command("subtractive.drift 0.5");
        assert!((a.patch.drift - 0.5).abs() < 1e-6);
        a.apply_command("subtractive.osc1.pw 0.3");
        assert!((a.patch.osc[0].pw - 0.3).abs() < 1e-6);

        a.apply_command("play C4");
        assert_eq!(root(&a), 60);

        // Unknown keys and junk values are ignored, not fatal.
        a.apply_command("bogus 123");
        a.apply_command("subtractive.filter.cutoff notanumber");
        assert!((a.patch.cutoff - 3000.0).abs() < 1.0);

        // The published state exposes those same keys.
        let s = a.state_text();
        assert!(s.contains("tempo 96"));
        assert!(s.contains("quality min"));
        assert!(s.contains("arp on"));
        assert!(s.contains("engine subtractive"));
        assert!(s.contains("subtractive.filter.cutoff 3000"));
        assert!(s.contains("subtractive.osc1.wave sqr"));
        assert!(s.contains("subtractive.sync on"));
        assert!(s.contains("subtractive.filter.mode hp"));
        assert!(s.contains("subtractive.filter.slope 24"));
        assert!(s.contains("subtractive.unison 3"));

        // Preset selection by index and by name.
        a.apply_command("patch 3");
        assert_eq!(a.patch_index, 3);
        assert!((a.patch.cutoff - 500.0).abs() < 1.0); // factory slot 3 (Reese Bass)
        a.apply_command("patch Warm Bloom");
        assert_eq!(a.patch_index, 0);
        assert!(a.state_text().contains("patch.name Warm Bloom"));
        // Slot 0 was edited above (cutoff 3000), and slots remember edits.
        assert!((a.patch.cutoff - 3000.0).abs() < 1.0);
        // Out-of-range index wraps rather than panicking.
        a.apply_command("patch 99");
        assert_eq!(a.patch_index, 99 % crate::synth::PRESET_COUNT);
    }

    // Config slots are mutable per instance: edit one, leave, come back, and
    // your edit is still there (factory values only seed them at startup).
    #[test]
    fn patch_slots_remember_edits() {
        let mut a = app();
        a.apply_command("patch 0");
        a.apply_command("subtractive.filter.cutoff 4321");
        assert!((a.patch.cutoff - 4321.0).abs() < 1.0);

        a.apply_command("patch 5"); // switch away
        assert_ne!(a.patch.cutoff, 4321.0); // slot 5's own value

        a.apply_command("patch 0"); // return to our edited slot
        assert!((a.patch.cutoff - 4321.0).abs() < 1.0); // edit survived
    }

    // --- loop recorder -----------------------------------------------------

    // Give a slot fabricated content so its action buttons/lane are testable
    // without a real timed recording.
    fn fill_loop(a: &mut App, slot: usize, events: Vec<LoopEvent>, len_beats: f64) {
        a.loops[slot] = LoopSlot {
            state: LoopState::Playing,
            layers: vec![events],
            len_beats,
            anchor_beat: 0.0,
            ..LoopSlot::default()
        };
    }

    fn ev(beat: f64, on: bool, note: u8) -> LoopEvent {
        LoopEvent { beat, on, note, freq: 440.0, pan: 0.0 }
    }

    #[test]
    fn grid_nav_widths_and_field_names() {
        let mut a = app();
        assert_eq!(a.selected_field_name(), "tempo");
        // Row 0 has 3 cells: tempo, timesig, keyboard.
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.selected_field_name(), "timesig");
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.selected_field_name(), "keyboard");
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)); // clamp
        assert_eq!(a.selected_field_name(), "keyboard");

        // Down to L1. Empty slot has only the loop cell.
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(a.selected_field_name(), "loop1");
        assert_eq!(a.row_width(1), 1);
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)); // no button cells yet
        assert_eq!(a.selected_field_name(), "loop1");

        // With content, the lane exposes quantize/mute/solo/undo/div/speed/
        // transpose/reset.
        fill_loop(&mut a, 0, vec![ev(0.0, true, 60), ev(1.0, false, 60)], 4.0);
        assert_eq!(a.row_width(1), 9);
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.selected_field_name(), "loop1.quantize");
    }

    #[test]
    fn loop_division_speed_transpose() {
        let mut a = app();
        fill_loop(&mut a, 0, vec![ev(0.0, true, 60)], 4.0);
        assert!((a.loops[0].span() - 4.0).abs() < 1e-9); // full loop

        // Division: cell 4, +/- cycles the fraction that plays.
        a.select_field("loop1.div");
        tap(&mut a, '+'); // 1/1 -> 1/2
        assert_eq!(a.loops[0].division_label(), "1/2");
        assert!((a.loops[0].span() - 2.0).abs() < 1e-9);

        // Speed: cell 5, increments the playback rate.
        a.select_field("loop1.speed");
        tap(&mut a, '+');
        assert!((a.loops[0].speed() - 1.25).abs() < 1e-9);

        // Transpose: cell 6, shifts semitones.
        a.select_field("loop1.transpose");
        tap(&mut a, '+');
        tap(&mut a, '+');
        assert_eq!(a.loops[0].transpose, 2);

        // Same via the text interface.
        a.apply_command("loop1 div 1/4");
        assert_eq!(a.loops[0].division_label(), "1/4");
        a.apply_command("loop1 speed 2.0");
        assert!((a.loops[0].speed() - 2.0).abs() < 1e-9);
        a.apply_command("loop1 transpose -5");
        assert_eq!(a.loops[0].transpose, -5);
    }

    #[test]
    fn ai_can_define_and_read_a_loop() {
        let mut a = app(); // 4/4
        a.apply_command("loop1 define 1 C4@0:1 E4@1:1 G4@2:2");
        let s = &a.loops[0];
        assert_eq!(s.state, LoopState::Playing);
        assert!((s.len_beats - 4.0).abs() < 1e-9); // 1 bar of 4 beats
        assert_eq!(s.layers.len(), 1);
        assert_eq!(s.layers[0].len(), 6); // three notes -> three on/off pairs
        assert_eq!(s.quantize_idx, 0); // free, so authored timing is exact

        // Overdub an authored layer.
        a.apply_command("loop1 layer C3@0:4");
        assert_eq!(a.loops[0].layers.len(), 2);

        // Read it back: the state exposes the notes in the same format.
        let text = a.state_text();
        assert!(text.contains("loop1 playing 1bars 2layers"));
        assert!(text.contains("loop1.layer1 C4@0:1 E4@1:1 G4@2:2"));
        assert!(text.contains("loop1.layer2 C3@0:4"));
    }

    #[test]
    fn quantize_snaps_playback_non_destructively() {
        let (tx, rx) = std::sync::mpsc::channel();
        let monitor = Arc::new(VoiceMonitor::new());
        let audio = AudioInfo { device: "test".into(), sample_rate: 48000 };
        let mut a = App::new(tx, audio, true, true, monitor, Transport::disconnected());
        while rx.try_recv().is_ok() {}

        // One note recorded off the grid at beat 0.4 in a 4-beat loop.
        fill_loop(&mut a, 0, vec![ev(0.4, true, 60)], 4.0);

        // Quantize 1/4 (grid 1.0) snaps 0.4 -> 0.0, so it fires at the downbeat.
        a.loops[0].quantize_idx = 1; // "1/4"
        a.play_loop_slot(0, 0.2, false);
        let quantized = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, SynthEvent::NoteOn { .. }))
            .count();
        assert_eq!(quantized, 1, "quantized note snaps forward to the downbeat");
        // The stored event keeps its exact beat — quantize is non-destructive.
        assert!((a.loops[0].layers[0][0].beat - 0.4).abs() < 1e-9);

        // Free (as recorded): the same note does NOT fire before beat 0.4.
        a.loops[0].quantize_idx = 0; // "free"
        a.loops[0].played_to = 0.0;
        a.loops[0].sounding.clear();
        while rx.try_recv().is_ok() {}
        a.play_loop_slot(0, 0.2, false);
        let free = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, SynthEvent::NoteOn { .. }))
            .count();
        assert_eq!(free, 0, "free timing fires at 0.4, not before");
    }

    #[test]
    fn loop_transpose_shifts_played_notes() {
        let (tx, rx) = std::sync::mpsc::channel();
        let monitor = Arc::new(VoiceMonitor::new());
        let audio = AudioInfo { device: "test".into(), sample_rate: 48000 };
        let mut a = App::new(tx, audio, true, true, monitor, Transport::disconnected());
        while rx.try_recv().is_ok() {}
        fill_loop(&mut a, 0, vec![ev(0.0, true, 60)], 4.0);
        a.loops[0].transpose = 5; // up a fourth
        a.play_loop_slot(0, 0.5, false);
        let ons: Vec<u16> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                SynthEvent::NoteOn { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        // Note 60 + 5 = 65, on slot 0's voice source (1).
        assert_eq!(ons, vec![voice_id(1, 65)]);
    }

    #[test]
    fn loop_mute_solo_undo_reset_commands() {
        let mut a = app();
        fill_loop(&mut a, 0, vec![ev(0.0, true, 60)], 4.0);
        a.loops[0].layers.push(vec![ev(1.0, true, 64)]); // a second layer

        a.apply_command("loop1 mute");
        assert!(a.loops[0].muted);
        a.apply_command("loop1 unmute");
        assert!(!a.loops[0].muted);

        a.apply_command("loop1 solo");
        assert!(a.loops[0].solo);

        // Undo pops the last layer; the loop survives.
        a.apply_command("loop1 undo");
        assert_eq!(a.loops[0].layers.len(), 1);
        assert!(a.loops[0].has_content());

        // Reset wipes it back to empty.
        a.apply_command("loop1 reset");
        assert!(!a.loops[0].has_content());
        assert_eq!(a.loops[0].state, LoopState::Empty);
    }

    #[test]
    fn recording_arms_on_empty_slot() {
        let mut a = app();
        a.select_field("loop2");
        a.press_loop_button(); // Space on empty loop cell -> arm
        assert!(matches!(&a.rec, Some(r) if r.slot == 1 && r.defining));
        assert_eq!(a.loops[1].state, LoopState::Armed);
        // Space again marks a bar-aligned stop.
        a.press_loop_button();
        assert!(matches!(&a.rec, Some(r) if r.stop_beat.is_some()));
    }

    #[test]
    fn recording_snapshots_already_sounding_notes() {
        let mut a = app();
        // A chord already latched/sounding on the live source.
        a.sent = vec![(60, 261.6, 0.0), (64, 329.6, 0.0)];
        // A defining recording that has already started (start in the past).
        a.rec = Some(Recording {
            slot: 0,
            defining: true,
            start_beat: 0.0,
            stop_beat: None,
            events: Vec::new(),
            count_in: Vec::new(),
        });
        a.snapshot_live();
        let notes: Vec<u8> = a
            .rec
            .as_ref()
            .unwrap()
            .events
            .iter()
            .filter(|e| e.on)
            .map(|e| e.note)
            .collect();
        assert_eq!(notes, vec![60, 64]);
    }

    #[test]
    fn loop_playback_fires_events_on_its_own_source() {
        let (tx, rx) = std::sync::mpsc::channel();
        let monitor = Arc::new(VoiceMonitor::new());
        let audio = AudioInfo { device: "test".into(), sample_rate: 48000 };
        let mut a = App::new(tx, audio, true, true, monitor, Transport::disconnected());
        while rx.try_recv().is_ok() {}

        // A 4-beat loop: note 60 on at beat 0, off at beat 1.
        fill_loop(&mut a, 0, vec![ev(0.0, true, 60), ev(1.0, false, 60)], 4.0);

        // Advance playback from beat 0 to 0.5 — the on at 0 should fire.
        a.play_loop_slot(0, 0.5, false);
        let ons: Vec<u16> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                SynthEvent::NoteOn { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        // Slot 0 uses voice source 1 -> id = (1<<8)|60.
        assert_eq!(ons, vec![voice_id(1, 60)]);

        // Advance to 1.5 — the off at 1 fires on the same source.
        a.play_loop_slot(0, 1.5, false);
        let offs: Vec<u16> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                SynthEvent::NoteOff { id } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(offs, vec![voice_id(1, 60)]);
    }

    #[test]
    fn solo_silences_other_slots() {
        let mut a = app();
        fill_loop(&mut a, 0, vec![ev(0.0, true, 60)], 4.0);
        fill_loop(&mut a, 1, vec![ev(0.0, true, 67)], 4.0);
        a.loops[1].sounding = vec![67]; // pretend it's sounding
        a.loops[0].solo = true;

        // With slot 0 soloed, slot 1 is inaudible and gets silenced.
        a.play_loop_slot(1, 0.5, /*any_solo*/ true);
        assert!(a.loops[1].sounding.is_empty());
    }

    #[test]
    fn state_text_reports_loops() {
        let mut a = app();
        let s = a.state_text();
        assert!(s.contains("loop1 empty 0bars 0layers"));
        fill_loop(&mut a, 0, vec![ev(0.0, true, 60)], 8.0);
        a.loops[0].muted = true;
        let s = a.state_text();
        assert!(s.contains(
            "loop1 playing 2bars 1layers quantize 1/16 div 1/1 speed 1.00x transpose 0 muted"
        ));
    }

    // --- drum machine ------------------------------------------------------

    // Enter the Drum view (Tab cycles Play -> Synth -> Drum).
    fn to_drum(a: &mut App) {
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.view, View::Drum);
    }

    #[test]
    fn drum_step_edit_and_track_select() {
        let mut a = app();
        to_drum(&mut a);
        // Number keys select the track; default track 0 is kick.
        assert_eq!(a.drum_sel, 0);
        assert_eq!(a.drum_tracks[0].inst, DrumInst::Kick);

        // q toggles step 0, k toggles step 15, of the selected track.
        tap(&mut a, 'q');
        tap(&mut a, 'k');
        assert!(a.drum_tracks[0].steps[0]);
        assert!(a.drum_tracks[0].steps[15]);
        tap(&mut a, 'q'); // toggle back off
        assert!(!a.drum_tracks[0].steps[0]);

        // Number key 2 selects the second track (snare), leaving track 0 intact.
        tap(&mut a, '2');
        assert_eq!(a.drum_sel, 1);
        tap(&mut a, 'w'); // step 1 on track 1
        assert!(a.drum_tracks[1].steps[1]);
    }

    #[test]
    fn drum_instrument_cycle_and_tap() {
        let mut a = app();
        to_drum(&mut a);
        // ,/. cycle the selected track's instrument.
        assert_eq!(a.drum_tracks[0].inst, DrumInst::Kick);
        tap(&mut a, '.');
        assert_eq!(a.drum_tracks[0].inst, DrumInst::Snare);
        tap(&mut a, ',');
        assert_eq!(a.drum_tracks[0].inst, DrumInst::Kick);

        // Space arms tap-record; a live trigger then writes into the grid.
        tap(&mut a, ' ');
        assert!(a.drum_tap);
        // At transport beat ~0, the nearest step is 0. Trigger kick (z).
        tap(&mut a, 'z');
        assert!(a.drum_tracks[0].steps[a.nearest_drum_step()]);
    }

    #[test]
    fn drum_text_interface_round_trip() {
        let mut a = app();
        a.apply_command("drum2.inst cowbell");
        a.apply_command("drum2.steps x...x...x...x...");
        assert_eq!(a.drum_tracks[1].inst, DrumInst::Cowbell);
        assert!(a.drum_tracks[1].steps[0] && a.drum_tracks[1].steps[4]);
        assert!(!a.drum_tracks[1].steps[1]);

        a.apply_command("drums.track 3");
        assert_eq!(a.drum_sel, 2);
        a.apply_command("drums.on off");
        assert!(!a.drums_on);

        // Per-track voice/clock params.
        a.apply_command("drum2.pitch 5");
        a.apply_command("drum2.level 0.8");
        a.apply_command("drum2.pan -0.5");
        a.apply_command("drum2.release 2.0");
        a.apply_command("drum2.div 1/2");
        a.apply_command("drum2.speed 2.0");
        a.apply_command("drum2.solo on");
        assert_eq!(a.drum_tracks[1].pitch, 5);
        assert!((a.drum_tracks[1].level - 0.8).abs() < 1e-6);
        assert!((a.drum_tracks[1].pan + 0.5).abs() < 1e-6);
        assert!((a.drum_tracks[1].release - 2.0).abs() < 1e-6);
        assert_eq!(a.drum_tracks[1].division_label(), "1/2");
        assert!((a.drum_tracks[1].speed() - 2.0).abs() < 1e-9);
        assert!(a.drum_tracks[1].solo);

        let s = a.state_text();
        assert!(s.contains("view drum") || s.contains("view play")); // view line present
        assert!(s.contains("drum2.inst cowbell"));
        assert!(s.contains("drum2.steps x...x...x...x..."));
        assert!(s.contains("drum2.pitch 5"));
        assert!(s.contains("drum2.pan -0.50"));
        assert!(s.contains("drum2.div 1/2"));
        assert!(s.contains("drum2.speed 2.00x"));
        assert!(s.contains("drums.track 3"));
        assert!(s.contains("drums.on off"));
    }

    #[test]
    fn drum_control_cursor_adjusts_cells() {
        let mut a = app();
        to_drum(&mut a);
        // Cursor starts on the instrument cell; Right moves to release, pitch...
        assert_eq!(a.drum_col, 0);
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)); // release
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)); // pitch
        assert_eq!(a.drum_col, 2);
        tap(&mut a, '+');
        assert_eq!(a.drum_tracks[0].pitch, 1);
        tap(&mut a, '-');
        tap(&mut a, '-');
        assert_eq!(a.drum_tracks[0].pitch, -1);

        // Move to the mute cell (col 6) and toggle it with +/-.
        for _ in 0..4 {
            a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        assert_eq!(a.drum_col, 6);
        tap(&mut a, '+');
        assert!(a.drum_tracks[0].mute);
    }
}
