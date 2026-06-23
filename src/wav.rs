//! 16-bit PCM WAV writing via `hound`.

use anyhow::{Context, Result};
use std::path::Path;

/// Target sample rate for all M8 output. The M8 expects 44.1 kHz.
pub const M8_SAMPLE_RATE: u32 = 44_100;

/// Write interleaved `f32` samples (range `[-1.0, 1.0]`) as a 16-bit PCM WAV.
///
/// `samples` must already be interleaved for `channels` (i.e. `L R L R ...`
/// for stereo, or a flat stream for mono). Values are clamped before being
/// scaled to `i16` so out-of-range peaks become hard-limited rather than
/// wrapping around.
pub fn write_wav(path: &Path, samples: &[f32], sample_rate: u32, channels: u16) -> Result<()> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating WAV file {}", path.display()))?;

    for &sample in samples {
        writer
            .write_sample(f32_to_i16(sample))
            .context("writing WAV sample")?;
    }

    writer.finalize().context("finalizing WAV file")?;
    Ok(())
}

/// Convert a normalized `f32` sample to `i16`, clamping to avoid wrap-around.
fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * i16::MAX as f32).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_out_of_range_samples() {
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX);
        assert_eq!(f32_to_i16(0.0), 0);
    }

    #[test]
    fn writes_full_chain_with_expected_shape() {
        let slot_length = 2.0_f64;
        let slots = 128usize;
        let channels = 1u16;
        let samples_per_slot = (slot_length * M8_SAMPLE_RATE as f64).round() as usize;
        let total = samples_per_slot * slots;

        // A fake render buffer of silence is enough to verify the container.
        let buffer = vec![0.0f32; total];

        let dir = std::env::temp_dir();
        let path = dir.join(format!("m8-wav-test-{}.wav", std::process::id()));
        write_wav(&path, &buffer, M8_SAMPLE_RATE, channels).unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);

        let frames = reader.len() as usize / channels as usize;
        assert_eq!(frames, slots * samples_per_slot);
        // Duration equals 128 * slot_length seconds.
        assert_eq!(
            frames,
            (slots as f64 * slot_length * M8_SAMPLE_RATE as f64) as usize
        );

        std::fs::remove_file(&path).ok();
    }
}
