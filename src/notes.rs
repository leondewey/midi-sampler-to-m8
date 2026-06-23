//! MIDI-note → M8 note-name conversion and slot-map construction.
//!
//! The M8 sends `C-1` for MIDI 0, `C0` for MIDI 12, `C4` for MIDI 60, and
//! `G9` for MIDI 127 (verified with MIDI Monitor). We mirror that mapping so
//! the generated CSV reads the same names the M8 displays.

/// Pitch-class names indexed by `midi % 12`.
const PITCH_CLASSES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Convert a MIDI note number to its M8 note name.
///
/// `octave = floor(midi / 12) - 1`, so MIDI 0 is `C-1` and MIDI 127 is `G9`.
pub fn midi_to_m8_note(midi: u8) -> String {
    let name = PITCH_CLASSES[(midi % 12) as usize];
    let octave = (midi as i32 / 12) - 1;
    format!("{name}{octave}")
}

/// One entry in the M8 sample-chain map: a single MIDI note recorded into a
/// fixed-length slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// Position in the chain (`midi - start_midi`).
    pub slot: u8,
    /// MIDI note that was played.
    pub midi: u8,
    /// M8 note name for `midi`.
    pub m8_note: String,
    /// Start offset of this slot within the final WAV, in seconds.
    pub start_seconds: f64,
    /// End offset of this slot within the final WAV, in seconds.
    pub end_seconds: f64,
}

/// Build the ordered slot map for MIDI notes `start_midi..=end_midi`.
///
/// Each slot occupies exactly `slot_length` seconds, laid back to back, so
/// `slot` index equals `midi - start_midi` and `start_seconds = slot * slot_length`.
pub fn build_slot_map(start_midi: u8, end_midi: u8, slot_length: f64) -> Vec<Slot> {
    (start_midi..=end_midi)
        .enumerate()
        .map(|(i, midi)| {
            let slot = i as u8;
            let start_seconds = i as f64 * slot_length;
            Slot {
                slot,
                midi,
                m8_note: midi_to_m8_note(midi),
                start_seconds,
                end_seconds: start_seconds + slot_length,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names_match_m8() {
        assert_eq!(midi_to_m8_note(0), "C-1");
        assert_eq!(midi_to_m8_note(1), "C#-1");
        assert_eq!(midi_to_m8_note(11), "B-1");
        assert_eq!(midi_to_m8_note(12), "C0");
        assert_eq!(midi_to_m8_note(24), "C1");
        assert_eq!(midi_to_m8_note(36), "C2");
        assert_eq!(midi_to_m8_note(48), "C3");
        assert_eq!(midi_to_m8_note(60), "C4");
        assert_eq!(midi_to_m8_note(108), "C8");
        assert_eq!(midi_to_m8_note(120), "C9");
        assert_eq!(midi_to_m8_note(127), "G9");
    }

    #[test]
    fn full_range_map_has_128_slots() {
        let map = build_slot_map(0, 127, 9.0);
        assert_eq!(map.len(), 128);
    }

    #[test]
    fn first_and_last_slots_are_correct() {
        let map = build_slot_map(0, 127, 9.0);

        let first = &map[0];
        assert_eq!(first.slot, 0);
        assert_eq!(first.midi, 0);
        assert_eq!(first.m8_note, "C-1");
        assert_eq!(first.start_seconds, 0.0);
        assert_eq!(first.end_seconds, 9.0);

        let last = &map[127];
        assert_eq!(last.slot, 127);
        assert_eq!(last.midi, 127);
        assert_eq!(last.m8_note, "G9");
        assert_eq!(last.start_seconds, 127.0 * 9.0);
        assert_eq!(last.end_seconds, 128.0 * 9.0);
    }

    #[test]
    fn slot_index_equals_midi_for_full_range() {
        let map = build_slot_map(0, 127, 9.0);
        for (i, slot) in map.iter().enumerate() {
            assert_eq!(slot.slot as usize, i);
            assert_eq!(slot.midi as usize, i);
        }
    }

    #[test]
    fn start_end_seconds_track_slot_length() {
        let map = build_slot_map(0, 127, 9.0);
        // Slot 60 (middle C) should start at 540s and end at 549s.
        assert_eq!(map[60].start_seconds, 540.0);
        assert_eq!(map[60].end_seconds, 549.0);
    }
}
