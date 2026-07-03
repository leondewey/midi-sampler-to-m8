# midi-sampler-to-m8

A MIDI auto-sampler that generates sample chains for the **Dirtywave M8**.

## How it works

The tool sends MIDI note triggers across a range (e.g., C-1 to C3) to a synth or instrument, records the audio output, and assembles all recordings into a single WAV file where each slice corresponds to one MIDI note number. This lets you sample an instrument across its full range and play it back from the M8 Sampler with note-accurate triggering.

The workflow includes optional **auto-detection** of which notes produce sound and **auto-measurement** of sustained tail length to avoid clipping long decays.

## Prerequisites

- **Rust 1.85+** (the app uses edition 2024)
- For the live `render` path: audio loopback software (e.g., **BlackHole** on macOS) to route synth output back into the recorder, plus a MIDI-capable instrument, synth, or DAW plugin to send notes to
- For the offline `render-sfz` path: the **`sfizz_render`** binary (see [Offline SFZ rendering](#offline-sfz-rendering-render-sfz)) — no loopback or MIDI device needed

## Quick start

```bash
# Build
cargo build --release

# See available MIDI and audio devices
./target/release/midi-sampler-to-m8 list-devices

# Record and assemble a sample chain (minimal example)
./target/release/midi-sampler-to-m8 render \
  --virtual-midi \
  --audio-input "BlackHole 2ch" \
  --output ./output/sample.wav \
  --note-length 1 \
  --auto-slot-length
```

See `--help` for all options, including `--probe` (skip silent notes) and `--auto-slot-length` (detect tail duration).

**Note:** `--virtual-midi` is available on **macOS and Linux only**. On Windows, you must specify an existing MIDI output port.

## Chords

Two flags turn the single-note autosampler into a chord sampler (built-in qualities: `maj`, `min`, `dim`, `aug`, `maj7`, `min7`, `dom7`, `sus2`, `sus4`, `5`):

- **`--chord <quality>`** — one quality per file, **slice index = root note** (same keymap as single notes). Pressing C4 on the M8 plays the recorded C4-major chord. The quality is in the filename, so no lookup is needed.
  ```bash
  ... --chord maj      # -> sample/maj_8s_128slots.wav
  ```
- **`--chords [q1,q2,...]`** — chord files, laid quality-major (all roots of the first quality, then the next). Pass **no value for all qualities**. Each quality keeps **every sounding root** and is never split across files; qualities are packed into as many files as needed to fit `--max-slices` (default 128). The pressed note no longer equals the root, so the **CSV legend is written by default** to map each slice to its chord.
  ```bash
  ... --chords maj,min,dim         # packed into fitted file(s), tagged by qualities
  ... --chords --file-per-chord    # all qualities, one file each
  ... --chords --per-octave        # one file per octave (that octave's roots x qualities)
  ```
  **`--per-octave`** is often the most playable layout: each file holds a single octave's
  roots across all the chosen qualities (12 roots × qualities usually fits the 128-slice
  budget), so you don't jump between files while playing within a region.

### Batch (one probe/measurement, many files)

The probe and `--auto-slot-length` measurement depend only on the instrument, so a single command can produce many files sharing that work. Add **`--notes`** to also emit the plain single-note chain alongside the chord files:

```bash
render --virtual-midi --audio-input "BlackHole 2ch" \
  --auto-slot-length --note-length 0.25 \
  --notes --chords \
  --output ./output/Yamaha-Grand-Palm.wav
```

This probes/measures once, then writes the note chain plus chord files (auto-split to fit the M8), each named for the qualities it contains, e.g. `Yamaha-Grand-Palm/maj-min_0.25s_122slots.wav`. Set the M8 **SLICE** to each file's slice count (shown per file in the summary).

## Offline SFZ rendering (`render-sfz`)

The `render` command above samples a *live* instrument in real time through a loopback
device — the right tool for hardware synths and DAW plugins. When your instrument is an
**`.sfz` SoundFont**, the `render-sfz` command renders it **offline instead**: the tool
drives the [sfizz](https://sfz.tools/sfizz/) engine directly, so there is **no MIDI port,
no BlackHole, and no real-time waiting**. An entire chain renders faster than real time,
and independent chains render **in parallel**.

### Prerequisite: the sfizz offline renderer

`render-sfz` shells out to **`sfizz_render`**. Install it once:

- Download a prebuilt from the [sfizz releases](https://github.com/sfztools/sfizz/releases)
  (the macOS/Windows bundles include `bin/sfizz_render` and `libsfizz`), **or** build sfizz
  from source with `-DSFIZZ_RENDER=ON`.
- Put `sfizz_render` on your `PATH`, or point at it with `--sfizz-render <PATH>`.

The command fails fast with install instructions if it can't find the engine.

### Usage

```bash
# One font, C4..C5, 2 s slots — output lands in ./Piano/ next to the .sfz
midi-sampler-to-m8 render-sfz \
  --sfz ./instruments/Piano.sfz \
  --start-midi 60 --end-midi 72 \
  --slot-length 2 --note-length 1
```

Because rendering is offline and cheap, the interesting flags produce **more material in
parallel**:

- **`--velocities 40,80,120`** — one chain per velocity (soft/medium/hard layers).
- **`--variations N`** — N takes per chain; takes after the first apply seeded per-note
  velocity jitter (`--velocity-jitter`, default ±8) so each is a distinct render.
- **multiple `--sfz a.sfz b.sfz …`** — batch many fonts in one run, each in its own folder.
- **`--jobs N`** — cap the parallel worker count (default: number of CPUs).

Filenames encode only the axes you actually varied, e.g.
`Piano/notes_v80_take02_1s_13slots.wav`. `--chord <quality>` (slice = root), `--channels`,
`--unpadded`, `--csv`, and `--json` work just like the live path. Preview the exact set of
files a run would produce with `--dry-run` (it never invokes the engine).

> v1 scope: `render-sfz` covers the note chain, `--chord` (slice = root), velocity layers,
> and variation takes. The live path's probe-driven `--chords` packing, `--per-octave`, and
> `--auto-slot-length` are not yet ported (offline rendering needs no probe). SF2 support and
> an in-process `libsfizz` backend are possible follow-ups.

## Output files

Everything for a render lands in a **folder named after your `--output` stem**, with short
filenames embedding the file's role, note length, and slot count. For `--output dir/Yamaha.wav`
each job writes the padded chain:
- **`dir/Yamaha/notes_8s_128slots.wav`** — the full 16-bit PCM chain (resampled to 44.1 kHz), every slot present so the slot index maps 1:1 to the MIDI note.

Add **`--unpadded`** to also write a compact copy with the leading and trailing runs of silent
slots removed (interior silent slots kept), e.g. `dir/Yamaha/notes_8s_21slots_unpadded.wav` —
the slot count in its name reflects the trimmed length. Off by default.

Chord files use the chord tag as the name (`maj-min_8s_122slots.wav`, `oct-C3_maj-min_8s_48slots.wav`).

Channels follow the source by default (`--channels auto`): a stereo input yields a stereo WAV, a mono input a mono WAV. Use `--channels mono` or `--channels stereo` to force a layout. The layout is recorded in the WAV header (and the JSON sidecar), not the filename.

The CSV/JSON sidecars are **opt-in** (off by default) and land in the same folder:
- **`--csv`** writes `<name>_map.csv` — per-slot metadata (MIDI note, start/end times, sounding status)
- **`--json`** writes `<name>_render.json` — full render configuration (for reproduction)

## Recording quality

To match the level of commercial samples and trigger tightly on the M8, each render is
cleaned up by default:

- **Loudness** — every file is peak-normalized to **-1 dBFS** (preserving note-to-note
  dynamics; silent slots stay silent). Tune with `--normalize-dbfs <dBFS>` or disable with
  `--no-normalize`.
- **Tighter attacks** — leading silence (MIDI/audio latency + attack gap) is trimmed so each
  slice starts on the sound, with a **5 ms lookback** that keeps the attack transient intact
  and a **3 ms fade-in** that removes the resulting click. Controls: `--onset-dbfs`,
  `--onset-lookback-ms`, `--fade-in-ms`, or `--no-trim-onset` to keep the raw start.

Preview any of this on a single note with `--test-note 60` before a full run.

### How long should notes be?

`--note-length` is how long the key is *held*; with `--auto-slot-length` the decay tail is
captured regardless. For sustained/pad sounds a longer hold (1–2 s) captures more evolving
body; for piano/keys/plucked instruments **~0.5–1 s** is usually plenty, and **0.25 s** gives
a snappier, more staccato result. Recording all 10 chord qualities is often overkill — a
practical core is `maj,min,maj7,min7,dom7` (add `dim`/`sus4` if you use them).

## Loading into the M8

1. Load the padded WAV (e.g. `sample/notes_8s_128slots.wav`) into the M8 Sampler instrument.
2. Set the sampler to:
   - **SLICE:** the slice count shown in the run summary (`80` for the default 128 slots; packed chord files print their own count)
   - **PLAY:** FWD
   - **START:** 00
   - **LEN:** FF (full length)
3. Play notes 0–127 on the sampler to trigger each slice. Note 0 plays the first slice (C-1), note 60 plays the 61st slice, etc.

## Development

```bash
cargo build
cargo test
```

All tests pass (69 unit tests + 9 integration tests).
