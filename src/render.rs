//! The render command: drive MIDI notes, record audio, assemble the chain.

use crate::audio::{self, Capture};
use crate::cli::RenderArgs;
use crate::config::{AudioConfig, FormatConfig, M8Config, MidiConfig, RenderConfig, RenderParams};
use crate::devices;
use crate::notes::{Slot, build_slot_map, midi_to_m8_note};
use crate::output;
use crate::wav::{self, M8_SAMPLE_RATE};
use anyhow::{Result, bail};
use midir::MidiOutputConnection;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Run the `render` command.
pub fn run(args: &RenderArgs) -> Result<()> {
    args.validate()?;

    let out_channels = args.channels.count();
    let slot_map = build_slot_map(args.start_midi, args.end_midi, args.slot_length);

    if args.dry_run {
        print_plan(args, &slot_map, out_channels);
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

    if args.virtual_midi {
        println!("MIDI output : virtual port '{midi_name}'");
    } else {
        println!("MIDI output : {midi_name}");
    }
    println!(
        "Audio input : {audio_name} ({} Hz, {} ch native)",
        capture.native_rate, capture.native_channels
    );
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

    // Single-note test mode.
    if let Some(note) = args.test_note {
        println!("Test note {note} ({})", midi_to_m8_note(note));
        let samples = render_one(
            &mut conn,
            &capture,
            note,
            args,
            args.slot_length,
            note_on,
            note_off,
            &shutdown,
        );
        all_notes_off(&mut conn, ch);
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
        let sample_midis: Vec<u8> = pick_spread(&pool, args.measure_notes as usize)
            .iter()
            .map(|&i| slot_map[i].midi)
            .collect();
        let len = measure_slot_length(
            &mut conn,
            &capture,
            args,
            &sample_midis,
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

    // Rebuild the slot map at the effective slot length.
    let slot_map = build_slot_map(args.start_midi, args.end_midi, effective_slot_length);

    // Which notes to actually record (skip silent ones unless --no-probe).
    let sounding_for_loop: Vec<bool> = if args.no_probe {
        vec![true; slot_map.len()]
    } else {
        probed.clone().unwrap_or_else(|| vec![true; slot_map.len()])
    };

    // Full chain.
    let target_frames = (effective_slot_length * M8_SAMPLE_RATE as f64).round() as usize;
    let slot_samples = target_frames * out_channels as usize;
    let mut full: Vec<f32> = Vec::with_capacity(slot_samples * slot_map.len());
    let mut statuses: Vec<String> = Vec::with_capacity(slot_map.len());
    let mut completed = 0usize;

    // Accurate estimate, now that the (possibly measured) slot length and the
    // set of sounding slots are both known.
    let to_record = sounding_for_loop.iter().filter(|&&s| s).count();
    let est_min = to_record as f64 * effective_slot_length / 60.0;
    println!(
        "Recording {to_record} slots at {effective_slot_length:.1}s each — estimated ~{est_min:.1} min."
    );

    let start = Instant::now();
    for (i, slot) in slot_map.iter().enumerate() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        print!(
            "\r{}  Slot {:>3}/{}  MIDI {:>3}  {:<4}  {}   ",
            progress_bar(i + 1, slot_map.len(), 10),
            slot.slot as usize + 1,
            slot_map.len(),
            slot.midi,
            slot.m8_note,
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
            &mut conn,
            &capture,
            slot.midi,
            args,
            effective_slot_length,
            note_on,
            note_off,
            &shutdown,
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

    all_notes_off(&mut conn, ch);

    if completed == 0 {
        bail!("no slots were rendered");
    }
    if completed < slot_map.len() {
        println!(
            "Interrupted after {completed}/{} slots. Writing partial output.",
            slot_map.len()
        );
    }
    while statuses.len() < slot_map.len() {
        statuses.push("skipped".to_string());
    }

    // Padded chain: every slot, identical length (slot index = MIDI note).
    let padded_path =
        output::numbered_wav_path(&args.output, slot_map.len(), args.note_length, false);
    wav::write_wav(&padded_path, &full, M8_SAMPLE_RATE, out_channels)?;

    // Unpadded copy: drop the leading/trailing runs of silent slots, keeping any
    // interior silent slots. `rendered` marks slots that actually produced sound.
    let unpadded_path = match rendered_bounds(&statuses) {
        Some((f, l)) => {
            let trimmed_count = l - f + 1;
            let path =
                output::numbered_wav_path(&args.output, trimmed_count, args.note_length, true);
            let slice = &full[f * slot_samples..(l + 1) * slot_samples];
            wav::write_wav(&path, slice, M8_SAMPLE_RATE, out_channels)?;
            Some(path)
        }
        // No sounding slots -> nothing to trim to, so skip the unpadded copy.
        _ => None,
    };

    // Sidecars are opt-in; their names match the padded WAV.
    let (csv_path, json_path) = output::sidecar_paths(&padded_path);
    let csv_written = if args.csv {
        output::write_csv_map(&csv_path, &slot_map, args.velocity, &statuses)?;
        Some(csv_path.clone())
    } else {
        None
    };
    let json_written = if args.json {
        let config = build_config(
            args,
            &midi_name,
            &audio_name,
            out_channels,
            effective_slot_length,
            slot_map.len() as u32,
            &padded_path,
            &csv_path,
        );
        output::write_json_config(&json_path, &config)?;
        Some(json_path)
    } else {
        None
    };

    print_summary(
        effective_slot_length,
        out_channels,
        completed,
        &slot_map,
        &padded_path,
        unpadded_path.as_deref(),
        csv_written.as_deref(),
        json_written.as_deref(),
    );
    Ok(())
}

/// Render and post-process a single note into a fixed-length slot.
#[allow(clippy::too_many_arguments)]
fn render_one(
    conn: &mut MidiOutputConnection,
    capture: &Capture,
    midi: u8,
    args: &RenderArgs,
    slot_length: f64,
    note_on: u8,
    note_off: u8,
    shutdown: &Arc<AtomicBool>,
) -> Vec<f32> {
    sleep(Duration::from_millis(args.pre_roll_ms));

    capture.arm();
    let _ = conn.send(&[note_on, midi, args.velocity]);

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
            let _ = conn.send(&[note_off, midi, 0]);
            sent_off = true;
            break;
        }
        if !sent_off && elapsed >= note_off_at {
            let _ = conn.send(&[note_off, midi, 0]);
            sent_off = true;
        }
        let step = chunk.min(slot_ms - elapsed);
        sleep(Duration::from_millis(step));
        elapsed += step;
    }
    if !sent_off {
        let _ = conn.send(&[note_off, midi, 0]);
    }

    let native = capture.disarm_take();
    let mut slot = audio::finalize_slot(
        &native,
        capture.native_rate,
        capture.native_channels,
        args.channels.count(),
        slot_length,
    );
    let fade_frames = (args.fade_ms as f64 * M8_SAMPLE_RATE as f64 / 1000.0) as usize;
    audio::apply_end_fade(&mut slot, args.channels.count(), fade_frames);
    slot
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
    sample_midis: &[u8],
    note_on: u8,
    note_off: u8,
    shutdown: &Arc<AtomicBool>,
) -> f64 {
    println!(
        "Measuring ring-out of {} note(s) to size the slot...",
        sample_midis.len()
    );
    let max_ms = (args.max_slot_length * 1000.0).round() as u64;
    let hold_ms = (args.note_length * 1000.0).round() as u64;
    let tail_window = (capture.native_rate as f64 * 0.15).round() as usize; // 150 ms
    let mut longest = 0.0f64;

    for &midi in sample_midis {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        capture.arm();
        let _ = conn.send(&[note_on, midi, args.velocity]);

        let mut elapsed = 0u64;
        let mut released = false;
        let mut quiet_ms = 0u64;
        let chunk = 50u64;
        while elapsed < max_ms {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !released && elapsed >= hold_ms {
                let _ = conn.send(&[note_off, midi, 0]);
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
            let _ = conn.send(&[note_off, midi, 0]);
        }
        let native = capture.disarm_take();
        let tail = last_sound_seconds(
            &native,
            capture.native_rate,
            capture.native_channels,
            args.decay_threshold,
        );
        println!(
            "  MIDI {midi:>3} {:<4}  tail {tail:.2}s",
            midi_to_m8_note(midi)
        );
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
            "\rProbe {:>3}/{}  MIDI {:>3}  {:<4}   ",
            i + 1,
            slot_map.len(),
            slot.midi,
            slot.m8_note
        );
        let _ = std::io::stdout().flush();

        capture.arm();
        let _ = conn.send(&[note_on, slot.midi, args.velocity]);

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
        let _ = conn.send(&[note_off, slot.midi, 0]);
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
    wav_path: &std::path::Path,
    csv_path: &std::path::Path,
) -> RenderConfig {
    let file_name = |p: &std::path::Path| {
        p.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
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
            m8_slice_hex: "80".to_string(),
        },
        m8: M8Config::standard(),
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

fn print_plan(args: &RenderArgs, slot_map: &[Slot], out_channels: u16) {
    let total = slot_map.len() as f64 * args.slot_length;
    let padded = output::numbered_wav_path(&args.output, slot_map.len(), args.note_length, false);
    let (csv, json) = output::sidecar_paths(&padded);
    let layout = if out_channels == 1 { "mono" } else { "stereo" };
    let midi_output = if args.virtual_midi {
        "virtual port 'midi-sampler-to-m8'".to_string()
    } else {
        args.midi_output.clone().unwrap_or_default()
    };
    println!("DRY RUN — no devices are opened\n");
    println!("  MIDI output : {midi_output}");
    println!("  Audio input : {}", args.audio_input);
    println!(
        "  Notes       : {}..{} ({} slots, slot index = MIDI note)",
        args.start_midi,
        args.end_midi,
        slot_map.len()
    );
    println!("  Velocity    : {}", args.velocity);
    println!("  Channel     : {}", args.channel);
    println!("  Note length : {}s", args.note_length);
    if args.auto_slot_length {
        println!(
            "  Slot length : auto (measured at runtime, max {}s)",
            args.max_slot_length
        );
    } else {
        println!("  Slot length : {}s", args.slot_length);
    }
    println!("  Pre-roll    : {}ms", args.pre_roll_ms);
    if args.no_probe {
        println!("  Probe       : off (record all {} slots)", slot_map.len());
    } else {
        println!(
            "  Probe       : on ({} ms/note, threshold {})",
            args.probe_ms, args.probe_threshold
        );
    }
    println!(
        "  Output WAV  : {} ({layout}, {} Hz, 16-bit)",
        padded.display(),
        M8_SAMPLE_RATE
    );
    println!("  Unpadded WAV: an extra copy with leading/trailing silent slots removed (count set at runtime)");
    if args.csv {
        println!("  CSV map     : {}", csv.display());
    }
    if args.json {
        println!("  JSON config : {}", json.display());
    }
    if args.auto_slot_length {
        println!("  Total time  : determined at runtime (slot length is measured)");
    } else {
        let qualifier = if args.no_probe {
            ""
        } else {
            " max; probe skips silent slots"
        };
        println!(
            "  Total time  : {:.1}s (~{:.1} min{qualifier})",
            total,
            total / 60.0
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn print_summary(
    slot_length: f64,
    out_channels: u16,
    completed: usize,
    slot_map: &[Slot],
    wav_path: &std::path::Path,
    unpadded_path: Option<&std::path::Path>,
    csv_path: Option<&std::path::Path>,
    json_path: Option<&std::path::Path>,
) {
    let layout = if out_channels == 1 { "mono" } else { "stereo" };
    println!("\nDone. Rendered {completed}/{} slots.", slot_map.len());
    println!(
        "  WAV      : {} ({layout}, {} Hz, 16-bit)",
        wav_path.display(),
        M8_SAMPLE_RATE
    );
    if let Some(p) = unpadded_path {
        println!("  Unpadded : {}", p.display());
    }
    if let Some(p) = csv_path {
        println!("  CSV      : {}", p.display());
    }
    if let Some(p) = json_path {
        println!("  JSON     : {}", p.display());
    }
    println!(
        "  Slot length: {:.3}s  ->  {} samples/slot",
        slot_length,
        (slot_length * M8_SAMPLE_RATE as f64).round() as usize
    );
    println!("\nLoad on the M8:");
    println!("  1. Copy the WAV to your M8 SD card.");
    println!("  2. Create a Sampler instrument and load the WAV.");
    println!("  3. Set:  SLICE = 80   PLAY = FWD   START = 00   LEN = FF");
    println!("  Each slice then maps to its MIDI note (slot index = MIDI note).");
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
    fn pick_spread_picks_evenly() {
        assert_eq!(pick_spread(&[10, 20, 30, 40, 50], 3), vec![10, 30, 50]);
        assert_eq!(pick_spread(&[10, 20, 30, 40, 50], 1), vec![30]);
        assert_eq!(pick_spread(&[10, 20], 5), vec![10, 20]); // n >= len
        assert_eq!(pick_spread(&[], 3), Vec::<usize>::new());
    }
}
