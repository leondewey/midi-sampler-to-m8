//! Offline SFZ rendering via the `sfizz_render` CLI.
//!
//! Where the live `render` path plays MIDI to an external instrument and records
//! the audio coming back in real time, this path drives the sfizz engine
//! *offline*: it authors a Standard MIDI File for a whole note/chord chain,
//! renders it to a WAV with `sfizz_render` faster than real time, and hands the
//! decoded buffer back for slicing. No MIDI port, no audio loopback, no
//! real-time waiting — which also makes it trivially parallel across chains.

use crate::notes::Slot;
use anyhow::{Context, Result, bail};
use midly::num::{u4, u7, u15, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, TrackEvent, TrackEventKind, Timing};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// SMF pulses-per-quarter. Paired with `TEMPO_US_PER_QUARTER` below so that one
/// quarter note lasts exactly one second and therefore one tick is exactly one
/// millisecond — event times in seconds map to integer ticks with ms resolution.
const PPQ: u16 = 1000;
/// Microseconds per quarter note: 1,000,000 µs = 1 second per quarter.
const TEMPO_US_PER_QUARTER: u32 = 1_000_000;

/// A decoded, offline-rendered WAV: interleaved `f32` samples plus its layout.
pub struct RenderedWav {
    /// Interleaved samples in `[-1.0, 1.0]` (`L R L R …` for stereo).
    pub samples: Vec<f32>,
    /// Channel count reported by the rendered WAV.
    pub channels: u16,
    /// Sample rate reported by the rendered WAV.
    pub sample_rate: u32,
}

/// Locate the `sfizz_render` binary: the explicit `--sfizz-render` path if given,
/// otherwise the first `sfizz_render` found on `PATH`.
pub fn locate_engine(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        bail!("--sfizz-render path does not exist or is not a file: {}", p.display());
    }
    find_on_path("sfizz_render").ok_or_else(|| {
        anyhow::anyhow!(
            "could not find `sfizz_render` on PATH.\n\
             Install the sfizz offline renderer (build sfizz with -DSFIZZ_RENDER=ON, or grab a\n\
             prebuilt from https://github.com/sfztools/sfizz/releases) and either put it on PATH\n\
             or pass its location with --sfizz-render <PATH>."
        )
    })
}

/// First executable named `name` found across the `PATH` directories.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|cand| cand.is_file())
}

/// Author a single-track SMF for the whole chain: sound each slot's notes at that
/// slot's boundary, hold them for `note_len_ms`, and end the track at the last
/// slot boundary so the rendered WAV spans exactly the chain length.
///
/// `velocities[i]` is the note-on velocity for slot `i` (all notes of a chord in
/// a slot share it). `slot_ms` is the integer slot length in milliseconds.
pub fn build_chain_smf(slots: &[Slot], slot_ms: u32, note_len_ms: u32, velocities: &[u8]) -> Vec<u8> {
    // Collect absolute-timed events, then convert to delta times. `order` keeps
    // note-offs (0) ahead of note-ons (1) that land on the same tick.
    let total_ms = slot_ms * slots.len() as u32;
    let mut events: Vec<(u32, u8, TrackEventKind<'static>)> = Vec::new();

    // Tempo first, at tick 0.
    events.push((
        0,
        0,
        TrackEventKind::Meta(MetaMessage::Tempo(u24::from_int_lossy(TEMPO_US_PER_QUARTER))),
    ));

    for (i, slot) in slots.iter().enumerate() {
        let on_tick = slot_ms * i as u32;
        let off_tick = (on_tick + note_len_ms).min(total_ms);
        let vel = velocities.get(i).copied().unwrap_or(100);
        for &note in &slot.notes {
            events.push((
                on_tick,
                1,
                TrackEventKind::Midi {
                    channel: u4::from_int_lossy(0),
                    message: MidiMessage::NoteOn {
                        key: u7::from_int_lossy(note),
                        vel: u7::from_int_lossy(vel),
                    },
                },
            ));
            events.push((
                off_tick,
                0,
                TrackEventKind::Midi {
                    channel: u4::from_int_lossy(0),
                    message: MidiMessage::NoteOff {
                        key: u7::from_int_lossy(note),
                        vel: u7::from_int_lossy(0),
                    },
                },
            ));
        }
    }

    // Stable sort by (tick, order) so simultaneous offs precede ons.
    events.sort_by_key(|(tick, order, _)| (*tick, *order));

    let mut track: Vec<TrackEvent<'static>> = Vec::with_capacity(events.len() + 1);
    let mut prev = 0u32;
    for (tick, _order, kind) in events {
        let delta = tick - prev;
        prev = tick;
        track.push(TrackEvent {
            delta: u28::from_int_lossy(delta),
            kind,
        });
    }
    // End the track at the final slot boundary (rendered with --use-eot).
    track.push(TrackEvent {
        delta: u28::from_int_lossy(total_ms - prev),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });

    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(u15::from_int_lossy(PPQ))),
        tracks: vec![track],
    };
    let mut buf = Vec::new();
    // Writing to an in-memory Vec is infallible in practice.
    smf.write_std(&mut buf)
        .expect("writing SMF to an in-memory buffer never fails");
    buf
}

