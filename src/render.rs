//! The render command: drive MIDI notes, record audio, assemble the chain.

use crate::audio::{self, Capture};
use crate::chords::ChordQuality;
use crate::cli::RenderArgs;
use crate::config::{AudioConfig, FormatConfig, M8Config, MidiConfig, RenderConfig, RenderParams};
use crate::devices;
use crate::notes::{Slot, build_slot_map, midi_to_m8_note};
use crate::output;
use crate::wav::{self, M8_SAMPLE_RATE};
use anyhow::{Context, Result, bail};
use midir::MidiOutputConnection;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Run the `render` command.
pub fn run(args: &RenderArgs) -> Result<()> {
    args.validate()?;

    // Probe map: single notes for note/packed modes, the chord per note for
    // --chord. The probe runs once over this map and is shared by every file.
    let slot_map = build_slot_map(args.start_midi, args.end_midi, args.slot_length, args.chord);

    if args.dry_run {
        print_plan(args, &slot_map);
        return Ok(());
    }

    let (mut conn, midi_name) = if args.virtual_midi {
        devices::open_virtual_midi_output("midi-sampler-to-m8")?
    } else {
        // validate() guarantees midi_output is Some here.
        devices::open_midi_output(args.midi_output.as_deref().unwrap_or_default())?
    };
    let (device, audio_name) = devices::open_audio_input(&args.audio_input)?;
    let capture = Capture::open(device)?;

    // Now that the source is open, resolve the output layout (Auto follows it).
    let out_channels = args.channels.resolve(capture.native_channels);

    if args.virtual_midi {
        println!("MIDI output : virtual port '{midi_name}'");
    } else {
        println!("MIDI output : {midi_name}");
    }
    println!(
        "Audio input : {audio_name} ({} Hz, {} ch native)",
        capture.native_rate, capture.native_channels
    );
    let layout = if out_channels == 1 { "mono" } else { "stereo" };
    if args.channels == crate::cli::Channels::Auto {
        println!("Output      : {layout} (auto, matched source)");
    } else {
        println!("Output      : {layout}");
    }
    if capture.native_rate != M8_SAMPLE_RATE {
        println!(
            "  note: capturing at {} Hz and resampling to {} Hz",
            capture.native_rate, M8_SAMPLE_RATE
        );
    }

    // A virtual port only exists while we run, so give the user a chance to
    // enable it as a MIDI input in their instrument before notes start.
    if args.virtual_midi {
        println!(
            "\nVirtual MIDI port '{midi_name}' is open.\n\
             In your instrument's MIDI settings, enable '{midi_name}' as an input,\n\
             then press Enter here to start..."
        );
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let s = shutdown.clone();
        let _ = ctrlc::set_handler(move || s.store(true, Ordering::SeqCst));
    }

    let ch = args.channel - 1;
    let note_on = 0x90 | ch;
    let note_off = 0x80 | ch;

    // Single-note (or single-chord) test mode.
    if let Some(note) = args.test_note {
        // Play the chord rooted at the test note when --chord is set, else the
        // bare note. (--chords is a packing mode and doesn't apply to one note.)
        let notes = match args.chord {
            Some(q) => crate::chords::chord_notes(note, q),
            None => vec![note],
        };
        match args.chord {
            Some(q) => println!("Test {}", crate::chords::chord_label(note, q)),
            None => println!("Test note {note} ({})", midi_to_m8_note(note)),
        }
        let samples = render_one(
            &mut conn,
            &capture,
            &notes,
            args,
            args.slot_length,
            out_channels,
            note_on,
            note_off,
            &shutdown,
        );
        all_notes_off(&mut conn, ch);
        let mut samples = samples;
        if !args.no_normalize {
            audio::normalize_peak(&mut samples, dbfs_to_amp(args.normalize_dbfs));
        }
        let path = output::test_note_path(&args.output, note);
        wav::write_wav(&path, &samples, M8_SAMPLE_RATE, out_channels)?;
        println!("Wrote test WAV: {}", path.display());
        return Ok(());
    }

    // Probe pass: needed to skip silent slots (unless --no-probe) and/or to
    // choose which notes to measure for --auto-slot-length.
    let probed: Option<Vec<bool>> = if args.no_probe && !args.auto_slot_length {
        None
    } else {
        let s = probe_sounding(
            &mut conn, &capture, args, note_on, note_off, &slot_map, &shutdown,
        );
        if shutdown.load(Ordering::SeqCst) {
            all_notes_off(&mut conn, ch);
            bail!("interrupted during probe; nothing recorded");
        }
        Some(s)
    };

    // Auto slot length: measure the ring-out and override --slot-length.
    let effective_slot_length = if args.auto_slot_length {
        let sounding_indices: Vec<usize> = match &probed {
            Some(p) => (0..p.len()).filter(|&i| p[i]).collect(),
            None => Vec::new(),
        };
        let pool: Vec<usize> = if sounding_indices.is_empty() {
            (0..slot_map.len()).collect()
        } else {
            sounding_indices
        };
        let sample_notes: Vec<Vec<u8>> = pick_spread(&pool, args.measure_notes as usize)
            .iter()
            .map(|&i| slot_map[i].notes.clone())
            .collect();
        let len = measure_slot_length(
            &mut conn,
            &capture,
            args,
            &sample_notes,
            note_on,
            note_off,
            &shutdown,
        );
        if shutdown.load(Ordering::SeqCst) {
            all_notes_off(&mut conn, ch);
            bail!("interrupted during measurement; nothing recorded");
        }
        len
    } else {
        args.slot_length
    };

    // Warn once if packing chords without a probe (blanks may be recorded).
    if args.chords.is_some() && probed.is_none() {
        println!(
            "warning: --no-probe with --chords packs the full {}..{} range, so unplayable \
             notes may be recorded as blank chords. Drop --no-probe to pack only sounding roots.",
            args.start_midi, args.end_midi
        );
    }

    // One or more output files, all recorded from the shared probe/measurement.
    let jobs = build_jobs(args, &probed, effective_slot_length);

    // Upfront cost estimate, so a large run announces its RAM/time before it starts.
    let slot_counts: Vec<usize> = jobs.iter().map(|j| j.slot_map.len()).collect();
    let record_counts: Vec<usize> = jobs
        .iter()
        .map(|j| job_sounding(j, &probed, args).iter().filter(|&&s| s).count())
        .collect();
    let (peak_bytes, total_secs) = batch_cost(
        &slot_counts,
        &record_counts,
        effective_slot_length,
        out_channels,
        args.pre_roll_ms,
    );
    println!(
        "\nBatch plan: {} file(s) — peak ~{} in RAM, ~{} total.",
        jobs.len(),
        human_bytes(peak_bytes),
        human_duration(total_secs),
    );
    // Give a big run a chance to be aborted before hours of recording begin.
    if peak_bytes >= MEM_WARN_BYTES {
        println!(
            "  This holds ~{} in memory. Starting in 5s — press Ctrl-C to abort.",
            human_bytes(peak_bytes)
        );
        for i in (1..=5).rev() {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            print!("\r  {i}... ");
            let _ = std::io::stdout().flush();
            sleep(Duration::from_secs(1));
        }
        println!();
        if shutdown.load(Ordering::SeqCst) {
            all_notes_off(&mut conn, ch);
            bail!("aborted before recording");
        }
    }

    let mut written: Vec<std::path::PathBuf> = Vec::new();
    let mut any_completed = false;
    for (ji, job) in jobs.iter().enumerate() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if jobs.len() > 1 {
            println!("\n=== File {}/{}: {} ===", ji + 1, jobs.len(), job.label);
        }

        let sounding_for_loop = job_sounding(job, &probed, args);
        let (mut full, statuses, completed) = record_chain(
            &mut conn,
            &capture,
            &job.slot_map,
            &sounding_for_loop,
            args,
            effective_slot_length,
            out_channels,
            note_on,
            note_off,
            &shutdown,
        );
        all_notes_off(&mut conn, ch);

        if completed == 0 {
            println!("  (no slots rendered)");
            continue;
        }
        // Peak-normalize the whole file (preserves note-to-note dynamics).
        if !args.no_normalize {
            audio::normalize_peak(&mut full, dbfs_to_amp(args.normalize_dbfs));
        }
        if completed < job.slot_map.len() {
            println!(
                "Interrupted after {completed}/{} slots. Writing partial output.",
                job.slot_map.len()
            );
        }
        any_completed = true;
        let paths = write_job_outputs(
            args,
            job,
            &full,
            &statuses,
            out_channels,
            layout,
            effective_slot_length,
            &midi_name,
            &audio_name,
        )?;
        written.extend(paths);
    }

    if !any_completed {
        bail!("no slots were rendered");
    }

    println!("\nWrote {} file(s):", written.len());
    for p in &written {
        println!("  {}", p.display());
    }
    print_m8_hint();
    Ok(())
}

