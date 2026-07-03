//! Command-line interface definitions and argument validation.

use crate::chords::ChordQuality;
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
    Render(Box<RenderArgs>),
    /// Render .sfz instruments offline (via sfizz_render) into M8 sample chains.
    /// No MIDI port or audio loopback; faster than real time and parallel.
    RenderSfz(Box<RenderSfzArgs>),
}

/// Output channel layout.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Channels {
    /// Follow the source: mono input -> mono, stereo input -> stereo.
    Auto,
    Mono,
    Stereo,
}

impl Channels {
    /// Resolve the output channel count, using the source's native channel count
    /// for `Auto`.
    pub fn resolve(self, native_channels: u16) -> u16 {
        match self {
            Channels::Mono => 1,
            Channels::Stereo => 2,
            Channels::Auto => {
                if native_channels >= 2 {
                    2
                } else {
                    1
                }
            }
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

    /// Output channel layout. `auto` follows the source (mono in -> mono out,
    /// stereo in -> stereo out); `mono`/`stereo` force a layout.
    #[arg(long, value_enum, default_value_t = Channels::Auto)]
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

    /// Also write the compact `_unpadded` copy with leading/trailing silent
    /// slots removed (interior silent slots kept). Off by default.
    #[arg(long)]
    pub unpadded: bool,

    /// Also write the `_map.csv` sidecar describing every slot.
    #[arg(long)]
    pub csv: bool,

    /// Also write the `_render.json` sidecar with the render config.
    #[arg(long)]
    pub json: bool,

    /// Also render the plain single-note chain (in addition to any chord files).
    /// Shares the one probe/measurement pass. A plain render with no chord flags
    /// already produces this.
    #[arg(long)]
    pub notes: bool,

    /// Record a chord of this quality rooted at each note instead of a single
    /// note. Slice index stays equal to the root note. Mutually exclusive with
    /// --chords.
    #[arg(long, value_enum)]
    pub chord: Option<ChordQuality>,

    /// Render chord files for these qualities (comma-separated); pass with no
    /// value for all qualities. Each quality gets every sounding root and is kept
    /// whole; qualities are packed into as many files as needed to fit the slice
    /// budget. Mutually exclusive with --chord.
    #[arg(long, value_enum, value_delimiter = ',', num_args = 0..)]
    pub chords: Option<Vec<ChordQuality>>,

    /// With --chords, write one file per quality instead of packing several
    /// whole qualities per file.
    #[arg(long)]
    pub file_per_chord: bool,

    /// With --chords, write one file per octave (that octave's roots x the
    /// selected qualities) so a playable region stays in one file.
    #[arg(long)]
    pub per_octave: bool,

    /// Slice budget per file for chord mode (the M8 fixed-slice count).
    #[arg(long, default_value_t = 128)]
    pub max_slices: usize,

    /// Disable peak normalization; keep the raw captured level.
    #[arg(long)]
    pub no_normalize: bool,

    /// Peak-normalization target in dBFS (each file's loudest point). Default -1.
    #[arg(long, default_value_t = -1.0)]
    pub normalize_dbfs: f64,

    /// Disable leading-silence trimming (keep the raw latency/attack gap).
    #[arg(long)]
    pub no_trim_onset: bool,

    /// Level (dBFS) at which a slot's sound is considered to start, for onset
    /// trimming. Default -55.
    #[arg(long, default_value_t = -55.0)]
    pub onset_dbfs: f64,

    /// Milliseconds kept before the detected onset, so the attack transient is
    /// preserved. Default 5.
    #[arg(long, default_value_t = 5)]
    pub onset_lookback_ms: u64,

    /// Fade-in at the start of each slot, in milliseconds, to avoid a click after
    /// trimming. 0 disables. Default 3.
    #[arg(long, default_value_t = 3)]
    pub fade_in_ms: u64,
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
        if self.chord.is_some() && self.chords.is_some() {
            bail!("use either --chord (one quality, slice = root) or --chords (packed), not both");
        }
        if self.file_per_chord && self.chords.is_none() {
            bail!("--file-per-chord only applies with --chords");
        }
        if self.per_octave && self.chords.is_none() {
            bail!("--per-octave only applies with --chords");
        }
        if self.per_octave && self.file_per_chord {
            bail!("use either --per-octave or --file-per-chord, not both");
        }
        if self.chords.is_some() {
            if self.max_slices < 1 {
                bail!("max-slices must be at least 1");
            }
            if self.max_slices > 255 {
                bail!(
                    "max-slices must be <= 255 (the M8 fixed-slice maximum); got {}",
                    self.max_slices
                );
            }
        }
        Ok(())
    }

    /// The chord qualities to render for `--chords`: the given list, all
    /// qualities when `--chords` was passed with no value, or empty when the flag
    /// was not given at all.
    pub fn resolved_chords(&self) -> Vec<ChordQuality> {
        match &self.chords {
            Some(v) if !v.is_empty() => v.clone(),
            Some(_) => ChordQuality::ALL.to_vec(),
            None => Vec::new(),
        }
    }
}

/// Arguments for the offline `render-sfz` command.
#[derive(Args, Debug, Clone)]
pub struct RenderSfzArgs {
    /// One or more `.sfz` instrument files. Each produces its own M8 sample
    /// chain, in a folder named after the file.
    #[arg(long, required = true, num_args = 1..)]
    pub sfz: Vec<PathBuf>,

