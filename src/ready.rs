//! `wm ready` — device-level readiness beacon.
//!
//! Answers the question: "Does this device have everything it needs to
//! actually converse?" This is distinct from `wm doctor` (per-unit start
//! health) and companion-degrade (mid-conversation failure phrases).
//!
//! **Checks performed:**
//! - **Brain** — `WM_ANTHROPIC_API_KEY` is non-empty in the bootstrap env.
//! - **Target** — `wintermute.target` is active (via `systemctl is-active`).
//! - **Audio** — at least one PipeWire/PulseAudio source and one sink present.
//! - **Bus** — agorabus socket is reachable.
//! - **Units** — all canonical wintermute units exist on disk.
//!
//! Verdict: `READY` (exit 0) or `NOT READY: <reasons>` (exit 1).
//!
//! Output formats: human text (default) or `--format json`.
//!
//! The `wm.health.ready` envelope shape matches companion-degrade's
//! `wm.health.*` design so the same off-device consumer can handle both.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{resolve_bootstrap_env_path, EnvConfig};
// Re-export BOOTSTRAP_ENV_OVERRIDE_VAR so tests can assert the constant name.
pub use crate::BOOTSTRAP_ENV_OVERRIDE_VAR;

// ---------------------------------------------------------------------------
// Public envelope — matches companion-degrade's wm.health.* schema
// ---------------------------------------------------------------------------

/// Emitted as a `wm.health.ready` envelope on agorabus.
/// Schema must remain compatible with companion-degrade's `wm.health.*` design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReadyEnvelope {
    /// Envelope topic.
    pub topic: String,
    /// ISO-8601 timestamp (seconds precision, UTC).
    pub ts: String,
    /// `true` ⟺ all checks passed.
    pub ready: bool,
    /// Per-check results.
    pub checks: Vec<CheckResult>,
    /// The single worst-failing reason (for the spoken boot line), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worst_reason: Option<String>,
    /// Boot phrase to speak when ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_phrase: Option<String>,
}

/// One readiness check's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// Short identifier (e.g. `brain`, `audio`, `bus`, `target`, `units`).
    pub name: String,
    /// Whether this check passed.
    pub ok: bool,
    /// Human-readable detail (empty string ⟺ no detail).
    pub detail: String,
}

// ---------------------------------------------------------------------------
// Check names (string constants to avoid typos in tests)
// ---------------------------------------------------------------------------

/// Check name for the API-key / brain check.
pub const CHECK_BRAIN: &str = "brain";
/// Check name for `wintermute.target` active check.
pub const CHECK_TARGET: &str = "target";
/// Check name for audio source+sink enumeration.
pub const CHECK_AUDIO: &str = "audio";
/// Check name for agorabus reachability.
pub const CHECK_BUS: &str = "bus";
/// Check name for canonical unit binary presence.
pub const CHECK_UNITS: &str = "units";

// ---------------------------------------------------------------------------
// Phrase bank (boot/deploy only — must not collide with companion-degrade's
// mid-conversation phrase bank in wm-brain)
// ---------------------------------------------------------------------------

/// The phrase spoken when the device is fully ready (boot path).
pub const BOOT_PHRASE_READY: &str = "Wintermute is ready.";

/// Map a NOT-READY check name to a single plain-language spoken line.
/// Lines must be non-technical: no unit names, no exit codes.
#[must_use]
pub fn not_ready_phrase(check_name: &str) -> &'static str {
    match check_name {
        CHECK_BRAIN => "Wintermute is up, but it can't reach its brain.",
        CHECK_TARGET => "Wintermute has not fully started yet.",
        CHECK_AUDIO => "Wintermute can't hear — its microphone or speaker is missing.",
        CHECK_BUS => "Wintermute can't reach its message bus.",
        CHECK_UNITS => "One or more of Wintermute's programs are missing.",
        _ => "Wintermute is not ready.",
    }
}