/// One output file to produce from the shared probe/measurement pass.
struct Job {
    slot_map: Vec<Slot>,
    /// Filename tag (chord quality/qualities); `None` for the plain note chain.
    tag: Option<String>,
    /// Record every slot (chord files) vs. skip probed-silent slots (notes).
    force_all: bool,
    /// Write the CSV legend even without `--csv` (chord files need the map).
    csv_default: bool,
    /// Human label for the per-file header.
    label: String,
}

/// Assemble the list of output files: the optional single-note chain, an optional
/// slice=root `--chord` file, and the auto-split `--chords` files.
fn build_jobs(args: &RenderArgs, probed: &Option<Vec<bool>>, slot_length: f64) -> Vec<Job> {
    let mut jobs = Vec::new();
    let chords = args.resolved_chords();

    // The plain note chain: explicit via --notes, or the default when no chord
    // flags are given at all.
    if args.notes || (chords.is_empty() && args.chord.is_none()) {
        jobs.push(Job {
            slot_map: build_slot_map(args.start_midi, args.end_midi, slot_length, None),
            tag: None,
            force_all: false,
            csv_default: false,
            label: "notes".to_string(),
        });
    }

    if let Some(q) = args.chord {
        jobs.push(Job {
            slot_map: build_slot_map(args.start_midi, args.end_midi, slot_length, Some(q)),
            tag: Some(q.short().to_string()),
            force_all: false,
            csv_default: false,
            label: format!("{} (slice=root)", q.short()),
        });
    }

    if !chords.is_empty() {
        let push_chord_job = |jobs: &mut Vec<Job>, roots: &[u8], chunk: &[ChordQuality], prefix: &str| {
            let qs = chunk.iter().map(|q| q.short()).collect::<Vec<_>>().join("-");
            let tag = if prefix.is_empty() {
                qs
            } else {
                format!("{prefix}_{qs}")
            };
            jobs.push(Job {
                slot_map: crate::notes::chord_slots(roots, chunk, slot_length),
                tag: Some(tag.clone()),
                force_all: true,
                csv_default: true,
                label: tag,
            });
        };

        if args.per_octave {
            // One file per octave: that octave's roots x the qualities (further
            // split only if a single octave overflows the slice budget).
            for (octave, oct_roots) in octave_groups(&sounding_roots(args, probed)) {
                let prefix = format!("oct-{}", midi_to_m8_note(octave * 12));
                for chunk in
                    split_chord_files(oct_roots.len(), &chords, args.max_slices, false)
                {
                    push_chord_job(&mut jobs, &oct_roots, &chunk, &prefix);
                }
            }
        } else {
            let roots = packed_roots(args, probed);
            for chunk in
                split_chord_files(roots.len(), &chords, args.max_slices, args.file_per_chord)
            {
                push_chord_job(&mut jobs, &roots, &chunk, "");
            }
        }
    }
    jobs
}

