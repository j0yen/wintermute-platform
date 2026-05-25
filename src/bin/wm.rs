//! `wm` — wintermute supervisor CLI.
//!
//! Iter-2 scope: clap subcommand surface for `status`, `mute`, `unmute`,
//! `restart`, `logs`, `version`, `say`. Each non-`version` subcommand
//! announces itself and exits 0; the Unix-socket protocol to `wmd-init`
//! lands in iter-3+.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};

const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(name = "wm", version = CRATE_VERSION, about = "wintermute supervisor CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show per-child status (uptime, restart count, last event).
    Status {
        /// Emit JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Mute active TTS and suspend wake handling.
    Mute,
    /// Resume from mute.
    Unmute,
    /// Restart all children or one named child.
    Restart {
        /// Child name (omit for all).
        child: Option<String>,
    },
    /// Tail the per-child stderr log.
    Logs {
        /// Child name (e.g. `wm-audio`).
        child: String,
        /// How many trailing lines to print.
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },
    /// Show this binary's version (the supervised children report their own).
    Version,
    /// Speak `text` immediately via `wm-tts`.
    Say {
        /// One or more words; joined with a single space.
        text: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wm: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Status { json } => {
            eprintln!(
                "wm status (json={json}): socket protocol pending — iter-3 wires wmd-init"
            );
        }
        Cmd::Mute => eprintln!("wm mute: pending (iter-3)"),
        Cmd::Unmute => eprintln!("wm unmute: pending (iter-3)"),
        Cmd::Restart { child } => match child {
            Some(c) => eprintln!("wm restart {c}: pending (iter-3)"),
            None => eprintln!("wm restart (all): pending (iter-3)"),
        },
        Cmd::Logs { child, tail } => {
            eprintln!("wm logs {child} --tail {tail}: pending (iter-3)");
        }
        Cmd::Version => {
            println!("wm {CRATE_VERSION}");
        }
        Cmd::Say { text } => {
            let utterance = text.join(" ");
            eprintln!("wm say {utterance:?}: pending (iter-3)");
        }
    }
    Ok(())
}
