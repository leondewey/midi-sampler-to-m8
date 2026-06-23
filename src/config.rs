//! Serde structures for the `_render.json` config written next to each WAV.
//!
//! Field names use `camelCase` to match the documented output format exactly.

use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderConfig {
    pub mode: String,
    pub output_wav: String,
    pub output_map: String,
    pub format: FormatConfig,
    pub midi: MidiConfig,
    pub audio: AudioConfig,
    pub render: RenderParams,
    pub m8: M8Config,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatConfig {
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MidiConfig {
    pub output: String,
    pub channel: u8,
    pub start_midi: u8,
    pub end_midi: u8,
    pub velocity: u8,
}

#[derive(Debug, Serialize)]
pub struct AudioConfig {
    pub input: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderParams {
    pub note_length_seconds: f64,
    pub slot_length_seconds: f64,
    pub pre_roll_ms: u64,
    pub slice_count: u32,
    pub m8_slice_hex: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct M8Config {
    pub load_into: String,
    pub slice: String,
    pub play: String,
    pub start: String,
    pub len: String,
}

impl M8Config {
    /// The fixed M8 settings the user applies after loading the WAV.
    pub fn standard() -> Self {
        M8Config {
            load_into: "Sampler".to_string(),
            slice: "80".to_string(),
            play: "FWD".to_string(),
            start: "00".to_string(),
            len: "FF".to_string(),
        }
    }
}

// `RenderConfig` itself wants the top-level keys in camelCase too.
impl RenderConfig {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}
