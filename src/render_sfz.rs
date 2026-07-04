//! The `render-sfz` command: render `.sfz` instruments offline into M8 sample
//! chains, in parallel.
//!
//! For every combination of `.sfz` file x base chain (notes / chord / packed
//! chord files) x velocity x variation take, this authors a chain SMF, renders
//! it with `sfizz_render` (faster than real time), slices the resulting WAV into
//! fixed-length slots, and writes the same M8 sample-chain WAV (plus optional
//! sidecars) as the live `render` path. All jobs run concurrently on a rayon
//! pool — the offline win the live capture can't get. With `--auto-slot-length`
//! each font's ring-out is measured up front (offline, cheaply) to size its slots.

use crate::audio;
use crate::cli::RenderSfzArgs;
use crate::config::{AudioConfig, FormatConfig, M8Config, MidiConfig, RenderConfig, RenderParams};
use crate::notes::{Slot, build_slot_map, chord_slots, midi_to_m8_note};
use crate::output;
use crate::render::{fit_roots, last_sound_seconds, octave_groups, pick_spread, split_chord_files};
use crate::sfz;
use crate::wav::{self, M8_SAMPLE_RATE};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A distinct chain to render for one font, before the velocity/variation fan-out.
struct BaseChain {
    slot_map: Vec<Slot>,
    /// Filename tag: `notes`, `maj`, `maj-min-dim`, …
    tag: String,
    /// Whether this is a packed chord file (used only to tag the JSON sidecar).
    is_chord: bool,
}

/// Per-font analysis done before the job fan-out: the slot length to use and the
/// chord roots (the sounding notes, capped to the slice budget).
struct FontPrep {
    slot_length: f64,
    chord_roots: Vec<u8>,
}

/// One output file to render: a base chain of a font, at a velocity, one take.
struct Job {
    sfz: PathBuf,
    /// Base output path whose stem/parent define this font's output folder.
    out_base: PathBuf,
    /// The chain's slots (already sized to `slot_length`).
    slot_map: Vec<Slot>,
    /// Slot length in seconds (per font when `--auto-slot-length`).
    slot_length: f64,
    /// Base note-on velocity for the take.
    velocity: u8,
    /// Variation index; 0 is the clean take, >0 apply seeded velocity jitter.
    variation: u32,
    /// Whether this is a packed chord file (used only to tag the JSON sidecar).
    is_chord: bool,
    /// Filename tag, e.g. `notes`, `maj-min-dim_v80_take02`.
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
    let velocities = args.resolved_velocities();

    if args.dry_run {
        // No engine, probe, or measurement: preview with the fixed slot length and
        // the full range as chord roots (narrowed to the sounding set at runtime).
        let full: Vec<u8> = (args.start_midi..=args.end_midi).collect();
        let preps: Vec<FontPrep> = args
            .sfz
            .iter()
            .map(|_| FontPrep {
                slot_length: args.slot_length,
                chord_roots: full.clone(),
            })
            .collect();
        let jobs = build_jobs(args, &velocities, &preps);
        print_plan(args, &jobs);
        return Ok(());
    }

    // Fail fast with a clear message if the engine is missing, before any work.
    let engine = sfz::locate_engine(args.sfizz_render.as_deref())?;
    println!("Engine      : {}", engine.display());

    // Per-font analysis: probe the sounding set (for compact --chords and to focus
    // the ring-out measurement), then size the slots.
    let meas_vel = velocities.iter().copied().max().unwrap_or(100);
    let need_probe = !args.no_probe && (args.chords.is_some() || args.auto_slot_length);
    if need_probe {
        println!("Probing sounding range ({} ms/note)...", args.probe_ms);
    }
    let preps: Vec<FontPrep> = args
        .sfz
        .iter()
        .map(|sfz| font_prep(&engine, sfz, args, &velocities, meas_vel, need_probe))
        .collect::<Result<Vec<_>>>()?;

    let jobs = build_jobs(args, &velocities, &preps);
    println!("Chains      : {}", jobs.len());

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
                let out = render_job(job, &engine, args);
                let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                match &out {
                    Ok(o) => println!("[{k}/{total}] {}", o.summary),
                    Err(e) => println!("[{k}/{total}] FAILED {}: {e:#}", job.label),
                }
                out
            })
            .collect()
    });

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

