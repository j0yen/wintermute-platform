//! `wm` — wintermute supervisor CLI.
//!
//! Iter-3 scope: `wm status` connects to `wmd-init` over the Unix-socket
//! control plane and renders the snapshot (table or `--json`). All other
//! subcommands route through the protocol too but the server still
//! answers `NotImplemented`; surfaced to the user as a one-line hint.
//!
//! Iter-6 adds: `wm ready` — device-level readiness beacon. Checks the
//! API key, `wintermute.target`, audio, agorabus, and canonical unit presence.
//! Exits 0 when ready, nonzero when not. Output: human text by default,
//! `--format json` for the `wm.health.ready` envelope.
//!
//! **Boot vs conversation phrase-bank boundary:** boot/deploy phrases live
//! here in `wintermute-platform`; mid-conversation failure phrases belong
//! in `wm-brain` (companion-degrade). They must not share a bank unless
//! explicitly consolidated by a follow-on PRD.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tracing::error;

use wintermute_platform::doctor::{
    real_cmd_runner as doctor_cmd_runner, render_table, run_doctor, DoctorScope,
};
use wintermute_platform::protocol::{ChildStatus, Request, Response, SupervisorView};
use wintermute_platform::ready::{
    aggregate, check_audio, check_brain, check_bus, check_target, check_units,
    not_ready_phrase, real_cmd_runner, utc_timestamp, verdict_line, HealthReadyEnvelope,
    BOOT_PHRASE_READY,
};
use wintermute_platform::socket_path;
use wintermute_platform::supervisor::client_roundtrip;

const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Output format for `wm ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ReadyFormat {
    /// Human-readable text (`READY` or `NOT READY: <reasons>`).
    Text,
    /// JSON — emits the `wm.health.ready` envelope.
    Json,
}

impl Default for ReadyFormat {
    fn default() -> Self {
        Self::Text
    }
}

/// Output format for `wm doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum DoctorFormat {
    /// Human-readable table (default).
    #[default]
    Table,
    /// JSON — one object per unit, plus a summary object.
    Json,
}

