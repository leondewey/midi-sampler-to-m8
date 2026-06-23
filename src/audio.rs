//! Audio input capture (cpal) plus channel down-mix and 44.1 kHz resampling.
//!
//! A single continuous input stream feeds an armed/disarmed buffer. The render
//! loop arms the buffer just before each Note On, sleeps for the slot duration,
//! then drains the captured native-rate samples and post-processes them into a
//! fixed-length, 44.1 kHz, mono-or-stereo slot.

use crate::wav::M8_SAMPLE_RATE;
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Target output sample rate for the M8.
pub const TARGET_RATE: u32 = M8_SAMPLE_RATE;

/// An open audio input stream with an armable capture buffer.
pub struct Capture {
    // The stream must stay alive for callbacks to fire; it is `!Send` on macOS,
    // so `Capture` always lives on the render thread.
    _stream: Stream,
    recording: Arc<AtomicBool>,
    buffer: Arc<Mutex<Vec<f32>>>,
    /// Native sample rate of the input device.
    pub native_rate: u32,
    /// Native channel count of the input device.
    pub native_channels: u16,
}

impl Capture {
    /// Open the input stream on `device` using its default input config.
    pub fn open(device: Device) -> Result<Self> {
        let supported = device
            .default_input_config()
            .context("getting default input config")?;
        let sample_format = supported.sample_format();
        let native_rate = supported.sample_rate();
        let native_channels = supported.channels();
        let config: StreamConfig = supported.into();

        let recording = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                config,
                make_callback::<f32>(recording.clone(), buffer.clone()),
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                config,
                make_callback::<i16>(recording.clone(), buffer.clone()),
                err_fn,
                None,
            ),
            SampleFormat::I32 => device.build_input_stream(
                config,
                make_callback::<i32>(recording.clone(), buffer.clone()),
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                config,
                make_callback::<u16>(recording.clone(), buffer.clone()),
                err_fn,
                None,
            ),
            other => bail!("unsupported input sample format: {other:?}"),
        }
        .context("building input stream")?;

        stream.play().context("starting input stream")?;

        Ok(Capture {
            _stream: stream,
            recording,
            buffer,
            native_rate,
            native_channels,
        })
    }

    /// Clear the buffer and begin capturing.
    pub fn arm(&self) {
        if let Ok(mut b) = self.buffer.lock() {
            b.clear();
        }
        self.recording.store(true, Ordering::SeqCst);
    }

    /// Peak absolute value of the most recent `frames` frames currently in the
    /// capture buffer (used to detect when a note has gone quiet). Does not
    /// drain the buffer.
    pub fn tail_peak(&self, frames: usize) -> f32 {
        let samples = frames * self.native_channels.max(1) as usize;
        match self.buffer.lock() {
            Ok(b) => {
                let start = b.len().saturating_sub(samples);
                b[start..].iter().fold(0.0f32, |m, &s| m.max(s.abs()))
            }
            Err(_) => 0.0,
        }
    }

    /// Stop capturing and return the captured interleaved native-rate samples.
    pub fn disarm_take(&self) -> Vec<f32> {
        self.recording.store(false, Ordering::SeqCst);
        match self.buffer.lock() {
            Ok(mut b) => std::mem::take(&mut *b),
            Err(_) => Vec::new(),
        }
    }
}

fn err_fn(err: cpal::Error) {
    eprintln!("audio stream error: {err}");
}