/// Analyse one font: probe its sounding range (for compact `--chords` and to
/// focus measurement), size the slots (`--auto-slot-length` or the fixed value),
/// and derive the chord roots (sounding notes capped to the slice budget).
fn font_prep(
    engine: &Path,
    sfz: &Path,
    args: &RenderSfzArgs,
    velocities: &[u8],
    meas_vel: u8,
    need_probe: bool,
) -> Result<FontPrep> {
    let full: Vec<u8> = (args.start_midi..=args.end_midi).collect();
    let sounding = if need_probe {
        probe_sounding(engine, sfz, args, meas_vel)?
    } else {
        full.clone()
    };
    let pool = if sounding.is_empty() { &full } else { &sounding };

    let slot_length = if args.auto_slot_length {
        let len = measure_slot_length(engine, sfz, args, pool, meas_vel)?;
        println!(
            "  {:<28} {} sounding, slot {len:.2}s",
            font_stem(sfz),
            sounding.len()
        );
        len
    } else {
        if need_probe {
            println!("  {:<28} {} sounding", font_stem(sfz), sounding.len());
        }
        args.slot_length
    };

    // Chord roots: the sounding notes, capped to the per-file slice budget.
    let chord_roots = if pool.len() > args.max_slices {
        fit_roots(pool, args.max_slices)
    } else {
        pool.clone()
    };
    let _ = velocities; // reserved for future per-velocity probing
    Ok(FontPrep {
        slot_length,
        chord_roots,
    })
}

/// Probe which notes in `start..=end` produce sound: render each briefly and keep
/// the ones whose peak reaches `--probe-threshold`. Offline analogue of the live
/// `render::probe_sounding` (which uses live capture).
fn probe_sounding(engine: &Path, sfz: &Path, args: &RenderSfzArgs, velocity: u8) -> Result<Vec<u8>> {
    let notes: Vec<u8> = (args.start_midi..=args.end_midi).collect();
    let slots = single_note_slots(&notes);
    let slot_ms = args.probe_ms as u32;
    let note_ms = args.probe_ms.saturating_sub(50).max(1) as u32;
    let vels = vec![velocity; slots.len()];
    let smf = sfz::build_chain_smf(&slots, slot_ms, note_ms, &vels);
    let rendered = sfz::render_chain(engine, sfz, &smf, args.sample_rate)
        .with_context(|| format!("probing {}", sfz.display()))?;

    let sc = rendered.channels.max(1) as usize;
    let fps = (args.probe_ms as f64 / 1000.0 * rendered.sample_rate as f64).round() as usize;
    let sounding = notes
        .iter()
        .enumerate()
        .filter_map(|(i, &m)| {
            let start = (i * fps * sc).min(rendered.samples.len());
            let end = ((i + 1) * fps * sc).min(rendered.samples.len());
            let peak = rendered.samples[start..end].iter().fold(0.0f32, |a, &s| a.max(s.abs()));
            (peak >= args.probe_threshold).then_some(m)
        })
        .collect();
    Ok(sounding)
}

/// Measure the longest ring-out among `pool` and return a slot length covering it
/// plus the margin, clamped to `[0.25, max_slot_length]`. Offline mirror of the
/// live `render::measure_slot_length`: render a spread of notes with a generous
/// slot length so tails don't overlap, then read each tail's end.
fn measure_slot_length(
    engine: &Path,
    sfz: &Path,
    args: &RenderSfzArgs,
    pool: &[u8],
    velocity: u8,
) -> Result<f64> {
    let idxs: Vec<usize> = (0..pool.len()).collect();
    let notes: Vec<u8> = pick_spread(&idxs, args.measure_notes as usize)
        .iter()
        .map(|&i| pool[i])
        .collect();
    let meas_slots = single_note_slots(&notes);

    let slot_ms = (args.max_slot_length * 1000.0).round() as u32;
    let note_ms = (args.note_length * 1000.0).round() as u32;
    let vels = vec![velocity; meas_slots.len()];
    let smf = sfz::build_chain_smf(&meas_slots, slot_ms, note_ms, &vels);
    let rendered = sfz::render_chain(engine, sfz, &smf, args.sample_rate)
        .with_context(|| format!("measuring ring-out of {}", sfz.display()))?;

    let sc = rendered.channels.max(1) as usize;
    let fps = (args.max_slot_length * rendered.sample_rate as f64).round() as usize;
    let mut longest = 0.0f64;
    for i in 0..meas_slots.len() {
        let start = (i * fps * sc).min(rendered.samples.len());
        let end = ((i + 1) * fps * sc).min(rendered.samples.len());
        let tail = last_sound_seconds(
            &rendered.samples[start..end],
            rendered.sample_rate,
            rendered.channels,
            args.decay_threshold,
        );
        longest = longest.max(tail);
    }
    Ok((longest + args.slot_margin).clamp(0.25, args.max_slot_length))
}