// ---------------------------------------------------------------------------
// Individual check implementations (pure where possible; side-effectful ones
// are wired through injected helper fns in `ReadinessProbe` so tests can
// substitute fakes)
// ---------------------------------------------------------------------------

/// Check whether the bootstrap-env API key is non-empty.
/// Injection point: `env_path` overrides the default path (set
/// `WM_BOOTSTRAP_ENV` in the test environment or pass `Some(path)`).
#[must_use]
pub fn check_brain(env_path_override: Option<&Path>) -> CheckResult {
    let path: PathBuf = env_path_override.map_or_else(resolve_bootstrap_env_path, Path::to_owned);
    match EnvConfig::load_optional(&path) {
        Ok(Some(cfg)) => {
            let key = cfg.keys.get("WM_ANTHROPIC_API_KEY").map_or("", String::as_str);
            if key.is_empty() {
                CheckResult {
                    name: CHECK_BRAIN.to_string(),
                    ok: false,
                    detail: format!(
                        "WM_ANTHROPIC_API_KEY is empty in {}",
                        path.display()
                    ),
                }
            } else {
                CheckResult {
                    name: CHECK_BRAIN.to_string(),
                    ok: true,
                    detail: String::new(),
                }
            }
        }
        Ok(None) => CheckResult {
            name: CHECK_BRAIN.to_string(),
            ok: false,
            detail: format!("bootstrap env not found: {}", path.display()),
        },
        Err(e) => CheckResult {
            name: CHECK_BRAIN.to_string(),
            ok: false,
            detail: format!("could not read bootstrap env: {e}"),
        },
    }
}

/// Injected helper type for running external commands.
/// Returns `(stdout, exit_ok)`.
pub type CmdRunner = Box<dyn Fn(&str, &[&str]) -> (String, bool)>;

/// Build a real `CmdRunner` that shells out.
#[must_use]
pub fn real_cmd_runner() -> CmdRunner {
    Box::new(|cmd: &str, args: &[&str]| {
        match std::process::Command::new(cmd).args(args).output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                (stdout, out.status.success())
            }
            Err(_) => (String::new(), false),
        }
    })
}

/// Check whether `wintermute.target` is active.
/// `run` is an injected command runner so the test can inject a fake.
#[must_use]
pub fn check_target(run: &CmdRunner) -> CheckResult {
    let (stdout, ok) = run("systemctl", &["is-active", "--quiet", "wintermute.target"]);
    if ok {
        CheckResult { name: CHECK_TARGET.to_string(), ok: true, detail: String::new() }
    } else {
        let detail = stdout.trim().to_string();
        CheckResult {
            name: CHECK_TARGET.to_string(),
            ok: false,
            detail: if detail.is_empty() {
                "wintermute.target is not active".to_string()
            } else {
                format!("wintermute.target: {detail}")
            },
        }
    }
}

/// Check for at least one audio source (mic) and one sink (speaker).
/// Uses `pactl list short sources` and `pactl list short sinks`.
/// Falls back gracefully if `pactl` is absent.
#[must_use]
pub fn check_audio(run: &CmdRunner) -> CheckResult {
    let (src_out, src_ok) = run("pactl", &["list", "short", "sources"]);
    let (sink_out, sink_ok) = run("pactl", &["list", "short", "sinks"]);

    // pactl exits 0 but may return empty if nothing is connected.
    let has_source = src_ok && !src_out.trim().is_empty();
    let has_sink = sink_ok && !sink_out.trim().is_empty();

    if has_source && has_sink {
        CheckResult { name: CHECK_AUDIO.to_string(), ok: true, detail: String::new() }
    } else if !src_ok && !sink_ok {
        CheckResult {
            name: CHECK_AUDIO.to_string(),
            ok: false,
            detail: "pactl not available or PulseAudio/PipeWire not running".to_string(),
        }
    } else {
        let mut parts = Vec::new();
        if !has_source { parts.push("no audio source (microphone)"); }
        if !has_sink   { parts.push("no audio sink (speaker)"); }
        CheckResult {
            name: CHECK_AUDIO.to_string(),
            ok: false,
            detail: parts.join("; "),
        }
    }
}