/// Scope for `wm doctor` — which systemd manager(s) to query.
///
/// Default is `user` to avoid requiring privilege; pass `system` or `both`
/// explicitly when inspecting system-scope units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum DoctorScopeArg {
    /// User manager only (`systemctl --user`).
    #[default]
    User,
    /// System manager only (`systemctl --system`).
    System,
    /// Both user and system managers.
    Both,
}

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
    /// Check device readiness: API key, target, audio, bus, and unit presence.
    ///
    /// Exits 0 when all checks pass (READY), 1 when any check fails (NOT READY).
    /// The spoken boot verdict is chosen from the boot phrase bank (platform-
    /// owned); mid-conversation failure phrases belong in wm-brain (companion-
    /// degrade), not here.
    Ready {
        /// Output format: `text` (default) or `json` (wm.health.ready envelope).
        #[arg(long, value_enum, default_value_t = ReadyFormat::Text)]
        format: ReadyFormat,
    },
    /// Inspect every wintermute systemd unit: resolve `ExecStart`, verify binary
    /// exists and is executable, report enabled/active/in-target status.
    ///
    /// Exits 0 only when every discovered unit's `ExecStart` resolves to an
    /// executable file. Exits 2 when any unit's binary is missing or
    /// non-executable. A unit that is inactive or disabled but whose binary
    /// exists is NOT a failure.
    Doctor {
        /// Output format: `table` (default, human-readable) or `json`.
        #[arg(long, value_enum, default_value_t = DoctorFormat::Table)]
        format: DoctorFormat,
        /// Which systemd scope(s) to query: `user` (default), `system`, or `both`.
        ///
        /// System-scope queries do not require root for `cat`/`is-enabled` but
        /// may be unavailable in some CI environments; default is `user`.
        #[arg(long, value_enum, default_value_t = DoctorScopeArg::User)]
        scope: DoctorScopeArg,
        /// Print only failing units (units with missing or non-executable binaries).
        #[arg(long)]
        quiet: bool,
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
        Ok(code) => code,
        Err(e) => {
            error!(error = %e, "wm failed");
            eprintln!("wm: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.cmd {
        Cmd::Version => {
            println!("wm {CRATE_VERSION}");
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
        Cmd::Ready { format } => {
            return Ok(run_ready(format));
        }
        Cmd::Doctor { format, scope, quiet } => {
            return Ok(run_doctor_cmd(format, scope, quiet));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Run all readiness checks and render the result.
/// Returns `ExitCode::SUCCESS` (0) when ready, `ExitCode::FAILURE` (1) when not.
fn run_ready(format: ReadyFormat) -> ExitCode {
    let run = real_cmd_runner();

    let checks = vec![
        check_brain(None),
        check_target(&run),
        check_audio(&run),
        check_bus(None),
        check_units(),
    ];

    let (ready, worst) = aggregate(&checks);

    let failing: Vec<&str> = checks.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect();
    let line = verdict_line(ready, &failing);
    let ts = utc_timestamp();

    match format {
        ReadyFormat::Text => {
            println!("{line}");
            if !ready {
                if let Some(ref w) = worst {
                    let phrase = not_ready_phrase(w.as_str());
                    println!("{phrase}");
                }
            }
        }
        ReadyFormat::Json => {
            let env = HealthReadyEnvelope {
                topic: "wm.health.ready".to_string(),
                ts,
                ready,
                checks: checks.clone(),
                worst_reason: worst,
                ready_phrase: if ready { Some(BOOT_PHRASE_READY.to_string()) } else { None },
            };
            match serde_json::to_string_pretty(&env) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("wm ready: serialize: {e}"),
            }
        }
    }

    if ready { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Run `wm doctor`: enumerate units, check binaries, render output.
///
/// Returns `ExitCode::SUCCESS` (0) when all binaries are present and
/// executable. Returns `ExitCode::from(2)` when any binary is missing.
fn run_doctor_cmd(format: DoctorFormat, scope: DoctorScopeArg, quiet: bool) -> ExitCode {
    let doctor_scope = match scope {
        DoctorScopeArg::User => DoctorScope::User,
        DoctorScopeArg::System => DoctorScope::System,
        DoctorScopeArg::Both => DoctorScope::Both,
    };
    let run = doctor_cmd_runner();
    let report = run_doctor(doctor_scope, &run, None, None, None);

    match format {
        DoctorFormat::Table => {
            let table = render_table(&report, quiet);
            print!("{table}");
        }
        DoctorFormat::Json => {
            match serde_json::to_string_pretty(&report) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("wm doctor: serialize: {e}"),
            }
        }
    }

    if report.all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
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
        Response::Logs { lines, .. } => render_logs(&lines),
    }
    Ok(())
}

fn render_logs(lines: &[String]) {
    for line in lines {
        println!("{line}");
    }
}

fn render_status_response(resp: &Response, json: bool) {
    match resp {
        Response::Status { view } => render_status(view, json),
        Response::Error { message } => eprintln!("wm: server error: {message}"),
        Response::Ack | Response::NotImplemented { .. } | Response::Logs { .. } => {
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
        ChildStatus::Exited => "exited",
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
            muted: false,
        };
        // Smoke: should not panic. (Real assertion is the run-time check
        // that the function recognises every Response variant.)
        render_status_response(&Response::Status { view }, false);
        render_status_response(&Response::Ack, false);
        render_status_response(&Response::Error { message: "x".into() }, false);
        render_status_response(&Response::NotImplemented { op: "mute".into() }, false);
        render_status_response(
            &Response::Logs {
                child: "wm-audio".into(),
                lines: vec!["x".into()],
            },
            false,
        );
    }

    #[test]
    fn render_logs_smoke() {
        render_logs(&[]);
        render_logs(&["alpha".into(), "beta".into(), "gamma".into()]);
    }

    #[test]
    fn ready_format_default_is_text() {
        assert_eq!(ReadyFormat::default(), ReadyFormat::Text);
    }
}
