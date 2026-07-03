//! Integration tests for the CLI surface, driven through the built binary.

use assert_cmd::Command;
use predicates::prelude::*;

fn bin() -> Command {
    Command::cargo_bin("midi-sampler-to-m8").unwrap()
}

#[test]
fn list_devices_command_exists() {
    // It may print "(none)" in CI, but it must not error on the argument itself.
    bin().arg("list-devices").assert().success();
}

#[test]
fn render_dry_run_prints_plan_without_devices() {
    bin()
        .args([
            "render",
            "--midi-output",
            "0",
            "--audio-input",
            "0",
            "--output",
            "plan.wav",
            "--note-length",
            "1",
            "--slot-length",
            "1",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("DRY RUN"))
        .stdout(predicate::str::contains("128 notes"))
        // Outputs land in a per-name folder with a short name embedding the note
        // length and slot count.
        .stdout(predicate::str::contains("plan/notes_1s_128slots.wav"));
}

#[test]
fn invalid_velocity_fails() {
    bin()
        .args([
            "render",
            "--midi-output",
            "0",
            "--audio-input",
            "0",
            "--output",
            "x.wav",
            "--velocity",
            "200",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("velocity"));
}

#[test]
fn invalid_channel_fails() {
    bin()
        .args([
            "render",
            "--midi-output",
            "0",
            "--audio-input",
            "0",
            "--output",
            "x.wav",
            "--channel",
            "17",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("channel"));
}

#[test]
fn invalid_slot_length_fails() {
    bin()
        .args([
            "render",
            "--midi-output",
            "0",
            "--audio-input",
            "0",
            "--output",
            "x.wav",
            "--slot-length",
            "0",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("slot-length"));
}

#[test]
fn non_44100_sample_rate_fails() {
    bin()
        .args([
            "render",
            "--midi-output",
            "0",
            "--audio-input",
            "0",
            "--output",
            "x.wav",
            "--sample-rate",
            "48000",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("44100"));
}

#[test]
fn render_sfz_dry_run_prints_plan_without_engine() {
    // The .sfz need not exist for a dry run — the engine is never invoked.
    bin()
        .args([
            "render-sfz",
            "--sfz",
            "Piano.sfz",
            "--start-midi",
            "60",
            "--end-midi",
            "72",
            "--slot-length",
            "2",
            "--note-length",
            "1",
            "--velocities",
            "40,90",
            "--variations",
            "2",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("DRY RUN"))
        .stdout(predicate::str::contains("Notes range : 60..72"))
        // 1 font x 2 velocities x 2 variations = 4 chains, tagged by axis.
        .stdout(predicate::str::contains("Output files (4)"))
        .stdout(predicate::str::contains("notes_v40_take01_1s_13slots.wav"));
}

#[test]
fn render_sfz_notes_plus_chords_lists_all_files() {
    bin()
        .args([
            "render-sfz",
            "--sfz",
            "Piano.sfz",
            "--note-length",
            "0.25",
            "--notes",
            "--chords",
            "maj,min,dim",
            "--auto-slot-length",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("measured per font at runtime"))
        // Default 0..127: notes chain pads to 128; each quality fills the 128-slice
        // budget, so maj/min/dim pack one-per-file -> notes + 3 chord files. (Chord
        // roots are narrowed to the sounding set at runtime; dry-run shows the
        // full-range upper bound.)
        .stdout(predicate::str::contains("Output files (4)"))
        .stdout(predicate::str::contains("notes_0.25s_128slots.wav"))
        .stdout(predicate::str::contains("maj_0.25s_128slots.wav"))
        .stdout(predicate::str::contains("dim_0.25s_128slots.wav"));
}

#[test]
fn render_sfz_chord_and_chords_are_mutually_exclusive() {
    bin()
        .args([
            "render-sfz",
            "--sfz",
            "Piano.sfz",
            "--chord",
            "maj",
            "--chords",
            "min",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--chord"));
}

#[test]
fn render_sfz_rejects_non_sfz_file() {
    bin()
        .args(["render-sfz", "--sfz", "instrument.sf2", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(".sfz"));
}

#[test]
fn render_sfz_non_44100_sample_rate_fails() {
    bin()
        .args([
            "render-sfz",
            "--sfz",
            "Piano.sfz",
            "--sample-rate",
            "48000",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("44100"));
}
