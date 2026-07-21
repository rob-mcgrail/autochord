//! Keys → notes and chords.
//!
//! The bottom (`z`) row is a seven-key window sitting over a real piano's white
//! keys; the top (`a`) row plays the black key in the gap after each white key.
//! Transposition slides the window along the piano, so the black-key triggers
//! shift with it — a top-row key plays a sharp at one window position and
//! nothing at the next (there's no black key above E or B).
//!
//! ```text
//! window +0:   s   d       g   h   j        C#  D#      F#  G#  A#
//!            z   x   c   v   b   n   m       C   D   E   F   G   A   B
//! ```

/// Bottom-row keys — always the white keys of the current window.
pub const WHITE_ROW: [char; 7] = ['z', 'x', 'c', 'v', 'b', 'n', 'm'];
/// Top-row keys — the black key (if any) in the gap after each white key.
pub const BLACK_ROW: [char; 6] = ['s', 'd', 'f', 'g', 'h', 'j'];

/// MIDI note of the `index`-th white key, with C4 (index 0) = MIDI 60.
fn white_to_midi(index: i32) -> i32 {
    const SEMITONES: [i32; 7] = [0, 2, 4, 5, 7, 9, 11]; // C D E F G A B
    60 + 12 * index.div_euclid(7) + SEMITONES[index.rem_euclid(7) as usize]
}

/// Map a keyboard key to its MIDI note for a given `window` (white-key offset).
///
/// The bottom row is seven consecutive white keys starting at `window`; the top
/// row plays the black key in the gap after each white key — present only where
/// the piano has one (not above E or B), so those top keys are silent there.
/// That's what keeps the bottom row on naturals and the top row on sharps no
/// matter how far the window is transposed.
pub fn note_for_key(c: char, window: i32) -> Option<u8> {
    let midi = if let Some(i) = WHITE_ROW.iter().position(|&k| k == c) {
        white_to_midi(window + i as i32)
    } else {
        let i = BLACK_ROW.iter().position(|&k| k == c)?;
        let white = white_to_midi(window + i as i32);
        // A black key exists above C, D, F, G, A — but not E or B.
        if !matches!(white.rem_euclid(12), 0 | 2 | 5 | 7 | 9) {
            return None;
        }
        white + 1
    };
    u8::try_from(midi).ok().filter(|&m| m <= 127)
}

/// Parse a note into a MIDI number: a raw number (`60`) or a name like `C4`,
/// `C#3`, `Bb5` (octave defaults to 4, C4 = 60).
pub fn parse_note(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u8>() {
        return Some(n);
    }
    let bytes = s.as_bytes();
    let base = match bytes.first()?.to_ascii_uppercase() {
        b'C' => 0,
        b'D' => 2,
        b'E' => 4,
        b'F' => 5,
        b'G' => 7,
        b'A' => 9,
        b'B' => 11,
        _ => return None,
    };
    let mut i = 1;
    let mut semis = base;
    match bytes.get(i) {
        Some(b'#') => {
            semis += 1;
            i += 1;
        }
        Some(b'b') => {
            semis -= 1;
            i += 1;
        }
        _ => {}
    }
    let octave: i32 = if i < bytes.len() {
        s.get(i..)?.parse().ok()?
    } else {
        4
    };
    u8::try_from((octave + 1) * 12 + semis).ok()
}

/// Note-letter name of a pitch class (0 = C … 11 = B), using sharps.
pub fn pitch_class_name(pc: u8) -> &'static str {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    NAMES[(pc % 12) as usize]
}

/// Human-readable note name with octave, e.g. MIDI 60 -> "C4".
pub fn note_name(note: u8) -> String {
    format!("{}{}", pitch_class_name(note), note as i32 / 12 - 1)
}

/// A latching chord quality, selected with the number keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    Power,
    Dim,
    Min,
    Maj,
    Sus,
}

impl Quality {
    /// Semitone offsets from the root that make up the chord.
    pub fn intervals(self) -> &'static [u8] {
        match self {
            Quality::Power => &[0, 7],    // root + fifth (no third)
            Quality::Dim => &[0, 3, 6],   // root, ♭3, ♭5
            Quality::Min => &[0, 3, 7],   // root, ♭3, 5
            Quality::Maj => &[0, 4, 7],   // root, 3, 5
            Quality::Sus => &[0, 5, 7],   // root, 4, 5 (sus4)
        }
    }
}

