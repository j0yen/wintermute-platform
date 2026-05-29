//! wintermute-platform — supervisor + CLI library.
//!
//! Iter-5 surface: iter-4 ([`spawn::Spawner`] + `start_all`) plus exit
//! observation. [`spawn::Spawner`] gains a `still_running` liveness
//! probe; [`supervisor::Supervisor::observe_exits`] uses it to
//! transition `Running → Exited` on PID loss. The `wmd-init` binary
//! opt-ins via `--spawn` to run `start_all` + a 1 s observation loop.
//! Restart policy, mute plumbing, log tailing, and agorabus publishing
//! land in iter-6+.

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod kiosk;
pub mod protocol;
pub mod ready;
pub mod spawn;
pub mod supervisor;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Canonical startup order for Fleet-1 children (PRD §2.3, intent-card `hard_constraints`).
pub const CHILD_STARTUP_ORDER: &[&str] =
    &["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd"];

/// Restart-storm rolling window length (PRD §2.3).
pub const RESTART_WINDOW_SECS: u64 = 60;

/// Restarts within the window that flip the supervisor into backoff.
pub const RESTART_STORM_THRESHOLD: u32 = 5;

/// Backoff sleep between restarts once in storm.
pub const BACKOFF_SECS: u64 = 30;

/// Minutes of failed restarts before spoken-error escalation.
pub const ESCALATION_MINS: u64 = 20;

/// Default path to the bootstrap env file (written by `wm-bootstrap`).
pub const BOOTSTRAP_ENV_PATH: &str = "/etc/wintermute/conf.d/00-bootstrap.env";

/// Override env var for the bootstrap env path (used by AC9 tests).
pub const BOOTSTRAP_ENV_OVERRIDE_VAR: &str = "WM_BOOTSTRAP_ENV";

/// Literal log phrase emitted on missing bootstrap config (AC9).
pub const NO_BOOTSTRAP_MESSAGE: &str = "no bootstrap config; halting";

/// Resolve the supervisor's control-plane Unix socket path.
#[must_use]
pub fn socket_path() -> PathBuf {
    socket_path_with(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Pure helper: build the socket path from an explicit `XDG_RUNTIME_DIR` value.
#[must_use]
pub fn socket_path_with(runtime_dir: Option<OsString>) -> PathBuf {
    let base = runtime_dir.map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    base.join("wintermute").join("init.sock")
}

/// Per-child log path under `~/.local/state/wintermute/logs/<name>.log` (PRD §2.4).
#[must_use]
pub fn child_log_path(child: &str) -> PathBuf {
    child_log_path_with(std::env::var_os("HOME"), child)
}

/// Pure helper: build the per-child log path from an explicit HOME value.
#[must_use]
pub fn child_log_path_with(home: Option<OsString>, child: &str) -> PathBuf {
    let base = home.map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    base.join(".local/state/wintermute/logs")
        .join(format!("{child}.log"))
}

/// Read the last `tail` lines of the per-child stderr log.
///
/// Missing log file → `Ok(vec![])` (a child that hasn't logged anything yet
/// is a normal state, not an error). Any other I/O error surfaces.
///
/// Lines are returned in original file order (oldest of the tail first).
/// Trailing CR/LF stripped per line.
///
/// # Errors
/// Surfaces I/O errors other than `NotFound` from reading the log file.
pub fn tail_child_log(child: &str, tail: usize) -> std::io::Result<Vec<String>> {
    tail_child_log_with(std::env::var_os("HOME"), child, tail)
}

/// Pure variant of [`tail_child_log`] taking an explicit HOME for tests.
///
/// # Errors
/// Surfaces I/O errors other than `NotFound`.
pub fn tail_child_log_with(
    home: Option<OsString>,
    child: &str,
    tail: usize,
) -> std::io::Result<Vec<String>> {
    let path = child_log_path_with(home, child);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if tail == 0 {
                return Ok(Vec::new());
            }
            let mut all: Vec<String> = text.lines().map(str::to_owned).collect();
            if all.len() > tail {
                let drop = all.len() - tail;
                all.drain(..drop);
            }
            Ok(all)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Resolve which bootstrap env path to use: `WM_BOOTSTRAP_ENV` override, else default.
#[must_use]
pub fn resolve_bootstrap_env_path() -> PathBuf {
    std::env::var_os(BOOTSTRAP_ENV_OVERRIDE_VAR)
        .map_or_else(|| PathBuf::from(BOOTSTRAP_ENV_PATH), PathBuf::from)
}

/// One supervised child: binary name + invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSpec {
    /// Logical child name (e.g. `wm-audio`).
    pub name: String,
    /// Executable name resolved on `$PATH`.
    pub exec: String,
    /// Arguments passed to the child binary.
    pub args: Vec<String>,
}

/// Build the canonical five-child spec list in startup order.
#[must_use]
pub fn canonical_children() -> Vec<ChildSpec> {
    CHILD_STARTUP_ORDER
        .iter()
        .map(|&n| ChildSpec {
            name: n.to_string(),
            exec: n.to_string(),
            args: Vec::new(),
        })
        .collect()
}

/// Loaded view of `/etc/wintermute/conf.d/00-bootstrap.env`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvConfig {
    /// Sorted key/value pairs parsed from the env file.
    pub keys: BTreeMap<String, String>,
}

impl EnvConfig {
    /// Parse env-file text. Comments (`#…`) and blank lines are ignored.
    /// Lines that don't contain `=` are silently skipped.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut keys = BTreeMap::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = trimmed.split_once('=') {
                keys.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Self { keys }
    }

    /// Load env from `path`. Returns `Ok(None)` if the file does not exist.
    ///
    /// # Errors
    /// Surfaces any I/O error other than `NotFound`.
    pub fn load_optional(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(Self::parse(&text))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Tracks restart events in a rolling window and computes the next backoff.
#[derive(Debug)]
pub struct RestartTracker {
    window: Duration,
    threshold: u32,
    backoff: Duration,
    history: VecDeque<Instant>,
}

impl Default for RestartTracker {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(RESTART_WINDOW_SECS),
            threshold: RESTART_STORM_THRESHOLD,
            backoff: Duration::from_secs(BACKOFF_SECS),
            history: VecDeque::new(),
        }
    }
}