/// Check whether the agorabus socket is reachable (socket file exists and
/// we can connect to it). We use a simple existence + connect probe.
#[must_use]
pub fn check_bus(bus_socket_path_override: Option<&Path>) -> CheckResult {
    let path: PathBuf =
        bus_socket_path_override.map_or_else(default_bus_socket_path, Path::to_owned);
    if !path.exists() {
        return CheckResult {
            name: CHECK_BUS.to_string(),
            ok: false,
            detail: format!("agorabus socket not found: {}", path.display()),
        };
    }
    // Try a non-blocking connect.
    match std::os::unix::net::UnixStream::connect(&path) {
        Ok(_) => CheckResult { name: CHECK_BUS.to_string(), ok: true, detail: String::new() },
        Err(e) => CheckResult {
            name: CHECK_BUS.to_string(),
            ok: false,
            detail: format!("agorabus socket not connectable: {e}"),
        },
    }
}

fn default_bus_socket_path() -> PathBuf {
    // Standard agorabus socket location.
    let xdg = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    xdg.join("agorabus").join("bus.sock")
}

/// Check that every canonical wintermute binary resolves on `$PATH`.
#[must_use]
pub fn check_units() -> CheckResult {
    let units = crate::CHILD_STARTUP_ORDER;
    let mut missing = Vec::new();
    for &unit in units {
        if which_on_path(unit).is_none() {
            missing.push(unit);
        }
    }
    if missing.is_empty() {
        CheckResult { name: CHECK_UNITS.to_string(), ok: true, detail: String::new() }
    } else {
        CheckResult {
            name: CHECK_UNITS.to_string(),
            ok: false,
            detail: format!("missing binaries: {}", missing.join(", ")),
        }
    }
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path_env| {
        std::env::split_paths(&path_env).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() { Some(candidate) } else { None }
        })
    })
}

// ---------------------------------------------------------------------------
// Verdict aggregation
// ---------------------------------------------------------------------------

/// Ordering of checks from most to least critical (used for worst-reason
/// selection for the spoken line).
const CHECK_PRIORITY: &[&str] = &[
    CHECK_BRAIN,
    CHECK_TARGET,
    CHECK_BUS,
    CHECK_AUDIO,
    CHECK_UNITS,
];

/// Aggregate a list of `CheckResult`s into a final verdict.
///
/// Returns `(all_ok, worst_failing_check_name_if_any)`.
#[must_use]
pub fn aggregate(checks: &[CheckResult]) -> (bool, Option<String>) {
    let all_ok = checks.iter().all(|c| c.ok);
    if all_ok {
        return (true, None);
    }
    // Pick the worst-failing check by priority order.
    for &priority_name in CHECK_PRIORITY {
        if checks.iter().any(|c| c.name == priority_name && !c.ok) {
            return (false, Some(priority_name.to_string()));
        }
    }
    // Fallback: first failing check.
    let worst = checks.iter().find(|c| !c.ok).map(|c| c.name.clone());
    (false, worst)
}

/// Build a human-readable one-line verdict string.
#[must_use]
pub fn verdict_line(ready: bool, failing: &[&str]) -> String {
    if ready {
        "READY".to_string()
    } else {
        let reasons = failing.join(", ");
        format!("NOT READY: {reasons}")
    }
}

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

/// Current UTC timestamp as an ISO-8601 string (seconds precision).
#[must_use]
pub fn utc_timestamp() -> String {
    // Use SystemTime; format manually — no chrono dep.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, rem2) = (rem / 3600, rem % 3600);
    let (m, s) = (rem2 / 60, rem2 % 60);
    // Compute calendar date from Unix epoch (Gregorian, UTC).
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Gregorian proleptic calendar from 1970-01-01.
    let mut y: u64 = 1970;
    loop {
        let leap = is_leap(y);
        let ydays: u64 = if leap { 366 } else { 365 };
        if days < ydays { break; }
        days -= ydays;
        y += 1;
    }
    let leap = is_leap(y);
    let month_days: &[u64] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo: u64 = 1;
    for &md in month_days {
        if days < md { break; }
        days -= md;
        mo += 1;
    }
    (y, mo, days + 1)
}

const fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // -----------------------------------------------------------------------
    // check_brain tests
    // -----------------------------------------------------------------------

    #[test]
    fn brain_check_fails_on_empty_key() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "WM_ANTHROPIC_API_KEY=").unwrap();
        let r = check_brain(Some(f.path()));
        assert_eq!(r.name, CHECK_BRAIN);
        assert!(!r.ok, "empty key should be NOT READY");
        assert!(r.detail.contains("WM_ANTHROPIC_API_KEY"));
    }

    #[test]
    fn brain_check_passes_on_non_empty_key() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "WM_ANTHROPIC_API_KEY=sk-live-test-key").unwrap();
        let r = check_brain(Some(f.path()));
        assert_eq!(r.name, CHECK_BRAIN);
        assert!(r.ok, "non-empty key should be READY");
        assert!(r.detail.is_empty());
    }

    #[test]
    fn brain_check_fails_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope.env");
        let r = check_brain(Some(&p));
        assert!(!r.ok);
        assert!(r.detail.contains("bootstrap env not found"));
    }

    #[test]
    fn brain_check_fails_when_key_absent_from_env_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "SOME_OTHER_VAR=value").unwrap();
        let r = check_brain(Some(f.path()));
        assert!(!r.ok);
        // Key not present → treated as empty.
        assert!(r.detail.contains("WM_ANTHROPIC_API_KEY"));
    }

    // -----------------------------------------------------------------------
    // check_target tests
    // -----------------------------------------------------------------------

    fn fake_runner(exit_ok: bool) -> CmdRunner {
        Box::new(move |_cmd, _args| (String::new(), exit_ok))
    }

    #[test]
    fn target_check_passes_when_systemctl_ok() {
        let run = fake_runner(true);
        let r = check_target(&run);
        assert_eq!(r.name, CHECK_TARGET);
        assert!(r.ok);
    }

    #[test]
    fn target_check_fails_when_systemctl_fails() {
        let run = fake_runner(false);
        let r = check_target(&run);
        assert!(!r.ok);
        assert!(!r.detail.is_empty());
    }

    // -----------------------------------------------------------------------
    // check_audio tests
    // -----------------------------------------------------------------------

    #[test]
    fn audio_check_passes_when_source_and_sink_present() {
        let run: CmdRunner = Box::new(|_cmd, args| {
            if args.contains(&"sources") || args.contains(&"sinks") {
                ("alsa_output.pci.0\n".to_string(), true)
            } else {
                (String::new(), false)
            }
        });
        let r = check_audio(&run);
        assert!(r.ok, "should pass when source+sink present");
    }

    #[test]
    fn audio_check_fails_when_no_source() {
        let run: CmdRunner = Box::new(|_cmd, args| {
            if args.contains(&"sinks") {
                ("alsa_output.pci.0\n".to_string(), true)
            } else {
                (String::new(), true) // exits ok but empty = no source
            }
        });
        let r = check_audio(&run);
        assert!(!r.ok);
        assert!(r.detail.contains("source"));
    }

    #[test]
    fn audio_check_fails_when_no_sink() {
        let run: CmdRunner = Box::new(|_cmd, args| {
            if args.contains(&"sources") {
                ("alsa_input.pci.0\n".to_string(), true)
            } else {
                (String::new(), true) // exits ok but empty = no sink
            }
        });
        let r = check_audio(&run);
        assert!(!r.ok);
        assert!(r.detail.contains("sink"));
    }

    #[test]
    fn audio_check_fails_when_pactl_absent() {
        let run: CmdRunner = Box::new(|_cmd, _args| (String::new(), false));
        let r = check_audio(&run);
        assert!(!r.ok);
        assert!(r.detail.contains("pactl") || r.detail.contains("PulseAudio") || r.detail.contains("PipeWire"));
    }

    // -----------------------------------------------------------------------
    // check_bus tests
    // -----------------------------------------------------------------------

    #[test]
    fn bus_check_fails_when_socket_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("bus.sock");
        let r = check_bus(Some(&missing));
        assert!(!r.ok);
        assert!(r.detail.contains("not found"));
    }

    // -----------------------------------------------------------------------
    // check_units tests
    // -----------------------------------------------------------------------

    // check_units relies on $PATH — tested indirectly; the binary build
    // smoke covers the pass case. We test only that the function returns a
    // properly named result.
    #[test]
    fn units_check_returns_correct_check_name() {
        let r = check_units();
        assert_eq!(r.name, CHECK_UNITS);
    }

    // -----------------------------------------------------------------------
    // aggregate + verdict_line tests
    // -----------------------------------------------------------------------

    #[test]
    fn aggregate_all_pass_returns_ready() {
        let checks = vec![
            CheckResult { name: CHECK_BRAIN.into(), ok: true, detail: String::new() },
            CheckResult { name: CHECK_TARGET.into(), ok: true, detail: String::new() },
            CheckResult { name: CHECK_AUDIO.into(), ok: true, detail: String::new() },
        ];
        let (ready, worst) = aggregate(&checks);
        assert!(ready);
        assert!(worst.is_none());
    }

    #[test]
    fn aggregate_brain_failure_is_worst() {
        let checks = vec![
            CheckResult { name: CHECK_BRAIN.into(), ok: false, detail: "no key".into() },
            CheckResult { name: CHECK_AUDIO.into(), ok: false, detail: "no mic".into() },
        ];
        let (ready, worst) = aggregate(&checks);
        assert!(!ready);
        assert_eq!(worst.as_deref(), Some(CHECK_BRAIN));
    }

    #[test]
    fn aggregate_target_beats_audio() {
        let checks = vec![
            CheckResult { name: CHECK_TARGET.into(), ok: false, detail: "inactive".into() },
            CheckResult { name: CHECK_AUDIO.into(), ok: false, detail: "no mic".into() },
        ];
        let (ready, worst) = aggregate(&checks);
        assert!(!ready);
        assert_eq!(worst.as_deref(), Some(CHECK_TARGET));
    }

    #[test]
    fn verdict_line_ready() {
        assert_eq!(verdict_line(true, &[]), "READY");
    }

    #[test]
    fn verdict_line_not_ready_lists_reasons() {
        let line = verdict_line(false, &["brain", "audio"]);
        assert!(line.starts_with("NOT READY:"));
        assert!(line.contains("brain"));
        assert!(line.contains("audio"));
    }

    // -----------------------------------------------------------------------
    // Not-ready phrase tests (AC4 — one plain sentence per reason)
    // -----------------------------------------------------------------------

    #[test]
    fn not_ready_phrases_are_plain_sentences() {
        for check in &[CHECK_BRAIN, CHECK_TARGET, CHECK_AUDIO, CHECK_BUS, CHECK_UNITS] {
            let phrase = not_ready_phrase(check);
            // Must be non-empty, single sentence (ends with period), no unit names.
            assert!(!phrase.is_empty(), "phrase for {check} is empty");
            assert!(phrase.ends_with('.'), "phrase for {check} doesn't end with period: {phrase}");
            assert!(!phrase.contains("service"), "phrase for {check} contains 'service'");
            assert!(!phrase.contains("systemctl"), "phrase for {check} contains 'systemctl'");
            assert!(!phrase.contains("exit code"), "phrase for {check} contains 'exit code'");
        }
    }

    // -----------------------------------------------------------------------
    // HealthReadyEnvelope shape test (AC5)
    // -----------------------------------------------------------------------

    #[test]
    fn health_ready_envelope_serializes_with_expected_fields() {
        let env = HealthReadyEnvelope {
            topic: "wm.health.ready".to_string(),
            ts: "2026-05-29T00:00:00Z".to_string(),
            ready: false,
            checks: vec![
                CheckResult { name: CHECK_BRAIN.into(), ok: false, detail: "no key".into() },
            ],
            worst_reason: Some(CHECK_BRAIN.to_string()),
            ready_phrase: None,
        };
        let json = serde_json::to_string(&env).unwrap();
        // AC5: envelope has the wm.health.* topic
        assert!(json.contains(r#""topic":"wm.health.ready""#));
        assert!(json.contains(r#""ready":false"#));
        assert!(json.contains(r#""name":"brain""#));
        assert!(json.contains(r#""worst_reason":"brain""#));
        // ready_phrase skipped when None
        assert!(!json.contains("ready_phrase"));
    }

    #[test]
    fn health_ready_envelope_ready_case() {
        let env = HealthReadyEnvelope {
            topic: "wm.health.ready".to_string(),
            ts: "2026-05-29T00:00:00Z".to_string(),
            ready: true,
            checks: vec![],
            worst_reason: None,
            ready_phrase: Some(BOOT_PHRASE_READY.to_string()),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""ready":true"#));
        assert!(json.contains("Wintermute is ready"));
    }

    // -----------------------------------------------------------------------
    // AC2 — live empty-key detection (integration test)
    // -----------------------------------------------------------------------

    #[test]
    fn brain_check_detects_live_empty_key_via_env_override() {
        // Simulate the actual bootstrap env on this device.
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "WM_ANTHROPIC_API_KEY=").unwrap();
        writeln!(f, "WM_USER_NAME=Mary").unwrap();
        let r = check_brain(Some(f.path()));
        assert!(!r.ok, "empty key must produce NOT READY");
        assert!(r.detail.contains("WM_ANTHROPIC_API_KEY"), "detail must name the key");
    }

    #[test]
    fn brain_check_passes_with_injected_non_empty_key() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "WM_ANTHROPIC_API_KEY=sk-ant-test123").unwrap();
        let r = check_brain(Some(f.path()));
        assert!(r.ok, "injected non-empty key must pass");
    }

    // -----------------------------------------------------------------------
    // AC3 — all-green case exits 0 and selects boot phrase
    // -----------------------------------------------------------------------

    #[test]
    fn all_green_verdict_is_ready_and_selects_boot_phrase() {
        let checks = vec![
            CheckResult { name: CHECK_BRAIN.into(), ok: true, detail: String::new() },
            CheckResult { name: CHECK_TARGET.into(), ok: true, detail: String::new() },
            CheckResult { name: CHECK_AUDIO.into(), ok: true, detail: String::new() },
            CheckResult { name: CHECK_BUS.into(), ok: true, detail: String::new() },
            CheckResult { name: CHECK_UNITS.into(), ok: true, detail: String::new() },
        ];
        let (ready, worst) = aggregate(&checks);
        assert!(ready);
        assert!(worst.is_none());
        let line = verdict_line(ready, &[]);
        assert_eq!(line, "READY");
        // Boot phrase selection.
        let phrase = BOOT_PHRASE_READY;
        assert!(!phrase.is_empty());
        assert!(phrase.contains("ready") || phrase.contains("Ready"));
    }

    // -----------------------------------------------------------------------
    // Timestamp helper smoke test
    // -----------------------------------------------------------------------

    #[test]
    fn utc_timestamp_format_looks_like_iso8601() {
        let ts = utc_timestamp();
        // Minimal shape: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "timestamp length wrong: {ts}");
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }

    // -----------------------------------------------------------------------
    // BOOTSTRAP_ENV_OVERRIDE_VAR is the right constant
    // -----------------------------------------------------------------------

    #[test]
    fn bootstrap_env_override_var_name_matches_platform_constant() {
        assert_eq!(BOOTSTRAP_ENV_OVERRIDE_VAR, "WM_BOOTSTRAP_ENV");
    }
}
