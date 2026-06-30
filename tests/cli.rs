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
        .stdout(predicate::str::contains("128 slots"))
        // Filename now embeds the slot count and note length.
        .stdout(predicate::str::contains("plan_128slots_1s.wav"));
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