/// Per-slot record flags for a job: chord files record every slot; note/slice-root
/// files skip slots the probe found silent (indexed by root position).
fn job_sounding(job: &Job, probed: &Option<Vec<bool>>, args: &RenderArgs) -> Vec<bool> {
    if job.force_all || args.no_probe {
        return vec![true; job.slot_map.len()];
    }
    match probed {
        Some(p) => job
            .slot_map
            .iter()
            .map(|s| p.get(s.slot as usize).copied().unwrap_or(true))
            .collect(),
        None => vec![true; job.slot_map.len()],
    }
}

/// Partition chord qualities into per-file chunks, keeping each quality whole and
/// each file within `max_slices` (given `roots_len` roots per quality). One
/// quality per file when `file_per_chord`.
fn split_chord_files(
    roots_len: usize,
    qualities: &[crate::chords::ChordQuality],
    max_slices: usize,
    file_per_chord: bool,
) -> Vec<Vec<crate::chords::ChordQuality>> {
    let per_file = if file_per_chord {
        1
    } else {
        (max_slices / roots_len.max(1)).max(1)
    };
    qualities.chunks(per_file).map(|c| c.to_vec()).collect()
}

/// Peak RAM (bytes) that above which the run pauses for an abortable countdown.
const MEM_WARN_BYTES: usize = 512 * 1024 * 1024;

/// Estimate a batch's peak RAM and total recording time. Files record one at a
/// time, so peak memory is the largest single file's `f32` chain buffer, not the
/// sum; total time counts only the slots actually recorded.
fn batch_cost(
    slot_counts: &[usize],
    record_counts: &[usize],
    slot_length: f64,
    out_channels: u16,
    pre_roll_ms: u64,
) -> (usize, f64) {
    let slot_samples = (slot_length * M8_SAMPLE_RATE as f64).round() as usize * out_channels as usize;
    let peak_bytes = slot_counts.iter().copied().max().unwrap_or(0)
        * slot_samples
        * std::mem::size_of::<f32>();
    let per_slot_secs = slot_length + pre_roll_ms as f64 / 1000.0;
    let total_secs = record_counts.iter().sum::<usize>() as f64 * per_slot_secs;
    (peak_bytes, total_secs)
}

/// Format a byte count as a compact `KB`/`MB`/`GB` string.
fn human_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{:.0} KB", b / KB)
    }
}

/// Format a duration in seconds as `~Ns` / `N.N min` / `N.N h`.
fn human_duration(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1} h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0} min", secs / 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

/// Record one chain (note or chord) and return the interleaved buffer, per-slot
/// statuses (padded to the map length), and the number of slots completed.
#[allow(clippy::too_many_arguments)]
fn record_chain(
    conn: &mut MidiOutputConnection,
    capture: &Capture,
    slot_map: &[Slot],
    sounding_for_loop: &[bool],
    args: &RenderArgs,
    slot_length: f64,
    out_channels: u16,
    note_on: u8,
    note_off: u8,
    shutdown: &Arc<AtomicBool>,
) -> (Vec<f32>, Vec<String>, usize) {
    let target_frames = (slot_length * M8_SAMPLE_RATE as f64).round() as usize;
    let slot_samples = target_frames * out_channels as usize;
    let mut full: Vec<f32> = Vec::with_capacity(slot_samples * slot_map.len());
    let mut statuses: Vec<String> = Vec::with_capacity(slot_map.len());
    let mut completed = 0usize;

    let to_record = sounding_for_loop.iter().filter(|&&s| s).count();
    let est_min = to_record as f64 * slot_length / 60.0;
    println!(
        "Recording {to_record} slots at {slot_length:.1}s each — estimated ~{est_min:.1} min."
    );

    let start = Instant::now();
    for (i, slot) in slot_map.iter().enumerate() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        print!(
            "\r{}  Slot {:>3}/{}  MIDI {:>3}  {:<8}  {}   ",
            progress_bar(i + 1, slot_map.len(), 10),
            slot.slot as usize + 1,
            slot_map.len(),
            slot.midi,
            slot.label,
            eta_label(start.elapsed(), i + 1, slot_map.len()),
        );
        let _ = std::io::stdout().flush();

        if !sounding_for_loop[i] {
            // Skip recording: write an instant silent slot.
            full.extend(std::iter::repeat_n(0.0f32, slot_samples));
            statuses.push("silent".to_string());
            completed += 1;
            continue;
        }

        let samples = render_one(
            conn,
            capture,
            &slot.notes,
            args,
            slot_length,
            out_channels,
            note_on,
            note_off,
            shutdown,
        );
        debug_assert_eq!(samples.len(), slot_samples);

        let peak = peak_abs(&samples);
        statuses.push(
            if is_sounding(peak, 1e-4) {
                "rendered"
            } else {
                "silent"
            }
            .to_string(),
        );
        full.extend_from_slice(&samples);
        completed += 1;
    }
    println!();

    while statuses.len() < slot_map.len() {
        statuses.push("skipped".to_string());
    }
    (full, statuses, completed)
}

