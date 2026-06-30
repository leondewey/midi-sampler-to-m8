//! Sidecar file writers: the CSV slot map and the JSON render config.

use crate::config::RenderConfig;
use crate::notes::Slot;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Write the `_map.csv` describing every slot's position and status.
///
/// `statuses[i]` is the status string (e.g. `rendered`, `silent`) for
/// `slots[i]`. Columns: `slot,midi_note,m8_note,start_seconds,end_seconds,velocity,status`.
pub fn write_csv_map(path: &Path, slots: &[Slot], velocity: u8, statuses: &[String]) -> Result<()> {
    let mut writer =
        csv::Writer::from_path(path).with_context(|| format!("creating CSV {}", path.display()))?;

    writer
        .write_record([
            "slot",
            "midi_note",
            "m8_note",
            "start_seconds",
            "end_seconds",
            "velocity",
            "status",
        ])
        .context("writing CSV header")?;

    for (slot, status) in slots.iter().zip(statuses.iter()) {
        writer
            .write_record([
                slot.slot.to_string(),
                slot.midi.to_string(),
                slot.m8_note.clone(),
                format!("{:.3}", slot.start_seconds),
                format!("{:.3}", slot.end_seconds),
                velocity.to_string(),
                status.clone(),
            ])
            .context("writing CSV row")?;
    }

    writer.flush().context("flushing CSV")?;
    Ok(())
}

/// Write the `_render.json` config.
pub fn write_json_config(path: &Path, config: &RenderConfig) -> Result<()> {
    let json = config.to_json().context("serializing render config")?;
    std::fs::write(path, json).with_context(|| format!("writing JSON {}", path.display()))?;
    Ok(())
}

/// Given an output WAV path, derive the sidecar paths next to it:
/// `<stem>_map.csv` and `<stem>_render.json`.
pub fn sidecar_paths(wav: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let stem = wav
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let dir = wav.parent().unwrap_or_else(|| Path::new("."));
    (
        dir.join(format!("{stem}_map.csv")),
        dir.join(format!("{stem}_render.json")),
    )
}

/// Build an output WAV path embedding the slot count and note length, e.g.
/// `<stem>_128slots_8s.wav` (padded) or `<stem>_21slots_8s_unpadded.wav`.
/// A whole note length renders without a decimal (`8.0` -> `8s`, `8.5` -> `8.5s`).
pub fn numbered_wav_path(out: &Path, slots: usize, note_length: f64, unpadded: bool) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let dir = out.parent().unwrap_or_else(|| Path::new("."));
    let ext = out
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wav".to_string());
    let suffix = if unpadded { "_unpadded" } else { "" };
    dir.join(format!("{stem}_{slots}slots_{note_length}s{suffix}.{ext}"))
}

/// Derive the per-note test WAV path: `<stem>_testNN.wav`.
pub fn test_note_path(wav: &Path, midi: u8) -> std::path::PathBuf {
    let stem = wav
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let dir = wav.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}_test{midi:02}.wav"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_wav_path_embeds_count_and_length() {
        // Padded: full count, whole note length renders without a decimal.
        assert_eq!(
            numbered_wav_path(Path::new("dir/out.wav"), 128, 8.0, false),
            PathBuf::from("dir/out_128slots_8s.wav")
        );
        // Unpadded: trimmed count plus the `_unpadded` marker.
        assert_eq!(
            numbered_wav_path(Path::new("dir/out.wav"), 21, 8.0, true),
            PathBuf::from("dir/out_21slots_8s_unpadded.wav")
        );
        // Fractional note length keeps its decimal.
        assert_eq!(
            numbered_wav_path(Path::new("out.wav"), 5, 8.5, false),
            PathBuf::from("out_5slots_8.5s.wav")
        );
    }
}
