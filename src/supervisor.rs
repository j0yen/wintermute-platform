//! In-memory supervisor state + UDS server.
//!
//! Iter-7 surface: iter-6 (`start_all` + `observe_exits` +
//! `restart_exited`) plus `clear_expired_backoffs`, which flips
//! `Backoff` slots back to `Exited` once the per-child wake-up
//! timestamp has passed. The wmd-init binary loop now owns both the
//! [`RestartTracker`] map and the `backoff_until` schedule, and ties
//! them together each tick: clear → observe → restart → record.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, info, warn};

use crate::protocol::{ChildStatus, Request, Response, SupervisorView};
use crate::spawn::Spawner;
use crate::{BACKOFF_SECS, ChildSpec, RestartTracker, canonical_children};

/// Result of one [`Supervisor::restart_exited`] decision for a child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartOutcome {
    /// Child was relaunched; carries the new PID and updated restart count.
    Restarted {
        /// Child name.
        name: String,
        /// New PID from the spawner.
        child_pid: u32,
        /// Total restarts since supervisor start, after this attempt.
        restart_count: u32,
    },
    /// Child tripped the storm threshold; slot flipped to `Backoff`.
    EnteredBackoff {
        /// Child name.
        name: String,
        /// Number of restarts inside the rolling window.
        restarts_in_window: usize,
    },
}

/// In-memory snapshot + UDS server.
#[derive(Debug, Clone)]
pub struct Supervisor {
    state: Arc<Mutex<SupervisorView>>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// Build a supervisor with the canonical five children, all `Pending`.
    #[must_use]
    pub fn new() -> Self {
        let view = SupervisorView::pending_for(&canonical_children());
        Self {
            state: Arc::new(Mutex::new(view)),
        }
    }

    /// Read-only snapshot of the current state — useful for tests.
    pub async fn snapshot(&self) -> SupervisorView {
        self.state.lock().await.clone()
    }

    /// Start every child in canonical order via `spawner`. Transitions
    /// each entry `Pending → Starting → Running` and records the
    /// returned PID. Returns the first error if any child fails to
    /// launch; already-started children stay `Running` (cleanup is the
    /// caller's responsibility — iter-5 wires the restart loop).
    ///
    /// # Errors
    /// Surfaces the underlying `Spawner::start` error verbatim.
    pub async fn start_all<S: Spawner + ?Sized>(&self, spawner: &S) -> Result<()> {
        self.start_specs(spawner, &canonical_children()).await
    }

    /// Variant of `start_all` that takes an explicit spec list — used by tests
    /// to drive the supervisor with a one-child fixture rather than the full five.
    ///
    /// # Errors
    /// Surfaces the underlying `Spawner::start` error verbatim.
    pub async fn start_specs<S: Spawner + ?Sized>(
        &self,
        spawner: &S,
        specs: &[ChildSpec],
    ) -> Result<()> {
        for spec in specs {
            {
                let mut g = self.state.lock().await;
                if let Some(slot) = g.children.iter_mut().find(|c| c.name == spec.name) {
                    slot.status = ChildStatus::Starting;
                    slot.last_event = Some("starting".into());
                }
            }
            let started = spawner
                .start(spec)
                .with_context(|| format!("start child {}", spec.name))?;
            let mut g = self.state.lock().await;
            if let Some(slot) = g.children.iter_mut().find(|c| c.name == spec.name) {
                slot.status = ChildStatus::Running;
                slot.child_pid = Some(started.child_pid);
                slot.last_event = Some(format!("running pid={}", started.child_pid));
            }
        }
        Ok(())
    }

    /// One observation pass: visit every `Running` child with a known
    /// PID and ask `spawner.still_running`. On `Ok(false)` flip the
    /// slot to [`ChildStatus::Exited`] and record the event. Other
    /// statuses (Pending/Starting/Backoff/Halted/Exited) are skipped.
    /// Returns the names that just transitioned, in order encountered.
    ///
    /// # Errors
    /// Surfaces the first `Spawner::still_running` error encountered.
    pub async fn observe_exits<S: Spawner + ?Sized>(
        &self,
        spawner: &S,
    ) -> Result<Vec<String>> {
        let snapshot: Vec<(String, u32)> = {
            let g = self.state.lock().await;
            g.children
                .iter()
                .filter(|c| c.status == ChildStatus::Running)
                .filter_map(|c| c.child_pid.map(|p| (c.name.clone(), p)))
                .collect()
        };
        let mut transitions = Vec::new();
        for (name, pid) in snapshot {
            let alive = spawner
                .still_running(pid)
                .with_context(|| format!("probe still_running for {name} pid={pid}"))?;
            if !alive {
                let mut g = self.state.lock().await;
                if let Some(slot) = g
                    .children
                    .iter_mut()
                    .find(|c| c.name == name && c.status == ChildStatus::Running)
                {
                    slot.status = ChildStatus::Exited;
                    slot.last_event = Some(format!("exited (lost pid {pid})"));
                    transitions.push(name);
                }
            }
        }
        Ok(transitions)
    }

