//! `wmd-init` — wintermute supervisor entry point.
//!
//! Iter-5 scope: load bootstrap env (or halt per AC9), bind the UDS
//! control plane, and — when `--spawn` is supplied — start the
//! canonical five children via `PeventSpawner` and run a 1 s
//! observation loop that flips `Running → Exited` on PID loss.
//! Restart-on-exit policy lives in iter-6.

use std::process::ExitCode;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use wintermute_platform::spawn::PeventSpawner;
use wintermute_platform::supervisor::Supervisor;
use wintermute_platform::{
    EnvConfig, NO_BOOTSTRAP_MESSAGE, canonical_children, resolve_bootstrap_env_path,
    socket_path,
};

/// Interval between exit-observation passes (PRD §2.3 calls for 1 s).
const OBSERVE_INTERVAL: Duration = Duration::from_millis(1_000);

#[derive(Debug, Default, Clone, Copy)]
struct CliArgs {
    spawn: bool,
}

fn parse_cli<I: IntoIterator<Item = String>>(args: I) -> Result<CliArgs, String> {
    let mut cli = CliArgs::default();
    for a in args.into_iter().skip(1) {
        match a.as_str() {
            "--spawn" => cli.spawn = true,
            "-h" | "--help" => {
                return Err(
                    "usage: wmd-init [--spawn]\n  --spawn   start the canonical children and run the exit-observer".into(),
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(cli)
}

fn main() -> ExitCode {
    init_tracing();
    let cli = match parse_cli(std::env::args()) {
        Ok(c) => c,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "build tokio runtime");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "wmd-init failed");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[allow(clippy::redundant_pub_crate)] // tokio::select! macro expansion false positive
async fn run(cli: CliArgs) -> anyhow::Result<()> {
    let env_path = resolve_bootstrap_env_path();
    info!(env_path = %env_path.display(), "resolving bootstrap config");

    let Some(cfg) = EnvConfig::load_optional(&env_path)? else {
        // AC9: literal phrase, exit code 0, wall-clock <2 s.
        error!("{NO_BOOTSTRAP_MESSAGE}");
        return Ok(());
    };
    info!(keys = cfg.keys.len(), "bootstrap env loaded");

    let kids = canonical_children();
    let names: Vec<&str> = kids.iter().map(|c| c.name.as_str()).collect();
    info!(children = ?names, count = kids.len(), spawn = cli.spawn, "supervised children");

    let sup = Supervisor::new();
    let sock = socket_path();
    info!(socket = %sock.display(), "binding control plane");

    let serve_task = {
        let sup = sup.clone();
        let sock = sock.clone();
        tokio::spawn(async move { sup.serve(&sock).await })
    };

    let observer_task = if cli.spawn {
        let spawner = PeventSpawner::default();
        if let Err(e) = sup.start_all(&spawner).await {
            warn!(error = %e, "start_all failed; observer will still run");
        }
        let sup = sup.clone();
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(OBSERVE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                match sup.observe_exits(&spawner).await {
                    Ok(transitions) if !transitions.is_empty() => {
                        for name in transitions {
                            info!(child = %name, "child exited");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "observe_exits failed"),
                }
            }
        }))
    } else {
        None
    };

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        () = wait_ctrl_c() => info!("SIGINT — shutting down"),
        _ = term.recv() => info!("SIGTERM — shutting down"),
        res = serve_task => {
            match res {
                Ok(Ok(())) => info!("server task exited"),
                Ok(Err(e)) => error!(error = %e, "server task error"),
                Err(e) => error!(error = %e, "server task join error"),
            }
        }
    }

    if let Some(h) = observer_task {
        h.abort();
    }

    let _ = tokio::fs::remove_file(&sock).await;
    Ok(())
}

async fn wait_ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!(error = %e, "ctrl_c handler");
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::{CliArgs, parse_cli};

    #[test]
    fn parse_cli_default_is_no_spawn() {
        let CliArgs { spawn } = parse_cli(vec!["wmd-init".into()]).expect("parse");
        assert!(!spawn);
    }

    #[test]
    fn parse_cli_spawn_flag_sets_spawn() {
        let CliArgs { spawn } = parse_cli(vec!["wmd-init".into(), "--spawn".into()]).expect("parse");
        assert!(spawn);
    }

    #[test]
    fn parse_cli_unknown_arg_errors() {
        let err = parse_cli(vec!["wmd-init".into(), "--whatever".into()]).unwrap_err();
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn parse_cli_help_short_and_long_both_error_out() {
        for flag in ["--help", "-h"] {
            let err = parse_cli(vec!["wmd-init".into(), flag.into()]).unwrap_err();
            assert!(err.contains("usage"));
        }
    }
}