/// The latching quality buttons, in display order: `(key, quality, label)`.
pub const QUALITIES: [(char, Quality, &str); 5] = [
    ('6', Quality::Power, "5"),
    ('7', Quality::Dim, "dim"),
    ('8', Quality::Min, "min"),
    ('9', Quality::Maj, "maj"),
    ('0', Quality::Sus, "sus"),
];

/// Look up the quality latched by a number key.
pub fn quality_for_key(c: char) -> Option<Quality> {
    QUALITIES
        .iter()
        .find(|(key, _, _)| *key == c)
        .map(|(_, q, _)| *q)
}

/// An extra tone stacked on top of the chord. Independent latching toggles, so
/// they can combine (e.g. 6/9, or maj + M7 = a major-7th chord).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Addition {
    OctDown,
    OctUp,
    Six,
    Min7,
    Maj7,
    Nine,
}

impl Addition {
    /// Semitones relative to the root (can be negative, e.g. an octave below).
    pub fn interval(self) -> i32 {
        match self {
            Addition::OctDown => -12, // octave below the root
            Addition::OctUp => 12,    // octave above the root
            Addition::Six => 9,       // major 6th
            Addition::Min7 => 10,     // ♭7 (dominant / minor 7th)
            Addition::Maj7 => 11,     // major 7th
            Addition::Nine => 14,     // 9th (octave + a major 2nd)
        }
    }
}

/// The latching addition buttons, in display order: `(key, addition, label)`.
pub const ADDITIONS: [(char, Addition, &str); 6] = [
    ('t', Addition::OctDown, "8vb"),
    ('y', Addition::OctUp, "8va"),
    ('u', Addition::Six, "6"),
    ('i', Addition::Min7, "m7"),
    ('o', Addition::Maj7, "M7"),
    ('p', Addition::Nine, "9"),
];

/// Look up the addition latched by a key.
pub fn addition_for_key(c: char) -> Option<Addition> {
    ADDITIONS
        .iter()
        .find(|(key, _, _)| *key == c)
        .map(|(_, a, _)| *a)
}

/// Build the MIDI notes for a chord: the quality's triad over the root (or just
/// the root when no quality is latched), plus any latched additions.
pub fn chord_notes(root: u8, quality: Option<Quality>, additions: &[Addition]) -> Vec<u8> {
    let mut notes: Vec<u8> = match quality {
        Some(q) => q.intervals().iter().map(|i| root + i).collect(),
        None => vec![root],
    };
    for add in additions {
        let id = (root as i32 + add.interval()).clamp(0, 127) as u8;
        if !notes.contains(&id) {
            notes.push(id);
        }
    }
    notes
}

/// The chord-quality symbol (without the root), e.g. `maj7`, `m7`, `7sus4`,
/// `dim7`, `6/9`. Prepend the root name and, if the bass differs, a `/bass`.
/// Returns an empty string for a plain major triad.
pub fn chord_symbol(quality: Option<Quality>, additions: &[Addition]) -> String {
    let has = |a: Addition| additions.contains(&a);
    let sixth = has(Addition::Six);
    let flat7 = has(Addition::Min7);
    let maj7 = has(Addition::Maj7);
    let ninth = has(Addition::Nine);

    match quality {
        Some(Quality::Power) => "5".to_string(),
        Some(Quality::Maj) => {
            if maj7 {
                if ninth { "maj9" } else { "maj7" }.to_string()
            } else if flat7 {
                if ninth { "9" } else { "7" }.to_string()
            } else if sixth {
                if ninth { "6/9" } else { "6" }.to_string()
            } else if ninth {
                "add9".to_string()
            } else {
                String::new() // plain major
            }
        }
        Some(Quality::Min) => {
            if maj7 {
                if ninth { "m(maj9)" } else { "m(maj7)" }.to_string()
            } else if flat7 {
                if ninth { "m9" } else { "m7" }.to_string()
            } else if sixth {
                if ninth { "m6/9" } else { "m6" }.to_string()
            } else if ninth {
                "m(add9)".to_string()
            } else {
                "m".to_string()
            }
        }
        Some(Quality::Dim) => {
            if flat7 {
                if ninth { "m9♭5" } else { "m7♭5" }.to_string() // half-diminished
            } else if sixth {
                "dim7".to_string() // major-6th over a dim triad = fully diminished
            } else {
                "dim".to_string()
            }
        }
        Some(Quality::Sus) => {
            if flat7 {
                if ninth { "9sus4" } else { "7sus4" }.to_string()
            } else if maj7 {
                "maj7sus4".to_string()
            } else if ninth {
                "sus2".to_string() // 9 without a 7th reads as the 2nd
            } else {
                "sus4".to_string()
            }
        }
        None => {
            // No third: root plus whatever tensions were latched.
            let mut s = String::new();
            if maj7 {
                s.push_str("maj7");
            } else if flat7 {
                s.push('7');
            }
            if sixth {
                s.push('6');
            }
            if ninth {
                s.push_str("add9");
            }
            if !s.is_empty() {
                s.push_str("(no3)");
            }
            s
        }
    }
}

