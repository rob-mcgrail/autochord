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
use crate::synth::{Patch, VoiceMonitor};
use crate::transport::Transport;

/// Clamp range for the Chord Voicing dial (clicks either side of neutral).
const VOICING_RANGE: i32 = 24;
/// Highest bass offset above the root (kept within the octave below the chord).
const BASS_MAX: i32 = 11;
/// White-key steps the window can be transposed either side of home.
const WINDOW_RANGE: i32 = 14;

/// Ignore repeated presses of the same chord button within this window, so OS
/// key-repeat can't rapidly flip a latch on and off.
const BUTTON_DEBOUNCE: Duration = Duration::from_millis(250);

/// Tempo bounds (BPM) and the arpeggiator's steps-per-beat: 16ths straight,
/// 16th-triplets in triplet mode.
const TEMPO_MIN: u32 = 20;
const TEMPO_MAX: u32 = 400;
const ARP_SUBDIV: u32 = 4;
const ARP_SUBDIV_TRIPLET: u32 = 6;
/// Longest arp phrase multiplier (each note lasts N grid steps).
const ARP_LENGTH_MAX: u32 = 16;

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
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Play,
    Synth,
}

/// An editable synth parameter. `usize` selects oscillator 0 or 1.
#[derive(Clone, Copy)]
enum Param {
    OscWave(usize),
    OscPitch(usize),
    OscFine(usize),
    OscLevel(usize),
    OscPan(usize),
    Noise,
    AmpA,
    AmpD,
    AmpS,
    AmpR,
    Cutoff,
    Resonance,
    FiltEnvAmt,
    FiltA,
    FiltD,
    FiltS,
    FiltR,
    PitchLfoRate,
    PitchLfoDepth,
    FiltLfoRate,
    FiltLfoDepth,
    Master,
}

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
            Param::Noise => bump(&mut p.noise, 0.05, 0.0, 1.0),
            Param::AmpA => bump(&mut p.amp.a, 0.01, 0.001, 4.0),
            Param::AmpD => bump(&mut p.amp.d, 0.01, 0.001, 4.0),
            Param::AmpS => bump(&mut p.amp.s, 0.05, 0.0, 1.0),
            Param::AmpR => bump(&mut p.amp.r, 0.01, 0.001, 4.0),
            Param::Cutoff => p.cutoff = (p.cutoff * 1.12f32.powi(dir)).clamp(20.0, 18000.0),
            Param::Resonance => bump(&mut p.resonance, 0.05, 0.0, 1.0),
            Param::FiltEnvAmt => bump(&mut p.filter_env_amount, 0.05, 0.0, 1.0),
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
            Param::Noise => pct(p.noise),
            Param::AmpA => secs(p.amp.a),
            Param::AmpD => secs(p.amp.d),
            Param::AmpS => pct(p.amp.s),
            Param::AmpR => secs(p.amp.r),
            Param::Cutoff => hz(p.cutoff),
            Param::Resonance => pct(p.resonance),
            Param::FiltEnvAmt => pct(p.filter_env_amount),
            Param::FiltA => secs(p.filter_env.a),
            Param::FiltD => secs(p.filter_env.d),
            Param::FiltS => pct(p.filter_env.s),
            Param::FiltR => secs(p.filter_env.r),
            Param::PitchLfoRate => lfo_hz(p.pitch_lfo_rate),
            Param::PitchLfoDepth => format!("{:.1}st", p.pitch_lfo_depth),
            Param::FiltLfoRate => lfo_hz(p.filter_lfo_rate),
            Param::FiltLfoDepth => pct(p.filter_lfo_depth),
            Param::Master => pct(p.master / 0.6),
        }
    }
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