impl RestartTracker {
    /// Record a restart at `now`; return the delay before attempting the next start.
    /// Below the storm threshold: zero delay. At or above: `backoff`.
    pub fn record(&mut self, now: Instant) -> Duration {
        if let Some(cutoff) = now.checked_sub(self.window) {
            while self.history.front().is_some_and(|&t| t < cutoff) {
                self.history.pop_front();
            }
        }
        self.history.push_back(now);
        let threshold = usize::try_from(self.threshold).unwrap_or(usize::MAX);
        if self.history.len() >= threshold {
            self.backoff
        } else {
            Duration::ZERO
        }
    }

    /// `true` when the rolling window already holds threshold-many restarts.
    #[must_use]
    pub fn in_storm(&self) -> bool {
        let threshold = usize::try_from(self.threshold).unwrap_or(usize::MAX);
        self.history.len() >= threshold
    }

    /// Number of restarts currently inside the rolling window.
    #[must_use]
    pub fn restarts_in_window(&self) -> usize {
        self.history.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_children_in_documented_order() {
        let kids = canonical_children();
        let names: Vec<&str> = kids.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd"]
        );
        assert_eq!(kids.len(), 5);
    }

    #[test]
    fn env_parse_handles_comments_blanks_and_kv() {
        let env = EnvConfig::parse(
            "# header\n\nWM_USER_NAME=Mary\n  WM_WAKE_WORD = hey_jarvis  \njunkline\n",
        );
        assert_eq!(env.keys.get("WM_USER_NAME"), Some(&"Mary".to_string()));
        assert_eq!(env.keys.get("WM_WAKE_WORD"), Some(&"hey_jarvis".to_string()));
        assert_eq!(env.keys.len(), 2);
    }