/// Minimal single-note slots for probe/measurement SMFs. `build_chain_smf` only
/// reads each slot's `notes` and its index, so the other fields are placeholders.
fn single_note_slots(notes: &[u8]) -> Vec<Slot> {
    notes
        .iter()
        .enumerate()
        .map(|(i, &m)| Slot {
            slot: i as u8,
            midi: m,
            m8_note: midi_to_m8_note(m),
            notes: vec![m],
            label: midi_to_m8_note(m),
            start_seconds: 0.0,
            end_seconds: 0.0,
        })
        .collect()
}

/// The distinct chains for one font at `slot_length`: the note chain (default or
/// via `--notes`), an optional `--chord` slice=root chain, and the packed
/// `--chords` files. The note chain and `--chord` span the full `--start/--end`
/// range (padded, key-aligned on the M8); `--chords` pack only `chord_roots`
/// (the probed sounding notes), so those files stay compact.
fn base_chains(args: &RenderSfzArgs, slot_length: f64, chord_roots: &[u8]) -> Vec<BaseChain> {
    let mut chains = Vec::new();
    let chords = args.resolved_chords();

    if args.notes || (args.chord.is_none() && chords.is_empty()) {
        chains.push(BaseChain {
            slot_map: build_slot_map(args.start_midi, args.end_midi, slot_length, None),
            tag: "notes".to_string(),
            is_chord: false,
        });
    }
    if let Some(q) = args.chord {
        chains.push(BaseChain {
            slot_map: build_slot_map(args.start_midi, args.end_midi, slot_length, Some(q)),
            tag: q.short().to_string(),
            is_chord: true,
        });
    }
    if !chords.is_empty() {
        if args.per_octave {
            // One file per octave: that octave's roots x the qualities (further
            // split only if a single octave overflows the slice budget).
            for (octave, oct_roots) in octave_groups(chord_roots) {
                let prefix = format!("oct-{}", midi_to_m8_note(octave * 12));
                for chunk in split_chord_files(oct_roots.len(), &chords, args.max_slices, false) {
                    let qs = chunk.iter().map(|q| q.short()).collect::<Vec<_>>().join("-");
                    chains.push(BaseChain {
                        slot_map: chord_slots(&oct_roots, &chunk, slot_length),
                        tag: format!("{prefix}_{qs}"),
                        is_chord: true,
                    });
                }
            }
        } else {
            for chunk in
                split_chord_files(chord_roots.len(), &chords, args.max_slices, args.file_per_chord)
            {
                let tag = chunk.iter().map(|q| q.short()).collect::<Vec<_>>().join("-");
                chains.push(BaseChain {
                    slot_map: chord_slots(chord_roots, &chunk, slot_length),
                    tag,
                    is_chord: true,
                });
            }
        }
    }
    chains
}