/// Write a job's WAV(s) and sidecars, print a per-file summary, and return the
/// paths written.
#[allow(clippy::too_many_arguments)]
fn write_job_outputs(
    args: &RenderArgs,
    job: &Job,
    full: &[f32],
    statuses: &[String],
    out_channels: u16,
    layout: &str,
    slot_length: f64,
    midi_name: &str,
    audio_name: &str,
) -> Result<Vec<std::path::PathBuf>> {
    let slot_samples = (slot_length * M8_SAMPLE_RATE as f64).round() as usize * out_channels as usize;
    let name = job.tag.as_deref().unwrap_or("notes");
    let mut written = Vec::new();

    // Everything for this render lands in a per-name folder.
    let folder = output::render_dir(&args.output);
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("creating output folder {}", folder.display()))?;

    // Padded chain: every slot, identical length.
    let padded_path =
        output::output_wav_path(&args.output, name, job.slot_map.len(), args.note_length, false);
    wav::write_wav(&padded_path, full, M8_SAMPLE_RATE, out_channels)?;
    written.push(padded_path.clone());

    // Unpadded copy (opt-in): drop leading/trailing silent slots, keep interior ones.
    let unpadded_path = match rendered_bounds(statuses).filter(|_| args.unpadded) {
        Some((f, l)) => {
            let trimmed_count = l - f + 1;
            let path =
                output::output_wav_path(&args.output, name, trimmed_count, args.note_length, true);
            wav::write_wav(
                &path,
                &full[f * slot_samples..(l + 1) * slot_samples],
                M8_SAMPLE_RATE,
                out_channels,
            )?;
            written.push(path.clone());
            Some(path)
        }
        _ => None,
    };

    // Sidecars: opt-in via --csv; chord files write the CSV legend by default.
    let (csv_path, json_path) = output::sidecar_paths(&padded_path);
    let csv_written = if args.csv || job.csv_default {
        output::write_csv_map(&csv_path, &job.slot_map, args.velocity, statuses)?;
        written.push(csv_path.clone());
        Some(csv_path.clone())
    } else {
        None
    };
    let json_written = if args.json {
        let config = build_config(
            args,
            midi_name,
            audio_name,
            out_channels,
            slot_length,
            job.slot_map.len() as u32,
            job.tag.as_deref(),
            &padded_path,
            &csv_path,
        );
        output::write_json_config(&json_path, &config)?;
        written.push(json_path.clone());
        Some(json_path)
    } else {
        None
    };

    let slice_hex = format!("{:02X}", job.slot_map.len());
    println!(
        "  WAV      : {} ({layout}, SLICE={slice_hex})",
        padded_path.display()
    );
    if let Some(p) = &unpadded_path {
        println!("  Unpadded : {}", p.display());
    }
    if let Some(p) = &csv_written {
        println!("  CSV      : {}", p.display());
    }
    if let Some(p) = &json_written {
        println!("  JSON     : {}", p.display());
    }
    Ok(written)
}

/// The M8 load instructions, printed once after all files are written.
fn print_m8_hint() {
    println!("\nLoad on the M8:");
    println!("  1. Copy the WAV(s) to your M8 SD card.");
    println!("  2. Create a Sampler instrument and load a WAV.");
    println!("  3. Set:  SLICE = <that file's count above>   PLAY = FWD   START = 00   LEN = FF");
    println!("  Each slice maps to its slot (slot index = the note you press).");
}