/// The synth editor laid out in three columns.
fn synth_columns() -> [Vec<Item>; 3] {
    use Param::*;
    [
        vec![
            Item::Head("OSC 1"),
            Item::P("wave", OscWave(0)),
            Item::P("pitch", OscPitch(0)),
            Item::P("fine", OscFine(0)),
            Item::P("level", OscLevel(0)),
            Item::P("pan", OscPan(0)),
            Item::Head("OSC 2"),
            Item::P("wave", OscWave(1)),
            Item::P("pitch", OscPitch(1)),
            Item::P("fine", OscFine(1)),
            Item::P("level", OscLevel(1)),
            Item::P("pan", OscPan(1)),
            Item::Head("NOISE"),
            Item::P("level", Noise),
        ],
        vec![
            Item::Head("AMP ENV"),
            Item::P("attack", AmpA),
            Item::P("decay", AmpD),
            Item::P("sustain", AmpS),
            Item::P("release", AmpR),
            Item::Head("FILTER"),
            Item::P("cutoff", Cutoff),
            Item::P("reso", Resonance),
            Item::P("env amt", FiltEnvAmt),
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
            Item::Head("MASTER"),
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
    arp_length: u32,
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
            arp_length: 1,
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
    /// Arp phrase length: each note lasts this many grid steps (`3`/`4`).
    arp_length: u32,
    /// Triplet feel (`5`): 16th-triplet grid instead of straight 16ths.
    arp_triplet: bool,
    /// Shared clock: tempo + beat grid, synced across instances (↑/↓ set it).
    transport: Transport,
    /// Arpeggiator runtime: pattern position, the note currently sounding, and
    /// the last global grid step we fired on. `rng` seeds the Random pattern.
    arp_pos: usize,
    arp_sounding: Option<u8>,
    last_step: i64,
    rng: u32,
    /// MIDI notes we've sent NoteOn for and not yet NoteOff'd — lets us silence
    /// cleanly when switching chords or arp mode.
    sent: Vec<u8>,
    /// The synth patch (edited in the Synth view, pushed to the audio thread).
    patch: Patch,
    /// Which screen is showing, and the synth-editor cursor `(column, row)`.
    view: View,
    synth_col: usize,
    synth_row: usize,
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
        let patch = Patch::default();
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
            arp_length: 1,
            arp_triplet: false,
            transport,
            arp_pos: 0,
            arp_sounding: None,
            last_step: 0,
            rng: 0x9E3779B9,
            sent: Vec::new(),
            patch,
            view: View::Play,
            synth_col: 0,
            synth_row: 0,
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

        // Tab toggles the synth-editor view (both views).
        if key.code == KeyCode::Tab {
            if key.kind == KeyEventKind::Press {
                self.view = match self.view {
                    View::Play => View::Synth,
                    View::Synth => View::Play,
                };
            }
            return;
        }

        let c = char_of(key.code);

        // Piano trigger keys work in BOTH views ("piano keys stay piano keys").
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
        }
    }

    /// Play-view controls: tempo, latch, lock, dials, transpose, chords, arp.
    fn play_key(&mut self, key: KeyEvent, c: Option<char>) {
        // Global tempo: up/down arrows (shared across instances via transport).
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            match key.code {
                KeyCode::Up => {
                    let t = (self.transport.tempo() + 5).min(TEMPO_MAX);
                    self.transport.set_tempo(t);
                    return;
                }
                KeyCode::Down => {
                    let t = self.transport.tempo().saturating_sub(5).max(TEMPO_MIN);
                    self.transport.set_tempo(t);
                    return;
                }
                _ => {}
            }
        }

        let Some(c) = c else {
            return;
        };

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

        // Voicing dials. Act on press AND repeat so holding a key sweeps the
        // voicing continuously, like turning a knob.
        if matches!(c, '-' | '=' | '+' | '[' | ']') {
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                self.turn_dial(c);
            }
            return;
        }

        // Transpose the whole keyboard a semitone (accept shifted or not).
        if matches!(c, '<' | ',' | '>' | '.') {
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                self.transpose_by(if matches!(c, '<' | ',') { -1 } else { 1 });
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

    /// Synth-view controls: arrows navigate the parameter grid, `-`/`+` adjust.
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
            Some('-') => self.synth_adjust(-1),
            Some('+') | Some('=') => self.synth_adjust(1),
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
            let _ = self.tx.send(SynthEvent::SetPatch(self.patch));
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
        // Re-pressing the key that's already sounding *at the same pitch* — a
        // key-repeat or redundant press. (If transpose has since moved this key
        // to a new pitch, fall through and re-trigger at that pitch.)
        if matches!(&self.current, Some(h) if h.key == key && h.root == root) {
            self.held = Some(key);
            // A locked note snaps back to its saved config, undoing any
            // ephemeral edits (revoice is a no-op if nothing changed).
            if let Some(lock) = self.locked.get(&key).cloned() {
                self.apply(lock);
                self.revoice();
            }
            // Re-clicking restarts the arp pattern (from the next step, so it
            // stays on the grid).
            if self.arp_on {
                self.arp_pos = 0;
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

    /// Slide the window `delta` white keys along the piano. A physically-held
    /// key re-pitches to its note at the new window (Kitty); a chord only
    /// ringing via latch, or anything in fallback, stays put. If the held key
    /// lands where there's no note (a black-key gap), it goes quiet.
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
        self.arp_length = cfg.arp_length;
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
            arp_length: self.arp_length,
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
            for note in &new {
                if !old.contains(note) {
                    self.send_on(root, *note);
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
            // Chord Voicing: - lowers (highest note down), + raises (lowest up).
            '-' => self.voicing = (self.voicing - 1).max(-VOICING_RANGE),
            '=' | '+' => self.voicing = (self.voicing + 1).min(VOICING_RANGE),
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
            for &note in &notes {
                self.send_on(root, note);
            }
        }
        self.current = Some(Held { key, root, notes });
    }

    fn stop_current(&mut self) {
        self.silence();
        self.current = None;
        self.arp_sounding = None;
    }

    /// Note-off everything currently sounding.
    fn silence(&mut self) {
        for id in std::mem::take(&mut self.sent) {
            let _ = self.tx.send(SynthEvent::NoteOff { id });
        }
    }

    /// Start tone `id`, tuned relative to `root` for the active temperament.
    fn send_on(&mut self, root: u8, id: u8) {
        if !self.sent.contains(&id) {
            let freq = tone_frequency(root, id, self.just);
            let _ = self.tx.send(SynthEvent::NoteOn { id, freq });
            self.sent.push(id);
        }
    }

    fn send_off(&mut self, id: u8) {
        if let Some(pos) = self.sent.iter().position(|&n| n == id) {
            self.sent.remove(pos);
            let _ = self.tx.send(SynthEvent::NoteOff { id });
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

    /// The current arp note index on the shared grid (one note every
    /// `arp_length` subdivisions), so a longer phrase = fewer notes per beat.
    fn arp_note_step(&self) -> i64 {
        let pos = self.transport.step_position(self.arp_subdiv());
        (pos / self.arp_length.max(1) as f64).floor() as i64
    }

    /// Change the arp phrase length by `delta` note-steps; re-anchor the clock
    /// so it doesn't fire a burst, and persist to the working brush.
    fn adjust_arp_length(&mut self, delta: i32) {
        self.arp_length = (self.arp_length as i32 + delta).clamp(1, ARP_LENGTH_MAX as i32) as u32;
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
        self.send_on(root, note);
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
    let chunks = Layout::vertical([
        Constraint::Length(1), // status
        Constraint::Length(1), // padding
        Constraint::Min(8),    // middle panel (controls or synth editor)
        Constraint::Length(1), // chord name
        Constraint::Length(3), // piano
    ])
    .split(frame.area());

    frame.render_widget(status(app), chunks[0]);
    match app.view {
        View::Play => frame.render_widget(controls(app), chunks[2]),
        View::Synth => render_synth(app, frame, chunks[2]),
    }
    frame.render_widget(chord_name(app), chunks[3]);
    render_piano(app, frame, chunks[4]);
}

/// The synth editor: three columns of parameters, arrow-navigated, `-`/`+` to
/// adjust. The piano keys still play, so you hear edits live.
fn render_synth(app: &App, frame: &mut Frame, area: Rect) {
    let cols = synth_columns();
    let areas = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(area);
    for (ci, items) in cols.iter().enumerate() {
        frame.render_widget(Paragraph::new(synth_column_lines(app, ci, items)), areas[ci]);
    }
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
        "-/+",
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
    Paragraph::new(vec![
        chord,
        adds,
        voicing,
        bass,
        arp_line(app),
        phrase_line(app),
        locked_row(app),
    ])
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
    spans.push(Span::styled(format!("×{}", app.arp_length), val(app.arp_length > 1)));
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
        View::Synth => "SYNTH · tab:play",
    };
    let release = if app.enhanced { "release on" } else { "release fallback" };
    let latch = if app.enhanced && !app.latch { "latch off" } else { "latch on" };
    let tuning = if app.just { "just" } else { "12-TET" };
    let z = note_for_key('z', app.window).map(note_name).unwrap_or_default();
    let text = format!(
        "autochord · {tab} · {release} · {latch} (q) · {tuning} · z:{z} < > · {} {}Hz",
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

    // < / > slide the window a white key at a time
    #[test]
    fn transpose_slides_the_window() {
        let mut a = app();
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
        tap(&mut held, 'q'); // latch off
        tap(&mut held, 'z'); // hold C
        tap(&mut held, '>'); // window up while held -> z now D
        assert_eq!(root(&held), 62);

        // latched: press then release, chord rings on
        let mut latched = app();
        tap(&mut latched, 'z'); // press C (latch on)
        release(&mut latched, 'z'); // release -> only ringing now
        tap(&mut latched, '>'); // transpose -> must NOT move it
        assert_eq!(root(&latched), 60);
    }

    // after transposing a ringing chord, re-hitting the key plays the new pitch
    #[test]
    fn repress_after_transpose_uses_new_pitch() {
        let mut a = app(); // latch on
        tap(&mut a, 'z'); // C (60), latched
        release(&mut a, 'z'); // ringing, not held
        tap(&mut a, '>'); // window up — ringing chord stays put
        assert_eq!(root(&a), 60);
        tap(&mut a, 'z'); // re-hit z -> now D (62)
        assert_eq!(root(&a), 62);
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

    // 3/4 shrink/extend the phrase (clamped at 1), 5 toggles triplet
    #[test]
    fn arp_phrase_controls() {
        let mut a = app();
        assert_eq!(a.arp_length, 1);
        a.adjust_arp_length(1);
        assert_eq!(a.arp_length, 2);
        a.adjust_arp_length(1);
        assert_eq!(a.arp_length, 3);
        a.adjust_arp_length(-1);
        assert_eq!(a.arp_length, 2);
        a.adjust_arp_length(-10);
        assert_eq!(a.arp_length, 1); // clamped
        assert!(!a.arp_triplet);
        a.toggle_triplet();
        assert!(a.arp_triplet);
    }

    // phrase length + triplet ride along with the per-note lock
    #[test]
    fn arp_phrase_locks_with_backtick() {
        let mut a = app();
        tap(&mut a, '/'); // arp on
        a.adjust_arp_length(1); // len 2
        a.toggle_triplet(); // triplet on
        tap(&mut a, 'z'); // play C with len2/triplet
        tap(&mut a, '`'); // lock C
        a.adjust_arp_length(1); // brush -> len 3
        a.toggle_triplet(); // brush -> straight
        tap(&mut a, 'n'); // play A (brush)
        assert_eq!(a.arp_length, 3);
        assert!(!a.arp_triplet);
        tap(&mut a, 'z'); // recall locked C
        assert_eq!(a.arp_length, 2);
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
}