    #[test]
    fn env_load_missing_returns_none() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("nope.env");
        let cfg = EnvConfig::load_optional(&p)?;
        assert!(cfg.is_none());
        Ok(())
    }

    #[test]
    fn env_load_present_returns_some() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("00.env");
        std::fs::write(&p, "WM_USER_NAME=Mary\n")?;
        let cfg = EnvConfig::load_optional(&p)?;
        assert!(cfg.is_some_and(|c| c.keys.len() == 1));
        Ok(())
    }

    #[test]
    fn restart_tracker_no_storm_below_threshold() {
        let mut t = RestartTracker::default();
        let now = Instant::now();
        for i in 0..4 {
            let d = t.record(now + Duration::from_millis(i * 100));
            assert_eq!(d, Duration::ZERO);
            assert!(!t.in_storm());
        }
        assert_eq!(t.restarts_in_window(), 4);
    }

    #[test]
    fn restart_tracker_storm_triggers_backoff() {
        let mut t = RestartTracker::default();
        let now = Instant::now();
        for i in 0..4 {
            t.record(now + Duration::from_millis(i * 100));
        }
        let d = t.record(now + Duration::from_millis(500));
        assert_eq!(d, Duration::from_secs(BACKOFF_SECS));
        assert!(t.in_storm());
    }

    #[test]
    fn restart_tracker_expires_old_entries() {
        let mut t = RestartTracker::default();
        let now = Instant::now();
        for i in 0..4 {
            t.record(now + Duration::from_millis(i * 10));
        }
        let later = now + Duration::from_secs(RESTART_WINDOW_SECS + 1);
        let d = t.record(later);
        assert_eq!(d, Duration::ZERO);
        assert!(!t.in_storm());
        assert_eq!(t.restarts_in_window(), 1);
    }

    #[test]
    fn socket_path_under_explicit_xdg_runtime_dir() {
        let p = socket_path_with(Some(OsString::from("/run/user/1000")));
        assert_eq!(p, PathBuf::from("/run/user/1000/wintermute/init.sock"));
    }

    #[test]
    fn socket_path_falls_back_to_tmp_when_unset() {
        let p = socket_path_with(None);
        assert_eq!(p, PathBuf::from("/tmp/wintermute/init.sock"));
    }

    #[test]
    fn child_log_path_under_explicit_home() {
        let p = child_log_path_with(Some(OsString::from("/home/test")), "wm-audio");
        assert_eq!(
            p,
            PathBuf::from("/home/test/.local/state/wintermute/logs/wm-audio.log")
        );
    }

    #[test]
    fn child_spec_serde_roundtrip() -> Result<(), serde_json::Error> {
        let spec = ChildSpec {
            name: "wm-audio".into(),
            exec: "wm-audio".into(),
            args: vec!["--verbose".into()],
        };
        let json = serde_json::to_string(&spec)?;
        let back: ChildSpec = serde_json::from_str(&json)?;
        assert_eq!(spec, back);
        Ok(())
    }

    #[test]
    fn tail_child_log_missing_file_returns_empty() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let lines = tail_child_log_with(Some(dir.path().as_os_str().to_owned()), "wm-audio", 20)?;
        assert!(lines.is_empty());
        Ok(())
    }

    fn write_child_log(home: &Path, child: &str, contents: &str) -> std::io::Result<()> {
        let log_dir = home.join(".local/state/wintermute/logs");
        std::fs::create_dir_all(&log_dir)?;
        std::fs::write(log_dir.join(format!("{child}.log")), contents)
    }

    #[test]
    fn tail_child_log_returns_last_n_lines_in_order() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        write_child_log(dir.path(), "wm-audio", "a\nb\nc\nd\ne\n")?;
        let lines = tail_child_log_with(Some(dir.path().as_os_str().to_owned()), "wm-audio", 3)?;
        assert_eq!(lines, vec!["c".to_string(), "d".into(), "e".into()]);
        Ok(())
    }

    #[test]
    fn tail_child_log_tail_larger_than_file_returns_all() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        write_child_log(dir.path(), "wm-stt", "one\ntwo\n")?;
        let lines = tail_child_log_with(Some(dir.path().as_os_str().to_owned()), "wm-stt", 20)?;
        assert_eq!(lines, vec!["one".to_string(), "two".into()]);
        Ok(())
    }

    #[test]
    fn tail_child_log_zero_returns_empty() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        write_child_log(dir.path(), "wm-tts", "x\ny\n")?;
        let lines = tail_child_log_with(Some(dir.path().as_os_str().to_owned()), "wm-tts", 0)?;
        assert!(lines.is_empty());
        Ok(())
    }
}