/// Render and post-process one slot — a single note or a chord — into a
/// fixed-length slot. Every note in `notes` is sounded together.
#[allow(clippy::too_many_arguments)]
fn render_one(
    conn: &mut MidiOutputConnection,
    capture: &Capture,
    notes: &[u8],
    args: &RenderArgs,
    slot_length: f64,
    out_channels: u16,
    note_on: u8,
    note_off: u8,
    shutdown: &Arc<AtomicBool>,
) -> Vec<f32> {
    let send_off = |conn: &mut MidiOutputConnection| {
        for &n in notes {
            let _ = conn.send(&[note_off, n, 0]);
        }
    };

    sleep(Duration::from_millis(args.pre_roll_ms));

    capture.arm();
    for &n in notes {
        let _ = conn.send(&[note_on, n, args.velocity]);
    }

    let slot_ms = (slot_length * 1000.0).round() as u64;
    // If the note is held at least as long as the slot, release just before the
    // end so the tail still fits; otherwise release at note_length.
    let note_off_at = if args.note_length >= slot_length {
        slot_ms.saturating_sub(100)
    } else {
        (args.note_length * 1000.0).round() as u64
    };

    let chunk = 50u64;
    let mut elapsed = 0u64;
    let mut sent_off = false;
    while elapsed < slot_ms {
        if shutdown.load(Ordering::SeqCst) {
            send_off(conn);
            sent_off = true;
            break;
        }
        if !sent_off && elapsed >= note_off_at {
            send_off(conn);
            sent_off = true;
        }
        let step = chunk.min(slot_ms - elapsed);
        sleep(Duration::from_millis(step));
        elapsed += step;
    }
    if !sent_off {
        send_off(conn);
    }

    let native = capture.disarm_take();
    // Leading-silence trim: threshold + lookback (both at the 44.1k output rate).
    let onset_threshold = if args.no_trim_onset {
        0.0
    } else {
        dbfs_to_amp(args.onset_dbfs)
    };
    let lookback_frames = (args.onset_lookback_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;
    let mut slot = audio::finalize_slot(
        &native,
        capture.native_rate,
        capture.native_channels,
        out_channels,
        slot_length,
        onset_threshold,
        lookback_frames,
    );
    let fade_in_frames = (args.fade_in_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;
    audio::apply_start_fade(&mut slot, out_channels, fade_in_frames);
    let fade_frames = (args.fade_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;
    audio::apply_end_fade(&mut slot, out_channels, fade_frames);
    slot
}

/// Convert a dBFS level to a linear amplitude in `[0, 1]`.
fn dbfs_to_amp(dbfs: f64) -> f32 {
    10f32.powf(dbfs as f32 / 20.0)
}

/// Send Note Off for every note plus an All-Notes-Off CC, so nothing sticks.
fn all_notes_off(conn: &mut MidiOutputConnection, ch: u8) {
    for n in 0..=127u8 {
        let _ = conn.send(&[0x80 | ch, n, 0]);
    }
    let _ = conn.send(&[0xB0 | ch, 123, 0]);
}

/// First and last slot index whose status is `rendered`, i.e. the inclusive
/// range to keep when trimming the leading/trailing silent slots. Interior
/// non-`rendered` slots stay inside the range. `None` if nothing was rendered.
fn rendered_bounds(statuses: &[String]) -> Option<(usize, usize)> {
    let first = statuses.iter().position(|s| s == "rendered")?;
    let last = statuses.iter().rposition(|s| s == "rendered")?;
    Some((first, last))
}

/// The roots to build chords on: the probed sounding roots, or the full
/// `start..=end` range under `--no-probe` (or when nothing sounded).
fn sounding_roots(args: &RenderArgs, probed: &Option<Vec<bool>>) -> Vec<u8> {
    let pool: Vec<u8> = match probed {
        Some(p) => (0..p.len())
            .filter(|&i| p[i])
            .map(|i| args.start_midi + i as u8)
            .collect(),
        None => (args.start_midi..=args.end_midi).collect(),
    };
    if pool.is_empty() {
        (args.start_midi..=args.end_midi).collect()
    } else {
        pool
    }
}

/// Roots for the packed (non-octave) chord layout: the sounding roots, capped to
/// `max_slices` (a single quality can't exceed the per-file slice budget) by
/// spreading evenly.
fn packed_roots(args: &RenderArgs, probed: &Option<Vec<bool>>) -> Vec<u8> {
    let pool = sounding_roots(args, probed);
    if pool.len() > args.max_slices {
        fit_roots(&pool, args.max_slices)
    } else {
        pool
    }
}

/// Group roots by octave (`midi / 12`), preserving order, into
/// `(octave_index, roots_in_octave)` pairs.
fn octave_groups(roots: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut groups: Vec<(u8, Vec<u8>)> = Vec::new();
    for &r in roots {
        let oct = r / 12;
        match groups.last_mut() {
            Some((o, v)) if *o == oct => v.push(r),
            _ => groups.push((oct, vec![r])),
        }
    }
    groups
}

/// Pick `count` roots spread evenly across `pool` (capped at the pool size).
fn fit_roots(pool: &[u8], count: usize) -> Vec<u8> {
    let count = count.max(1).min(pool.len());
    let idx: Vec<usize> = (0..pool.len()).collect();
    pick_spread(&idx, count).iter().map(|&i| pool[i]).collect()
}

/// Peak absolute sample value of a buffer.
fn peak_abs(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
}

/// Whether a measured peak counts as audible sound.
fn is_sounding(peak: f32, threshold: f32) -> bool {
    peak >= threshold
}

/// Time (seconds) of the last frame whose level reaches `threshold` in an
/// interleaved native-rate buffer. `0.0` if the buffer is entirely below it.
fn last_sound_seconds(native: &[f32], rate: u32, channels: u16, threshold: f32) -> f64 {
    let ch = channels.max(1) as usize;
    let frames = native.len() / ch;
    let mut last: Option<usize> = None;
    for f in 0..frames {
        if (0..ch).any(|c| native[f * ch + c].abs() >= threshold) {
            last = Some(f);
        }
    }
    match last {
        Some(f) => (f + 1) as f64 / rate as f64,
        None => 0.0,
    }
}

/// Evenly spaced picks across `indices` (endpoints included for `n >= 2`,
/// the middle element for `n == 1`). Returns at most `indices.len()` picks.
fn pick_spread(indices: &[usize], n: usize) -> Vec<usize> {
    if indices.is_empty() || n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![indices[indices.len() / 2]];
    }
    if n >= indices.len() {
        return indices.to_vec();
    }
    let mut out: Vec<usize> = (0..n)
        .map(|k| indices[k * (indices.len() - 1) / (n - 1)])
        .collect();
    out.dedup();
    out
}

/// Measure the ring-out of the sampled notes and return a slot length that
/// covers the longest tail plus a margin, clamped to `[0.25, max_slot_length]`.
fn measure_slot_length(
    conn: &mut MidiOutputConnection,
    capture: &Capture,
    args: &RenderArgs,
    sample_notes: &[Vec<u8>],
    note_on: u8,
    note_off: u8,
    shutdown: &Arc<AtomicBool>,
) -> f64 {
    println!(
        "Measuring ring-out of {} note(s) to size the slot...",
        sample_notes.len()
    );
    let max_ms = (args.max_slot_length * 1000.0).round() as u64;
    let hold_ms = (args.note_length * 1000.0).round() as u64;
    let tail_window = (capture.native_rate as f64 * 0.15).round() as usize; // 150 ms
    let mut longest = 0.0f64;

    for notes in sample_notes {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let send_off = |conn: &mut MidiOutputConnection| {
            for &n in notes {
                let _ = conn.send(&[note_off, n, 0]);
            }
        };
        capture.arm();
        for &n in notes {
            let _ = conn.send(&[note_on, n, args.velocity]);
        }

        let mut elapsed = 0u64;
        let mut released = false;
        let mut quiet_ms = 0u64;
        let chunk = 50u64;
        while elapsed < max_ms {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !released && elapsed >= hold_ms {
                send_off(conn);
                released = true;
            }
            let step = chunk.min(max_ms - elapsed);
            sleep(Duration::from_millis(step));
            elapsed += step;
            // Early-stop once the note has been quiet for ~400 ms post-release.
            if released {
                if capture.tail_peak(tail_window) < args.decay_threshold {
                    quiet_ms += step;
                    if quiet_ms >= 400 {
                        break;
                    }
                } else {
                    quiet_ms = 0;
                }
            }
        }
        if !released {
            send_off(conn);
        }
        let native = capture.disarm_take();
        let tail = last_sound_seconds(
            &native,
            capture.native_rate,
            capture.native_channels,
            args.decay_threshold,
        );
        let root = notes.first().copied().unwrap_or(0);
        println!("  MIDI {root:>3} {:<4}  tail {tail:.2}s", midi_to_m8_note(root));
        longest = longest.max(tail);
    }
    all_notes_off(conn, args.channel - 1);

    let chosen = (longest + args.slot_margin).clamp(0.25, args.max_slot_length);
    if longest + args.slot_margin >= args.max_slot_length {
        println!(
            "Chosen slot length: {chosen:.2}s (hit the {:.1}s cap — raise --max-slot-length or set --slot-length manually if notes are cut).",
            args.max_slot_length
        );
    } else {
        println!(
            "Chosen slot length: {chosen:.2}s (longest tail {longest:.2}s + {:.2}s margin).",
            args.slot_margin
        );
    }
    chosen
}

/// Quickly play every note in the map and detect which ones produce sound,
/// so the main pass can skip recording silent slots. Returns one bool per slot.
fn probe_sounding(
    conn: &mut MidiOutputConnection,
    capture: &Capture,
    args: &RenderArgs,
    note_on: u8,
    note_off: u8,
    slot_map: &[Slot],
    shutdown: &Arc<AtomicBool>,
) -> Vec<bool> {
    println!(
        "Probing {} notes ({} ms each) to find which produce sound...",
        slot_map.len(),
        args.probe_ms
    );
    let mut sounding = vec![false; slot_map.len()];

    for (i, slot) in slot_map.iter().enumerate() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        print!(
            "\rProbe {:>3}/{}  MIDI {:>3}  {:<8}   ",
            i + 1,
            slot_map.len(),
            slot.midi,
            slot.label
        );
        let _ = std::io::stdout().flush();

        capture.arm();
        for &n in &slot.notes {
            let _ = conn.send(&[note_on, n, args.velocity]);
        }

        let mut elapsed = 0u64;
        let chunk = 25u64;
        while elapsed < args.probe_ms {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let step = chunk.min(args.probe_ms - elapsed);
            sleep(Duration::from_millis(step));
            elapsed += step;
        }
        for &n in &slot.notes {
            let _ = conn.send(&[note_off, n, 0]);
        }
        let native = capture.disarm_take();
        sounding[i] = is_sounding(peak_abs(&native), args.probe_threshold);
    }
    println!();
    all_notes_off(conn, args.channel - 1);

    let count = sounding.iter().filter(|&&s| s).count();
    let first = sounding.iter().position(|&s| s);
    let last = sounding.iter().rposition(|&s| s);
    match (first, last) {
        (Some(f), Some(l)) => println!(
            "Found {count} sounding notes (MIDI {}..{}, {} to {}).",
            slot_map[f].midi,
            slot_map[l].midi,
            slot_map[f].m8_note,
            slot_map[l].m8_note,
        ),
        _ => println!("No sounding notes detected — every slot will be silent."),
    }
    sounding
}

#[allow(clippy::too_many_arguments)]
fn build_config(
    args: &RenderArgs,
    midi_name: &str,
    audio_name: &str,
    out_channels: u16,
    slot_length: f64,
    slice_count: u32,
    chord_tag: Option<&str>,
    wav_path: &std::path::Path,
    csv_path: &std::path::Path,
) -> RenderConfig {
    let file_name = |p: &std::path::Path| {
        p.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    // The M8 SLICE setting is the slice count in hex (128 -> "80", 120 -> "78").
    let slice_hex = format!("{slice_count:02X}");
    RenderConfig {
        mode: "midi-render".to_string(),
        output_wav: file_name(wav_path),
        output_map: file_name(csv_path),
        format: FormatConfig {
            sample_rate: M8_SAMPLE_RATE,
            bit_depth: 16,
            channels: out_channels,
        },
        midi: MidiConfig {
            output: midi_name.to_string(),
            channel: args.channel,
            start_midi: args.start_midi,
            end_midi: args.end_midi,
            velocity: args.velocity,
        },
        audio: AudioConfig {
            input: audio_name.to_string(),
        },
        render: RenderParams {
            note_length_seconds: args.note_length,
            slot_length_seconds: slot_length,
            pre_roll_ms: args.pre_roll_ms,
            slice_count,
            m8_slice_hex: slice_hex.clone(),
            chord: chord_tag.map(|t| t.to_string()),
        },
        m8: M8Config {
            slice: slice_hex,
            ..M8Config::standard()
        },
    }
}

/// A fixed-width text progress bar like `[####------] 42%`.
fn progress_bar(done: usize, total: usize, width: usize) -> String {
    let frac = if total == 0 {
        1.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    };
    let filled = (frac * width as f64).round() as usize;
    let pct = (frac * 100.0).round() as u32;
    format!(
        "[{}{}] {:>3}%",
        "#".repeat(filled),
        "-".repeat(width - filled),
        pct
    )
}

/// Estimated time remaining, projected from the average pace so far. Returns
/// `~N.N min left` once at least one slot is done, otherwise a placeholder.
fn eta_label(elapsed: Duration, done: usize, total: usize) -> String {
    if done == 0 || done >= total {
        return "~-- left".to_string();
    }
    let per = elapsed.as_secs_f64() / done as f64;
    let remaining_min = per * (total - done) as f64 / 60.0;
    format!("~{remaining_min:.1} min left")
}

fn print_plan(args: &RenderArgs, slot_map: &[Slot]) {
    let midi_output = if args.virtual_midi {
        "virtual port 'midi-sampler-to-m8'".to_string()
    } else {
        args.midi_output.clone().unwrap_or_default()
    };
    println!("DRY RUN — no devices are opened\n");
    println!("  MIDI output : {midi_output}");
    println!("  Audio input : {}", args.audio_input);
    println!(
        "  Notes range : {}..{} ({} notes)",
        args.start_midi,
        args.end_midi,
        slot_map.len()
    );
    println!("  Velocity    : {}", args.velocity);
    println!("  Channel     : {}", args.channel);
    println!("  Note length : {}s", args.note_length);
    if args.auto_slot_length {
        println!(
            "  Slot length : auto (measured once at runtime, max {}s)",
            args.max_slot_length
        );
    } else {
        println!("  Slot length : {}s", args.slot_length);
    }
    println!("  Pre-roll    : {}ms", args.pre_roll_ms);
    if args.no_probe {
        println!("  Probe       : off");
    } else {
        println!(
            "  Probe       : on ({} ms/note, threshold {}) — runs once, shared by all files",
            args.probe_ms, args.probe_threshold
        );
    }
    if args.chords.is_some() {
        let qs = args
            .resolved_chords()
            .iter()
            .map(|q| q.short())
            .collect::<Vec<_>>()
            .join(",");
        if args.per_octave {
            println!("  Chords      : [{qs}] one file per octave (that octave's roots x qualities)");
        } else if args.file_per_chord {
            println!("  Chords      : one file per quality [{qs}]");
        } else {
            println!(
                "  Chords      : [{qs}] packed into files of whole qualities (<= {} slices each; grouping set at runtime)",
                args.max_slices
            );
        }
        if args.no_probe && !args.auto_slot_length {
            println!("  warning     : --no-probe packs the full range, so unplayable notes may be recorded as blank chords");
        }
    }

    // The files that would be produced. Slot counts depend on the runtime-detected
    // playable range, so these use the full range as an indicative count.
    let range = slot_map.len();
    let name = |tag: &str, count: usize| {
        output::output_wav_path(&args.output, tag, count, args.note_length, false)
            .display()
            .to_string()
    };
    let chords = args.resolved_chords();
    println!("\n  Output folder: {}", output::render_dir(&args.output).display());
    println!("  Output files (counts indicative; finalized at runtime):");
    if args.notes || (chords.is_empty() && args.chord.is_none()) {
        println!("    - {}", name("notes", range));
    }
    if let Some(q) = args.chord {
        println!("    - {}", name(q.short(), range));
    }
    if !chords.is_empty() {
        if args.per_octave {
            println!(
                "    - {} … (one file per octave, each tagged oct-<note> with its qualities)",
                name("oct-<note>_<qualities>", range)
            );
        } else if args.file_per_chord {
            for q in &chords {
                println!("    - {}", name(q.short(), range));
            }
        } else {
            // The quality-to-file grouping depends on the playable-root count, so
            // show the naming pattern rather than a (misleading) fixed split.
            println!(
                "    - {} … (chord qualities packed into one or more files, each tagged with its qualities)",
                name("<qualities>", range)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_fills_and_percents() {
        assert_eq!(progress_bar(0, 10, 10), "[----------]   0%");
        assert_eq!(progress_bar(5, 10, 10), "[#####-----]  50%");
        assert_eq!(progress_bar(10, 10, 10), "[##########] 100%");
        // Empty total renders as complete rather than dividing by zero.
        assert_eq!(progress_bar(0, 0, 4), "[####] 100%");
    }

    #[test]
    fn eta_label_projects_from_pace() {
        // 10s for 1 of 5 slots -> ~40s for the remaining 4 -> ~0.7 min.
        assert_eq!(eta_label(Duration::from_secs(10), 1, 5), "~0.7 min left");
        // Nothing done yet, or all done -> placeholder.
        assert_eq!(eta_label(Duration::from_secs(0), 0, 5), "~-- left");
        assert_eq!(eta_label(Duration::from_secs(50), 5, 5), "~-- left");
    }

    #[test]
    fn rendered_bounds_trims_ends_and_keeps_interior() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Leading/trailing silence dropped; interior silent slot (index 3) kept.
        assert_eq!(
            rendered_bounds(&s(&["silent", "silent", "rendered", "silent", "rendered", "silent"])),
            Some((2, 4))
        );
        // A trailing interrupted run ("skipped") is excluded.
        assert_eq!(
            rendered_bounds(&s(&["rendered", "skipped", "skipped"])),
            Some((0, 0))
        );
        // No rendered slots -> no range.
        assert_eq!(rendered_bounds(&s(&["silent", "silent"])), None);
    }

    #[test]
    fn peak_abs_finds_largest_magnitude() {
        assert_eq!(peak_abs(&[0.0, -0.7, 0.3, 0.5]), 0.7);
        assert_eq!(peak_abs(&[]), 0.0);
    }

    #[test]
    fn is_sounding_uses_threshold() {
        assert!(is_sounding(0.01, 0.003));
        assert!(is_sounding(0.003, 0.003));
        assert!(!is_sounding(0.0, 0.003));
        assert!(!is_sounding(0.001, 0.003));
    }

    #[test]
    fn last_sound_seconds_finds_tail_end() {
        // 100 loud mono frames then 100 silent, at 1000 Hz -> tail ends at 0.1s.
        let mut buf = vec![0.5f32; 100];
        buf.extend(std::iter::repeat_n(0.0f32, 100));
        let t = last_sound_seconds(&buf, 1000, 1, 0.003);
        assert!((t - 0.1).abs() < 1e-9, "got {t}");

        // All silence -> 0.0.
        assert_eq!(last_sound_seconds(&vec![0.0f32; 200], 1000, 1, 0.003), 0.0);
    }

    #[test]
    fn last_sound_seconds_handles_stereo_interleaving() {
        // 50 stereo frames (100 samples) loud, then silence; 1000 Hz -> 0.05s.
        let mut buf = vec![0.4f32; 100];
        buf.extend(std::iter::repeat_n(0.0f32, 100));
        let t = last_sound_seconds(&buf, 1000, 2, 0.003);
        assert!((t - 0.05).abs() < 1e-9, "got {t}");
    }

    #[test]
    fn fit_roots_spreads_and_caps() {
        let pool: Vec<u8> = (36..=96).collect(); // 61 candidate roots
        // Capping to 12 keeps the endpoints and spreads evenly.
        let roots = fit_roots(&pool, 12);
        assert_eq!(roots.len(), 12);
        assert_eq!(roots.first(), Some(&36));
        assert_eq!(roots.last(), Some(&96));
        // Never more than the pool size.
        assert_eq!(fit_roots(&pool, 200).len(), 61);
        assert_eq!(fit_roots(&[60, 62], 5).len(), 2);
    }

    #[test]
    fn octave_groups_splits_on_octave_boundary() {
        // Roots 48..=71 span two octaves (48-59 -> octave 4, 60-71 -> octave 5).
        let roots: Vec<u8> = (48..=71).collect();
        let groups = octave_groups(&roots);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, 4);
        assert_eq!(groups[0].1, (48..=59).collect::<Vec<u8>>());
        assert_eq!(groups[1].0, 5);
        assert_eq!(groups[1].1, (60..=71).collect::<Vec<u8>>());
        // Octave 4's C is MIDI 48 -> "C3" in the M8 naming.
        assert_eq!(midi_to_m8_note(groups[0].0 * 12), "C3");
    }

    #[test]
    fn dbfs_to_amp_matches_known_levels() {
        assert!((dbfs_to_amp(0.0) - 1.0).abs() < 1e-6);
        assert!((dbfs_to_amp(-6.0) - 0.501_187).abs() < 1e-4);
        assert!(dbfs_to_amp(-55.0) < 0.002);
    }

    #[test]
    fn batch_cost_peaks_on_largest_file() {
        // One 128-slot job + two 122-slot jobs, 20s stereo, 100ms pre-roll.
        let slot_samples = (20.0 * M8_SAMPLE_RATE as f64).round() as usize * 2;
        let (peak, total) = batch_cost(&[128, 122, 122], &[128, 122, 122], 20.0, 2, 100);
        // Peak tracks the largest file (128), not the sum.
        assert_eq!(peak, 128 * slot_samples * 4);
        // Total time = all recorded slots * (slot_length + pre_roll).
        assert!((total - (128 + 122 + 122) as f64 * (20.0 + 0.1)).abs() < 1e-6);
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(900 * 1024 * 1024), "900 MB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
        assert_eq!(human_bytes(512 * 1024), "512 KB");
    }

    #[test]
    fn split_chord_files_keeps_qualities_whole() {
        use crate::chords::ChordQuality::*;
        let nine = [Maj, Min, Dim, Aug, Maj7, Min7, Dom7, Sus2, Sus4];
        // 61 roots, max 128 -> 2 whole qualities per file -> 5 files [2,2,2,2,1].
        let files = split_chord_files(61, &nine, 128, false);
        assert_eq!(files.iter().map(|c| c.len()).collect::<Vec<_>>(), vec![2, 2, 2, 2, 1]);
        // file-per-chord -> one quality each.
        let per = split_chord_files(61, &nine, 128, true);
        assert_eq!(per.len(), 9);
        assert!(per.iter().all(|c| c.len() == 1));
        // Roots wider than the budget still fit one whole quality per file.
        assert!(split_chord_files(200, &nine, 128, false)
            .iter()
            .all(|c| c.len() == 1));
    }

    #[test]
    fn pick_spread_picks_evenly() {
        assert_eq!(pick_spread(&[10, 20, 30, 40, 50], 3), vec![10, 30, 50]);
        assert_eq!(pick_spread(&[10, 20, 30, 40, 50], 1), vec![30]);
        assert_eq!(pick_spread(&[10, 20], 5), vec![10, 20]); // n >= len
        assert_eq!(pick_spread(&[], 3), Vec::<usize>::new());
    }
}