    /// Path to the `sfizz_render` binary. Defaults to `sfizz_render` on PATH.
    #[arg(long)]
    pub sfizz_render: Option<PathBuf>,

    /// Output WAV path; its stem/parent set the output folder. When omitted,
    /// each `.sfz` renders into a folder beside it, named after the file.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Note-on velocity (0..=127). Ignored when --velocities is given.
    #[arg(long, default_value_t = 100)]
    pub velocity: u8,

    /// Render one chain per velocity (comma-separated, each 0..=127), e.g.
    /// `--velocities 40,80,120`. Overrides --velocity.
    #[arg(long, value_delimiter = ',')]
    pub velocities: Option<Vec<u8>>,

    /// How long each note is held, in seconds.
    #[arg(long, default_value_t = 4.0)]
    pub note_length: f64,

    /// Total length per slot, in seconds (captures the release tail).
    #[arg(long, default_value_t = 5.0)]
    pub slot_length: f64,

    /// Output sample rate. v1 requires 44100.
    #[arg(long, default_value_t = 44_100)]
    pub sample_rate: u32,

    /// Output channel layout. `auto` follows the render (sfizz is stereo);
    /// `mono`/`stereo` force a layout.
    #[arg(long, value_enum, default_value_t = Channels::Auto)]
    pub channels: Channels,

    /// First MIDI note to render (0..=127).
    #[arg(long, default_value_t = 21)]
    pub start_midi: u8,

    /// Last MIDI note to render (0..=127).
    #[arg(long, default_value_t = 108)]
    pub end_midi: u8,

    /// Render a chord of this quality rooted at each note (slice = root).
    /// Mutually exclusive with --chords.
    #[arg(long, value_enum)]
    pub chord: Option<ChordQuality>,

    /// Render chord files for these qualities (comma-separated); pass with no
    /// value for all qualities. Each quality keeps every root in range and is
    /// packed into as many files as fit --max-slices. Mutually exclusive with
    /// --chord.
    #[arg(long, value_enum, value_delimiter = ',', num_args = 0..)]
    pub chords: Option<Vec<ChordQuality>>,

    /// With --chords, write one file per quality instead of packing several.
    #[arg(long)]
    pub file_per_chord: bool,

    /// Slice budget per file for chord packing (the M8 fixed-slice count).
    #[arg(long, default_value_t = 128)]
    pub max_slices: usize,

