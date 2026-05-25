//! `wm` — wintermute supervisor CLI.
//!
//! Iter-3 scope: `wm status` connects to `wmd-init` over the Unix-socket
//! control plane and renders the snapshot (table or `--json`). All other
//! subcommands route through the protocol too but the server still
//! answers `NotImplemented`; surfaced to the user as a one-line hint.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::error;

use wintermute_platform::protocol::{ChildStatus, Request, Response, SupervisorView};
use wintermute_platform::socket_path;
use wintermute_platform::supervisor::client_roundtrip;

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
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wm: build runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "wm failed");
            eprintln!("wm: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Version => {
            println!("wm {CRATE_VERSION}");
            return Ok(());
        }
        Cmd::Status { json } => {
            let resp = client_roundtrip(&socket_path(), &Request::Status).await?;
            render_status_response(&resp, json);
        }
        Cmd::Mute => one_shot_command(Request::Mute).await?,
        Cmd::Unmute => one_shot_command(Request::Unmute).await?,
        Cmd::Restart { child } => one_shot_command(Request::Restart { child }).await?,
        Cmd::Logs { child, tail } => one_shot_command(Request::Logs { child, tail }).await?,
        Cmd::Say { text } => {
            let utterance = text.join(" ");
            one_shot_command(Request::Say { text: utterance }).await?;
        }
    }
    Ok(())
}

async fn one_shot_command(req: Request) -> anyhow::Result<()> {
    let resp = client_roundtrip(&socket_path(), &req).await?;
    match resp {
        Response::Ack => println!("ok"),
        Response::NotImplemented { op } => {
            eprintln!("wm: {op}: server has no handler yet (iter-4)");
        }
        Response::Error { message } => {
            eprintln!("wm: server error: {message}");
        }
        Response::Status { .. } => {
            eprintln!("wm: unexpected status payload for command");
        }
    }
    Ok(())
}

fn render_status_response(resp: &Response, json: bool) {
    match resp {
        Response::Status { view } => render_status(view, json),
        Response::Error { message } => eprintln!("wm: server error: {message}"),
        Response::Ack | Response::NotImplemented { .. } => {
            eprintln!("wm: unexpected response for status request");
        }
    }
}

fn render_status(view: &SupervisorView, json: bool) {
    if json {
        match serde_json::to_string_pretty(view) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("wm: serialize status: {e}"),
        }
        return;
    }
    println!("{:<10}  {:<8}  {:>8}  {:>9}  last_event", "CHILD", "STATUS", "UPTIME", "RESTARTS");
    for c in &view.children {
        let last = c.last_event.as_deref().unwrap_or("-");
        println!(
            "{:<10}  {:<8}  {:>8}  {:>9}  {}",
            c.name,
            status_label(c.status),
            format_uptime(c.uptime_secs),
            c.restart_count,
            last,
        );
    }
}

const fn status_label(s: ChildStatus) -> &'static str {
    match s {
        ChildStatus::Pending => "pending",
        ChildStatus::Starting => "starting",
        ChildStatus::Running => "running",
        ChildStatus::Backoff => "backoff",
        ChildStatus::Halted => "halted",
    }
}

fn format_uptime(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let rem = secs % 60;
    if mins < 60 {
        return format!("{mins}m{rem:02}s");
    }
    let hours = mins / 60;
    let mrem = mins % 60;
    format!("{hours}h{mrem:02}m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wintermute_platform::protocol::ChildState;

    #[test]
    fn status_labels_cover_every_variant() {
        assert_eq!(status_label(ChildStatus::Pending), "pending");
        assert_eq!(status_label(ChildStatus::Starting), "starting");
        assert_eq!(status_label(ChildStatus::Running), "running");
        assert_eq!(status_label(ChildStatus::Backoff), "backoff");
        assert_eq!(status_label(ChildStatus::Halted), "halted");
    }

    #[test]
    fn format_uptime_buckets() {
        assert_eq!(format_uptime(0), "-");
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(65), "1m05s");
        assert_eq!(format_uptime(3725), "1h02m");
    }

    #[test]
    fn render_status_response_handles_status_variant() {
        let view = SupervisorView {
            protocol_version: 1,
            children: vec![ChildState {
                name: "wm-audio".into(),
                status: ChildStatus::Pending,
                uptime_secs: 0,
                restart_count: 0,
                last_event: None,
                child_pid: None,
            }],
        };
        // Smoke: should not panic. (Real assertion is the run-time check
        // that the function recognises every Response variant.)
        render_status_response(&Response::Status { view }, false);
        render_status_response(&Response::Ack, false);
        render_status_response(&Response::Error { message: "x".into() }, false);
        render_status_response(&Response::NotImplemented { op: "mute".into() }, false);
    }
}