    /// Restart children currently in [`ChildStatus::Exited`] under the
    /// guidance of `trackers`. For each exited child, `record(now)` is
    /// called on its tracker; a `ZERO` delay means immediate restart
    /// (slot flips `Exited → Starting → Running` with a fresh PID),
    /// and a non-zero delay means the rolling window is saturated and
    /// the slot flips `Exited → Backoff`. Children without a tracker
    /// entry get one created with `RestartTracker::default()`.
    /// Returns one [`RestartOutcome`] per Exited child encountered, in
    /// canonical order.
    ///
    /// Other lifecycle phases (Pending/Starting/Running/Backoff/Halted)
    /// are skipped. The actual `BACKOFF_SECS` sleep is the binary's
    /// responsibility — this pass is single-shot.
    ///
    /// # Errors
    /// Surfaces the first `Spawner::start` error encountered.
    pub async fn restart_exited<S: Spawner + ?Sized>(
        &self,
        spawner: &S,
        trackers: &mut BTreeMap<String, RestartTracker>,
        now: Instant,
    ) -> Result<Vec<RestartOutcome>> {
        let exited: Vec<(String, String)> = {
            let g = self.state.lock().await;
            g.children
                .iter()
                .filter(|c| c.status == ChildStatus::Exited)
                .map(|c| (c.name.clone(), c.name.clone()))
                .collect()
        };
        let mut outcomes = Vec::with_capacity(exited.len());
        for (name, exec_name) in exited {
            let tracker = trackers.entry(name.clone()).or_default();
            let delay = tracker.record(now);
            let restarts_in_window = tracker.restarts_in_window();
            if delay.is_zero() {
                let started = spawner
                    .start(&ChildSpec {
                        name: name.clone(),
                        exec: exec_name,
                        args: Vec::new(),
                    })
                    .with_context(|| format!("restart child {name}"))?;
                let updated_count = {
                    let mut g = self.state.lock().await;
                    g.children
                        .iter_mut()
                        .find(|c| c.name == name)
                        .map_or(0, |slot| {
                            slot.status = ChildStatus::Running;
                            slot.child_pid = Some(started.child_pid);
                            slot.restart_count = slot.restart_count.saturating_add(1);
                            slot.last_event = Some(format!(
                                "restarted pid={} (#{})",
                                started.child_pid, slot.restart_count
                            ));
                            slot.restart_count
                        })
                };
                outcomes.push(RestartOutcome::Restarted {
                    name,
                    child_pid: started.child_pid,
                    restart_count: updated_count,
                });
            } else {
                {
                    let mut g = self.state.lock().await;
                    if let Some(slot) = g.children.iter_mut().find(|c| c.name == name) {
                        slot.status = ChildStatus::Backoff;
                        slot.last_event = Some(format!(
                            "backoff ({restarts_in_window} restarts in window)"
                        ));
                    }
                }
                outcomes.push(RestartOutcome::EnteredBackoff {
                    name,
                    restarts_in_window,
                });
            }
        }
        Ok(outcomes)
    }

    /// Flip [`ChildStatus::Backoff`] slots back to [`ChildStatus::Exited`]
    /// once their entry in `due` has elapsed (`now >= due[name]`).
    /// Returns the names that just transitioned, in canonical order.
    /// Slots not in `Backoff` and slots without a `due` entry are skipped.
    pub async fn clear_expired_backoffs(
        &self,
        due: &BTreeMap<String, Instant>,
        now: Instant,
    ) -> Vec<String> {
        let mut transitions = Vec::new();
        {
            let mut g = self.state.lock().await;
            for slot in &mut g.children {
                if slot.status != ChildStatus::Backoff {
                    continue;
                }
                if let Some(&until) = due.get(&slot.name) {
                    if now >= until {
                        slot.status = ChildStatus::Exited;
                        slot.last_event = Some("backoff expired; retrying".into());
                        transitions.push(slot.name.clone());
                    }
                }
            }
        }
        transitions
    }