    /// Also render the plain single-note chain alongside any chord files.
    /// (A run with no chord flags already produces it.)
    #[arg(long)]
    pub notes: bool,

    /// Measure each font's ring-out and use it as the slot length (overrides
    /// --slot-length). Pair with a short --note-length for snug slots.
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

    /// Number of variation takes per chain (default 1). Takes beyond the first
    /// apply seeded per-note velocity jitter so each is a distinct render.
    #[arg(long, default_value_t = 1)]
    pub variations: u32,

    /// Peak +/- velocity jitter for variation takes. Defaults to 8 when
    /// --variations > 1, otherwise 0. The first take is always the clean one.
    #[arg(long)]
    pub velocity_jitter: Option<u8>,

    /// Fade-out at the end of each slot, in milliseconds. 0 disables.
    #[arg(long, default_value_t = 10)]
    pub fade_ms: u64,

    /// Fade-in at the start of each slot, in milliseconds. 0 disables.
    #[arg(long, default_value_t = 3)]
    pub fade_in_ms: u64,

    /// Disable peak normalization.
    #[arg(long)]
    pub no_normalize: bool,

    /// Peak-normalization target in dBFS (each file's loudest point). Default -1.
    #[arg(long, default_value_t = -1.0)]
    pub normalize_dbfs: f64,

    /// Also write the compact `_unpadded` copy (leading/trailing silent slots
    /// removed; interior silent slots kept).
    #[arg(long)]
    pub unpadded: bool,

    /// Also write the `_map.csv` sidecar describing every slot.
    #[arg(long)]
    pub csv: bool,

    /// Also write the `_render.json` sidecar with the render config.
    #[arg(long)]
    pub json: bool,

    /// Cap the number of parallel render jobs. Default: number of CPUs.
    #[arg(long)]
    pub jobs: Option<usize>,

    /// Print the render plan without running the engine.
    #[arg(long)]
    pub dry_run: bool,
}

impl RenderSfzArgs {
    /// Validate user-supplied values, returning a clear error on failure.
    pub fn validate(&self) -> Result<()> {
        if self.sfz.is_empty() {
            bail!("provide at least one --sfz file");
        }
        for p in &self.sfz {
            let is_sfz = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("sfz"));
            if !is_sfz {
                bail!("--sfz expects a .sfz file (got {})", p.display());
            }
        }
        if self.velocity > 127 {
            bail!("velocity must be 0..=127 (got {})", self.velocity);
        }
        if let Some(vs) = &self.velocities {
            if vs.is_empty() {
                bail!("--velocities needs at least one value");
            }
            if let Some(&bad) = vs.iter().find(|&&v| v > 127) {
                bail!("each --velocities value must be 0..=127 (got {bad})");
            }
        }
        if self.slot_length <= 0.0 || self.slot_length.is_nan() {
            bail!("slot-length must be greater than 0 (got {})", self.slot_length);
        }
        if self.note_length <= 0.0 || self.note_length.is_nan() {
            bail!("note-length must be greater than 0 (got {})", self.note_length);
        }
        if self.sample_rate != 44_100 {
            bail!(
                "v1 only supports --sample-rate 44100 (got {}); omit the flag",
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
        if self.variations < 1 {
            bail!("variations must be at least 1");
        }
        if let Some(j) = self.jobs
            && j == 0
        {
            bail!("--jobs must be at least 1");
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
                bail!("decay-threshold must be >= 0 (got {})", self.decay_threshold);
            }
            if self.slot_margin < 0.0 || self.slot_margin.is_nan() {
                bail!("slot-margin must be >= 0 (got {})", self.slot_margin);
            }
        }
        if self.chord.is_some() && self.chords.is_some() {
            bail!("use either --chord (one quality, slice = root) or --chords (packed), not both");
        }
        if self.file_per_chord && self.chords.is_none() {
            bail!("--file-per-chord only applies with --chords");
        }
        if self.chords.is_some() {
            if self.max_slices < 1 {
                bail!("max-slices must be at least 1");
            }
            if self.max_slices > 255 {
                bail!(
                    "max-slices must be <= 255 (the M8 fixed-slice maximum); got {}",
                    self.max_slices
                );
            }
        }
        Ok(())
    }