/// Build a capture callback for sample type `T`, normalizing to `f32`.
fn make_callback<T>(
    recording: Arc<AtomicBool>,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static
where
    T: SizedSample + Sample + Send + 'static,
    f32: FromSample<T>,
{
    move |data: &[T], _: &cpal::InputCallbackInfo| {
        if recording.load(Ordering::SeqCst)
            && let Ok(mut b) = buffer.lock()
        {
            b.extend(data.iter().map(|&s| f32::from_sample(s)));
        }
    }
}

/// Turn a captured interleaved native-rate buffer into a fixed-length output
/// slot: down-mix/expand to `out_channels`, resample to 44.1 kHz, then force
/// exactly `round(slot_length * 44100)` frames so every slot is identical-size.
///
/// Returns interleaved `f32` samples (`frames * out_channels` long).
pub fn finalize_slot(
    native: &[f32],
    native_rate: u32,
    native_channels: u16,
    out_channels: u16,
    slot_length: f64,
) -> Vec<f32> {
    let nch = native_channels.max(1) as usize;
    let frames = native.len() / nch;

    // Build per-channel output streams at the native rate.
    let out_native: Vec<Vec<f32>> = if out_channels <= 1 {
        // Mono: average all native channels per frame.
        let mut mono = Vec::with_capacity(frames);
        for f in 0..frames {
            let mut sum = 0.0f32;
            for c in 0..nch {
                sum += native[f * nch + c];
            }
            mono.push(sum / nch as f32);
        }
        vec![mono]
    } else {
        // Stereo: take L/R from the first two native channels, or duplicate a
        // mono source into both.
        (0..2usize)
            .map(|target| {
                let src = if nch >= 2 { target } else { 0 };
                (0..frames).map(|f| native[f * nch + src]).collect()
            })
            .collect()
    };

    let target_frames = (slot_length * TARGET_RATE as f64).round() as usize;
    let oc = out_native.len();

    let resampled: Vec<Vec<f32>> = out_native
        .into_iter()
        .map(|ch| {
            let mut r = resample_linear(&ch, native_rate, TARGET_RATE);
            r.resize(target_frames, 0.0); // pad or truncate to exact length
            r
        })
        .collect();

    // Interleave.
    let mut out = vec![0.0f32; target_frames * oc];
    for f in 0..target_frames {
        for (c, channel) in resampled.iter().enumerate() {
            out[f * oc + c] = channel[f];
        }
    }
    out
}

/// Apply a linear fade-out over the last `fade_frames` frames of an interleaved
/// buffer, ramping the gain from ~1.0 down to 0.0 at the final frame. Prevents a
/// click where a slot is cut. No-op when `fade_frames` is 0.
pub fn apply_end_fade(samples: &mut [f32], channels: u16, fade_frames: usize) {
    let ch = channels.max(1) as usize;
    let frames = samples.len() / ch;
    let n = fade_frames.min(frames);
    if n == 0 {
        return;
    }
    let start = frames - n;
    for j in 0..n {
        // gain 1.0 at the first faded frame, 0.0 at the last.
        let gain = (n - 1 - j) as f32 / (n - 1).max(1) as f32;
        let frame = start + j;
        for c in 0..ch {
            samples[frame * ch + c] *= gain;
        }
    }
}

/// Resample a single channel from `from_rate` to `to_rate` with linear
/// interpolation. Adequate for one-shot resampling of captured sampler audio.
pub fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    if from_rate == to_rate {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = input[idx.min(last)];
        let b = input[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_same_rate_is_identity() {
        let input = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(resample_linear(&input, 44100, 44100), input);
    }

    #[test]
    fn resample_preserves_constant_signal() {
        let input = vec![0.5f32; 480];
        let out = resample_linear(&input, 48000, 44100);
        // ratio 0.91875 -> 480 * 0.91875 = 441 frames.
        assert_eq!(out.len(), 441);
        for s in out {
            assert!((s - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn finalize_mono_produces_exact_length() {
        // 480 native frames, 2 channels, 48 kHz -> 0.01s slot at 44.1k = 441 frames.
        let native: Vec<f32> = (0..960).map(|_| 0.25).collect();
        let out = finalize_slot(&native, 48000, 2, 1, 0.01);
        assert_eq!(out.len(), 441); // mono => 1 sample per frame
    }

    #[test]
    fn finalize_stereo_produces_interleaved_exact_length() {
        let native: Vec<f32> = (0..960).map(|_| 0.25).collect();
        let out = finalize_slot(&native, 48000, 2, 2, 0.01);
        assert_eq!(out.len(), 441 * 2); // stereo interleaved
    }

    #[test]
    fn end_fade_ramps_to_zero() {
        // Mono: fade last 4 frames of a constant buffer.
        let mut buf = vec![1.0f32; 8];
        apply_end_fade(&mut buf, 1, 4);
        assert_eq!(buf[0..4], [1.0, 1.0, 1.0, 1.0]); // untouched
        assert_eq!(buf[7], 0.0); // last frame fully faded
        assert!(buf[4] > buf[5] && buf[5] > buf[6] && buf[6] > buf[7]); // descending
    }

    #[test]
    fn end_fade_is_noop_when_zero() {
        let mut buf = vec![0.5f32; 6];
        apply_end_fade(&mut buf, 2, 0);
        assert_eq!(buf, vec![0.5f32; 6]);
    }

    #[test]
    fn end_fade_clamps_to_length() {
        // Stereo, 3 frames; request a longer fade than exists.
        let mut buf = vec![1.0f32; 6];
        apply_end_fade(&mut buf, 2, 100);
        assert_eq!(buf[4], 0.0); // last frame, both channels faded
        assert_eq!(buf[5], 0.0);
    }

    #[test]
    fn finalize_pads_short_capture_with_silence() {
        // Only 10 native frames but a 0.01s slot needs 441 -> padded with zeros.
        let native: Vec<f32> = vec![1.0; 10];
        let out = finalize_slot(&native, 44100, 1, 1, 0.01);
        assert_eq!(out.len(), 441);
        assert_eq!(out[440], 0.0);
    }
}
