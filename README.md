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

## Output files

The slot count and note length are embedded in the filename. For each render, you get two WAVs:
- **`sample_128slots_8s.wav`** — the full 16-bit PCM chain (resampled to 44.1 kHz), every slot present so the slot index maps 1:1 to the MIDI note.
- **`sample_21slots_8s_unpadded.wav`** — a compact copy with the leading and trailing runs of silent slots removed (interior silent slots are kept). The slot count in its name reflects the trimmed length.

The CSV/JSON sidecars are **opt-in** (off by default):
- **`--csv`** writes `<name>_map.csv` — per-slot metadata (MIDI note, start/end times, sounding status)
- **`--json`** writes `<name>_render.json` — full render configuration (for reproduction)

## Loading into the M8

1. Load the padded WAV (e.g. `sample_128slots_8s.wav`) into the M8 Sampler instrument.
2. Set the sampler to:
   - **SLICE:** 80 (fixed-slice mode)
   - **PLAY:** FWD
   - **START:** 00
   - **LEN:** FF (full length)
3. Play notes 0–127 on the sampler to trigger each slice. Note 0 plays the first slice (C-1), note 60 plays the 61st slice, etc.

## Development

```bash
cargo build
cargo test
```

All tests pass (39 unit tests + 6 integration tests).