/// Assemble the job list: fonts x base chains x velocities x variations. Tags
/// disambiguate filenames only along the axes actually varied.
fn build_jobs(args: &RenderSfzArgs, velocities: &[u8], font_preps: &[FontPrep]) -> Vec<Job> {
    let multi_font = args.sfz.len() > 1;
    let multi_vel = velocities.len() > 1;
    let multi_var = args.variations > 1;

    let mut jobs = Vec::new();
    for (fi, sfz) in args.sfz.iter().enumerate() {
        let prep = &font_preps[fi];
        let slot_length = prep.slot_length;
        let out_base = font_output_base(args, sfz, multi_font);
        let font = font_stem(sfz);
        for chain in base_chains(args, slot_length, &prep.chord_roots) {
            for &velocity in velocities {
                for variation in 0..args.variations {
                    let vel_tag = if multi_vel { format!("_v{velocity}") } else { String::new() };
                    let take_tag = if multi_var {
                        format!("_take{:02}", variation + 1)
                    } else {
                        String::new()
                    };
                    let name = format!("{}{vel_tag}{take_tag}", chain.tag);
                    jobs.push(Job {
                        sfz: sfz.clone(),
                        out_base: out_base.clone(),
                        slot_map: chain.slot_map.clone(),
                        slot_length,
                        velocity,
                        variation,
                        is_chord: chain.is_chord,
                        label: format!("{font} / {name}"),
                        name,
                    });
                }
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
        Some(o) => o.with_file_name(format!("{}.wav", font_stem(sfz))),
        None => sfz.with_extension("wav"),
    }
}

/// The file-stem of an `.sfz` path, for folder/label naming.
fn font_stem(sfz: &Path) -> String {
    sfz.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sfz".to_string())
}

/// Render one job end to end: author the SMF, run the engine, slice + finalize
/// every slot, normalize, and write the WAV (plus opt-in sidecars).
fn render_job(job: &Job, engine: &Path, args: &RenderSfzArgs) -> Result<JobOutput> {
    let slot_ms = (job.slot_length * 1000.0).round() as u32;
    let note_ms = (args.note_length * 1000.0).round() as u32;
    let velocities = slot_velocities(job.slot_map.len(), job.velocity, args.effective_jitter(), job.variation);

    let smf = sfz::build_chain_smf(&job.slot_map, slot_ms, note_ms, &velocities);
    let rendered = sfz::render_chain(engine, &job.sfz, &smf, args.sample_rate)
        .with_context(|| format!("rendering {}", job.sfz.display()))?;

    let out_channels = args.channels.resolve(rendered.channels);
    let sc = rendered.channels.max(1) as usize;
    let fps = (job.slot_length * rendered.sample_rate as f64).round() as usize;
    let fade_in_frames = (args.fade_in_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;
    let fade_frames = (args.fade_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;

    let target_frames = (job.slot_length * M8_SAMPLE_RATE as f64).round() as usize;
    let mut full: Vec<f32> =
        Vec::with_capacity(target_frames * out_channels as usize * job.slot_map.len());
    let mut statuses: Vec<String> = Vec::with_capacity(job.slot_map.len());

    for i in 0..job.slot_map.len() {
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
            job.slot_length,
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

    write_job_outputs(job, args, &full, &statuses, out_channels)
}

/// Write a job's WAV(s) and opt-in sidecars, returning the paths and a summary.
fn write_job_outputs(
    job: &Job,
    args: &RenderSfzArgs,
    full: &[f32],
    statuses: &[String],
    out_channels: u16,
) -> Result<JobOutput> {
    let slot_samples =
        (job.slot_length * M8_SAMPLE_RATE as f64).round() as usize * out_channels as usize;
    let slots = job.slot_map.len();
    let mut written = Vec::new();

    let folder = output::render_dir(&job.out_base);
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("creating output folder {}", folder.display()))?;

    let padded_path =
        output::output_wav_path(&job.out_base, &job.name, slots, args.note_length, false);
    wav::write_wav(&padded_path, full, M8_SAMPLE_RATE, out_channels)?;
    written.push(padded_path.clone());

    if args.unpadded
        && let Some((f, l)) = rendered_bounds(statuses)
    {
        let trimmed = l - f + 1;
        let path = output::output_wav_path(&job.out_base, &job.name, trimmed, args.note_length, true);
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
        output::write_csv_map(&csv_path, &job.slot_map, job.velocity, statuses)?;
        written.push(csv_path.clone());
    }
    if args.json {
        let config = build_config(job, args, out_channels, &padded_path, &csv_path);
        output::write_json_config(&json_path, &config)?;
        written.push(json_path);
    }

    let slice_hex = format!("{slots:02X}");
    let layout = if out_channels == 1 { "mono" } else { "stereo" };
    let summary = format!(
        "{} -> {} ({layout}, {:.2}s slots, SLICE={slice_hex})",
        job.label,
        padded_path.display(),
        job.slot_length,
    );
    Ok(JobOutput { written, summary })
}

/// Build the `_render.json` config for an SFZ render.
fn build_config(
    job: &Job,
    args: &RenderSfzArgs,
    out_channels: u16,
    wav_path: &Path,
    csv_path: &Path,
) -> RenderConfig {
    let file_name = |p: &Path| {
        p.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let slice_count = job.slot_map.len() as u32;
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
            slot_length_seconds: job.slot_length,
            pre_roll_ms: 0,
            slice_count,
            m8_slice_hex: slice_hex.clone(),
            chord: (job.is_chord || args.chord.is_some()).then(|| job.name.clone()),
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

fn print_plan(args: &RenderSfzArgs, jobs: &[Job]) {
    println!("DRY RUN — the engine is not run\n");
    println!("  SFZ files   : {}", args.sfz.len());
    for p in &args.sfz {
        println!("    - {}", p.display());
    }
    println!(
        "  Engine      : {}",
        args.sfizz_render
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "sfizz_render (from PATH)".to_string())
    );
    println!(
        "  Notes range : {}..{}",
        args.start_midi, args.end_midi
    );
    println!("  Note length : {}s", args.note_length);
    if args.auto_slot_length {
        println!(
            "  Slot length : auto (measured per font at runtime, max {}s)",
            args.max_slot_length
        );
    } else {
        println!("  Slot length : {}s", args.slot_length);
    }
    println!(
        "  Velocities  : {}",
        args.resolved_velocities().iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
    );
    println!(
        "  Variations  : {} (velocity jitter +/-{})",
        args.variations,
        args.effective_jitter()
    );
    if let Some(q) = args.chord {
        println!("  Chord       : {} (slice = root)", q.short());
    }
    let chords = args.resolved_chords();
    if !chords.is_empty() {
        let qs = chords.iter().map(|q| q.short()).collect::<Vec<_>>().join(",");
        if args.file_per_chord {
            println!("  Chords      : [{qs}] one file per quality");
        } else {
            println!("  Chords      : [{qs}] packed into files of <= {} slices", args.max_slices);
        }
        if !args.no_probe {
            println!(
                "                (roots narrowed to the sounding set at runtime — chord slot counts below are the full-range upper bound)"
            );
        }
    }
    println!("\n  Output files ({}):", jobs.len());
    for job in jobs {
        let path =
            output::output_wav_path(&job.out_base, &job.name, job.slot_map.len(), args.note_length, false);
        println!("    - {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chords::ChordQuality;

    #[test]
    fn clean_take_uses_base_velocity() {
        assert_eq!(slot_velocities(4, 100, 8, 0), vec![100, 100, 100, 100]);
        assert_eq!(slot_velocities(3, 90, 0, 5), vec![90, 90, 90]);
    }

    #[test]
    fn variation_take_jitters_within_range_and_is_deterministic() {
        let a = slot_velocities(16, 100, 8, 1);
        let b = slot_velocities(16, 100, 8, 1);
        assert_eq!(a, b, "same seed -> identical take");
        assert!(a.iter().all(|&v| (92..=108).contains(&v)), "within +/-8 of 100");
        let c = slot_velocities(16, 100, 8, 2);
        assert_ne!(a, c);
    }

    #[test]
    fn jitter_clamps_to_valid_velocity() {
        let v = slot_velocities(64, 3, 8, 1);
        assert!(v.iter().all(|&x| (1..=127).contains(&x)));
    }

    #[test]
    fn notes_chain_is_default_and_explicit() {
        // Plain run: one notes chain.
        let args = base_args();
        assert_eq!(base_chain_tags(&args), vec!["notes"]);
        // --notes alongside chords: notes + one packed file per fitting group.
        let mut a = base_args();
        a.chords = Some(vec![ChordQuality::Maj, ChordQuality::Min]);
        a.notes = true;
        let tags = base_chain_tags(&a);
        assert_eq!(tags.first().map(String::as_str), Some("notes"));
        assert!(tags.iter().any(|t| t.contains("maj")));
    }

    #[test]
    fn chords_pack_and_file_per_chord() {
        // 88 roots (21..108), 3 qualities, max 128 -> 1 quality/file -> 3 files.
        let mut a = base_args();
        a.chords = Some(vec![ChordQuality::Maj, ChordQuality::Min, ChordQuality::Dim]);
        assert_eq!(base_chain_tags(&a), vec!["maj", "min", "dim"]);
        // --file-per-chord forces one quality each regardless of budget.
        a.file_per_chord = true;
        assert_eq!(base_chain_tags(&a).len(), 3);
    }

    #[test]
    fn build_jobs_multiplies_chains_by_velocity_and_variation() {
        let mut a = base_args();
        a.chords = Some(vec![ChordQuality::Maj, ChordQuality::Min]); // 2 chord files + no notes
        a.velocities = Some(vec![40, 90]);
        a.variations = 2;
        let vels = a.resolved_velocities();
        let jobs = build_jobs(&a, &vels, &[prep(a.slot_length)]);
        // 2 base chains x 2 velocities x 2 variations = 8 jobs.
        assert_eq!(jobs.len(), 8);
        assert!(jobs.iter().any(|j| j.name == "maj_v40_take01"));
        assert!(jobs.iter().any(|j| j.name == "min_v90_take02"));
        // These are all chord jobs (tagged for the JSON sidecar only).
        assert!(jobs.iter().all(|j| j.is_chord));
    }

    #[test]
    fn single_axis_names_stay_bare() {
        let a = base_args();
        let jobs = build_jobs(&a, &a.resolved_velocities(), &[prep(a.slot_length)]);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "notes");
    }

    #[test]
    fn per_font_slot_length_flows_into_jobs() {
        let mut a = base_args();
        a.sfz = vec![PathBuf::from("a.sfz"), PathBuf::from("b.sfz")];
        let jobs = build_jobs(&a, &a.resolved_velocities(), &[prep(2.5), prep(7.0)]);
        let a_len = jobs.iter().find(|j| j.sfz.ends_with("a.sfz")).unwrap().slot_length;
        let b_len = jobs.iter().find(|j| j.sfz.ends_with("b.sfz")).unwrap().slot_length;
        assert_eq!((a_len, b_len), (2.5, 7.0));
    }

    #[test]
    fn chords_pack_only_sounding_roots() {
        // Chord files pack the sounding roots, not the full range. A 10-root
        // sounding set: both qualities fit one file (10*2 <= 128) -> one 20-slot file.
        let mut a = base_args();
        a.chords = Some(vec![ChordQuality::Maj, ChordQuality::Min]);
        let sounding: Vec<u8> = (60..70).collect();
        let packed: Vec<_> = base_chains(&a, 5.0, &sounding)
            .into_iter()
            .filter(|c| c.is_chord)
            .collect();
        assert_eq!(packed.len(), 1);
        assert_eq!(packed[0].slot_map.len(), 20, "10 sounding roots x 2 qualities");

        // --file-per-chord: one quality per file, each compact to the sounding roots.
        a.file_per_chord = true;
        let per: Vec<_> = base_chains(&a, 5.0, &sounding)
            .into_iter()
            .filter(|c| c.is_chord)
            .collect();
        assert_eq!(per.len(), 2, "one file per quality");
        assert!(per.iter().all(|c| c.slot_map.len() == 10), "compact to sounding roots");
    }

    #[test]
    fn font_output_base_layouts() {
        let mut args = base_args();
        args.output = Some(PathBuf::from("out/Piano.wav"));
        assert_eq!(
            font_output_base(&args, Path::new("Piano.sfz"), false),
            PathBuf::from("out/Piano.wav")
        );
        assert_eq!(
            font_output_base(&args, Path::new("some/dir/Rhodes.sfz"), true),
            PathBuf::from("out/Rhodes.wav")
        );
        args.output = None;
        assert_eq!(
            font_output_base(&args, Path::new("some/dir/Rhodes.sfz"), false),
            PathBuf::from("some/dir/Rhodes.wav")
        );
    }

    /// Tags of the base chains for a config (order-preserving). Uses the full
    /// range as chord roots (as the runtime does when the probe is skipped).
    fn base_chain_tags(args: &RenderSfzArgs) -> Vec<String> {
        let roots: Vec<u8> = (args.start_midi..=args.end_midi).collect();
        base_chains(args, 5.0, &roots).into_iter().map(|c| c.tag).collect()
    }

    /// A `FontPrep` with the given slot length and a full 0..127 root set.
    fn prep(slot_length: f64) -> FontPrep {
        FontPrep {
            slot_length,
            chord_roots: (0u8..=127).collect(),
        }
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
            chords: None,
            file_per_chord: false,
            per_octave: false,
            max_slices: 128,
            notes: false,
            auto_slot_length: false,
            max_slot_length: 20.0,
            measure_notes: 8,
            decay_threshold: 0.000125,
            slot_margin: 0.7,
            no_probe: false,
            probe_ms: 250,
            probe_threshold: 0.003,
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
