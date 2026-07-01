//! Chord qualities: interval tables and helpers to turn a root MIDI note into
//! the set of notes to sound and a human-readable label.

use crate::notes::midi_to_m8_note;
use clap::ValueEnum;

/// A built-in chord quality, defined by its semitone intervals from the root.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ChordQuality {
    /// Major triad.
    Maj,
    /// Minor triad.
    Min,
    /// Diminished triad.
    Dim,
    /// Augmented triad.
    Aug,
    /// Major seventh.
    Maj7,
    /// Minor seventh.
    Min7,
    /// Dominant seventh.
    Dom7,
    /// Suspended second.
    Sus2,
    /// Suspended fourth.
    Sus4,
    /// Power chord (root + fifth).
    #[value(name = "5")]
    Power,
}

impl ChordQuality {
    /// Semitone offsets from the root that make up the chord.
    pub fn intervals(self) -> &'static [i8] {
        match self {
            ChordQuality::Maj => &[0, 4, 7],
            ChordQuality::Min => &[0, 3, 7],
            ChordQuality::Dim => &[0, 3, 6],
            ChordQuality::Aug => &[0, 4, 8],
            ChordQuality::Maj7 => &[0, 4, 7, 11],
            ChordQuality::Min7 => &[0, 3, 7, 10],
            ChordQuality::Dom7 => &[0, 4, 7, 10],
            ChordQuality::Sus2 => &[0, 2, 7],
            ChordQuality::Sus4 => &[0, 5, 7],
            ChordQuality::Power => &[0, 7],
        }
    }

    /// Short tag used in filenames and labels (e.g. `maj`, `5`).
    pub fn short(self) -> &'static str {
        match self {
            ChordQuality::Maj => "maj",
            ChordQuality::Min => "min",
            ChordQuality::Dim => "dim",
            ChordQuality::Aug => "aug",
            ChordQuality::Maj7 => "maj7",
            ChordQuality::Min7 => "min7",
            ChordQuality::Dom7 => "dom7",
            ChordQuality::Sus2 => "sus2",
            ChordQuality::Sus4 => "sus4",
            ChordQuality::Power => "5",
        }
    }
}

/// The MIDI notes that sound for `quality` rooted at `root`, dropping any tone
/// above MIDI 127. The root is always included, so the result is never empty.
pub fn chord_notes(root: u8, quality: ChordQuality) -> Vec<u8> {
    quality
        .intervals()
        .iter()
        .filter_map(|&iv| {
            let n = root as i16 + iv as i16;
            (0..=127).contains(&n).then_some(n as u8)
        })
        .collect()
}

/// Human-readable chord label, e.g. `C4 maj`.
pub fn chord_label(root: u8, quality: ChordQuality) -> String {
    format!("{} {}", midi_to_m8_note(root), quality.short())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_match_theory() {
        assert_eq!(ChordQuality::Maj.intervals(), &[0, 4, 7]);
        assert_eq!(ChordQuality::Min7.intervals(), &[0, 3, 7, 10]);
        assert_eq!(ChordQuality::Power.intervals(), &[0, 7]);
    }

    #[test]
    fn chord_notes_builds_from_root() {
        assert_eq!(chord_notes(60, ChordQuality::Maj), vec![60, 64, 67]);
        assert_eq!(chord_notes(60, ChordQuality::Min7), vec![60, 63, 67, 70]);
    }

    #[test]
    fn chord_notes_drops_tones_above_127() {
        // Root 125 maj would be 125,129,132 -> only the root fits.
        assert_eq!(chord_notes(125, ChordQuality::Maj), vec![125]);
        // Root 122 maj: 122,126,129 -> drops the 129.
        assert_eq!(chord_notes(122, ChordQuality::Maj), vec![122, 126]);
    }

    #[test]
    fn labels_read_like_the_m8() {
        assert_eq!(chord_label(60, ChordQuality::Maj), "C4 maj");
        assert_eq!(chord_label(0, ChordQuality::Power), "C-1 5");
    }
}
