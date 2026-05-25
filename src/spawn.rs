//! Pluggable child-process launcher + liveness probe.
//!
//! Iter-5 surface: the iter-4 [`Spawner::start`] entry point plus a
//! [`Spawner::still_running`] liveness probe. `PeventSpawner` reads
//! `/proc/<pid>` to answer the probe; `StubSpawner` consults an
//! in-memory map controlled by tests. The supervisor uses the probe
//! to transition a `Running` slot to `Exited` when the kernel drops
//! the PID. Restart policy itself lives in iter-6.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::ChildSpec;

/// Result of a successful `start` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedChild {
    /// `pevent` job id (logical child name on the supervisor side).
    pub id: String,
    /// PID of the supervised process (`pevent`'s grandchild).
    pub child_pid: u32,
    /// PID of the `pevent` supervisor itself. `0` for stub spawners that don't fork.
    pub supervisor_pid: u32,
}

/// Abstract launcher; lets tests inject a deterministic stub.
pub trait Spawner: Send + Sync {
    /// Start `spec` and return the assigned PIDs.
    ///
    /// # Errors
    /// Implementations bubble any failure to launch or parse the launcher's response.
    fn start(&self, spec: &ChildSpec) -> Result<StartedChild>;

    /// Liveness probe. Returns `Ok(true)` while the process named by
    /// `child_pid` is still in the process table, `Ok(false)` once it
    /// is gone. Implementations may use any side-effect-free check.
    ///
    /// # Errors
    /// Surfaces any unexpected I/O error (`NotFound` is the expected
    /// not-running signal and must be reported as `Ok(false)`).
    fn still_running(&self, child_pid: u32) -> Result<bool>;
}

/// Real launcher: shells out to `pevent run --id <name> -- <exec> <args...>`.
#[derive(Debug, Clone, Default)]
pub struct PeventSpawner {
    /// Override the `pevent` binary name (defaults to "pevent" on `$PATH`).
    pub binary: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PeventRunReply {
    id: String,
    child_pid: u32,
    supervisor_pid: u32,
}

impl Spawner for PeventSpawner {
    fn start(&self, spec: &ChildSpec) -> Result<StartedChild> {
        let bin = self.binary.as_deref().unwrap_or("pevent");
        let output = Command::new(bin)
            .arg("run")
            .arg("--id")
            .arg(&spec.name)
            .arg("--")
            .arg(&spec.exec)
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("invoke `{bin} run --id {}`", spec.name))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "pevent run failed (status {}): {}",
                output.status,
                stderr.trim()
            ));
        }
        let stdout = std::str::from_utf8(&output.stdout)
            .context("pevent stdout was not utf-8")?
            .trim();
        let reply: PeventRunReply = serde_json::from_str(stdout)
            .with_context(|| format!("parse pevent reply: {stdout:?}"))?;
        if reply.id != spec.name {
            return Err(anyhow!(
                "pevent echoed id {:?}, expected {:?}",
                reply.id,
                spec.name
            ));
        }
        Ok(StartedChild {
            id: reply.id,
            child_pid: reply.child_pid,
            supervisor_pid: reply.supervisor_pid,
        })
    }

    fn still_running(&self, child_pid: u32) -> Result<bool> {
        let path = PathBuf::from(format!("/proc/{child_pid}"));
        match std::fs::metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
        }
    }
}

#[cfg(test)]
#[allow(unreachable_pub)] // test-only stub; `pub` is needed so sibling tests can use it.
pub(crate) mod testing {
    use super::{Result, Spawner, StartedChild};
    use crate::ChildSpec;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// Deterministic stub: returns synthetic PIDs and records the names it was asked to start.
    #[derive(Debug, Default)]
    pub struct StubSpawner {
        /// Names this stub has been asked to start, in order.
        pub calls: Mutex<Vec<String>>,
        /// Base PID. The Nth call returns `base_pid + N`.
        pub base_pid: u32,
        /// PIDs that the test wants `still_running` to report as gone.
        pub exited: Mutex<HashSet<u32>>,
    }

