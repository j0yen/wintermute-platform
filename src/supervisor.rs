//! In-memory supervisor state + UDS server.
//!
//! Iter-5 surface: everything iter-4 had (UDS server, `start_all` /
//! `start_specs` against a pluggable [`Spawner`]) plus an
//! `observe_exits` pass that asks the spawner about each `Running`
//! child and transitions `Running → Exited` on PID loss. `wmd-init`
//! now opt-ins via the `--spawn` flag to call `start_all` and the
//! observer loop; without it the binary still serves a pending view
//! (preserves AC9).
//!
//! Restart-on-exit policy lives in iter-6 (`RestartTracker`
//! integration); this iter only detects the transition.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::protocol::{ChildStatus, Request, Response, SupervisorView};
use crate::spawn::Spawner;
use crate::{ChildSpec, canonical_children};

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
        Request::Mute => Response::NotImplemented { op: "mute".into() },
        Request::Unmute => Response::NotImplemented {
            op: "unmute".into(),
        },
        Request::Restart { .. } => Response::NotImplemented {
            op: "restart".into(),
        },
        Request::Logs { .. } => Response::NotImplemented {
            op: "logs".into(),
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
    async fn dispatch_mute_returns_not_implemented_with_op_name() {
        let state = Mutex::new(SupervisorView::pending_for(&canonical_children()));
        let resp = dispatch(Request::Mute, &state).await;
        assert_eq!(resp, Response::NotImplemented { op: "mute".into() });
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
}
