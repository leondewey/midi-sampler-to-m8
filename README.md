# midi-sampler-to-m8

A MIDI auto-sampler that generates sample chains for the **Dirtywave M8**.

## How it works

The tool sends MIDI note triggers across a range (e.g., C-1 to C3) to a synth or instrument, records the audio output, and assembles all recordings into a single WAV file where each slice corresponds to one MIDI note number. This lets you sample an instrument across its full range and play it back from the M8 Sampler with note-accurate triggering.

The workflow includes optional **auto-detection** of which notes produce sound and **auto-measurement** of sustained tail length to avoid clipping long decays.

## Prerequisites

- **Rust 1.85+** (the app uses edition 2024)
- Audio loopback software (e.g., **BlackHole** on macOS) to route synth output back into the recorder
- A MIDI-capable instrument, synth, or DAW plugin to send notes to

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
  ... --chord maj      # -> sample_maj_stereo_128slots_8s.wav
  ```
- **`--chords <q1,q2,...>`** — *packed* mode: fills the slice budget (`--max-slices`, default 128) with **roots × qualities**, laid quality-major (all roots of the first quality, then the next). Each quality gets `max_slices / num_qualities` roots, spread evenly across the instrument's playable range. The pressed note no longer equals the root, so the **CSV legend is written by default** to map each slice to its chord.
  ```bash
  ... --chords maj,min,dim   # 42 roots x 3 qualities = 126 slices, + _map.csv legend
  ```

Both reuse the full pipeline (probe, `--auto-slot-length`, padded + unpadded copies). Set the M8 **SLICE** to the slice count shown in the summary (e.g. `78` for 120 slices, `80` for 128).

## Output files

The channel layout, slot count, and note length are embedded in the filename. For each render, you get two WAVs:
- **`sample_stereo_128slots_8s.wav`** — the full 16-bit PCM chain (resampled to 44.1 kHz), every slot present so the slot index maps 1:1 to the MIDI note.
- **`sample_stereo_21slots_8s_unpadded.wav`** — a compact copy with the leading and trailing runs of silent slots removed (interior silent slots are kept). The slot count in its name reflects the trimmed length.

Channels follow the source by default (`--channels auto`): a stereo input yields a stereo WAV, a mono input a mono WAV. Use `--channels mono` or `--channels stereo` to force a layout.

The CSV/JSON sidecars are **opt-in** (off by default):
- **`--csv`** writes `<name>_map.csv` — per-slot metadata (MIDI note, start/end times, sounding status)
- **`--json`** writes `<name>_render.json` — full render configuration (for reproduction)

## Loading into the M8

1. Load the padded WAV (e.g. `sample_stereo_128slots_8s.wav`) into the M8 Sampler instrument.
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

All tests pass (48 unit tests + 6 integration tests).
