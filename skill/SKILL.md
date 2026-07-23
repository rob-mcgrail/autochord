---
name: autochord
description: Read and control a running autochord terminal synth from text — inspect its live state and change tempo, chords, arpeggiator, and every synth parameter by writing plain key/value commands. Use when asked to play, drive, tweak, or read the state of autochord.
---

# Controlling autochord

`autochord` is a terminal chord synth. Every running instance exposes its full
state as text and accepts plain `key value` commands — no server, no API, just
files (and a matching CLI). You read the same keys you write.

## Where things live

A shared per-user directory (`$TMPDIR/autochord-<user>/`, e.g.
`/var/folders/.../T/autochord-rob/` on macOS, `/tmp/autochord-<user>/` on Linux):

- `global` — the shared clock: `tempo <bpm>` and `epoch_ms <n>`.
- `<pid>.state` — a running instance's full state, one `key value` per line.
- `<pid>.in` — that instance's inbox. Append `key value` lines; the app applies
  them within ~a frame and deletes the file.

Use the `autochord` CLI (preferred — it finds the paths for you) or the files
directly.

## Read state

```sh
autochord ls              # list running instance PIDs
autochord state           # global block + every instance's full state
autochord state 12345     # just instance 12345
autochord patches         # list the built-in preset/config slots (index + name)
```
(Equivalent: `cat $TMPDIR/autochord-$USER/12345.state`.)

The state is `key value` lines, e.g.:

```
# autochord instance 12345
tempo 120
view play
latch on
tuning just
transpose 0
quality maj
additions m7 9
voicing 0
bass off
arp on
pattern up
phrase 1
triplet off
timesig 4/4
field tempo
chord Cmaj9
notes C4 E4 G4 B4 D5
loop1 empty 0bars 0layers
loop2 playing 2bars 2layers quantize 1/16 div 1/1 speed 1.00x transpose 0
loop2.layer1 C4@0:1 E4@1:1 G4@2:2
loop2.layer2 C3@0:8
loop3 empty 0bars 0layers
loop4 empty 0bars 0layers
drums.track 1
drums.on on
drums.tap off
drums.kit 0
drums.kit.name 808
drum1.inst kick
drum1.steps x...x...x...x...
drum2.inst snare
drum2.steps ....x.......x...
...
patch 0
patch.name Warm Bloom
engine subtractive
subtractive.osc1.wave tri
subtractive.osc1.pitch 0
...
subtractive.filter.cutoff 6000
...
subtractive.master 0.16
```

`engine` names the active synth engine; its parameters are namespaced under
that name (`subtractive.*`). The subtractive engine is the first of what may be
several — a different engine exposes its own `<engine>.*` keys.

## Write / change state

**The command keys are the same as the state keys.** To change something, send
the key with a new value:

```sh
autochord send 12345 subtractive.filter.cutoff 3000
autochord send 12345 tempo 96
autochord send 12345 quality min
autochord send 12345 arp on
autochord send 12345 play E4
```
(Equivalent: `echo "subtractive.filter.cutoff 3000" >> $TMPDIR/autochord-$USER/12345.in`.)

Send several at once by repeating `send`, or by appending multiple lines to the
inbox file. The instance drains the inbox each frame, applies each line in
order, deletes the file, and republishes its `.state`. Re-read `state` to
confirm the effect.

### Commands

- `tempo <bpm>` — global (affects all instances)
- `timesig 4/4|3/4` — bar length the loop recorder locks to
- `latch on|off`, `tuning just|et`, `transpose <white-key-steps>`
- `quality none|power|dim|min|maj|sus`
- `additions <list>` — e.g. `additions m7 9`; `additions -` to clear
  (names: `8vb 8va 6 m7 M7 9`)
- `voicing <n>` (inversion cascade), `bass off|<0-11>`
- `arp on|off`, `pattern up|down|updown|random`, `phrase <mult>`
  (`0.125 0.25 0.5 1 2 3 4 …`), `triplet on|off`
- `play <note>` (e.g. `play C4`, `play F#3`, `play 60`), `stop`
- `field <name>` — move the play-page selection cursor: `tempo`, `timesig`,
  `keyboard`, or `loopN[.mute|.solo|.undo|.div|.speed|.transpose|.reset]`