/// Render `smf` against `sfz` with `sfizz_render`, returning the decoded WAV.
///
/// Writes the SMF and output WAV to uniquely-named temp files, spawns the
/// engine, reads the result back, and cleans up. Safe to call from many threads
/// at once (temp names are process- and counter-unique).
pub fn render_chain(engine: &Path, sfz: &Path, smf: &[u8], sample_rate: u32) -> Result<RenderedWav> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir();
    let mid_path = tmp.join(format!("sfizz-{}-{n}.mid", std::process::id()));
    let wav_path = tmp.join(format!("sfizz-{}-{n}.wav", std::process::id()));

    std::fs::write(&mid_path, smf)
        .with_context(|| format!("writing temp MIDI {}", mid_path.display()))?;

    let result = (|| {
        let output = Command::new(engine)
            .arg("--sfz")
            .arg(sfz)
            .arg("--midi")
            .arg(&mid_path)
            .arg("--wav")
            .arg(&wav_path)
            .arg("--samplerate")
            .arg(sample_rate.to_string())
            .arg("--use-eot")
            .output()
            .with_context(|| format!("running {}", engine.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "sfizz_render failed for {} ({}):\n{}",
                sfz.display(),
                output.status,
                stderr.trim()
            );
        }
        read_wav_f32(&wav_path)
    })();

    // Best-effort cleanup regardless of outcome.
    let _ = std::fs::remove_file(&mid_path);
    let _ = std::fs::remove_file(&wav_path);
    result
}

/// Read a WAV into interleaved `f32`, converting from int or float PCM.
fn read_wav_f32(path: &Path) -> Result<RenderedWav> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("opening rendered WAV {}", path.display()))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .context("reading float WAV samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<_, _>>()
                .context("reading int WAV samples")?
        }
    };
    Ok(RenderedWav {
        samples,
        channels: spec.channels,
        sample_rate: spec.sample_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::build_slot_map;

    #[test]
    fn smf_places_notes_on_slot_boundaries() {
        // Two single-note slots, 1s each, held 0.5s.
        let slots = build_slot_map(60, 61, 1.0, None);
        let smf_bytes = build_chain_smf(&slots, 1000, 500, &[100, 100]);

        let parsed = Smf::parse(&smf_bytes).expect("round-trip parse");
        assert_eq!(parsed.tracks.len(), 1);

        // Walk the single track accumulating absolute ticks, collecting note-ons.
        let mut abs = 0u32;
        let mut note_ons: Vec<(u32, u8)> = Vec::new();
        let mut saw_eot = false;
        for ev in &parsed.tracks[0] {
            abs += ev.delta.as_int();
            match ev.kind {
                TrackEventKind::Midi {
                    message: MidiMessage::NoteOn { key, vel },
                    ..
                } if vel.as_int() > 0 => note_ons.push((abs, key.as_int())),
                TrackEventKind::Meta(MetaMessage::EndOfTrack) => saw_eot = true,
                _ => {}
            }
        }
        assert!(saw_eot, "track must end with EndOfTrack");
        // Note 60 at t=0ms, note 61 at t=1000ms.
        assert_eq!(note_ons, vec![(0, 60), (1000, 61)]);
    }

    #[test]
    fn smf_chord_sounds_all_notes_together() {
        use crate::chords::ChordQuality;
        // One C-major slot: notes 60/64/67 all on at tick 0.
        let slots = build_slot_map(60, 60, 2.0, Some(ChordQuality::Maj));
        let smf_bytes = build_chain_smf(&slots, 2000, 1000, &[90]);
        let parsed = Smf::parse(&smf_bytes).unwrap();

        let mut abs = 0u32;
        let mut ons = Vec::new();
        for ev in &parsed.tracks[0] {
            abs += ev.delta.as_int();
            if let TrackEventKind::Midi {
                message: MidiMessage::NoteOn { key, vel },
                ..
            } = ev.kind
                && vel.as_int() > 0
            {
                ons.push((abs, key.as_int(), vel.as_int()));
            }
        }
        assert_eq!(
            ons,
            vec![(0, 60, 90), (0, 64, 90), (0, 67, 90)],
            "all chord tones start together at the slot boundary with the slot velocity"
        );
    }

    #[test]
    fn read_wav_round_trips_int_pcm() {
        // Write a tiny 16-bit stereo WAV and read it back as f32.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sfz-read-test-{}.wav", std::process::id()));
        crate::wav::write_wav(&path, &[0.5, -0.5, 0.25, -0.25], 44_100, 2).unwrap();

        let w = read_wav_f32(&path).unwrap();
        assert_eq!(w.channels, 2);
        assert_eq!(w.sample_rate, 44_100);
        assert_eq!(w.samples.len(), 4);
        assert!((w.samples[0] - 0.5).abs() < 1e-3);
        assert!((w.samples[1] + 0.5).abs() < 1e-3);
        std::fs::remove_file(&path).ok();
    }
}
