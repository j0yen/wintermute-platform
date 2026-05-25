//! `wmd-init` — wintermute supervisor entry point.
//!
//! Iter-3 scope: load the bootstrap env file (or halt cleanly per AC9),
//! bind the UDS control plane at `socket_path()`, and serve
//! `Request::Status` from an in-memory snapshot. SIGTERM / Ctrl-C exit
//! gracefully. Real child spawning lands in iter-4 once we wrap
//! `pevent` (PRD §2.3 Option A).

use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use wintermute_platform::supervisor::Supervisor;
use wintermute_platform::{
    canonical_children, resolve_bootstrap_env_path, socket_path, EnvConfig,
    NO_BOOTSTRAP_MESSAGE,
};

fn main() -> ExitCode {
    init_tracing();
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
    match rt.block_on(run()) {
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

#[allow(clippy::redundant_pub_crate)] // tokio::select! macro expansion false positive
async fn run() -> anyhow::Result<()> {
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
    info!(children = ?names, count = kids.len(), "supervised children");

    let sup = Supervisor::new();
    let sock = socket_path();
    info!(socket = %sock.display(), "binding control plane");

    let serve_task = {
        let sup = sup.clone();
        let sock = sock.clone();
        tokio::spawn(async move { sup.serve(&sock).await })
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

    let _ = tokio::fs::remove_file(&sock).await;
    Ok(())
}

async fn wait_ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!(error = %e, "ctrl_c handler");
    }
}