/// The Chord Voicing dial: the inversion cascade.
///
/// Each positive click takes the current **lowest** note up an octave; each
/// negative click takes the current **highest** note down an octave. The note
/// count is preserved — notes are moved, not added — so the voicing rolls
/// through inversions and spreads well beyond one octave. Because the note that
/// moves is whichever is currently lowest/highest (not always the root), and
/// because the number of clicks per "octave lap" depends on how many notes the
/// chord has, the stepping is deliberately loose. `clicks == 0` is unchanged.
pub fn voice_chord(base: &[u8], clicks: i32) -> Vec<u8> {
    if base.is_empty() {
        return Vec::new();
    }
    let mut notes: Vec<i32> = base.iter().map(|&n| n as i32).collect();
    notes.sort_unstable();
    notes.dedup();

    for _ in 0..clicks.abs() {
        if clicks > 0 {
            // lowest note up an octave
            if let Some(idx) = arg_min(&notes) {
                if notes[idx] + 12 <= 127 {
                    notes[idx] += 12;
                }
            }
        } else {
            // highest note down an octave
            if let Some(idx) = arg_max(&notes) {
                if notes[idx] - 12 >= 0 {
                    notes[idx] -= 12;
                }
            }
        }
        notes.sort_unstable();
    }

    notes.dedup();
    notes.iter().map(|&m| m as u8).collect()
}

fn arg_min(v: &[i32]) -> Option<usize> {
    v.iter().enumerate().min_by_key(|(_, &m)| m).map(|(i, _)| i)
}

fn arg_max(v: &[i32]) -> Option<usize> {
    v.iter().enumerate().max_by_key(|(_, &m)| m).map(|(i, _)| i)
}

/// Convert a MIDI note number to its 12-TET frequency in Hz (A4 / MIDI 69 = 440).
pub fn midi_to_freq(note: u8) -> f32 {
    440.0 * 2f32.powf((note as f32 - 69.0) / 12.0)
}

/// Just-intonation ratio for a tone `semitones` (signed) from the chord root.
/// The residue within an octave gets a small-integer ratio so triads lock
/// beatless; whole octaves are folded in as factors of two. Intervals without
/// a listed ratio fall back to equal temperament.
pub fn just_ratio(semitones: i32) -> f64 {
    let octave = semitones.div_euclid(12);
    let residue = semitones.rem_euclid(12);
    let base = match residue {
        0 => 1.0,          // unison
        2 => 9.0 / 8.0,    // major second (used by the 9th)
        3 => 6.0 / 5.0,    // minor third
        4 => 5.0 / 4.0,    // major third
        5 => 4.0 / 3.0,    // perfect fourth (sus4)
        6 => 7.0 / 5.0,    // diminished fifth (septimal tritone → 5:6:7 triad)
        7 => 3.0 / 2.0,    // perfect fifth
        9 => 5.0 / 3.0,    // major sixth (add 6)
        10 => 7.0 / 4.0,   // harmonic seventh (m7 / dom7)
        11 => 15.0 / 8.0,  // major seventh
        other => 2f64.powf(other as f64 / 12.0),
    };
    base * 2f64.powi(octave)
}

/// Frequency of a chord tone identified by MIDI note `id`, tuned relative to the
/// chord `root`. In just mode the tone is a pure ratio above (or below) the
/// root's 12-TET pitch (so different roots stay in tune with each other);
/// otherwise it is plain 12-TET.
pub fn tone_frequency(root: u8, id: u8, just: bool) -> f32 {
    if just {
        let offset = id as i32 - root as i32;
        (midi_to_freq(root) as f64 * just_ratio(offset)) as f32
    } else {
        midi_to_freq(id)
    }
}