    /// One iteration of the supervisor loop. The order is load-bearing:
    /// clearing backoffs first means a slot whose process is still gone
    /// can be restarted in the same tick. Mutates `trackers` and
    /// `backoff_until` in place so the caller can hand both back next
    /// tick. Failures inside individual phases are logged and surfaced
    /// via tracing, never returned — keeping the loop alive matches the
    /// PRD's "keep trying" intent.
    pub async fn observer_pass<S: Spawner + ?Sized>(
        &self,
        spawner: &S,
        trackers: &mut BTreeMap<String, RestartTracker>,
        backoff_until: &mut BTreeMap<String, Instant>,
        now: Instant,
    ) {
        let cleared = self.clear_expired_backoffs(backoff_until, now).await;
        for name in cleared {
            backoff_until.remove(&name);
            info!(child = %name, "backoff expired; retrying");
        }
        match self.observe_exits(spawner).await {
            Ok(transitions) => {
                for name in transitions {
                    info!(child = %name, "child exited");
                }
            }
            Err(e) => warn!(error = %e, "observe_exits failed"),
        }
        match self.restart_exited(spawner, trackers, now).await {
            Ok(outcomes) => {
                for outcome in outcomes {
                    match outcome {
                        RestartOutcome::Restarted {
                            name,
                            child_pid,
                            restart_count,
                        } => {
                            info!(
                                child = %name,
                                pid = child_pid,
                                count = restart_count,
                                "child restarted",
                            );
                        }
                        RestartOutcome::EnteredBackoff {
                            name,
                            restarts_in_window,
                        } => {
                            warn!(
                                child = %name,
                                restarts = restarts_in_window,
                                backoff_secs = BACKOFF_SECS,
                                "entered backoff",
                            );
                            backoff_until
                                .insert(name, now + Duration::from_secs(BACKOFF_SECS));
                        }
                    }
                }
            }
            Err(e) => warn!(error = %e, "restart_exited failed"),
        }
    }

    /// Run the supervisor loop forever at `interval`. Owns the
    /// `RestartTracker` map and the `backoff_until` schedule. Designed
    /// to be spawned via `tokio::spawn` and aborted on shutdown by the
    /// caller.
    pub async fn run_observer_loop<S: Spawner + ?Sized>(
        self,
        spawner: &S,
        interval: Duration,
    ) {
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let mut backoff_until: BTreeMap<String, Instant> = BTreeMap::new();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let now = Instant::now();
            self.observer_pass(spawner, &mut trackers, &mut backoff_until, now)
                .await;
        }
    }

    /// Bind `socket_path` and serve forever. Parent directory is
    /// created if missing; any stale socket file is removed first.
    ///
    /// # Errors
    /// Bubbles bind, mkdir, and accept errors. Per-connection failures
    /// are logged and do not stop the listener.
    pub async fn serve(&self, socket_path: &Path) -> Result<()> {
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        // Remove a stale socket file if any; ignore NotFound.
        if let Err(e) = tokio::fs::remove_file(socket_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e).with_context(|| {
                    format!("remove stale socket {}", socket_path.display())
                });
            }
        }
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        info!(socket = %socket_path.display(), "wmd-init listening");
        loop {
            let (stream, _peer) = listener
                .accept()
                .await
                .context("accept on init.sock")?;
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, state).await {
                    error!(error = %e, "client connection failed");
                }
            });
        }
    }
}

async fn handle_conn(
    stream: UnixStream,
    state: Arc<Mutex<SupervisorView>>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.context("read_line")?;
        if n == 0 {
            debug!("client closed connection");
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        let resp = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => dispatch(req, &state).await,
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        let mut bytes = serde_json::to_vec(&resp).context("serialize response")?;
        bytes.push(b'\n');
        write_half.write_all(&bytes).await.context("write response")?;
        write_half.flush().await.context("flush response")?;
    }
}

