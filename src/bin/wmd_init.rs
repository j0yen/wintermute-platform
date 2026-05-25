//! `wmd-init` — wintermute supervisor entry point.
//!
//! Iter-2 scope: load the bootstrap env file (or halt cleanly per AC9),
//! log the canonical child list, then exit 0. Real child spawning and the
//! Unix-socket control plane land in iter-3+ once we wrap `pevent` (PRD
//! §2.3 Option A) and answer `wm`'s subcommands over the socket.

use std::process::ExitCode;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use wintermute_platform::{
    canonical_children, resolve_bootstrap_env_path, EnvConfig, NO_BOOTSTRAP_MESSAGE,
};

fn main() -> ExitCode {
    init_tracing();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "wmd-init failed");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn run() -> anyhow::Result<()> {
    let env_path = resolve_bootstrap_env_path();
    info!(env_path = %env_path.display(), "resolving bootstrap config");

    match EnvConfig::load_optional(&env_path)? {
        None => {
            // AC9: literal phrase, exit code 0, wall-clock <2s.
            error!("{NO_BOOTSTRAP_MESSAGE}");
            Ok(())
        }
        Some(cfg) => {
            info!(keys = cfg.keys.len(), "bootstrap env loaded");
            let kids = canonical_children();
            let names: Vec<&str> = kids.iter().map(|c| c.name.as_str()).collect();
            info!(children = ?names, count = kids.len(), "supervised children");
            info!("iter-2 scaffold complete; child spawning lands in iter-3");
            Ok(())
        }
    }
}
