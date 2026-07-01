//! Sidecar file writers: the CSV slot map and the JSON render config.

use crate::config::RenderConfig;
use crate::notes::Slot;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Write the `_map.csv` describing every slot's position and status.
///
/// `statuses[i]` is the status string (e.g. `rendered`, `silent`) for
/// `slots[i]`. Columns:
/// `slot,midi_note,m8_note,chord,notes,start_seconds,end_seconds,velocity,status`.
/// In single-note mode `chord` is the note name and `notes` is the single note,
/// so the extra columns are harmless for note renders and act as the chord legend
/// for chord renders.
pub fn write_csv_map(path: &Path, slots: &[Slot], velocity: u8, statuses: &[String]) -> Result<()> {
    let mut writer =
        csv::Writer::from_path(path).with_context(|| format!("creating CSV {}", path.display()))?;

    writer
        .write_record([
            "slot",
            "midi_note",
            "m8_note",
            "chord",
            "notes",
            "start_seconds",
            "end_seconds",
            "velocity",
            "status",
        ])
        .context("writing CSV header")?;

    for (slot, status) in slots.iter().zip(statuses.iter()) {
        let notes = slot
            .notes
            .iter()
            .map(|&n| crate::notes::midi_to_m8_note(n))
            .collect::<Vec<_>>()
            .join(" ");
        writer
            .write_record([
                slot.slot.to_string(),
                slot.midi.to_string(),
                slot.m8_note.clone(),
                slot.label.clone(),
                notes,
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

/// Build an output WAV path embedding an optional tag, the channel layout, the
/// slot count, and the note length, e.g. `<stem>_mono_128slots_8s.wav`,
/// `<stem>_maj_stereo_128slots_8s.wav`, or
/// `<stem>_packed_stereo_21slots_8s_unpadded.wav`. A whole note length renders
/// without a decimal (`8.0` -> `8s`, `8.5` -> `8.5s`).
pub fn numbered_wav_path(
    out: &Path,
    tag: Option<&str>,
    layout: &str,
    slots: usize,
    note_length: f64,
    unpadded: bool,
) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let dir = out.parent().unwrap_or_else(|| Path::new("."));
    let ext = out
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wav".to_string());
    let tag = tag.map(|t| format!("_{t}")).unwrap_or_default();
    let suffix = if unpadded { "_unpadded" } else { "" };
    dir.join(format!(
        "{stem}{tag}_{layout}_{slots}slots_{note_length}s{suffix}.{ext}"
    ))
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
            numbered_wav_path(Path::new("dir/out.wav"), None, "mono", 128, 8.0, false),
            PathBuf::from("dir/out_mono_128slots_8s.wav")
        );
        // Unpadded: trimmed count plus the `_unpadded` marker, stereo layout.
        assert_eq!(
            numbered_wav_path(Path::new("dir/out.wav"), None, "stereo", 21, 8.0, true),
            PathBuf::from("dir/out_stereo_21slots_8s_unpadded.wav")
        );
        // Fractional note length keeps its decimal.
        assert_eq!(
            numbered_wav_path(Path::new("out.wav"), None, "mono", 5, 8.5, false),
            PathBuf::from("out_mono_5slots_8.5s.wav")
        );
        // A tag (chord quality / packed) is inserted after the stem, before the layout.
        assert_eq!(
            numbered_wav_path(Path::new("out.wav"), Some("maj"), "stereo", 128, 8.0, false),
            PathBuf::from("out_maj_stereo_128slots_8s.wav")
        );
        assert_eq!(
            numbered_wav_path(Path::new("out.wav"), Some("packed"), "stereo", 120, 8.0, true),
            PathBuf::from("out_packed_stereo_120slots_8s_unpadded.wav")
        );
    }
}
