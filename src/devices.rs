//! MIDI output and audio input enumeration and matching.

use anyhow::{Result, anyhow, bail};
use cpal::Device;
use cpal::traits::{DeviceTrait, HostTrait};
#[cfg(unix)]
use midir::os::unix::VirtualOutput;
use midir::{MidiOutput, MidiOutputConnection};

const CLIENT_NAME: &str = "midi-sampler-to-m8";

/// Names of all MIDI output ports, in port order.
pub fn midi_output_names() -> Result<Vec<String>> {
    let out = MidiOutput::new(CLIENT_NAME).map_err(|e| anyhow!("opening MIDI: {e}"))?;
    Ok(out
        .ports()
        .iter()
        .map(|p| out.port_name(p).unwrap_or_else(|_| "<unknown>".to_string()))
        .collect())
}

/// Names of all audio input devices, in enumeration order.
pub fn audio_input_names() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    for dev in host.input_devices()? {
        names.push(device_name(&dev));
    }
    Ok(names)
}

fn device_name(dev: &Device) -> String {
    dev.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

/// Print MIDI outputs and audio inputs (the `list-devices` command).
pub fn list_devices() -> Result<()> {
    println!("MIDI outputs:");
    let midi = midi_output_names()?;
    if midi.is_empty() {
        println!("  (none)");
    }
    for (i, n) in midi.iter().enumerate() {
        println!("  [{i}] {n}");
    }

    println!("\nAudio inputs:");
    let audio = audio_input_names()?;
    if audio.is_empty() {
        println!("  (none)");
    }
    for (i, n) in audio.iter().enumerate() {
        println!("  [{i}] {n}");
    }

    println!(
        "\nTip: pass --virtual-midi to create a 'midi-sampler-to-m8' port your\n     instrument can listen to (instead of picking a --midi-output above)."
    );
    Ok(())
}

/// Resolve a device spec (index, exact name, or unique substring) to an index.
///
/// Multiple substring matches are an error and list the candidates.
pub fn resolve_index(spec: &str, names: &[String], kind: &str) -> Result<usize> {
    if let Ok(i) = spec.parse::<usize>() {
        if i < names.len() {
            return Ok(i);
        }
        bail!(
            "{kind} index {i} is out of range (have {} devices)",
            names.len()
        );
    }

    if let Some(i) = names.iter().position(|n| n == spec) {
        return Ok(i);
    }

    let needle = spec.to_lowercase();
    let matches: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();

    match matches.as_slice() {
        [] => bail!(
            "no {kind} matching \"{spec}\". Available:\n{}",
            numbered(names)
        ),
        [one] => Ok(*one),
        many => {
            let listed = many
                .iter()
                .map(|&i| format!("  [{i}] {}", names[i]))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("multiple {kind}s match \"{spec}\":\n{listed}");
        }
    }
}

fn numbered(names: &[String]) -> String {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| format!("  [{i}] {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Open a MIDI output connection matching `spec`, returning the connection and
/// the resolved device name.
pub fn open_midi_output(spec: &str) -> Result<(MidiOutputConnection, String)> {
    let out = MidiOutput::new(CLIENT_NAME).map_err(|e| anyhow!("opening MIDI: {e}"))?;
    let ports = out.ports();
    let names: Vec<String> = ports
        .iter()
        .map(|p| out.port_name(p).unwrap_or_else(|_| "<unknown>".to_string()))
        .collect();
    let idx = resolve_index(spec, &names, "MIDI output")?;
    let name = names[idx].clone();
    let conn = out
        .connect(&ports[idx], CLIENT_NAME)
        .map_err(|e| anyhow!("connecting to MIDI output \"{name}\": {e}"))?;
    Ok((conn, name))
}

/// Create a virtual MIDI output port that other apps can listen to as a MIDI
/// input. Returns the connection and the port name.
#[cfg(unix)]
pub fn open_virtual_midi_output(name: &str) -> Result<(MidiOutputConnection, String)> {
    let out = MidiOutput::new(CLIENT_NAME).map_err(|e| anyhow!("opening MIDI: {e}"))?;
    let conn = out
        .create_virtual(name)
        .map_err(|e| anyhow!("creating virtual MIDI port \"{name}\": {e}"))?;
    Ok((conn, name.to_string()))
}

#[cfg(not(unix))]
pub fn open_virtual_midi_output(_name: &str) -> Result<(MidiOutputConnection, String)> {
    bail!("--virtual-midi is not supported on Windows")
}

/// Open the audio input device matching `spec`, returning the device and its
/// resolved name.
pub fn open_audio_input(spec: &str) -> Result<(Device, String)> {
    let host = cpal::default_host();
    let devices: Vec<Device> = host.input_devices()?.collect();
    let names: Vec<String> = devices.iter().map(device_name).collect();
    let idx = resolve_index(spec, &names, "audio input")?;
    let name = names[idx].clone();
    let device = devices
        .into_iter()
        .nth(idx)
        .ok_or_else(|| anyhow!("audio input index {idx} disappeared"))?;
    Ok((device, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec![
            "BlackHole 2ch".to_string(),
            "MacBook Pro Microphone".to_string(),
            "Aggregate Device".to_string(),
        ]
    }

    #[test]
    fn resolves_by_index() {
        assert_eq!(resolve_index("0", &names(), "audio input").unwrap(), 0);
        assert_eq!(resolve_index("2", &names(), "audio input").unwrap(), 2);
    }

    #[test]
    fn index_out_of_range_errors() {
        assert!(resolve_index("9", &names(), "audio input").is_err());
    }

    #[test]
    fn resolves_by_exact_and_substring() {
        assert_eq!(resolve_index("BlackHole 2ch", &names(), "x").unwrap(), 0);
        assert_eq!(resolve_index("blackhole", &names(), "x").unwrap(), 0);
        assert_eq!(resolve_index("Microphone", &names(), "x").unwrap(), 1);
    }

    #[test]
    fn ambiguous_substring_errors() {
        let n = vec!["Device A".to_string(), "Device B".to_string()];
        assert!(resolve_index("device", &n, "x").is_err());
    }
}
