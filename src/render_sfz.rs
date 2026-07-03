//! The `render-sfz` command: render `.sfz` instruments offline into M8 sample
//! chains, in parallel.
//!
//! For every combination of `.sfz` file x velocity x variation take, this
//! authors a chain SMF, renders it with `sfizz_render` (faster than real time),
//! slices the resulting WAV into fixed-length slots, and writes the same M8
//! sample-chain WAV (plus optional sidecars) as the live `render` path. All jobs
//! run concurrently on a rayon pool — the offline win the live capture can't get.

use crate::audio;
use crate::cli::RenderSfzArgs;
use crate::config::{AudioConfig, FormatConfig, M8Config, MidiConfig, RenderConfig, RenderParams};
use crate::notes::{Slot, build_slot_map};
use crate::output;
use crate::sfz;
use crate::wav::{self, M8_SAMPLE_RATE};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// One output file to render: a font at a velocity, one variation take.
struct Job {
    sfz: PathBuf,
    /// Base output path whose stem/parent define this font's output folder.
    out_base: PathBuf,
    /// Base note-on velocity for the take.
    velocity: u8,
    /// Variation index; 0 is the clean take, >0 apply seeded velocity jitter.
    variation: u32,
    /// Filename tag, e.g. `notes`, `maj`, `notes_v80_take02`.
    name: String,
    /// Human label for the per-file summary line.
    label: String,
}

/// Outcome of one rendered job, printed after the parallel batch completes.
struct JobOutput {
    written: Vec<PathBuf>,
    summary: String,
}

/// Run the `render-sfz` command.
pub fn run(args: &RenderSfzArgs) -> Result<()> {
    args.validate()?;

    let slots = build_slot_map(args.start_midi, args.end_midi, args.slot_length, args.chord);
    let velocities = args.resolved_velocities();
    let jitter = args.effective_jitter();
    let jobs = build_jobs(args, &velocities);

    if args.dry_run {
        print_plan(args, &slots, &velocities, &jobs);
        return Ok(());
    }

    // Fail fast with a clear message if the engine is missing, before any work.
    let engine = sfz::locate_engine(args.sfizz_render.as_deref())?;
    println!("Engine      : {}", engine.display());
    println!(
        "Chains      : {} ({} font(s) x {} velocity(ies) x {} variation(s))",
        jobs.len(),
        args.sfz.len(),
        velocities.len(),
        args.variations
    );
    println!(
        "Range       : MIDI {}..{} ({} slots), slot {}s, note {}s{}",
        args.start_midi,
        args.end_midi,
        slots.len(),
        args.slot_length,
        args.note_length,
        args.chord
            .map(|q| format!(", chord {}", q.short()))
            .unwrap_or_default(),
    );

    let pool = {
        let mut b = rayon::ThreadPoolBuilder::new();
        if let Some(j) = args.jobs {
            b = b.num_threads(j);
        }
        b.build().context("building the render thread pool")?
    };
    println!("Parallelism : {} worker(s)\n", pool.current_num_threads());

    let done = AtomicUsize::new(0);
    let total = jobs.len();
    let results: Vec<Result<JobOutput>> = pool.install(|| {
        jobs.par_iter()
            .map(|job| {
                let out = render_job(job, &engine, args, &slots, jitter);
                let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                match &out {
                    Ok(o) => println!("[{k}/{total}] {}", o.summary),
                    Err(e) => println!("[{k}/{total}] FAILED {}: {e:#}", job.label),
                }
                out
            })
            .collect()
    });

    // Report: collect written paths, surface the first error (if any) at the end.
    let mut written: Vec<PathBuf> = Vec::new();
    let mut first_err: Option<anyhow::Error> = None;
    for r in results {
        match r {
            Ok(o) => written.extend(o.written),
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    if !written.is_empty() {
        println!("\nWrote {} file(s):", written.len());
        for p in &written {
            println!("  {}", p.display());
        }
        print_m8_hint();
    }
    if let Some(e) = first_err {
        return Err(e).context("one or more chains failed to render");
    }
    if written.is_empty() {
        anyhow::bail!("no files were written");
    }
    Ok(())
}

/// Assemble the job list: the cartesian product of fonts x velocities x
/// variations. Tags disambiguate filenames only along the axes actually varied.
fn build_jobs(args: &RenderSfzArgs, velocities: &[u8]) -> Vec<Job> {
    let role = args.chord.map(|q| q.short().to_string()).unwrap_or_else(|| "notes".to_string());
    let multi_font = args.sfz.len() > 1;
    let multi_vel = velocities.len() > 1;
    let multi_var = args.variations > 1;

    let mut jobs = Vec::new();
    for sfz in &args.sfz {
        let out_base = font_output_base(args, sfz, multi_font);
        for &velocity in velocities {
            for variation in 0..args.variations {
                let vel_tag = if multi_vel { format!("_v{velocity}") } else { String::new() };
                let take_tag = if multi_var {
                    format!("_take{:02}", variation + 1)
                } else {
                    String::new()
                };
                let name = format!("{role}{vel_tag}{take_tag}");
                let font = sfz
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "sfz".to_string());
                jobs.push(Job {
                    sfz: sfz.clone(),
                    out_base: out_base.clone(),
                    velocity,
                    variation,
                    label: format!("{font} / {name}"),
                    name,
                });
            }
        }
    }
    jobs
}

/// The base output path for a font: `--output` verbatim for a single font, one
/// file per font under `--output`'s parent for several, or (no `--output`) a
/// folder beside the `.sfz` named after it.
fn font_output_base(args: &RenderSfzArgs, sfz: &Path, multi_font: bool) -> PathBuf {
    match &args.output {
        Some(o) if !multi_font => o.clone(),
        Some(o) => {
            let stem = sfz
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "sfz".to_string());
            o.with_file_name(format!("{stem}.wav"))
        }
        None => sfz.with_extension("wav"),
    }
}

