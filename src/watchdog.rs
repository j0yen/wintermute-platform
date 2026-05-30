//! Unit recovery watchdog — core logic.
//!
//! Detects wintermute units in `failed` state, clears them via
//! `systemctl reset-failed`, and restarts them with capped exponential
//! backoff. After `GIVE_UP_AFTER` consecutive failed recovery attempts for
//! the same unit, the watchdog stops retrying and emits a `wm.health.*`
//! event so the rest of the fleet can surface the degraded state.
//!
//! **Policy summary** (from PRD §2.1):
//! - Backoff: 2 s → 4 s → 8 s … capped at [`MAX_BACKOFF_SECS`] (300 s).
//! - Give-up after [`GIVE_UP_AFTER`] consecutive failed recoveries.
//! - Never touches units that don't match the wintermute name prefix.
//! - Never touches `wintermute-watchdog` itself (no self-restart storms).
//!
//! The [`RecoveryState`] struct and all backoff arithmetic are `#[cfg(test)]`-
//! exercisable without touching systemd.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Minimum backoff before the first retry (seconds).
pub const INITIAL_BACKOFF_SECS: u64 = 2;

/// Ceiling for per-unit backoff (5 minutes).
pub const MAX_BACKOFF_SECS: u64 = 300;

/// After this many consecutive failed recoveries the watchdog gives up on
/// the unit and emits a health event rather than retrying.
pub const GIVE_UP_AFTER: u32 = 8;

/// Prefix that all wintermute units must share (name filter).
pub const WM_UNIT_PREFIX: &str = "wm";

/// Alternative prefix accepted for wintermute units.
pub const WM_UNIT_PREFIX2: &str = "wintermute";

/// The watchdog's own unit name — excluded from recovery to prevent
/// self-restart storms.
pub const WATCHDOG_UNIT_NAME: &str = "wintermute-watchdog.service";

// ── Backoff table ──────────────────────────────────────────────────────────

/// Per-unit recovery state maintained in the watchdog's in-memory table.
#[derive(Debug)]
pub struct RecoveryState {
    /// Number of consecutive recovery attempts that failed (the unit ended
    /// up in `failed` again after we tried to start it).
    pub consecutive_failures: u32,
    /// Current backoff interval. Doubles each failure, capped at
    /// [`MAX_BACKOFF_SECS`].
    pub current_backoff: Duration,
    /// When we may next attempt a recovery (`None` = may attempt now).
    pub retry_after: Option<Instant>,
    /// `true` once [`GIVE_UP_AFTER`] consecutive failures have elapsed.
    pub gave_up: bool,
}

impl Default for RecoveryState {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            current_backoff: Duration::from_secs(INITIAL_BACKOFF_SECS),
            retry_after: None,
            gave_up: false,
        }
    }
}

impl RecoveryState {
    /// Returns `true` if we may attempt a recovery at `now`.
    #[must_use]
    pub fn may_attempt(&self, now: Instant) -> bool {
        if self.gave_up {
            return false;
        }
        self.retry_after.is_none_or(|t| now >= t)
    }

    /// Record a successful recovery: clear the failure count and backoff.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.current_backoff = Duration::from_secs(INITIAL_BACKOFF_SECS);
        self.retry_after = None;
    }

    /// Record a failed recovery at `now`. Advances the backoff and returns
    /// `true` if the unit is now given up on.
    pub fn record_failure(&mut self, now: Instant) -> bool {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= GIVE_UP_AFTER {
            self.gave_up = true;
            return true;
        }
        self.retry_after = Some(now + self.current_backoff);
        // Double for next failure, cap at ceiling.
        let next = self
            .current_backoff
            .saturating_mul(2)
            .min(Duration::from_secs(MAX_BACKOFF_SECS));
        self.current_backoff = next;
        false
    }
}

// ── Unit filter ────────────────────────────────────────────────────────────

/// Returns `true` when `unit` belongs to the wintermute fleet and is not the
/// watchdog itself.
///
/// Accepted prefixes: `wm-*`, `wm.*`, `wintermute-*`, `wintermute.*`.
/// `wintermute.target` is not a service; it passes the name check but callers
/// should only call this on units actually in `failed` state.
#[must_use]
pub fn is_wintermute_unit(unit: &str) -> bool {
    if unit == WATCHDOG_UNIT_NAME {
        return false;
    }
    let u = unit.strip_suffix(".service").unwrap_or(unit);
    u.starts_with("wm-")
        || u.starts_with("wm.")
        || u == "wmd"
        || u.starts_with("wmd-")
        || u.starts_with("wintermute-")
        || u.starts_with("wintermute.")
}

