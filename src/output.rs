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

/// Build an output WAV path inside a per-render folder named after the output
/// stem, with a short name embedding the file's role, note length, and slot
/// count. For `--output dir/Yamaha.wav`:
/// `dir/Yamaha/notes_0.25s_128slots.wav`,
/// `dir/Yamaha/maj-min_0.25s_122slots.wav`, or `..._unpadded.wav`.
/// A whole note length renders without a decimal (`8.0` -> `8s`, `8.5` -> `8.5s`).
pub fn output_wav_path(
    out: &Path,
    name: &str,
    slots: usize,
    note_length: f64,
    unpadded: bool,
) -> PathBuf {
    let folder = render_dir(out);
    let suffix = if unpadded { "_unpadded" } else { "" };
    folder.join(format!("{name}_{note_length}s_{slots}slots{suffix}.wav"))
}

/// The per-render output folder: `<parent>/<stem>/`.
pub fn render_dir(out: &Path) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let dir = out.parent().unwrap_or_else(|| Path::new("."));
    dir.join(stem)
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
    fn output_wav_path_uses_folder_and_short_name() {
        // Notes chain, whole note length renders without a decimal.
        assert_eq!(
            output_wav_path(Path::new("dir/Yamaha.wav"), "notes", 128, 0.25, false),
            PathBuf::from("dir/Yamaha/notes_0.25s_128slots.wav")
        );
        // Chord file with the `_unpadded` marker.
        assert_eq!(
            output_wav_path(Path::new("dir/Yamaha.wav"), "maj-min", 122, 0.25, true),
            PathBuf::from("dir/Yamaha/maj-min_0.25s_122slots_unpadded.wav")
        );
        // No parent dir -> folder next to the cwd; whole seconds stay integer.
        assert_eq!(
            output_wav_path(Path::new("out.wav"), "maj", 128, 8.0, false),
            PathBuf::from("out/maj_8s_128slots.wav")
        );
    }
}
