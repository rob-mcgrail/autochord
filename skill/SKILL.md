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
chord Cmaj9
notes C4 E4 G4 B4 D5
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
- `latch on|off`, `tuning just|et`, `transpose <white-key-steps>`
- `quality none|power|dim|min|maj|sus`
- `additions <list>` — e.g. `additions m7 9`; `additions -` to clear
  (names: `8vb 8va 6 m7 M7 9`)
- `voicing <n>` (inversion cascade), `bass off|<0-11>`
- `arp on|off`, `pattern up|down|updown|random`, `phrase <mult>`
  (`0.125 0.25 0.5 1 2 3 4 …`), `triplet on|off`
- `play <note>` (e.g. `play C4`, `play F#3`, `play 60`), `stop`
- any synth param by its `<engine>.`-prefixed key — for the subtractive engine:
  `subtractive.osc1.wave sine|tri|sqr`, `subtractive.osc1.pitch`,
  `subtractive.osc1.fine`, `subtractive.osc1.level`, `subtractive.osc1.pan`,
  `subtractive.osc2.*`, `subtractive.noise`,
  `subtractive.amp.attack|decay|sustain|release`,
  `subtractive.filter.cutoff|reso|env`,
  `subtractive.filterenv.attack|decay|sustain|release`,
  `subtractive.pitchlfo.rate|depth`, `subtractive.filterlfo.rate|depth`,
  `subtractive.glide`, `subtractive.spread`, `subtractive.master`

Unknown keys and unparseable values are ignored. Values are clamped to valid
ranges.

## Notes

- A `.state` older than a few seconds means a dead instance (files are cleaned
  up on the next launch).
- `tempo` is shared, so changing it in one instance changes it for all.
