//! Acceptance tests for `wm doctor` — closes AC2 (live fleet discovery)
//! and AC5 (read-only) from PRD-wintermute-fleet-install-doctor.
//!
//! The deterministic in-process portions of `wm doctor` — ExecStart
//! parsing, `%h`/`%u`/`%U` specifier expansion, executable detection,
//! in-target membership, exit-code mapping (AC1, AC3, AC4) — are
//! covered by the lib unit tests in `src/doctor.rs`. AC6 (`--help`
//! documents `doctor`) is covered by clap's generated help in
//! `src/bin/wm.rs`. What lives here is the contract that:
//!
//!   * AC2 — `run_doctor` against the *live* systemd-user manager on a
//!     fleet host discovers >= 6 wintermute units with no hard-coded
//!     list, and every unit carries the documented JSON fields.
//!   * AC5 — `run_doctor` is strictly read-only: it issues no mutating
//!     `systemctl` verb (start/stop/restart/reload/reset-failed/
//!     enable/disable/kill/daemon-reload/...) and writes no files.
//!
//! AC5 is fully deterministic and runs everywhere (it injects a
//! recording `CmdRunner`, so it needs no live systemd). AC2 measures a
//! real host and is therefore `WM_PLATFORM_HARDWARE_SMOKE=1`-gated, in
//! the same convention as `hardware_acs.rs`, so an accidental
//! `--ignored` run on a bare CI box cannot false-fail.

#![allow(clippy::expect_used, clippy::panic, clippy::missing_panics_doc)]

use std::env;
use std::sync::{Arc, Mutex};

use wintermute_platform::doctor::{run_doctor, CmdRunner, DoctorScope};

// ---------------------------------------------------------------------------
// AC5 — read-only (deterministic; runs everywhere)
// ---------------------------------------------------------------------------

/// Verbs that mutate the systemd manager or unit state. If `wm doctor`
/// ever issues one of these it has violated PRD §2.3 ("No system
/// mutation"). The list is deliberately broad.
const MUTATING_VERBS: &[&str] = &[
    "start",
    "stop",
    "restart",
    "try-restart",
    "reload",
    "reload-or-restart",
    "reset-failed",
    "enable",
    "disable",
    "mask",
    "unmask",
    "kill",
    "daemon-reload",
    "daemon-reexec",
    "set-property",
    "edit",
    "link",
    "revert",
    "preset",
    "isolate",
];

/// Read-only verbs `wm doctor` is permitted to issue.
const ALLOWED_VERBS: &[&str] = &[
    "cat",
    "is-enabled",
    "is-active",
    "list-units",
    "list-unit-files",
    "show",
];

/// Build a `CmdRunner` that records every invocation and returns empty
/// output (so the probe code runs end-to-end without a live systemctl).
fn recording_runner(log: Arc<Mutex<Vec<(String, Vec<String>)>>>) -> CmdRunner {
    Box::new(move |cmd: &str, args: &[&str]| {
        log.lock()
            .expect("log mutex")
            .push((cmd.to_string(), args.iter().map(|s| (*s).to_string()).collect()));
        // Empty output: discover_units finds nothing, probes degrade
        // gracefully. We only care about *what was asked*, not results.
        (String::new(), true)
    })
}