- `loopN <action>` — the multitrack looper (N = 1..4). Actions:
  `record` (start → stop → overdub, same as pressing the loop's Space button),
  `stop`, `mute`, `unmute`, `solo`, `unsolo`, `undo` (drop last layer),
  `reset` (wipe to empty).
- `loopN quantize <free|1/4|1/8|1/8T|1/16|1/16T|1/32>` — snap playback to a grid
  (non-destructive; `free` = as recorded). `loopN div <1/1|1/2|1/4|1/8>` — play
  only that fraction of the loop (repeats sooner); `loopN speed <mult>` —
  playback rate 0.25–4 (e.g. `speed 1.5`); `loopN transpose <semitones>` —
  shift the loop's pitch (−24..24).
- `loopN define <bars> <note@beat:dur> ...` — **author a loop directly** (no
  real-time recording): create/replace slot N with a loop `bars` bars long from
  a list of notes. `note` is a name or MIDI number, `beat` is the start (in
  beats from the loop's downbeat), `dur` the length in beats. e.g.
  `autochord send <pid> loop1 define 1 C4@0:1 E4@1:1 G4@2:2`
- `loopN layer <note@beat:dur> ...` — overdub another authored layer onto an
  existing loop (beats within its current length).
- `patch <index-or-name>` — select a config slot by index (`patch 3`) or name
  (`patch Reese Bass`, case-insensitive). `autochord patches` lists them. The
  synth glides into the new patch over ~a beat rather than jumping.
- any synth param by its `<engine>.`-prefixed key — for the subtractive engine:
  - osc (1 & 2): `subtractive.osc1.wave sine|tri|sqr`, `subtractive.osc1.pitch`,
    `subtractive.osc1.fine`, `subtractive.osc1.pw` (pulse width 0.02–0.98),
    `subtractive.osc1.level`, `subtractive.osc1.pan`, `subtractive.osc2.*`
  - mix/mod: `subtractive.sub`, `subtractive.ring`, `subtractive.fm`,
    `subtractive.sync on|off`, `subtractive.pwm`, `subtractive.noise`
  - amp env: `subtractive.amp.attack|decay|sustain|release`
  - filter: `subtractive.filter.cutoff|reso|env`,
    `subtractive.filter.mode lp|hp|bp`, `subtractive.filter.slope 12|24`,
    `subtractive.filter.keytrack`
  - filter env: `subtractive.filterenv.attack|decay|sustain|release`
  - LFOs: `subtractive.pitchlfo.rate|depth`, `subtractive.filterlfo.rate|depth`
  - global: `subtractive.glide`, `subtractive.spread`, `subtractive.drift`
    (analog wander), `subtractive.drive` (saturation), `subtractive.unison 1-4`,
    `subtractive.detune` (cents), `subtractive.master`

**Loops** are baked note-tapes, layered (overdub), phase-locked to the
transport, and each slot plays on its own voice channel so they layer without
clashing. Each slot reads back as `loopN <empty|armed|rec|playing> <bars>bars
<layers>layers [quantize <q> div <d> speed <s>x transpose <t>] [muted] [solo]`,
followed by one `loopN.layerK <note@beat:dur> ...` line per layer — the same
format `define` takes, so loops round-trip through the interface.

### Drum machine

An 808-style 16-step sequencer, 8 tracks, on the shared 16th grid. Kit
instruments (13): `kick snare hihat openhat cowbell tom ride clap rim clave
maracas conga crash`.

- `drums.track <1-8>` — selected track. `drums.on on|off` — sequencer enable.
  `drums.tap on|off` — tap-record arm. `drums.hit <inst>` — trigger a drum once.
- `drums.kit <index-or-name>` — synthesis voicing (0-7), reskins the whole kit
  without changing patterns: `808 909 acoustic lofi chip electro deep tape`.
- `drumN.inst <inst>` — track N's instrument (N = 1..8).
- `drumN.steps <16 chars>` — track N's pattern; `x`/`1`/`#`/`*` = hit, anything
  else (e.g. `.`) = rest. e.g. `drum1.steps x...x...x...x...` (four-on-the-floor).
- Per-track voice/clock: `drumN.release <0.25-4>`, `drumN.pitch <-24..24>`
  (semitones), `drumN.level <0-1.5>`, `drumN.pan <-1..1>`, `drumN.solo on|off`,
  `drumN.mute on|off`, `drumN.div <1/1|1/2|1/4|1/8>` (fraction of the 16 steps
  that plays), `drumN.speed <0.25-4>x` (per-track playback rate).

Each track reads back as `drumN.inst`, `drumN.steps`, and each of the params
above, so beats round-trip through the interface.

Unknown keys and unparseable values are ignored. Values are clamped to valid
ranges.

## Notes

- A `.state` older than a few seconds means a dead instance (files are cleaned
  up on the next launch).
- `tempo` is shared, so changing it in one instance changes it for all.
- The 24 patches are **mutable config slots**, per instance (per pid): editing a
  synth param changes the current slot, and switching away and back recalls your
  modified version. The hardcoded preset values only seed the slots at startup;
  they reset when the instance restarts.
