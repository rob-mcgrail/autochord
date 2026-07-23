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

- **Chord Voicing** (`;` / `'`): an inversion cascade. `'` lifts the current
  *lowest* note up an octave; `;` drops the current *highest* down an octave.
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

### The navigable field grid

Below the chord controls is a **selection grid** you drive with the arrow keys.
The top-right of the screen carries the transport row — **Tempo · Time Sig ·
Keys** — and below the chord controls sit the four **loop lanes**. `←`/`→` move
across a row, `↑`/`↓` move between rows, and **`+` / `-`** (or `<` / `>`,
`,` / `.`) adjust the selected transport field:

- **Tempo** — ±1 BPM (starts at 120; shared across instances).
- **Time Sig** — toggles 4/4 ↔ 3/4 (sets the bar length loops lock to).
- **Keys** — transposes the whole keyboard a white-key step at a time (the
  piano keys and locks shift with it; a physically **held** key re-pitches
  live, a latched/ringing chord stays put). Shown as `z:C4 +n`.

### Arpeggiator

`/` toggles the arpeggiator: instead of sounding a chord all at once, it plays
the voiced notes one at a time in a pattern. `1` / `2` cycle the pattern
(**up**, **down**, **up-down**, **random**); **`3` / `4`** change the phrase
length — `4` slows it down (`×2 … ×8`, longer notes) and `3` speeds it up
(`÷2 ÷4 ÷8`, down to 128th-note buzzes); **`5`** toggles a **triplet** feel
(16th-triplet grid). Arp on/off, pattern, phrase length and triplet are all part
of the per-note lock, so one key can be a strummed chord and another a triplet
arpeggio — but tempo stays global.

The arp runs off a shared wall-clock grid, so changing chords (or re-hitting
the current one to restart the pattern) swaps in on the **next step** — it stays
on the grid and the pulse never drifts.

**Multiple instances stay in sync.** Tempo and the beat grid live in a small
per-user file (`$TMPDIR/autochord-<user>/global`); every autochord process on
the machine derives its arp steps from that shared epoch and tempo, so two (or
more) instances arpeggiate in exact lockstep. Change the tempo in any one and
the others follow within a moment.

### Loop recorder

Four **loop lanes** sit below the chord controls — a keyboard-driven multitrack
looper. Navigate to a lane with the arrows and press **Space** on its `loop`
cell to record:

1. **Space** starts recording — always on the next **bar** line. The very first
   loop gets a one-bar **count-in** (quarter-note pips, accented downbeat) so
   you start clean; the lane shows `count 4…3…` then `REC`.
2. **Space** again ends it, snapped to a whole number of bars — while it plays
   out the rest of the bar the lane turns **yellow** (`ending`), then the loop
   starts cycling, phase-locked to the transport.
3. From then on **Space overdubs** another layer onto the same loop.

Loops **bake the notes that actually sounded**, so whatever you did — chords,
single notes, arpeggios, voicing/bass/addition changes mid-pass, or a chord
already latched when you hit record — is captured, and each of the four slots
is fully independent (one can arpeggiate while another holds chords). Each slot
plays on its own voice channel, so layering never clashes.

Arrow **right** from a recorded lane through its cells:

- **mute · solo · undo · reset** — press **Space** to fire them: `mute`
  silences the lane, `solo` silences the others, `undo` drops the last overdub
  layer, `reset` wipes the slot back to empty.
- **quantize · div · speed · transpose** — adjust with **`+` / `-`**:
  `quantize` snaps playback to a grid (`free`, or 1/4 … 1/32 incl. triplets;
  default 1/16) and is **non-destructive** — set it back to `free` to hear the
  loop exactly as recorded; `div` plays only a fraction of the loop so it
  repeats sooner (1/1 · 1/2 · 1/4 · 1/8); `speed` changes the playback rate
  (0.25×–4×); `transpose` shifts the loop's pitch in semitones. Each is
  independent per slot.

Each lane shows its state (**REC** red while recording/overdubbing, **yellow**
during count-in or the ending bar), bar count, layer count, and a moving
playhead. Agents can also **author loops directly** over the text interface
(`loop1 define 1 C4@0:1 E4@1:1 G4@2:2`) and read their notes back — see below.

## Drum machine (Tab again)

A third **Tab** press opens an **808-style step sequencer** — eight tracks of a
16-step grid, running on the same shared clock (16th notes, so a groove stays
locked to the arp and loops, across instances).

