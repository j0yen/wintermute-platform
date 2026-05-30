//! `wintermute-watchdog` — unit recovery daemon.
//!
//! Monitors the systemd user manager for wintermute units in `failed` state,
//! clears each via `systemctl reset-failed`, and restarts it. Uses capped
//! exponential backoff to avoid hot-looping a genuinely-broken unit; gives up
//! after [`GIVE_UP_AFTER`] consecutive failures and emits a `wm.health.*`
//! event.
//!
//! # Backoff policy
//!
//! | Attempt | Min wait before next attempt |
//! |---------|------------------------------|
//! | 1st     | 2 s (initial)                |
//! | 2nd     | 4 s                          |
//! | 3rd     | 8 s                          |
//! | …       | doubles each time …          |
//! | ≥9th    | 300 s (ceiling)              |
//!
//! After `GIVE_UP_AFTER` (8) consecutive failed recoveries the unit is
//! blacklisted and a `wm.health.unit-failed` log line is emitted.
//!
//! # `StartLimit` tuning rationale
//!
//! Fleet units ship with `StartLimitIntervalSec=0` so that a transient flap
//! never permanently bricks a unit in systemd's start-limit-hit state before
//! this watchdog can intervene. The watchdog is the fleet-wide backstop for
//! recovery; disabling systemd's own start limit on watched units is safe
//! because the watchdog enforces its own capped backoff ceiling of
//! `MAX_BACKOFF_SECS` (300 s).

use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use wintermute_platform::watchdog::{
    GIVE_UP_AFTER, MAX_BACKOFF_SECS, WATCHDOG_UNIT_NAME, WatchdogEngine, SystemctlReal,
};

/// How often the watchdog polls for failed units.
const POLL_INTERVAL_SECS: u64 = 5;

/// `wintermute-watchdog` — systemd unit recovery daemon.
///
/// Detects wintermute units in `failed` state, clears them, and restarts them
/// with capped exponential backoff. After `GIVE_UP_AFTER` consecutive failed
/// recoveries for a unit, emits a wm.health.unit-failed event and stops
/// retrying that unit.
///
/// Backoff policy: 2 s → 4 s → 8 s … capped at `MAX_BACKOFF_SECS` (300 s).
/// Give-up threshold: `GIVE_UP_AFTER` (8) consecutive failures.
/// `StartLimit` tuning: fleet units ship `StartLimitIntervalSec=0` so a flap
/// never permanently bricks a unit before the watchdog can act.
/// Self-exclusion: `wintermute-watchdog.service` is never in the recovery scope.
#[derive(Debug, Parser)]
#[command(
    name = "wintermute-watchdog",
    version,
    about = "Monitors wintermute units for failed state and recovers them with capped exponential backoff."
)]
struct Cli {
    /// Poll interval in seconds (default: 5).
    #[arg(long, default_value_t = POLL_INTERVAL_SECS, value_name = "SECS")]
    poll_interval: u64,

    /// Dry-run: detect and log failed units but do not call systemctl.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> std::process::ExitCode {
    init_tracing();
    let cli = Cli::parse();

    info!(
        poll_interval_secs = cli.poll_interval,
        dry_run = cli.dry_run,
        give_up_after = GIVE_UP_AFTER,
        max_backoff_secs = MAX_BACKOFF_SECS,
        watchdog_unit = WATCHDOG_UNIT_NAME,
        "wintermute-watchdog starting"
    );

    if cli.dry_run {
        info!("dry-run mode: no systemctl calls will be made");
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "failed to build tokio runtime");
            return std::process::ExitCode::FAILURE;
        }
    };

    match rt.block_on(run(cli)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "wintermute-watchdog exited with error");
            std::process::ExitCode::FAILURE
        }
    }
}

#[allow(clippy::redundant_pub_crate)] // tokio::select! macro emits pub(crate) items in private ctx
async fn run(cli: Cli) -> Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let interval = Duration::from_secs(cli.poll_interval);
    let mut engine = WatchdogEngine::new(SystemctlReal);

    info!(
        poll_interval_secs = cli.poll_interval,
        "watchdog loop started"
    );

    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {
                let now = Instant::now();
                if cli.dry_run {
                    // In dry-run mode we use a no-op backend that only logs.
                    let mut dry_engine = WatchdogEngine::new(DryRunBackend);
                    match dry_engine.tick(now) {
                        Ok(events) => {
                            for ev in events {
                                warn!(event = %ev.format(), "wm.health [dry-run]");
                            }
                        }
                        Err(e) => error!(error = %e, "watchdog tick error [dry-run]"),
                    }
                } else {
                    match engine.tick(now) {
                        Ok(events) => {
                            for ev in events {
                                warn!(event = %ev.format(), "wm.health: gave up on unit");
                            }
                        }
                        Err(e) => error!(error = %e, "watchdog tick error"),
                    }
                }
            }
            Some(()) = sigterm.recv() => {
                info!("SIGTERM received, shutting down");
                break;
            }
            Some(()) = sigint.recv() => {
                info!("SIGINT received, shutting down");
                break;
            }
        }
    }
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

// ── Dry-run backend ──────────────────────────────────────────────────────

struct DryRunBackend;

impl wintermute_platform::watchdog::SystemctlBackend for DryRunBackend {
    fn list_failed_units(
        &self,
    ) -> Result<Vec<String>, wintermute_platform::watchdog::WatchdogError> {
        use wintermute_platform::watchdog::WatchdogError;
        let out = std::process::Command::new("systemctl")
            .args(["--user", "--state=failed", "--no-legend", "--plain"])
            .output()
            .map_err(WatchdogError::Io)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut units = Vec::new();
        for line in text.lines() {
            if let Some(name) = line.split_whitespace().next() {
                units.push(name.to_string());
                info!(unit = name, "dry-run: would reset-failed and start");
            }
        }
        Ok(units)
    }

    fn reset_failed(
        &self,
        unit: &str,
    ) -> Result<(), wintermute_platform::watchdog::WatchdogError> {
        info!(unit, "dry-run: would reset-failed");
        Ok(())
    }

    fn start_unit(&self, unit: &str) -> Result<(), wintermute_platform::watchdog::WatchdogError> {
        info!(unit, "dry-run: would start");
        Ok(())
    }

    fn unit_is_failed(
        &self,
        unit: &str,
    ) -> Result<bool, wintermute_platform::watchdog::WatchdogError> {
        info!(unit, "dry-run: would check is-failed");
        Ok(false)
    }
}