/// Render one job end to end: author the SMF, run the engine, slice + finalize
/// every slot, normalize, and write the WAV (plus opt-in sidecars).
fn render_job(
    job: &Job,
    engine: &Path,
    args: &RenderSfzArgs,
    slots: &[Slot],
    jitter: u8,
) -> Result<JobOutput> {
    let slot_ms = (args.slot_length * 1000.0).round() as u32;
    let note_ms = (args.note_length * 1000.0).round() as u32;
    let velocities = slot_velocities(slots.len(), job.velocity, jitter, job.variation);

    let smf = sfz::build_chain_smf(slots, slot_ms, note_ms, &velocities);
    let rendered = sfz::render_chain(engine, &job.sfz, &smf, args.sample_rate)
        .with_context(|| format!("rendering {}", job.sfz.display()))?;

    let out_channels = args.channels.resolve(rendered.channels);
    let sc = rendered.channels.max(1) as usize;
    let fps = (args.slot_length * rendered.sample_rate as f64).round() as usize;
    let fade_in_frames = (args.fade_in_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;
    let fade_frames = (args.fade_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;

    let target_frames = (args.slot_length * M8_SAMPLE_RATE as f64).round() as usize;
    let mut full: Vec<f32> = Vec::with_capacity(target_frames * out_channels as usize * slots.len());
    let mut statuses: Vec<String> = Vec::with_capacity(slots.len());

    for i in 0..slots.len() {
        let start = (i * fps * sc).min(rendered.samples.len());
        let end = ((i + 1) * fps * sc).min(rendered.samples.len());
        // Reuse the live path's finalizer: down-mix/expand, (identity) resample to
        // 44.1 kHz, and force exact slot length. Onset trimming is off — offline
        // note-ons already sit exactly on the slot boundary.
        let mut slot = audio::finalize_slot(
            &rendered.samples[start..end],
            rendered.sample_rate,
            rendered.channels,
            out_channels,
            args.slot_length,
            0.0,
            0,
        );
        audio::apply_start_fade(&mut slot, out_channels, fade_in_frames);
        audio::apply_end_fade(&mut slot, out_channels, fade_frames);
        let peak = slot.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        statuses.push(if peak >= 1e-4 { "rendered" } else { "silent" }.to_string());
        full.extend_from_slice(&slot);
    }

    if !args.no_normalize {
        audio::normalize_peak(&mut full, dbfs_to_amp(args.normalize_dbfs));
    }

    write_job_outputs(job, args, slots, &full, &statuses, out_channels)
}

/// Write a job's WAV(s) and opt-in sidecars, returning the paths and a summary.
fn write_job_outputs(
    job: &Job,
    args: &RenderSfzArgs,
    slots: &[Slot],
    full: &[f32],
    statuses: &[String],
    out_channels: u16,
) -> Result<JobOutput> {
    let slot_samples = (args.slot_length * M8_SAMPLE_RATE as f64).round() as usize * out_channels as usize;
    let mut written = Vec::new();

    let folder = output::render_dir(&job.out_base);
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("creating output folder {}", folder.display()))?;

    let padded_path =
        output::output_wav_path(&job.out_base, &job.name, slots.len(), args.note_length, false);
    wav::write_wav(&padded_path, full, M8_SAMPLE_RATE, out_channels)?;
    written.push(padded_path.clone());

    if args.unpadded
        && let Some((f, l)) = rendered_bounds(statuses)
    {
        let trimmed = l - f + 1;
        let path =
            output::output_wav_path(&job.out_base, &job.name, trimmed, args.note_length, true);
        wav::write_wav(
            &path,
            &full[f * slot_samples..(l + 1) * slot_samples],
            M8_SAMPLE_RATE,
            out_channels,
        )?;
        written.push(path);
    }

    let (csv_path, json_path) = output::sidecar_paths(&padded_path);
    if args.csv {
        output::write_csv_map(&csv_path, slots, job.velocity, statuses)?;
        written.push(csv_path.clone());
    }
    if args.json {
        let config = build_config(job, args, slots.len() as u32, out_channels, &padded_path, &csv_path);
        output::write_json_config(&json_path, &config)?;
        written.push(json_path);
    }

    let slice_hex = format!("{:02X}", slots.len());
    let layout = if out_channels == 1 { "mono" } else { "stereo" };
    let summary = format!(
        "{} -> {} ({layout}, SLICE={slice_hex})",
        job.label,
        padded_path.display()
    );
    Ok(JobOutput { written, summary })
}

/// Build the `_render.json` config for an SFZ render.
fn build_config(
    job: &Job,
    args: &RenderSfzArgs,
    slice_count: u32,
    out_channels: u16,
    wav_path: &Path,
    csv_path: &Path,
) -> RenderConfig {
    let file_name = |p: &Path| {
        p.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let slice_hex = format!("{slice_count:02X}");
    RenderConfig {
        mode: "sfz-render".to_string(),
        output_wav: file_name(wav_path),
        output_map: file_name(csv_path),
        format: FormatConfig {
            sample_rate: M8_SAMPLE_RATE,
            bit_depth: 16,
            channels: out_channels,
        },
        midi: MidiConfig {
            output: "sfizz_render".to_string(),
            channel: 1,
            start_midi: args.start_midi,
            end_midi: args.end_midi,
            velocity: job.velocity,
        },
        audio: AudioConfig {
            input: file_name(&job.sfz),
        },
        render: RenderParams {
            note_length_seconds: args.note_length,
            slot_length_seconds: args.slot_length,
            pre_roll_ms: 0,
            slice_count,
            m8_slice_hex: slice_hex.clone(),
            chord: args.chord.map(|q| q.short().to_string()),
        },
        m8: M8Config {
            slice: slice_hex,
            ..M8Config::standard()
        },
    }
}

/// Per-slot velocities for a take. Variation 0 (or zero jitter) is the clean
/// take; later variations perturb each slot deterministically within +/-jitter.
fn slot_velocities(n: usize, base: u8, jitter: u8, variation: u32) -> Vec<u8> {
    if variation == 0 || jitter == 0 {
        return vec![base; n];
    }
    let span = jitter as i32 * 2 + 1;
    (0..n)
        .map(|i| {
            let r = splitmix64(((variation as u64) << 40) ^ (i as u64) ^ 0xA5A5_1234_5678);
            let delta = (r % span as u64) as i32 - jitter as i32;
            (base as i32 + delta).clamp(1, 127) as u8
        })
        .collect()
}

/// A small deterministic hash → pseudo-random `u64` (SplitMix64 finalizer).
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Convert a dBFS level to a linear amplitude in `[0, 1]`.
fn dbfs_to_amp(dbfs: f64) -> f32 {
    10f32.powf(dbfs as f32 / 20.0)
}

/// First/last slot index whose status is `rendered` (the inclusive range to keep
/// when trimming leading/trailing silence). Interior silent slots stay in range.
fn rendered_bounds(statuses: &[String]) -> Option<(usize, usize)> {
    let first = statuses.iter().position(|s| s == "rendered")?;
    let last = statuses.iter().rposition(|s| s == "rendered")?;
    Some((first, last))
}

/// The M8 load instructions, printed once after all files are written.
fn print_m8_hint() {
    println!("\nLoad on the M8:");
    println!("  1. Copy the WAV(s) to your M8 SD card.");
    println!("  2. Create a Sampler instrument and load a WAV.");
    println!("  3. Set:  SLICE = <that file's count above>   PLAY = FWD   START = 00   LEN = FF");
    println!("  Each slice maps to its slot (slot index = the note you press).");
}

fn print_plan(args: &RenderSfzArgs, slots: &[Slot], velocities: &[u8], jobs: &[Job]) {
    println!("DRY RUN — the engine is not run\n");
    println!("  SFZ files   : {}", args.sfz.len());
    for p in &args.sfz {
        println!("    - {}", p.display());
    }
    println!("  Engine      : {}", args
        .sfizz_render
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "sfizz_render (from PATH)".to_string()));
    println!(
        "  Notes range : {}..{} ({} slots)",
        args.start_midi,
        args.end_midi,
        slots.len()
    );
    println!("  Note length : {}s", args.note_length);
    println!("  Slot length : {}s", args.slot_length);
    println!(
        "  Velocities  : {}",
        velocities.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
    );
    println!(
        "  Variations  : {} (velocity jitter +/-{})",
        args.variations,
        args.effective_jitter()
    );
    if let Some(q) = args.chord {
        println!("  Chord       : {} (slice = root)", q.short());
    }
    println!("\n  Output files ({}):", jobs.len());
    for job in jobs {
        let path = output::output_wav_path(&job.out_base, &job.name, slots.len(), args.note_length, false);
        println!("    - {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_take_uses_base_velocity() {
        // Variation 0 is always flat at the base velocity.
        assert_eq!(slot_velocities(4, 100, 8, 0), vec![100, 100, 100, 100]);
        // Zero jitter keeps every take flat too.
        assert_eq!(slot_velocities(3, 90, 0, 5), vec![90, 90, 90]);
    }

    #[test]
    fn variation_take_jitters_within_range_and_is_deterministic() {
        let a = slot_velocities(16, 100, 8, 1);
        let b = slot_velocities(16, 100, 8, 1);
        assert_eq!(a, b, "same seed -> identical take");
        assert!(a.iter().all(|&v| (92..=108).contains(&v)), "within +/-8 of 100");
        // A different variation index yields a different sequence.
        let c = slot_velocities(16, 100, 8, 2);
        assert_ne!(a, c);
    }

    #[test]
    fn jitter_clamps_to_valid_velocity() {
        // Base near the floor can't go below 1 (nor above 127).
        let v = slot_velocities(64, 3, 8, 1);
        assert!(v.iter().all(|&x| (1..=127).contains(&x)));
    }

    #[test]
    fn build_jobs_products_all_axes() {
        let mut args = base_args();
        args.sfz = vec![PathBuf::from("a.sfz"), PathBuf::from("b.sfz")];
        args.velocities = Some(vec![40, 80]);
        args.variations = 2;
        let vels = args.resolved_velocities();
        let jobs = build_jobs(&args, &vels);
        // 2 fonts x 2 velocities x 2 variations = 8 chains.
        assert_eq!(jobs.len(), 8);
        // Names encode the varied axes; all are unique.
        let mut names: Vec<_> = jobs.iter().map(|j| format!("{}|{}", j.sfz.display(), j.name)).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 8);
        assert!(jobs.iter().any(|j| j.name == "notes_v40_take01"));
        assert!(jobs.iter().any(|j| j.name == "notes_v80_take02"));
    }

    #[test]
    fn single_axis_names_stay_bare() {
        let args = base_args();
        let vels = args.resolved_velocities();
        let jobs = build_jobs(&args, &vels);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "notes");
    }

    #[test]
    fn font_output_base_layouts() {
        let mut args = base_args();
        // Single font + explicit output: used verbatim.
        args.output = Some(PathBuf::from("out/Piano.wav"));
        assert_eq!(
            font_output_base(&args, Path::new("Piano.sfz"), false),
            PathBuf::from("out/Piano.wav")
        );
        // Several fonts + output: one file per font under the output's parent.
        assert_eq!(
            font_output_base(&args, Path::new("some/dir/Rhodes.sfz"), true),
            PathBuf::from("out/Rhodes.wav")
        );
        // No output: folder beside the .sfz named after it.
        args.output = None;
        assert_eq!(
            font_output_base(&args, Path::new("some/dir/Rhodes.sfz"), false),
            PathBuf::from("some/dir/Rhodes.wav")
        );
    }

    fn base_args() -> RenderSfzArgs {
        RenderSfzArgs {
            sfz: vec![PathBuf::from("inst.sfz")],
            sfizz_render: None,
            output: None,
            velocity: 100,
            velocities: None,
            note_length: 4.0,
            slot_length: 5.0,
            sample_rate: 44_100,
            channels: crate::cli::Channels::Auto,
            start_midi: 21,
            end_midi: 108,
            chord: None,
            variations: 1,
            velocity_jitter: None,
            fade_ms: 10,
            fade_in_ms: 3,
            no_normalize: false,
            normalize_dbfs: -1.0,
            unpadded: false,
            csv: false,
            json: false,
            jobs: None,
            dry_run: false,
        }
    }
}