// ── Health event ──────────────────────────────────────────────────────────

/// Emitted when the watchdog gives up on a unit after [`GIVE_UP_AFTER`]
/// consecutive failed recoveries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthEvent {
    /// The unit that is permanently degraded.
    pub unit: String,
    /// Number of recovery attempts made before giving up.
    pub attempts: u32,
}

impl HealthEvent {
    /// Format as a `wm.health.*` log/event line.
    #[must_use]
    pub fn format(&self) -> String {
        format!(
            "wm.health.unit-failed unit={} attempts={}",
            self.unit, self.attempts
        )
    }
}

// ── Watchdog engine ────────────────────────────────────────────────────────

/// The watchdog's recovery engine. Owns the per-unit backoff table and
/// orchestrates detection, clearing, and restarting of failed units.
///
/// The actual `systemctl` calls are injected via the [`SystemctlBackend`]
/// trait so the engine can be unit-tested without a live systemd.
pub struct WatchdogEngine<B: SystemctlBackend> {
    /// The backend used for all systemctl operations. Exposed for test spies.
    pub backend: B,
    table: HashMap<String, RecoveryState>,
}

impl<B: SystemctlBackend> WatchdogEngine<B> {
    /// Construct a new engine with the given backend.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            table: HashMap::new(),
        }
    }

    /// Run one watchdog tick.
    ///
    /// 1. Lists failed units via the backend.
    /// 2. Filters to wintermute units.
    /// 3. For each unit that is past its backoff, attempts `reset-failed` +
    ///    `start`.
    /// 4. Re-queries the unit after the start; records success or failure.
    /// 5. Returns any `HealthEvent`s generated this tick.
    ///
    /// # Errors
    /// Returns the first fatal backend error (non-retryable I/O failures).
    pub fn tick(&mut self, now: Instant) -> Result<Vec<HealthEvent>, WatchdogError> {
        let failed = self.backend.list_failed_units()?;
        let mut events = Vec::new();

        for unit in failed {
            if !is_wintermute_unit(&unit) {
                continue;
            }
            let state = self.table.entry(unit.clone()).or_default();
            if !state.may_attempt(now) {
                continue;
            }
            // Attempt recovery.
            let reset_ok = self.backend.reset_failed(&unit).is_ok();
            let start_ok = reset_ok && self.backend.start_unit(&unit).is_ok();

            if start_ok {
                // Check if the unit actually came up.
                let still_failed = self
                    .backend
                    .unit_is_failed(&unit)
                    .unwrap_or(true);
                if still_failed {
                    let gave_up = self
                        .table
                        .entry(unit.clone())
                        .or_default()
                        .record_failure(now);
                    if gave_up {
                        let attempts = self
                            .table
                            .get(&unit)
                            .map_or(GIVE_UP_AFTER, |s| s.consecutive_failures);
                        events.push(HealthEvent { unit, attempts });
                    }
                } else {
                    self.table
                        .entry(unit)
                        .or_default()
                        .record_success();
                }
            } else {
                let gave_up = self
                    .table
                    .entry(unit.clone())
                    .or_default()
                    .record_failure(now);
                if gave_up {
                    let attempts = self
                        .table
                        .get(&unit)
                        .map_or(GIVE_UP_AFTER, |s| s.consecutive_failures);
                    events.push(HealthEvent { unit, attempts });
                }
            }
        }
        Ok(events)
    }

    /// Access the current backoff table (for testing / introspection).
    #[must_use]
    pub const fn table(&self) -> &HashMap<String, RecoveryState> {
        &self.table
    }
}

// ── Backend trait ─────────────────────────────────────────────────────────

/// Abstraction over `systemctl` calls. Implemented by [`SystemctlReal`] for
/// production use and by test doubles in unit tests.
pub trait SystemctlBackend {
    /// List unit names currently in the `failed` activation state.
    ///
    /// # Errors
    /// Returns an error on fatal I/O failures.
    fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError>;

