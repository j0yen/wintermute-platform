//! In-memory supervisor state + UDS server.
//!
//! Iter-3 surface: bind `socket_path()`, accept clients, answer
//! `Request::Status` with a pending-children snapshot, and respond
//! `NotImplemented` for everything else. Real child spawning, mute
//! plumbing, log tailing, and TTS pass-through land in iter-4+.
//!
//! The supervisor never panics on client errors — bad framing and
//! disconnects are logged and the connection is dropped.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::canonical_children;
use crate::protocol::{Request, Response, SupervisorView};

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
}