async fn dispatch(req: Request, state: &Mutex<SupervisorView>) -> Response {
    match req {
        Request::Status => {
            let view = state.lock().await.clone();
            Response::Status { view }
        }
        Request::Mute => {
            state.lock().await.muted = true;
            Response::Ack
        }
        Request::Unmute => {
            state.lock().await.muted = false;
            Response::Ack
        }
        Request::Restart { .. } => Response::NotImplemented {
            op: "restart".into(),
        },
        Request::Logs { child, tail } => match crate::tail_child_log(&child, tail) {
            Ok(lines) => Response::Logs { child, lines },
            Err(e) => Response::Error {
                message: format!("tail {child}: {e}"),
            },
        },
        Request::Say { .. } => Response::NotImplemented { op: "say".into() },
    }
}

/// Connect to a running supervisor and issue one request, returning
/// the single response. Used by `wm` and by integration tests.
///
/// # Errors
/// Surfaces connect, write, read, and parse errors.
pub async fn client_roundtrip(socket_path: &Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect {}", socket_path.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut bytes = serde_json::to_vec(req).context("serialize request")?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await.context("send request")?;
    write_half.flush().await.context("flush request")?;
    write_half.shutdown().await.ok();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.context("read response")?;
    if n == 0 {
        anyhow::bail!("supervisor closed connection without responding");
    }
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let resp: Response = serde_json::from_str(trimmed).context("parse response")?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ChildStatus;

    #[tokio::test(flavor = "current_thread")]
    async fn new_supervisor_has_five_pending_children() {
        let s = Supervisor::new();
        let view = s.snapshot().await;
        assert_eq!(view.children.len(), 5);
        assert!(view.children.iter().all(|c| c.status == ChildStatus::Pending));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_status_returns_pending_view() {
        let view = SupervisorView::pending_for(&canonical_children());
        let state = Mutex::new(view);
        let resp = dispatch(Request::Status, &state).await;
        assert!(matches!(resp, Response::Status { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_mute_acks_and_flips_state() {
        let state = Mutex::new(SupervisorView::pending_for(&canonical_children()));
        assert!(!state.lock().await.muted);
        let resp = dispatch(Request::Mute, &state).await;
        assert_eq!(resp, Response::Ack);
        assert!(state.lock().await.muted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_unmute_acks_and_clears_state() {
        let mut view = SupervisorView::pending_for(&canonical_children());
        view.muted = true;
        let state = Mutex::new(view);
        let resp = dispatch(Request::Unmute, &state).await;
        assert_eq!(resp, Response::Ack);
        assert!(!state.lock().await.muted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_mute_then_unmute_toggles_muted() {
        let state = Mutex::new(SupervisorView::pending_for(&canonical_children()));
        assert_eq!(dispatch(Request::Mute, &state).await, Response::Ack);
        assert!(state.lock().await.muted);
        assert_eq!(dispatch(Request::Unmute, &state).await, Response::Ack);
        assert!(!state.lock().await.muted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_status_after_mute_reflects_muted_flag() {
        let state = Mutex::new(SupervisorView::pending_for(&canonical_children()));
        let _ = dispatch(Request::Mute, &state).await;
        let resp = dispatch(Request::Status, &state).await;
        match resp {
            Response::Status { view } => assert!(view.muted),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_logs_returns_empty_for_unknown_child() {
        let state = Mutex::new(SupervisorView::pending_for(&canonical_children()));
        let resp = dispatch(
            Request::Logs {
                child: "wm-nonexistent-7d3f9c".into(),
                tail: 5,
            },
            &state,
        )
        .await;
        match resp {
            Response::Logs { child, lines } => {
                assert_eq!(child, "wm-nonexistent-7d3f9c");
                assert!(lines.is_empty(), "missing log file should yield empty tail");
            }
            other => panic!("expected Response::Logs, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_all_marks_all_children_running_in_canonical_order() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(20_000);
        let sup = Supervisor::new();
        sup.start_all(&stub).await?;
        let view = sup.snapshot().await;
        let names: Vec<&str> = view.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd"]
        );
        for child in &view.children {
            assert_eq!(child.status, ChildStatus::Running, "{}", child.name);
            let pid = child
                .child_pid
                .ok_or_else(|| anyhow::anyhow!("{} missing pid", child.name))?;
            assert_eq!(
                child.last_event.as_deref(),
                Some(format!("running pid={pid}").as_str()),
            );
        }
        assert_eq!(
            stub.called_names(),
            vec!["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd"],
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observe_exits_flips_running_to_exited_when_pid_gone() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(40_000);
        let sup = Supervisor::new();
        sup.start_all(&stub).await?;
        let pid_audio = sup
            .snapshot()
            .await
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .and_then(|c| c.child_pid)
            .ok_or_else(|| anyhow::anyhow!("wm-audio missing pid"))?;
        stub.mark_exited(pid_audio);
        let transitions = sup.observe_exits(&stub).await?;
        assert_eq!(transitions, vec!["wm-audio".to_string()]);
        let view = sup.snapshot().await;
        let audio = view
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .ok_or_else(|| anyhow::anyhow!("wm-audio absent"))?;
        assert_eq!(audio.status, ChildStatus::Exited);
        assert_eq!(
            audio.last_event.as_deref(),
            Some(format!("exited (lost pid {pid_audio})").as_str())
        );
        // Siblings still Running.
        for other in view.children.iter().filter(|c| c.name != "wm-audio") {
            assert_eq!(other.status, ChildStatus::Running, "{}", other.name);
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observe_exits_is_idempotent_when_nothing_changed() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(50_000);
        let sup = Supervisor::new();
        sup.start_all(&stub).await?;
        let t1 = sup.observe_exits(&stub).await?;
        let t2 = sup.observe_exits(&stub).await?;
        assert!(t1.is_empty());
        assert!(t2.is_empty());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observe_exits_skips_pending_and_starting_slots() -> Result<()> {
        // Nothing started — observation pass must do nothing.
        let stub = crate::spawn::testing::StubSpawner::new(60_000);
        let sup = Supervisor::new();
        let transitions = sup.observe_exits(&stub).await?;
        assert!(transitions.is_empty());
        // And subsequent snapshot shows all five still Pending.
        let view = sup.snapshot().await;
        for c in &view.children {
            assert_eq!(c.status, ChildStatus::Pending);
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_exited_relaunches_exited_child_and_increments_count() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(80_000);
        let sup = Supervisor::new();
        sup.start_all(&stub).await?;
        let pid_tts = sup
            .snapshot()
            .await
            .children
            .iter()
            .find(|c| c.name == "wm-tts")
            .and_then(|c| c.child_pid)
            .ok_or_else(|| anyhow::anyhow!("wm-tts missing pid"))?;
        stub.mark_exited(pid_tts);
        let _ = sup.observe_exits(&stub).await?;

        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let outcomes = sup
            .restart_exited(&stub, &mut trackers, Instant::now())
            .await?;
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        match &outcomes[0] {
            RestartOutcome::Restarted {
                name,
                child_pid,
                restart_count,
            } => {
                assert_eq!(name, "wm-tts");
                assert_ne!(*child_pid, pid_tts, "expected fresh PID");
                assert_eq!(*restart_count, 1);
            }
            other => panic!("expected Restarted, got {other:?}"),
        }

        let view = sup.snapshot().await;
        let tts = view
            .children
            .iter()
            .find(|c| c.name == "wm-tts")
            .ok_or_else(|| anyhow::anyhow!("wm-tts absent"))?;
        assert_eq!(tts.status, ChildStatus::Running);
        assert_eq!(tts.restart_count, 1);
        assert_ne!(tts.child_pid, Some(pid_tts));
        assert!(tts
            .last_event
            .as_deref()
            .is_some_and(|s| s.starts_with("restarted pid=") && s.contains("(#1)")));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_exited_flips_to_backoff_on_storm_threshold() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(90_000);
        let sup = Supervisor::new();
        sup.start_specs(
            &stub,
            &[ChildSpec {
                name: "wm-audio".into(),
                exec: "wm-audio".into(),
                args: vec![],
            }],
        )
        .await?;
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let base = Instant::now();
        // Five back-to-back crash + restart cycles inside the window.
        for i in 0..5 {
            let pid = sup
                .snapshot()
                .await
                .children
                .iter()
                .find(|c| c.name == "wm-audio")
                .and_then(|c| c.child_pid)
                .ok_or_else(|| anyhow::anyhow!("wm-audio missing pid on cycle {i}"))?;
            stub.mark_exited(pid);
            let _ = sup.observe_exits(&stub).await?;
            let outcomes = sup
                .restart_exited(
                    &stub,
                    &mut trackers,
                    base + std::time::Duration::from_millis(i * 100),
                )
                .await?;
            assert_eq!(outcomes.len(), 1, "cycle {i}: {outcomes:?}");
            if i < 4 {
                assert!(
                    matches!(&outcomes[0], RestartOutcome::Restarted { .. }),
                    "cycle {i} expected restart, got {:?}",
                    outcomes[0]
                );
            } else {
                match &outcomes[0] {
                    RestartOutcome::EnteredBackoff {
                        name,
                        restarts_in_window,
                    } => {
                        assert_eq!(name, "wm-audio");
                        assert_eq!(*restarts_in_window, 5);
                    }
                    other => panic!("cycle 4 expected EnteredBackoff, got {other:?}"),
                }
            }
        }
        let view = sup.snapshot().await;
        let audio = view
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .ok_or_else(|| anyhow::anyhow!("wm-audio absent"))?;
        assert_eq!(audio.status, ChildStatus::Backoff);
        assert_eq!(audio.restart_count, 4, "4 successful restarts before storm");
        assert!(audio
            .last_event
            .as_deref()
            .is_some_and(|s| s.starts_with("backoff (5 restarts in window)")));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_exited_is_noop_when_nothing_exited() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(100_000);
        let sup = Supervisor::new();
        sup.start_all(&stub).await?;
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let outcomes = sup
            .restart_exited(&stub, &mut trackers, Instant::now())
            .await?;
        assert!(outcomes.is_empty());
        assert!(trackers.is_empty(), "tracker map untouched when no work");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_exited_handles_each_child_with_its_own_tracker() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(110_000);
        let sup = Supervisor::new();
        sup.start_all(&stub).await?;
        // Crash two of the five.
        let pids: Vec<(String, u32)> = sup
            .snapshot()
            .await
            .children
            .iter()
            .filter(|c| c.name == "wm-stt" || c.name == "wm-dialog")
            .filter_map(|c| c.child_pid.map(|p| (c.name.clone(), p)))
            .collect();
        assert_eq!(pids.len(), 2);
        for (_, pid) in &pids {
            stub.mark_exited(*pid);
        }
        let _ = sup.observe_exits(&stub).await?;

        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let outcomes = sup
            .restart_exited(&stub, &mut trackers, Instant::now())
            .await?;
        assert_eq!(outcomes.len(), 2);
        // Each child should have exactly one tracker entry with one restart.
        assert_eq!(trackers.len(), 2);
        for name in ["wm-stt", "wm-dialog"] {
            let t = trackers
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("no tracker for {name}"))?;
            assert_eq!(t.restarts_in_window(), 1, "{name}");
        }
        let view = sup.snapshot().await;
        for name in ["wm-stt", "wm-dialog"] {
            let c = view
                .children
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| anyhow::anyhow!("{name} absent"))?;
            assert_eq!(c.status, ChildStatus::Running);
            assert_eq!(c.restart_count, 1);
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_specs_records_pid_from_spawner() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(7_000);
        let sup = Supervisor::new();
        sup.start_specs(
            &stub,
            &[ChildSpec {
                name: "wm-audio".into(),
                exec: "wm-audio".into(),
                args: vec![],
            }],
        )
        .await?;
        let view = sup.snapshot().await;
        let audio = view
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .ok_or_else(|| anyhow::anyhow!("wm-audio absent"))?;
        assert_eq!(audio.status, ChildStatus::Running);
        assert_eq!(audio.child_pid, Some(7_001));
        // The other four stay pending.
        for other in view.children.iter().filter(|c| c.name != "wm-audio") {
            assert_eq!(other.status, ChildStatus::Pending);
            assert!(other.child_pid.is_none());
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_expired_backoffs_flips_due_slots_to_exited() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(200_000);
        let sup = Supervisor::new();
        sup.start_specs(
            &stub,
            &[ChildSpec {
                name: "wm-audio".into(),
                exec: "wm-audio".into(),
                args: vec![],
            }],
        )
        .await?;
        // Saturate the tracker so restart_exited flips to Backoff on next exit.
        let pid = sup
            .snapshot()
            .await
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .and_then(|c| c.child_pid)
            .ok_or_else(|| anyhow::anyhow!("audio pid missing"))?;
        stub.mark_exited(pid);
        let _ = sup.observe_exits(&stub).await?;
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        // Pre-fill window so the first restart attempt trips backoff.
        let t = trackers.entry("wm-audio".into()).or_default();
        let base = Instant::now();
        for i in 0..crate::RESTART_STORM_THRESHOLD {
            let _ = t.record(base + std::time::Duration::from_millis(u64::from(i)));
        }
        let outcomes = sup
            .restart_exited(&stub, &mut trackers, base)
            .await?;
        assert!(matches!(
            outcomes.as_slice(),
            [RestartOutcome::EnteredBackoff { .. }]
        ));

        let mut due: BTreeMap<String, Instant> = BTreeMap::new();
        let until = base + std::time::Duration::from_secs(crate::BACKOFF_SECS);
        due.insert("wm-audio".into(), until);

        // Before the deadline → no transition.
        let early = sup.clear_expired_backoffs(&due, base).await;
        assert!(early.is_empty(), "premature flip: {early:?}");
        let v = sup.snapshot().await;
        assert_eq!(
            v.children
                .iter()
                .find(|c| c.name == "wm-audio")
                .map(|c| c.status),
            Some(ChildStatus::Backoff),
        );

        // Past the deadline → flips Backoff → Exited.
        let after = until + std::time::Duration::from_millis(1);
        let cleared = sup.clear_expired_backoffs(&due, after).await;
        assert_eq!(cleared, vec!["wm-audio".to_string()]);
        let v = sup.snapshot().await;
        let slot = v
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .ok_or_else(|| anyhow::anyhow!("audio missing"))?;
        assert_eq!(slot.status, ChildStatus::Exited);
        assert_eq!(
            slot.last_event.as_deref(),
            Some("backoff expired; retrying"),
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_expired_backoffs_skips_non_backoff_slots() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(210_000);
        let sup = Supervisor::new();
        sup.start_all(&stub).await?;
        let now = Instant::now();
        let mut due: BTreeMap<String, Instant> = BTreeMap::new();
        // Mark every child due in the past — but none are in Backoff.
        for name in ["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd"] {
            due.insert(name.into(), now - std::time::Duration::from_secs(10));
        }
        let cleared = sup.clear_expired_backoffs(&due, now).await;
        assert!(cleared.is_empty(), "flipped a non-backoff slot: {cleared:?}");
        let v = sup.snapshot().await;
        assert!(v.children.iter().all(|c| c.status == ChildStatus::Running));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observer_pass_restarts_a_freshly_crashed_child() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(300_000);
        let sup = Supervisor::new();
        sup.start_specs(
            &stub,
            &[ChildSpec {
                name: "wm-audio".into(),
                exec: "wm-audio".into(),
                args: vec![],
            }],
        )
        .await?;
        let pid = sup
            .snapshot()
            .await
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .and_then(|c| c.child_pid)
            .ok_or_else(|| anyhow::anyhow!("audio pid missing"))?;
        stub.mark_exited(pid);
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let mut backoff_until: BTreeMap<String, Instant> = BTreeMap::new();
        sup.observer_pass(&stub, &mut trackers, &mut backoff_until, Instant::now())
            .await;
        let v = sup.snapshot().await;
        let slot = v
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .ok_or_else(|| anyhow::anyhow!("audio absent"))?;
        assert_eq!(slot.status, ChildStatus::Running);
        assert_eq!(slot.restart_count, 1);
        assert!(backoff_until.is_empty());
        assert_eq!(
            trackers
                .get("wm-audio")
                .map(super::RestartTracker::restarts_in_window),
            Some(1),
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observer_pass_records_backoff_when_storm_trips() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(310_000);
        let sup = Supervisor::new();
        sup.start_specs(
            &stub,
            &[ChildSpec {
                name: "wm-audio".into(),
                exec: "wm-audio".into(),
                args: vec![],
            }],
        )
        .await?;
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let mut backoff_until: BTreeMap<String, Instant> = BTreeMap::new();
        // Crash → restart cycle, repeated until the window saturates.
        // The first RESTART_STORM_THRESHOLD - 1 ticks restart cleanly;
        // the threshold-th flips to backoff and records a due time.
        let base = Instant::now();
        for i in 0..crate::RESTART_STORM_THRESHOLD {
            // Whatever PID is currently in the slot, mark it gone.
            let pid = sup
                .snapshot()
                .await
                .children
                .iter()
                .find(|c| c.name == "wm-audio")
                .and_then(|c| c.child_pid)
                .ok_or_else(|| anyhow::anyhow!("audio pid missing at iter {i}"))?;
            stub.mark_exited(pid);
            sup.observer_pass(
                &stub,
                &mut trackers,
                &mut backoff_until,
                base + Duration::from_millis(u64::from(i) * 10),
            )
            .await;
        }
        let v = sup.snapshot().await;
        let slot = v
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .ok_or_else(|| anyhow::anyhow!("audio absent"))?;
        assert_eq!(slot.status, ChildStatus::Backoff);
        let until = backoff_until
            .get("wm-audio")
            .copied()
            .ok_or_else(|| anyhow::anyhow!("backoff_until missing entry"))?;
        // The recorded due time is ~BACKOFF_SECS from the last tick.
        let last_tick = base
            + Duration::from_millis(
                u64::from(crate::RESTART_STORM_THRESHOLD.saturating_sub(1)) * 10,
            );
        let expected = last_tick + Duration::from_secs(BACKOFF_SECS);
        assert_eq!(until, expected);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observer_pass_clears_backoff_then_retries_in_a_later_tick() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(320_000);
        let sup = Supervisor::new();
        sup.start_specs(
            &stub,
            &[ChildSpec {
                name: "wm-audio".into(),
                exec: "wm-audio".into(),
                args: vec![],
            }],
        )
        .await?;
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let mut backoff_until: BTreeMap<String, Instant> = BTreeMap::new();
        let base = Instant::now();
        // Saturate the window so the next exit trips backoff.
        for i in 0..crate::RESTART_STORM_THRESHOLD {
            let pid = sup
                .snapshot()
                .await
                .children
                .iter()
                .find(|c| c.name == "wm-audio")
                .and_then(|c| c.child_pid)
                .ok_or_else(|| anyhow::anyhow!("audio pid missing at iter {i}"))?;
            stub.mark_exited(pid);
            sup.observer_pass(
                &stub,
                &mut trackers,
                &mut backoff_until,
                base + Duration::from_millis(u64::from(i) * 10),
            )
            .await;
        }
        assert_eq!(
            sup.snapshot()
                .await
                .children
                .iter()
                .find(|c| c.name == "wm-audio")
                .map(|c| c.status),
            Some(ChildStatus::Backoff),
        );
        // Tick well past both the recorded due time AND the rolling
        // window, so clear_expired_backoffs flips Backoff → Exited and
        // restart_exited sees an empty history (no longer saturated) and
        // relaunches the child cleanly in the same pass.
        let later = base
            + Duration::from_secs(crate::RESTART_WINDOW_SECS + BACKOFF_SECS + 1);
        sup.observer_pass(&stub, &mut trackers, &mut backoff_until, later)
            .await;
        let v = sup.snapshot().await;
        let slot = v
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .ok_or_else(|| anyhow::anyhow!("audio absent"))?;
        assert_eq!(slot.status, ChildStatus::Running);
        assert!(
            !backoff_until.contains_key("wm-audio"),
            "backoff_until should be cleared after successful retry",
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_expired_backoffs_ignores_slots_without_due_entry() -> Result<()> {
        let stub = crate::spawn::testing::StubSpawner::new(220_000);
        let sup = Supervisor::new();
        sup.start_specs(
            &stub,
            &[ChildSpec {
                name: "wm-audio".into(),
                exec: "wm-audio".into(),
                args: vec![],
            }],
        )
        .await?;
        // Force the slot to Backoff via the tracker-saturation path.
        let pid = sup
            .snapshot()
            .await
            .children
            .iter()
            .find(|c| c.name == "wm-audio")
            .and_then(|c| c.child_pid)
            .ok_or_else(|| anyhow::anyhow!("audio pid missing"))?;
        stub.mark_exited(pid);
        let _ = sup.observe_exits(&stub).await?;
        let mut trackers: BTreeMap<String, RestartTracker> = BTreeMap::new();
        let base = Instant::now();
        let t = trackers.entry("wm-audio".into()).or_default();
        for i in 0..crate::RESTART_STORM_THRESHOLD {
            let _ = t.record(base + std::time::Duration::from_millis(u64::from(i)));
        }
        let _ = sup.restart_exited(&stub, &mut trackers, base).await?;
        // Empty `due` map → no transition even with elapsed time.
        let due: BTreeMap<String, Instant> = BTreeMap::new();
        let way_later = base + std::time::Duration::from_secs(crate::BACKOFF_SECS * 10);
        let cleared = sup.clear_expired_backoffs(&due, way_later).await;
        assert!(cleared.is_empty());
        let v = sup.snapshot().await;
        assert_eq!(
            v.children
                .iter()
                .find(|c| c.name == "wm-audio")
                .map(|c| c.status),
            Some(ChildStatus::Backoff),
        );
        Ok(())
    }
}