    /// Run `systemctl [--user] reset-failed <unit>`.
    ///
    /// # Errors
    /// Returns an error if the command fails.
    fn reset_failed(&self, unit: &str) -> Result<(), WatchdogError>;

    /// Run `systemctl [--user] start <unit>`.
    ///
    /// # Errors
    /// Returns an error if the command fails.
    fn start_unit(&self, unit: &str) -> Result<(), WatchdogError>;

    /// Query whether a unit is still in `failed` state.
    ///
    /// # Errors
    /// Returns an error if the query fails fatally.
    fn unit_is_failed(&self, unit: &str) -> Result<bool, WatchdogError>;
}

// ── Real systemctl backend ────────────────────────────────────────────────

/// Production backend that shells out to `systemctl --user`.
pub struct SystemctlReal;

/// Errors emitted by the watchdog engine or backend.
#[derive(Debug)]
pub enum WatchdogError {
    /// A `systemctl` invocation returned non-zero or could not be spawned.
    SystemctlFailed {
        /// The operation attempted.
        op: String,
        /// The unit involved (empty for list operations).
        unit: String,
        /// Error details.
        detail: String,
    },
    /// An I/O error unrelated to systemctl.
    Io(std::io::Error),
}

impl std::fmt::Display for WatchdogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemctlFailed { op, unit, detail } => {
                write!(f, "systemctl {op} {unit}: {detail}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for WatchdogError {}

impl SystemctlBackend for SystemctlReal {
    fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "--state=failed", "--no-legend", "--plain", "-o", "json"])
            .output()
            .map_err(WatchdogError::Io)?;
        if !out.status.success() {
            // Older systemd versions don't support --json; fall back to text.
            return list_failed_units_text();
        }
        parse_failed_json(&String::from_utf8_lossy(&out.stdout))
    }

    fn reset_failed(&self, unit: &str) -> Result<(), WatchdogError> {
        run_systemctl(&["--user", "reset-failed", unit])
    }

    fn start_unit(&self, unit: &str) -> Result<(), WatchdogError> {
        run_systemctl(&["--user", "start", unit])
    }

    fn unit_is_failed(&self, unit: &str) -> Result<bool, WatchdogError> {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-failed", unit])
            .output()
            .map_err(WatchdogError::Io)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(stdout.trim() == "failed")
    }
}

fn run_systemctl(args: &[&str]) -> Result<(), WatchdogError> {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .status()
        .map_err(WatchdogError::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(WatchdogError::SystemctlFailed {
            op: args.first().copied().unwrap_or("").to_string(),
            unit: args.last().copied().unwrap_or("").to_string(),
            detail: format!("exit code {:?}", status.code()),
        })
    }
}