#[test]
fn ac5_doctor_issues_no_mutating_systemctl_verb() {
    let log: Arc<Mutex<Vec<(String, Vec<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let run = recording_runner(Arc::clone(&log));

    // Exercise every scope so we cover --user and --system call paths.
    for scope in [DoctorScope::User, DoctorScope::System, DoctorScope::Both] {
        let _ = run_doctor(scope, &run, Some("/home/jsy"), Some("jsy"), Some(1000));
    }

    let calls = log.lock().expect("log mutex");
    assert!(!calls.is_empty(), "AC5: doctor issued no commands at all — runner not wired?");

    for (cmd, args) in calls.iter() {
        // Only systemctl is ever shelled out to.
        assert_eq!(
            cmd, "systemctl",
            "AC5: doctor shelled out to a non-systemctl command: {cmd} {args:?}"
        );

        // Find the subcommand verb: the first arg that is not a scope flag.
        let verb = args
            .iter()
            .find(|a| *a != "--user" && *a != "--system")
            .map(String::as_str)
            .unwrap_or("");

        assert!(
            !MUTATING_VERBS.contains(&verb),
            "AC5: doctor issued a MUTATING systemctl verb '{verb}' (full args: {args:?}). \
             `wm doctor` must be strictly read-only."
        );
        assert!(
            ALLOWED_VERBS.contains(&verb),
            "AC5: doctor issued an unexpected systemctl verb '{verb}' (full args: {args:?}). \
             Only read-only verbs {ALLOWED_VERBS:?} are permitted."
        );
    }
}

#[test]
fn ac5_doctor_writes_no_files() {
    // Snapshot a scratch dir's directory listing + the HOME the probe
    // sees, run doctor, and assert nothing was created. doctor never
    // takes a path to write, but this guards against regressions that
    // add caching / logging side effects.
    let scratch = tempfile::tempdir().expect("tempdir");
    let before: Vec<_> = std::fs::read_dir(scratch.path())
        .expect("read scratch")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();

    let log: Arc<Mutex<Vec<(String, Vec<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let run = recording_runner(log);
    let _ = run_doctor(
        DoctorScope::Both,
        &run,
        Some(scratch.path().to_str().expect("utf8 path")),
        Some("jsy"),
        Some(1000),
    );

    let after: Vec<_> = std::fs::read_dir(scratch.path())
        .expect("read scratch")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();

    assert_eq!(
        before, after,
        "AC5: doctor created or removed files under the home/scratch dir — it must not mutate the filesystem"
    );
}

// ---------------------------------------------------------------------------
// AC2 — live fleet discovery (hardware-witness-gated)
// ---------------------------------------------------------------------------

/// The minimum wintermute units a fleet host must expose (PRD §1 / AC2):
/// wm-audio, wm-dialog, wm-stt, wm-tts, wmd, wmd-init.
const MIN_FLEET_UNITS: usize = 6;

fn require_fleet_witness() -> bool {
    env::var("WM_PLATFORM_HARDWARE_SMOKE").unwrap_or_default() == "1"
}

/// AC2: on a live fleet host, `run_doctor` against the real systemd-user
/// manager discovers >= 6 wintermute units (no hard-coded list) and each
/// unit serializes to the documented JSON object shape.
///
/// Gated on `WM_PLATFORM_HARDWARE_SMOKE=1`. Run on the fleet host with:
///
///     WM_PLATFORM_HARDWARE_SMOKE=1 \
///       cargo test --release --test acceptance_doctor ac2 -- --nocapture
#[test]
fn ac2_discovers_live_fleet() {
    if !require_fleet_witness() {
        // Not on a fleet host (e.g. bare CI). Skip rather than fail:
        // AC2 is an explicitly live-environment acceptance criterion.
        eprintln!(
            "ac2_discovers_live_fleet: SKIP (set WM_PLATFORM_HARDWARE_SMOKE=1 on a fleet host)"
        );
        return;
    }

    let run = wintermute_platform::doctor::real_cmd_runner();
    let report = run_doctor(DoctorScope::User, &run, None, None, None);

    assert!(
        report.units.len() >= MIN_FLEET_UNITS,
        "AC2: discovered only {} units, expected >= {} (wm-audio, wm-dialog, wm-stt, wm-tts, wmd, wmd-init)",
        report.units.len(),
        MIN_FLEET_UNITS
    );

    // No hard-coded list: the units we found must have come from
    // discovery, and the canonical six must all be present.
    let names: Vec<&str> = report.units.iter().map(|u| u.unit.as_str()).collect();
    for expected in [
        "wm-audio.service",
        "wm-dialog.service",
        "wm-stt.service",
        "wm-tts.service",
        "wmd.service",
        "wmd-init.service",
    ] {
        assert!(
            names.contains(&expected),
            "AC2: live fleet did not surface {expected}; discovered: {names:?}"
        );
    }

    // Every unit carries the documented JSON fields and serializes cleanly.
    let json = serde_json::to_string(&report.units).expect("AC2: report serializes to JSON");
    for field in [
        "\"unit\"",
        "\"exec_start\"",
        "\"resolved\"",
        "\"exists\"",
        "\"executable\"",
        "\"enabled\"",
        "\"active\"",
        "\"in_target\"",
    ] {
        assert!(
            json.contains(field),
            "AC2: serialized unit JSON is missing documented field {field}"
        );
    }
}
