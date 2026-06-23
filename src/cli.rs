//! Command-line interface definitions and argument validation.

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "midi-sampler-to-m8",
    version,
    about = "Autosample any MIDI-playable instrument into a Dirtywave M8 sample-chain WAV"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List available MIDI outputs and audio inputs.
    ListDevices,
    /// Send MIDI notes, record the audio output, and build an M8 sample chain.
    Render(RenderArgs),
}

/// Output channel layout.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Channels {
    Mono,
    Stereo,
}

impl Channels {
    pub fn count(self) -> u16 {
        match self {
            Channels::Mono => 1,
            Channels::Stereo => 2,
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct RenderArgs {
    /// MIDI output device (index, exact name, or unique substring).
    /// Omit when using --virtual-midi.
    #[arg(long)]
    pub midi_output: Option<String>,

    /// Create a virtual MIDI output port named "midi-sampler-to-m8" instead of
    /// connecting to an existing device. Enable it as a MIDI input in your
    /// instrument. Mutually exclusive with --midi-output.
    #[arg(long)]
    pub virtual_midi: bool,

    /// Audio input device (index, exact name, or unique substring).
    #[arg(long)]
    pub audio_input: String,

    /// Output WAV path. Sidecar `_map.csv` and `_render.json` are written too.
    #[arg(long)]
    pub output: PathBuf,

    /// Note-on velocity (0..=127).
    #[arg(long, default_value_t = 100)]
    pub velocity: u8,

    /// MIDI channel (1..=16).
    #[arg(long, default_value_t = 1)]
    pub channel: u8,

    /// How long the note is held, in seconds.
    #[arg(long, default_value_t = 8.0)]
    pub note_length: f64,

    /// Total recorded length per slot, in seconds (captures the release tail).
    #[arg(long, default_value_t = 9.0)]
    pub slot_length: f64,

    /// Silence before each Note On, in milliseconds.
    #[arg(long, default_value_t = 100)]
    pub pre_roll_ms: u64,

    /// Output sample rate. v1 requires 44100.
    #[arg(long, default_value_t = 44_100)]
    pub sample_rate: u32,

    /// Output channel layout.
    #[arg(long, value_enum, default_value_t = Channels::Mono)]
    pub channels: Channels,

    /// First MIDI note to render (0..=127).
    #[arg(long, default_value_t = 0)]
    pub start_midi: u8,

    /// Last MIDI note to render (0..=127).
    #[arg(long, default_value_t = 127)]
    pub end_midi: u8,

    /// Print the render plan without opening any devices.
    #[arg(long)]
    pub dry_run: bool,

    /// Render and record only this single MIDI note, to a short test WAV.
    #[arg(long)]
    pub test_note: Option<u8>,

    /// Disable the probe pass and record every note at full slot length.
    #[arg(long)]
    pub no_probe: bool,

    /// Probe note hold, in milliseconds (how long each note is sounded while
    /// detecting whether it produces sound).
    #[arg(long, default_value_t = 300)]
    pub probe_ms: u64,

    /// Peak level (0.0..=1.0) above which a probed note counts as sounding.
    #[arg(long, default_value_t = 0.003)]
    pub probe_threshold: f32,

    /// Measure the longest note's ring-out at runtime and use it as the slot
    /// length (overrides --slot-length). Pair with a short --note-length to get
    /// snug slots on a ring-out instrument.
    #[arg(long)]
    pub auto_slot_length: bool,

    /// Upper bound (and measurement ceiling) for the auto slot length, seconds.
    #[arg(long, default_value_t = 20.0)]
    pub max_slot_length: f64,

    /// How many notes to measure when auto-detecting the slot length.
    #[arg(long, default_value_t = 8)]
    pub measure_notes: u8,

    /// Level below which a ring-out tail counts as silent, for auto slot length
    /// (default ≈ -78 dBFS, so the full decay is captured).
    #[arg(long, default_value_t = 0.000125)]
    pub decay_threshold: f32,

    /// Seconds of margin added after the measured tail, for auto slot length.
    #[arg(long, default_value_t = 0.7)]
    pub slot_margin: f64,

    /// Fade-out applied to the end of each recorded slot, in milliseconds, to
    /// avoid a click at the slot boundary. 0 disables.
    #[arg(long, default_value_t = 10)]
    pub fade_ms: u64,
}

impl RenderArgs {
    /// Validate user-supplied values, returning a clear error on failure.
    pub fn validate(&self) -> Result<()> {
        match (self.midi_output.is_some(), self.virtual_midi) {
            (true, true) => bail!("use either --midi-output or --virtual-midi, not both"),
            (false, false) => {
                bail!("specify a MIDI output: --midi-output <NAME|INDEX> or --virtual-midi")
            }
            _ => {}
        }
        if self.velocity > 127 {
            bail!("velocity must be 0..=127 (got {})", self.velocity);
        }
        if !(1..=16).contains(&self.channel) {
            bail!("channel must be 1..=16 (got {})", self.channel);
        }
        if self.slot_length <= 0.0 || self.slot_length.is_nan() {
            bail!(
                "slot-length must be greater than 0 (got {})",
                self.slot_length
            );
        }
        if self.note_length <= 0.0 || self.note_length.is_nan() {
            bail!(
                "note-length must be greater than 0 (got {})",
                self.note_length
            );
        }
        if self.sample_rate != 44_100 {
            bail!(
                "v1 only supports --sample-rate 44100 (got {}); set your audio device to 44.1 kHz or omit the flag",
                self.sample_rate
            );
        }
        if self.start_midi > 127 || self.end_midi > 127 {
            bail!("MIDI notes must be 0..=127");
        }
        if self.start_midi > self.end_midi {
            bail!(
                "start-midi ({}) must be <= end-midi ({})",
                self.start_midi,
                self.end_midi
            );
        }
        if let Some(n) = self.test_note
            && n > 127
        {
            bail!("test-note must be 0..=127 (got {n})");
        }
        if self.probe_ms == 0 {
            bail!("probe-ms must be greater than 0");
        }
        if self.probe_threshold < 0.0 || self.probe_threshold.is_nan() {
            bail!(
                "probe-threshold must be >= 0 (got {})",
                self.probe_threshold
            );
        }
        if self.auto_slot_length {
            if self.max_slot_length <= 0.0 || self.max_slot_length.is_nan() {
                bail!(
                    "max-slot-length must be greater than 0 (got {})",
                    self.max_slot_length
                );
            }
            if self.measure_notes == 0 {
                bail!("measure-notes must be at least 1");
            }
            if self.decay_threshold < 0.0 || self.decay_threshold.is_nan() {
                bail!(
                    "decay-threshold must be >= 0 (got {})",
                    self.decay_threshold
                );
            }
            if self.slot_margin < 0.0 || self.slot_margin.is_nan() {
                bail!("slot-margin must be >= 0 (got {})", self.slot_margin);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RenderArgs {
        RenderArgs {
            midi_output: Some("0".into()),
            virtual_midi: false,
            audio_input: "0".into(),
            output: PathBuf::from("out.wav"),
            velocity: 100,
            channel: 1,
            note_length: 8.0,
            slot_length: 9.0,
            pre_roll_ms: 100,
            sample_rate: 44_100,
            channels: Channels::Mono,
            start_midi: 0,
            end_midi: 127,
            dry_run: false,
            test_note: None,
            no_probe: false,
            probe_ms: 300,
            probe_threshold: 0.003,
            auto_slot_length: false,
            max_slot_length: 20.0,
            measure_notes: 8,
            decay_threshold: 0.000125,
            slot_margin: 0.7,
            fade_ms: 10,
        }
    }

    #[test]
    fn valid_defaults_pass() {
        assert!(base().validate().is_ok());
    }

    #[test]
    fn virtual_midi_alone_passes() {
        let mut a = base();
        a.midi_output = None;
        a.virtual_midi = true;
        assert!(a.validate().is_ok());
    }

    #[test]
    fn no_midi_source_fails() {
        let mut a = base();
        a.midi_output = None;
        a.virtual_midi = false;
        assert!(a.validate().is_err());
    }

    #[test]
    fn both_midi_sources_fail() {
        let mut a = base();
        a.virtual_midi = true; // midi_output already Some
        assert!(a.validate().is_err());
    }

    #[test]
    fn invalid_velocity_fails() {
        let mut a = base();
        a.velocity = 200;
        assert!(a.validate().is_err());
    }

    #[test]
    fn invalid_channel_fails() {
        let mut a = base();
        a.channel = 0;
        assert!(a.validate().is_err());
        a.channel = 17;
        assert!(a.validate().is_err());
    }

    #[test]
    fn invalid_slot_length_fails() {
        let mut a = base();
        a.slot_length = 0.0;
        assert!(a.validate().is_err());
    }

    #[test]
    fn non_44100_sample_rate_fails() {
        let mut a = base();
        a.sample_rate = 48_000;
        assert!(a.validate().is_err());
    }

    #[test]
    fn invalid_probe_values_fail() {
        let mut a = base();
        a.probe_ms = 0;
        assert!(a.validate().is_err());
        let mut b = base();
        b.probe_threshold = -1.0;
        assert!(b.validate().is_err());
    }

    #[test]
    fn invalid_auto_slot_length_values_fail() {
        let mut a = base();
        a.auto_slot_length = true;
        a.max_slot_length = 0.0;
        assert!(a.validate().is_err());
        let mut b = base();
        b.auto_slot_length = true;
        b.measure_notes = 0;
        assert!(b.validate().is_err());
        // The same bad values are ignored when auto is off.
        let mut c = base();
        c.max_slot_length = 0.0;
        c.measure_notes = 0;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn channels_count_mapping() {
        assert_eq!(Channels::Mono.count(), 1);
        assert_eq!(Channels::Stereo.count(), 2);
    }
}