fn list_failed_units_text() -> Result<Vec<String>, WatchdogError> {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "--state=failed", "--no-legend", "--plain"])
        .output()
        .map_err(WatchdogError::Io)?;
    if !out.status.success() {
        return Err(WatchdogError::SystemctlFailed {
            op: "list-units".into(),
            unit: String::new(),
            detail: format!("exit code {:?}", out.status.code()),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut units = Vec::new();
    for line in text.lines() {
        // Text output format: "  <unit>  loaded failed failed  <desc>"
        if let Some(name) = line.split_whitespace().next() {
            units.push(name.to_string());
        }
    }
    Ok(units)
}

fn parse_failed_json(text: &str) -> Result<Vec<String>, WatchdogError> {
    // `systemctl -o json` emits a JSON array of objects with a "unit" key.
    // We do a lightweight parse rather than pulling in serde_json here.
    let mut units = Vec::new();
    for line in text.lines() {
        // Each object on its own line (ndjson-ish) or classic JSON array.
        // Look for `"unit":"<name>"` patterns.
        let mut rest = line;
        while let Some(idx) = rest.find("\"unit\"") {
            rest = &rest[idx + 6..];
            let rest2 = rest.trim_start_matches([' ', '\t', ':']);
            if let Some(inner) = rest2.strip_prefix('"') {
                if let Some(end) = inner.find('"') {
                    units.push(inner[..end].to_string());
                }
            }
        }
    }
    // If JSON parse yielded nothing fall back to text.
    if units.is_empty() {
        list_failed_units_text()
    } else {
        Ok(units)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Backoff schedule ──────────────────────────────────────────────────

    #[test]
    fn backoff_schedule_is_monotonic_and_capped() {
        let mut state = RecoveryState::default();
        let t0 = Instant::now();
        let mut prev = Duration::ZERO;
        let mut gave_up = false;
        for i in 0..GIVE_UP_AFTER {
            let before = state.current_backoff;
            assert!(
                before >= prev || i == 0,
                "backoff not monotonic at step {i}"
            );
            prev = before;
            assert!(
                before <= Duration::from_secs(MAX_BACKOFF_SECS),
                "backoff exceeds ceiling at step {i}"
            );
            gave_up = state.record_failure(t0 + Duration::from_secs(u64::from(i) * MAX_BACKOFF_SECS));
        }
        assert!(gave_up, "should have given up after GIVE_UP_AFTER failures");
    }

    #[test]
    fn backoff_ceiling_never_exceeded() {
        let mut state = RecoveryState::default();
        let t0 = Instant::now();
        for i in 0..20u32 {
            state.record_failure(t0 + Duration::from_secs(u64::from(i) * 1000));
            assert!(
                state.current_backoff <= Duration::from_secs(MAX_BACKOFF_SECS),
                "ceiling exceeded at step {i}"
            );
        }
    }

    #[test]
    fn backoff_doubles_until_ceiling() {
        let mut state = RecoveryState::default();
        let t0 = Instant::now();
        // First few failures: 2→4→8→16→32→64→128→256 (give-up at step 8 before doubling).
        // When GIVE_UP_AFTER is hit, record_failure returns early so current_backoff
        // stays at the last value (256) rather than being advanced to 300+.
        let expected_sequence = [2u64, 4, 8, 16, 32, 64, 128, 256, 256, 256];
        for (i, &expected) in expected_sequence.iter().enumerate() {
            assert_eq!(
                state.current_backoff,
                Duration::from_secs(expected),
                "step {i}: expected {expected}s"
            );
            if state.consecutive_failures < GIVE_UP_AFTER {
                state.record_failure(t0 + Duration::from_secs(u64::try_from(i).unwrap_or(0) * 1000));
            }
        }
    }

    // ── Per-unit independent state ────────────────────────────────────────

    #[test]
    fn per_unit_backoff_state_is_independent() {
        struct StubFailed(Vec<String>);
        impl SystemctlBackend for StubFailed {
            fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
                Ok(self.0.clone())
            }
            fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
                Ok(())
            }
            fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
                Ok(())
            }
            fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
                Ok(true) // always still failed
            }
        }

        let mut engine = WatchdogEngine::new(StubFailed(vec![
            "wm-audio.service".into(),
            "wm-tts.service".into(),
        ]));
        let t0 = Instant::now();
        // tick enough times that wm-audio accumulates more failures.
        // Each tick advances by MAX_BACKOFF_SECS to always be past backoff.
        for i in 0..4u32 {
            let _ = engine.tick(t0 + Duration::from_secs(u64::from(i) * MAX_BACKOFF_SECS * 2));
        }
        let audio = engine.table().get("wm-audio.service").map(|s| s.consecutive_failures);
        let tts = engine.table().get("wm-tts.service").map(|s| s.consecutive_failures);
        // Both should have the same count since both always-fail.
        assert_eq!(audio, tts, "states should be symmetric for equal-fail units");
        // And both must be >0 (they failed).
        assert!(audio.unwrap_or(0) > 0);
    }

    // ── Give-up after K failures ──────────────────────────────────────────

    #[test]
    fn gives_up_after_k_consecutive_failures() {
        let mut state = RecoveryState::default();
        let t0 = Instant::now();
        let mut gave_up = false;
        for i in 0..GIVE_UP_AFTER {
            gave_up = state.record_failure(t0 + Duration::from_secs(u64::from(i) * 1000));
        }
        assert!(gave_up);
        assert!(state.gave_up);
        // After giving up, may_attempt must always return false.
        assert!(!state.may_attempt(t0 + Duration::from_secs(99_999)));
    }

    #[test]
    fn success_resets_failure_count() {
        let mut state = RecoveryState::default();
        let t0 = Instant::now();
        for i in 0..3u32 {
            state.record_failure(t0 + Duration::from_secs(u64::from(i) * 10));
        }
        assert_eq!(state.consecutive_failures, 3);
        state.record_success();
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.current_backoff, Duration::from_secs(INITIAL_BACKOFF_SECS));
        assert!(state.may_attempt(t0));
    }

    // ── wintermute-unit filter ────────────────────────────────────────────

    #[test]
    fn filter_accepts_wintermute_units() {
        assert!(is_wintermute_unit("wm-audio.service"));
        assert!(is_wintermute_unit("wm-tts.service"));
        assert!(is_wintermute_unit("wm-stt.service"));
        assert!(is_wintermute_unit("wm-dialog.service"));
        assert!(is_wintermute_unit("wmd.service"));
        assert!(is_wintermute_unit("wintermute-ready.service"));
        assert!(is_wintermute_unit("wmd-init.service"));
    }

    #[test]
    fn filter_rejects_non_wintermute_units() {
        assert!(!is_wintermute_unit("sshd.service"));
        assert!(!is_wintermute_unit("NetworkManager.service"));
        assert!(!is_wintermute_unit("pipewire.service"));
        assert!(!is_wintermute_unit("gdm.service"));
        assert!(!is_wintermute_unit("bluetooth.service"));
    }

    #[test]
    fn filter_rejects_watchdog_itself() {
        assert!(!is_wintermute_unit(WATCHDOG_UNIT_NAME));
    }

    // ── Health event shape ────────────────────────────────────────────────

    #[test]
    fn health_event_format_contains_unit_and_attempts() {
        let ev = HealthEvent {
            unit: "wm-audio.service".into(),
            attempts: 8,
        };
        let s = ev.format();
        assert!(s.contains("wm.health."), "missing wm.health. prefix: {s}");
        assert!(s.contains("wm-audio.service"), "missing unit: {s}");
        assert!(s.contains("attempts=8"), "missing attempts: {s}");
    }

    #[test]
    fn health_event_emitted_on_give_up() {
        struct AlwaysFailed;
        impl SystemctlBackend for AlwaysFailed {
            fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
                Ok(vec!["wm-audio.service".into()])
            }
            fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
                Ok(())
            }
            fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
                Ok(())
            }
            fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
                Ok(true) // never recovers
            }
        }

        let mut engine = WatchdogEngine::new(AlwaysFailed);
        let t0 = Instant::now();
        let mut give_up_event: Option<HealthEvent> = None;
        for i in 0..u64::from(GIVE_UP_AFTER) + 2 {
            let events = engine
                .tick(t0 + Duration::from_secs(i * MAX_BACKOFF_SECS * 2))
                .unwrap();
            for ev in events {
                give_up_event = Some(ev);
            }
        }
        assert!(give_up_event.is_some(), "no give-up health event emitted");
        let ev = give_up_event.unwrap();
        assert_eq!(ev.unit, "wm-audio.service");
    }

    // ── Non-wintermute units are not touched ──────────────────────────────

    #[test]
    fn non_wintermute_failed_unit_never_touched() {
        struct SpyBackend {
            touched: std::cell::Cell<bool>,
        }
        impl SystemctlBackend for SpyBackend {
            fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
                Ok(vec!["sshd.service".into(), "NetworkManager.service".into()])
            }
            fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
                self.touched.set(true);
                Ok(())
            }
            fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
                self.touched.set(true);
                Ok(())
            }
            fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
                Ok(false)
            }
        }

        let spy = SpyBackend { touched: std::cell::Cell::new(false) };
        let mut engine = WatchdogEngine::new(spy);
        engine.tick(Instant::now()).unwrap();
        assert!(
            !engine.backend.touched.get(),
            "non-wintermute unit was touched"
        );
    }

    // ── Watchdog does not recover its own unit ────────────────────────────

    #[test]
    fn watchdog_does_not_recover_itself() {
        struct SelfFailed {
            touched: std::cell::Cell<bool>,
        }
        impl SystemctlBackend for SelfFailed {
            fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
                Ok(vec![WATCHDOG_UNIT_NAME.to_string()])
            }
            fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
                self.touched.set(true);
                Ok(())
            }
            fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
                self.touched.set(true);
                Ok(())
            }
            fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
                Ok(false)
            }
        }

        let spy = SelfFailed { touched: std::cell::Cell::new(false) };
        let mut engine = WatchdogEngine::new(spy);
        engine.tick(Instant::now()).unwrap();
        assert!(
            !engine.backend.touched.get(),
            "watchdog tried to recover its own unit"
        );
    }
}
