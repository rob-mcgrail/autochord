# autochord — notes for Claude / agents working on this repo

A keyboard-driven chord-synth controller in the terminal (ratatui + cpal),
inspired by the Telepathic Instruments Orchid. Play chords from the piano row,
shape them with quality/addition buttons and the voicing dials, arpeggiate, and
edit a full subtractive synth engine — all from the keyboard.

## Layout

- `src/main.rs` — entry point; CLI subcommand dispatch, then the TUI run loop.
- `src/app.rs` — all app state, key handling, the latch/lock model, arpeggiator,
  the navigable play-page grid (`sel_row`/`sel_col`), the multitrack loop
  recorder (`LoopSlot`/`Recording`, baked note-tapes on per-source voice ids),
  the 808-style drum machine (`DrumInst`/`DrumTrack`, a 16-step sequencer on the
  shared grid), the synth editor, and the **text control interface** (`serve`,
  `state_text`, `apply_command`, and the `Param` key/raw/set_raw machinery). The
  three views (`View::Play|Synth|Drum`) cycle with Tab.
- `src/synth.rs` — the DSP: oscillators (variable-width pulse, sub, ring/FM/sync),
  a multimode SVF (LP/HP/BP, 12/24 dB, key-track), exponential envelopes, three
  LFOs, unison, and analog imperfections (random start phase, per-voice drift +
  jitter, drive); the 808-style `DrumVoice` kit; the hardcoded `presets()` bank
  and `Patch::lerp` (for the beat-length patch glide). New `Patch` fields must be
  added to `Patch::default`, `Patch::lerp`, and a `Param` variant in `app.rs`.
- `src/audio.rs` — cpal stream + the `SynthEvent` channel to the audio thread.
- `src/control.rs` — the on-disk text interface (paths, inbox, publishing, CLI).
- `src/transport.rs` — the shared musical clock (tempo + epoch) across instances.
- `src/notes.rs` — note names, MIDI helpers, chord spelling.
- `skill/SKILL.md` — the agent-facing guide, shipped in the binary and installed
  by `autochord install-skill`.

## The text control interface (READ THIS BEFORE ADDING FEATURES)

autochord is fully controllable and observable as plain text — no MCP server, no
socket. This is a first-class product surface, not a debug hatch.

**Where.** A per-user directory, `std::env::temp_dir()/autochord-<user>/`:

- `global` — shared clock (`tempo`, `epoch_ms`), owned by `transport.rs`.
- `<pid>.state` — one instance's complete state, `key value` per line.
- `<pid>.in` — that instance's inbox; agents append `key value` lines.

**Read.** `App::state_text()` serialises *every* piece of state as `key value`
lines. `autochord state [pid]` / `autochord ls` expose it from the CLI.

**Write.** An agent appends `key value` lines to `<pid>.in` (or runs
`autochord send <pid> <key> <value>`). Each UI frame the run loop calls
`App::serve()`, which:

1. `control.take_commands()` — reads `<pid>.in`, deletes it, returns the lines.
2. `apply_command(line)` for each — parses `key value` and mutates the *same*
   state a keypress would, going through the same helpers (`sync_working`,
   `revoice`, `set_tempo`, `SynthEvent::SetPatch`, …). Unknown keys and
   unparseable/out-of-range values are ignored (values are clamped), never fatal.
3. Re-publishes `state_text()` to `<pid>.state` (throttled, or immediately after
   applying commands).

**The keys are symmetric:** the key you read in `.state` is the key you write in
`.in`. Do not invent a separate command vocabulary.

### The rule for all future features

> Any new feature that adds or changes state MUST be exposed through this
> interface, both directions:
>
> 1. **Readable** — add its line(s) to `App::state_text()`.
> 2. **Writable** — handle its key in `App::apply_command()` (or, for a synth
>    parameter, add a `Param` variant so it flows through `all_params()` /
>    `param_by_key()` automatically).
> 3. **Documented** — add the key to `skill/SKILL.md` so agents discover it.
>
> A feature that only responds to a keypress is incomplete. If a human can
> change it, an agent must be able to read and set it by the same key.

### Synth-engine namespacing

The subtractive synth is the **first of possibly several** engines. Its
parameters are namespaced under the engine name (`subtractive.filter.cutoff`,
`subtractive.osc1.wave`, …) via the `SYNTH_ENGINE` const and `Param::key()`, and
`state_text()` emits an `engine <name>` line. A future engine (FM, wavetable, …)
gets its own name and its params fall under that prefix. Keep engine-specific
parameters namespaced; keep genuinely global controls (tempo, chord config, arp)
unprefixed.

## Build / test

```sh
cargo build              # or ./run.sh to build + launch
cargo test
cargo clippy --all-targets
cargo build --release
```

Keep clippy clean. There's a DSP sanity test (`engine_stays_finite_and_in_range`)
and a control round-trip test (`control_commands_apply_and_round_trip`) — extend
the latter when you add commands.