    /// The chord qualities to render for `--chords`: the given list, all
    /// qualities when `--chords` was passed with no value, or empty when the
    /// flag was not given at all.
    pub fn resolved_chords(&self) -> Vec<ChordQuality> {
        match &self.chords {
            Some(v) if !v.is_empty() => v.clone(),
            Some(_) => ChordQuality::ALL.to_vec(),
            None => Vec::new(),
        }
    }

    /// The velocities to render one chain each for: `--velocities` if given,
    /// otherwise the single `--velocity`.
    pub fn resolved_velocities(&self) -> Vec<u8> {
        match &self.velocities {
            Some(v) if !v.is_empty() => v.clone(),
            _ => vec![self.velocity],
        }
    }

    /// Effective per-note velocity jitter for variation takes: the explicit
    /// `--velocity-jitter`, else 8 when more than one variation is requested.
    pub fn effective_jitter(&self) -> u8 {
        self.velocity_jitter
            .unwrap_or(if self.variations > 1 { 8 } else { 0 })
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
            unpadded: false,
            csv: false,
            json: false,
            notes: false,
            chord: None,
            chords: None,
            file_per_chord: false,
            per_octave: false,
            max_slices: 128,
            no_normalize: false,
            normalize_dbfs: -1.0,
            no_trim_onset: false,
            onset_dbfs: -55.0,
            onset_lookback_ms: 5,
            fade_in_ms: 3,
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
    fn channels_resolve_mapping() {
        // Forced layouts ignore the source channel count.
        assert_eq!(Channels::Mono.resolve(2), 1);
        assert_eq!(Channels::Stereo.resolve(1), 2);
        // Auto follows the source.
        assert_eq!(Channels::Auto.resolve(1), 1);
        assert_eq!(Channels::Auto.resolve(2), 2);
        assert_eq!(Channels::Auto.resolve(4), 2);
    }

    #[test]
    fn chord_and_chords_are_mutually_exclusive() {
        let mut a = base();
        a.chord = Some(ChordQuality::Maj);
        a.chords = Some(vec![ChordQuality::Min]);
        assert!(a.validate().is_err());
    }

    #[test]
    fn file_per_chord_requires_chords() {
        let mut a = base();
        a.file_per_chord = true;
        assert!(a.validate().is_err());
        a.chords = Some(vec![ChordQuality::Maj]);
        assert!(a.validate().is_ok());
    }

    #[test]
    fn per_octave_requires_chords_and_excludes_file_per_chord() {
        let mut a = base();
        a.per_octave = true;
        assert!(a.validate().is_err()); // needs --chords
        a.chords = Some(vec![ChordQuality::Maj]);
        assert!(a.validate().is_ok());
        a.file_per_chord = true;
        assert!(a.validate().is_err()); // not both
    }

    #[test]
    fn packed_max_slices_is_capped_at_255() {
        let mut a = base();
        a.chords = Some(vec![ChordQuality::Maj]);
        a.max_slices = 256;
        assert!(a.validate().is_err());
        a.max_slices = 255;
        assert!(a.validate().is_ok());
    }

    #[test]
    fn resolved_chords_expands_empty_to_all() {
        let mut a = base();
        a.chords = None;
        assert!(a.resolved_chords().is_empty());
        a.chords = Some(vec![]);
        assert_eq!(a.resolved_chords().len(), ChordQuality::ALL.len());
        a.chords = Some(vec![ChordQuality::Maj]);
        assert_eq!(a.resolved_chords(), vec![ChordQuality::Maj]);
    }
}