- **`z`–`m`** live-trigger tracks 1–7 (each pad plays that track's tuned sound).
- **Number keys 1–8** select a track; **`q`–`i`** toggle its steps 1–8, **`a`–`k`**
  steps 9–16.
- **13-piece kit** (assign any track to any of them with the instrument cell):
  **kick · snare · hihat · open hat · cowbell · tom · ride · clap · rim · clave ·
  maracas · conga · crash** — so you can stack eight kicks for 16ths, or any mix.
- **Per-track controls** to the right of the steps — arrow **←/→** to select a
  cell, **`+` / `-`** to adjust: **instrument · release · pitch · volume · pan ·
  solo · mute · divide · speed**. Divide/speed give each track its own clock
  (e.g. one lane on 8ths, another ripping 32nds); pitch/release/volume/pan shape
  each hit; solo/mute per track.
- **Space** arms **tap-record**: hit the `z`–`m` pads in time and they land
  **quantized** onto the selected track's grid.

**Kits.** **Home** / **End** page through **8 synthesis voicings** — `808 · 909
· acoustic · lofi · chip · electro · deep · tape` — that reskin the whole kit
(tuning, decay, drive, bit-crush, brightness, square vs sine bodies) without
touching your patterns. The current kit shows in the drum header.

The playhead lights each track's current column as it sweeps (respecting that
track's divide/speed). The whole kit keeps grooving while you're on other tabs.
Everything is exposed to the text interface — `drumN.inst`, `drumN.steps`
(a 16-char `x`/`.` pattern), `drumN.release|pitch|level|pan|solo|mute|div|speed`,
`drums.track`, `drums.on`, `drums.tap`, `drums.hit <inst>`, `drums.kit
<index-or-name>` — so agents can program beats too.

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
autochord patches                         # list the built-in preset/config slots
autochord send <pid> tempo 96             # change the (shared) tempo
autochord send <pid> quality min          # switch chord quality
autochord send <pid> arp on               # turn the arpeggiator on
autochord send <pid> play C4              # play a chord on middle C
autochord send <pid> patch Reese Bass     # load a preset (by name or index)
autochord send <pid> timesig 3/4          # set the bar length loops lock to
autochord send <pid> loop1 record         # record/stop/overdub loop slot 1
autochord send <pid> loop2 mute           # mute/solo/undo/reset a loop slot
autochord send <pid> loop1 define 1 C4@0:1 E4@1:1 G4@2:2   # author a loop directly
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
grid and **`-` / `+`** (or `<` / `>`, `,` / `.`) change the selected value
(hold to ramp).

Signal path: `osc A + osc B (+ sub, ring) + noise → resonant multimode filter →
drive → amp`, in stereo (each oscillator can be panned). The editor is four
columns:

| Group | Parameters |
|-------|------------|
| Osc A / Osc B | wave (sine / tri / square), pitch offset, fine detune (cents), **pulse width**, level, pan |
| Mix / Mod | **sub** (octave-down square), **ring** (osc A×B), **fm** (A→B), **sync** (hard-sync B to A), **pwm** (pulse-width-mod depth), noise |
| Amp env | attack, decay, sustain, release (exponential, analog-style) |
| Filter | cutoff, resonance, envelope amount, **mode** (LP/HP/BP), **slope** (12/24 dB), **key-track** |
| Filter env | attack, decay, sustain, release (its own ADSR, sweeps the cutoff) |
| Pitch LFO | rate, depth (vibrato) — rate steps finely below 1 Hz |
| Filter LFO | rate, depth (cutoff wobble) |
| Global | glide, spread, **drift** (analog wander), **drive** (saturation), **unison** (1–4 stacked voices), **detune** (unison spread), master volume |

**Spread** fans a chord's notes across the stereo field by their position in
the chord — symmetric around center, re-centered for every chord, so it never
lopsides as you move around the keyboard. It applies to arpeggios too (each step
pans by its place in the chord), and layers on top of the per-oscillator pans.

**Glide** is switchable portamento: notes slide in pitch from the previous note
over the glide time — great on leads and arpeggios.

**Analog character.** Every voice starts at a **random oscillator phase**, and
**drift** adds a slow per-voice pitch wander plus subtle cutoff/level jitter, so
no two notes are bit-identical and stacked voices shimmer. Envelopes use
**exponential** curves (natural attack/decay), and **drive** soft-saturates the
output for grit. **Unison** stacks up to four detuned copies per note for
supersaw thickness.

Under the hood: PolyBLEP pulse oscillators (variable width) to tame aliasing, a
topology-preserving state-variable filter (per-voice, stereo, LP/HP/BP, optional
24 dB cascade) with key-tracking, a sub-oscillator, ring-mod / FM / hard-sync
between the two oscillators, three free-running LFOs (pitch, filter, PWM), and
exponential ADSR envelopes. The whole patch is a small `Copy` struct pushed to
the audio thread on every edit.

### Presets & config slots

There are **24 built-in patches** — pads, keys, plucks, basses, leads, arps and
FX. **PgUp** / **PgDn** cycle through them (in either view); the current name
shows in the status bar and atop the synth editor. Switching doesn't snap: the
synth **glides** from the old patch to the new one over about a beat (tempo-tied,
kept fast), so continuous parameters sweep smoothly.

Think of the 24 as **mutable config slots** with hardcoded starting points. Edits
are remembered **per slot, per instance** — tweak a patch, cycle away, come back,
and your version is still there. The factory values only seed the slots at launch,
so restarting the app resets them. Agents can jump straight to one with
`autochord send <pid> patch <name-or-index>` (see the text interface above).

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
