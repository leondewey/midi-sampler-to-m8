//! `midi-sampler-to-m8`: autosample any MIDI-playable instrument by sending
//! MIDI notes, recording the real audio output, and packaging the result as a
//! Dirtywave M8 fixed-slice sample-chain WAV.

mod audio;
mod cli;
mod config;
mod devices;
mod notes;
mod output;
mod render;
mod wav;

use clap::Parser;
use cli::{Cli, Command};

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::ListDevices => devices::list_devices(),
        Command::Render(args) => render::run(&args),
    };

    if let Err(err) = result {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
