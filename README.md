# autochord

A keyboard-driven synth controller that lives in your terminal — an
[Orchid](https://telepathicinstruments.com/)-style instrument you play from the
computer keyboard.

This is the **bones**: play a key to sound a chord built on that root. It's
**monophonic** — a new root cuts off the previous chord. The keys are laid out
like one octave of a piano starting at middle C, white keys on the `z` row and
sharps on the `a` row:

```
black:     s   d       g   h   j        C#  D#      F#  G#  A#
white:   z   x   c   v   b   n   m       C   D   E   F   G   A   B
```

### Chords

The number keys latch a chord **quality**; every root you then play is voiced
into it. They're toggles — press the lit one again to go back to single notes,
or press another to switch. Changing quality/additions re-voices a held chord
live, moving only the tones that change so shared tones ring through.

| Key | Quality | Intervals |
|-----|---------|-----------|
| `6` | power (5) | root · 5 |
| `7` | dim     | root · ♭3 · ♭5 |
| `8` | min     | root · ♭3 · 5  |
| `9` | maj     | root · 3 · 5   |
| `0` | sus     | root · 4 · 5 (sus4) |

**Additions** (`t y u i o p`) stack extra tones on top — independent toggles,
so they combine:

| Key | Add | Interval |
|-----|-----|----------|
| `t` | 8vb | octave below the root |
| `y` | 8va | octave above the root |
| `u` | 6   | major sixth |
| `i` | m7  | ♭7 (dominant / minor 7th) |
| `o` | M7  | major 7th |
| `p` | 9   | ninth |

So `9` then `z` plays C major (C E G); `8` then `b` plays G minor (G B♭ D);
`9` + `o` + `z` is a Cmaj7.

### Tuning

Because the voices are pure sine waves, chords are tuned to **just intonation**
by default: each chord tone is a small-integer ratio above the root's pitch
(major third 5/4, fifth 3/2, and so on), so triads lock together beatless. The
root itself stays at its 12-TET pitch, so different roots remain in tune with
each other. The status bar shows `tuning: just`.

Pass `--et` (or `--equal-temperament` / `--no-just`) to use plain equal
temperament instead — where pure-sine major thirds beat noticeably.

### Voicing dials

Two dials reshape the chord — the Orchid's signature move. They're **sticky
across chords**, so a voicing character carries as you change roots.

- **Chord Voicing** (`-` / `+`): an inversion cascade. `+` lifts the current
  *lowest* note up an octave; `-` drops the current *highest* down an octave.
  The note count is preserved, so the voicing rolls through inversions and
  spreads well past one octave. Because it moves whichever note is currently
  lowest/highest (not always the root), the stepping is loose and exploratory —
  not a tidy 1-2-3 inversion stepper. Shown as `voicing: ±n`.
- **Bass** (`[` / `]`): a *separate* bass note beneath the chord, moved
  independently. `]` engages it (root, an octave below) and raises it a
  semitone at a time; `[` lowers it, switching off below the root. Move it to a
  chord tone for slash chords (C/E, C/G). Shown as `bass: off` / `bass: root+n`.

Hold a dial key to sweep continuously; everything stays in tune, since just
intonation folds octaves correctly.

### Chord memory (backtick)

Backtick (`` ` ``) **locks** the current chord config (quality, additions,
voicing, bass) to the note you're playing. From then on, playing that key
recalls the locked config — regardless of what you've since selected. Backtick
again on a locked note unlocks it.

The lock is a **frozen snapshot**: editing while a locked note sounds changes
only what you hear, not the lock; re-pressing the note snaps it back to the
saved config, and the lock only updates if you unlock and re-lock. Passing
through a locked note doesn't disturb your **working** config either — the next
non-locked note resumes whatever you had before. The `locked` row up top lists
the memories (highlighting the one backtick will unlock).

### Arpeggiator

`/` toggles the arpeggiator: instead of sounding a chord all at once, it plays
the voiced notes one at a time in a pattern. `1` / `2` cycle the pattern
(**up**, **down**, **up-down**, **random**); **`3` / `4`** change the phrase
length — `4` slows it down (`×2 … ×8`, longer notes) and `3` speeds it up
(`÷2 ÷4 ÷8`, down to 128th-note buzzes); **`5`** toggles a **triplet** feel
(16th-triplet grid). The **↑** /
**↓** arrows set the global tempo (starts at 120 BPM). Arp on/off, pattern,
phrase length and triplet are all part of the per-note lock, so one key can be a
strummed chord and another a triplet arpeggio — but tempo stays global.

The arp runs off a shared wall-clock grid, so changing chords (or re-hitting
the current one to restart the pattern) swaps in on the **next step** — it stays
on the grid and the pulse never drifts.

**Multiple instances stay in sync.** Tempo and the beat grid live in a small
per-user file (`$TMPDIR/autochord-<user>/global`); every autochord process on
the machine derives its arp steps from that shared epoch and tempo, so two (or
more) instances arpeggiate in exact lockstep. Change the tempo (↑/↓) in any one
and the others follow within a moment.

### Transpose

`<` / `>` (or `,` / `.`, no shift needed) transpose the whole keyboard a
semitone at a time — the piano keys and locks all shift with it. A key you're
physically **holding** re-pitches live as you transpose; a chord that's only
**ringing** via latch stays put (and in a fallback terminal, where holds can't
be detected, transpose only affects the next note you play). Shown as
`transpose ±n` in the status line.

## Text control interface (for agents & scripts)

Every running instance is fully **readable and writable as plain text** — no
server, no socket. It's how an AI (or a shell script) drives and inspects
autochord. State lives in a per-user directory,
`$TMPDIR/autochord-<user>/`:

- `global` — the shared clock (`tempo`, `epoch_ms`).
- `<pid>.state` — that instance's *complete* state, one `key value` per line.
- `<pid>.in` — that instance's inbox; append `key value` lines and the running
  app drains, applies, and deletes them each frame, then republishes `.state`.

The keys are **symmetric**: the key you read is the key you write. The CLI finds
the paths for you:

```sh
autochord ls                              # list running instance PIDs
autochord state [pid]                     # dump global + per-instance state
autochord send <pid> tempo 96             # change the (shared) tempo
autochord send <pid> quality min          # switch chord quality
autochord send <pid> arp on               # turn the arpeggiator on
autochord send <pid> play C4              # play a chord on middle C
autochord send <pid> subtractive.filter.cutoff 3000   # tweak the synth
autochord install-skill                   # write the agent skill into ./.claude/skills
```

Synth-engine parameters are namespaced under the active engine
(`subtractive.*`) — the subtractive synth is the first of possibly several, and
`state` reports the live one via an `engine` line. Unknown keys and
out-of-range values are ignored (clamped), never fatal. See
[`skill/SKILL.md`](skill/SKILL.md) for the full agent guide, and
[`CLAUDE.md`](CLAUDE.md) for the rule that **every future feature must be exposed
here** — readable in `state_text`, writable in `apply_command`.

## Synth (Tab)

The voices are a small two-oscillator subtractive synth. **Tab** flips between
the play view and a synth editor; the piano keys still play in both, so you hear
edits live. In the editor, **arrow keys** move the cursor through the parameter
grid and **`-` / `+`** change the selected value (hold to ramp).

Signal path: `osc A + osc B + noise → resonant low-pass filter → amp`, in
stereo (each oscillator can be panned).

| Group | Parameters |
|-------|------------|
| Osc A / Osc B | wave (sine / tri / square), pitch offset, fine detune (cents), level, pan |
| Noise | level |
| Amp env | attack, decay, sustain, release |
| Filter | cutoff, resonance, envelope amount |
| Filter env | attack, decay, sustain, release (its own ADSR, sweeps the cutoff) |
| Pitch LFO | rate, depth (vibrato) — rate steps finely below 1 Hz |
| Filter LFO | rate, depth (cutoff wobble) |
| Global | glide (portamento time, 0 = off), spread (stereo width), master volume |

**Spread** fans a chord's notes across the stereo field by their position in
the chord — symmetric around center, re-centered for every chord, so it never
lopsides as you move around the keyboard. It applies to arpeggios too (each step
pans by its place in the chord), and layers on top of the per-oscillator pans.

**Glide** is switchable portamento: notes slide in pitch from the previous note
over the glide time — great on leads and arpeggios.

Under the hood: PolyBLEP square to tame aliasing, a topology-preserving
state-variable filter (per-voice, stereo) for the resonant low-pass, two
free-running LFOs, and proper ADSR envelopes. The whole patch is a small `Copy`
struct pushed to the audio thread on every edit.

## Sustain, latch, and key-release

A terminal normally only tells you when a key goes **down**, never when it comes
**up**. The **Kitty keyboard protocol** fixes that: at startup autochord calls
crossterm's `supports_keyboard_enhancement()` and, where supported, pushes
`REPORT_EVENT_TYPES` so every event carries a `Press` / `Repeat` / `Release`
kind — giving true key-up. Support: Kitty, WezTerm, Ghostty, foot, Alacritty ≥
0.13, Rio, recent Windows Terminal (status bar shows `release: ON`).

How chords sustain depends on that:

- **Latch mode** (default, and the *only* mode without key-release): a played
  chord keeps ringing after you let go, until you play another root. In a
  Kitty-protocol terminal, `q` toggles latch **off** — then chords are
  "key-press defined": they sound only while the key is held and stop on
  release. `q` toggles it back on. Status bar: `latch: on/off (q)`.
- **Fallback** (Terminal.app, older iTerm2, plain conhost, …): no key-up
  events, so chords always latch. `q` cancels the sounding chord (silence until
  the next one). Status bar: `latch: always (q cancels)`. A **single note with
  no chord selected doesn't latch** — each hit is a brief one-shot, so you can
  tap out basslines and leads.

`q` never quits — quit with `Esc` or `Ctrl-C`.

> Upgrade path if terminal support isn't enough: an OS-level global keyboard
> hook (`rdev` / `device_query`) gives true key up/down on *any* terminal, at
> the cost of macOS Accessibility permissions and capturing keys even when the
> app isn't focused. Deliberately not the default here.

## Architecture

```
main.rs      terminal setup/teardown, CLI subcommands, keyboard negotiation, event loop
app.rs       UI state, play/synth views, key routing, synth editor, text control interface
audio.rs     cpal stereo output stream; drains note + patch events on the RT thread
synth.rs     the synth engine — 2 osc + noise, resonant SVF, ADSRs, LFOs; VoiceMonitor
control.rs   the on-disk text interface: state files, per-instance inbox, the CLI
transport.rs the shared musical clock (tempo + epoch) that syncs all instances
notes.rs     key/chord mapping, qualities, additions, voicing, tuning, chord names
```

The UI thread sends `NoteOn`/`NoteOff` down an `mpsc` channel; the audio
callback drains it (non-blocking) each buffer and updates its voices. No locks
on the audio path. The synth also mirrors its gated notes into a shared,
lock-free `VoiceMonitor` (a 128-bit atomic set); the UI reads that to light the
piano, so the display always reflects what's actually sounding rather than a
separately-maintained copy.

## Running it

Needs Rust, a real terminal, and an audio device — so it builds and runs
natively on the host (a Linux container can't produce a macOS audio binary,
nor reach CoreAudio to make sound).

Install Rust once (user-local, no sudo, removable with `rustup self uninstall`):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Then:

```sh
./run.sh            # release build + run; binary at target/release/autochord
./run.sh --debug    # debug build + run (compiles faster)
./run.sh --et       # disable just intonation (plain 12-TET)
```

The same binary also runs the text-interface subcommands without a TUI —
`autochord state|send|ls|install-skill|help` (see [above](#text-control-interface-for-agents--scripts)).

On macOS, launch it from **Ghostty / WezTerm / Kitty** to get real key-release,
which unlocks the `q` latch toggle; Terminal.app always latches (`q` cancels).