    impl StubSpawner {
        /// Fresh stub with no recorded calls.
        pub fn new(base_pid: u32) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                base_pid,
                exited: Mutex::new(HashSet::new()),
            }
        }
        /// Snapshot of the names this stub has been asked to start.
        pub fn called_names(&self) -> Vec<String> {
            self.calls.lock().map_or_else(|_| Vec::new(), |g| g.clone())
        }
        /// Mark `pid` as exited; subsequent `still_running(pid)` returns `false`.
        pub fn mark_exited(&self, pid: u32) {
            if let Ok(mut g) = self.exited.lock() {
                g.insert(pid);
            }
        }
    }

    impl Spawner for StubSpawner {
        fn start(&self, spec: &ChildSpec) -> Result<StartedChild> {
            let n_after = {
                let mut g = self
                    .calls
                    .lock()
                    .map_err(|e| anyhow::anyhow!("stub lock poisoned: {e}"))?;
                g.push(spec.name.clone());
                u32::try_from(g.len()).unwrap_or(u32::MAX)
            };
            Ok(StartedChild {
                id: spec.name.clone(),
                child_pid: self.base_pid + n_after,
                supervisor_pid: 0,
            })
        }

        fn still_running(&self, child_pid: u32) -> Result<bool> {
            let g = self
                .exited
                .lock()
                .map_err(|e| anyhow::anyhow!("stub lock poisoned: {e}"))?;
            Ok(!g.contains(&child_pid))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testing::StubSpawner;

    #[test]
    fn stub_spawner_records_calls_and_returns_unique_pids() -> Result<()> {
        let stub = StubSpawner::new(10_000);
        let a = stub.start(&ChildSpec {
            name: "wm-audio".into(),
            exec: "wm-audio".into(),
            args: vec![],
        })?;
        let b = stub.start(&ChildSpec {
            name: "wm-tts".into(),
            exec: "wm-tts".into(),
            args: vec![],
        })?;
        assert_eq!(a.id, "wm-audio");
        assert_eq!(a.child_pid, 10_001);
        assert_eq!(a.supervisor_pid, 0);
        assert_eq!(b.child_pid, 10_002);
        assert_eq!(stub.called_names(), vec!["wm-audio", "wm-tts"]);
        Ok(())
    }

    #[test]
    fn pevent_run_reply_parses_canonical_shape() -> Result<()> {
        let line = r#"{"id": "wm-audio", "child_pid": 4242, "supervisor_pid": 4241}"#;
        let r: PeventRunReply = serde_json::from_str(line)?;
        assert_eq!(r.id, "wm-audio");
        assert_eq!(r.child_pid, 4242);
        assert_eq!(r.supervisor_pid, 4241);
        Ok(())
    }

    #[test]
    fn stub_still_running_flips_on_mark_exited() -> Result<()> {
        let stub = StubSpawner::new(30_000);
        let a = stub.start(&ChildSpec {
            name: "wm-audio".into(),
            exec: "wm-audio".into(),
            args: vec![],
        })?;
        assert!(stub.still_running(a.child_pid)?);
        stub.mark_exited(a.child_pid);
        assert!(!stub.still_running(a.child_pid)?);
        // An unrelated PID is unaffected.
        assert!(stub.still_running(a.child_pid + 7)?);
        Ok(())
    }

    #[test]
    fn pevent_still_running_for_self_pid() -> Result<()> {
        // Our own PID is always live while we're running.
        let s = PeventSpawner::default();
        assert!(s.still_running(std::process::id())?);
        Ok(())
    }

    #[test]
    fn pevent_still_running_false_for_impossible_pid() -> Result<()> {
        // Linux pid_max ≤ 2^22 in practice; u32::MAX is guaranteed absent.
        let s = PeventSpawner::default();
        assert!(!s.still_running(u32::MAX)?);
        Ok(())
    }
}
