//! Wire protocol for the `wm` ↔ `wmd-init` Unix-socket control plane.
//!
//! Framing: newline-delimited JSON. One `Request` per line from the
//! client; one `Response` per line back. Reused for in-process tests
//! and the real UDS hop.

use serde::{Deserialize, Serialize};

use crate::ChildSpec;

/// Wire version: bumped when an incompatible field is added.
pub const PROTOCOL_VERSION: u32 = 1;

/// One client → supervisor command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Return the current `SupervisorView`.
    Status,
    /// Mute active TTS and suspend wake handling.
    Mute,
    /// Resume from mute.
    Unmute,
    /// Restart all children, or one named child.
    Restart {
        /// Logical child name; `None` ⇒ all.
        child: Option<String>,
    },
    /// Tail the per-child stderr log.
    Logs {
        /// Logical child name.
        child: String,
        /// Trailing line count.
        tail: usize,
    },
    /// Speak `text` via `wm-tts`.
    Say {
        /// Utterance to read aloud.
        text: String,
    },
}

/// One supervisor → client response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Full status snapshot.
    Status {
        /// Snapshot payload.
        view: SupervisorView,
    },
    /// Generic acknowledgement for commands without a payload.
    Ack,
    /// Server understood the op but has not wired it yet.
    NotImplemented {
        /// Echoed op name from the request.
        op: String,
    },
    /// Server-side failure handling the request.
    Error {
        /// Human-readable error text.
        message: String,
    },
}

/// Snapshot of every supervised child.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorView {
    /// Protocol version stamped on every snapshot for forward compatibility.
    pub protocol_version: u32,
    /// One entry per child, in canonical startup order.
    pub children: Vec<ChildState>,
}

/// Per-child lifecycle state surfaced to `wm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildState {
    /// Logical name (e.g. `wm-audio`).
    pub name: String,
    /// Lifecycle phase.
    pub status: ChildStatus,
    /// Seconds since last successful start; `0` while pending or halted.
    pub uptime_secs: u64,
    /// Restart count since supervisor start.
    pub restart_count: u32,
    /// Most recent supervisor event for this child, if any.
    pub last_event: Option<String>,
    /// PID of the live supervised process, set once started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pid: Option<u32>,
}

/// Lifecycle phase reported in `ChildState.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    /// Never started this supervisor run.
    Pending,
    /// Start command issued; process not yet observed running.
    Starting,
    /// Last observation: process alive.
    Running,
    /// Last observation: process is gone (PID no longer in process table).
    /// Restart policy (iter-6) decides whether to flip back to `Starting`.
    Exited,
    /// Restart loop in backoff (>= storm threshold restarts in window).
    Backoff,
    /// Supervisor halted this child intentionally.
    Halted,
}

impl ChildState {
    /// Build a fresh pending entry from a spec.
    #[must_use]
    pub fn pending_from(spec: &ChildSpec) -> Self {
        Self {
            name: spec.name.clone(),
            status: ChildStatus::Pending,
            uptime_secs: 0,
            restart_count: 0,
            last_event: None,
            child_pid: None,
        }
    }
}

impl SupervisorView {
    /// Build an initial view: every child in `Pending`, version stamped.
    #[must_use]
    pub fn pending_for(specs: &[ChildSpec]) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            children: specs.iter().map(ChildState::pending_from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_status_roundtrip() -> Result<(), serde_json::Error> {
        let req = Request::Status;
        let json = serde_json::to_string(&req)?;
        assert_eq!(json, r#"{"op":"status"}"#);
        let back: Request = serde_json::from_str(&json)?;
        assert_eq!(req, back);
        Ok(())
    }

    #[test]
    fn request_restart_with_child_roundtrip() -> Result<(), serde_json::Error> {
        let req = Request::Restart {
            child: Some("wm-stt".into()),
        };
        let json = serde_json::to_string(&req)?;
        let back: Request = serde_json::from_str(&json)?;
        assert_eq!(req, back);
        assert!(json.contains(r#""op":"restart""#));
        assert!(json.contains(r#""child":"wm-stt""#));
        Ok(())
    }

    #[test]
    fn request_logs_with_tail_roundtrip() -> Result<(), serde_json::Error> {
        let req = Request::Logs {
            child: "wm-audio".into(),
            tail: 20,
        };
        let json = serde_json::to_string(&req)?;
        let back: Request = serde_json::from_str(&json)?;
        assert_eq!(req, back);
        Ok(())
    }

    #[test]
    fn request_say_roundtrip() -> Result<(), serde_json::Error> {
        let req = Request::Say {
            text: "hello mary".into(),
        };
        let json = serde_json::to_string(&req)?;
        let back: Request = serde_json::from_str(&json)?;
        assert_eq!(req, back);
        Ok(())
    }

    #[test]
    fn response_status_roundtrip_with_pending_view() -> Result<(), serde_json::Error> {
        let view = SupervisorView::pending_for(&crate::canonical_children());
        let resp = Response::Status { view };
        let json = serde_json::to_string(&resp)?;
        let back: Response = serde_json::from_str(&json)?;
        assert_eq!(resp, back);
        assert!(matches!(back, Response::Status { .. }), "expected Status variant");
        if let Response::Status { view: v } = back {
            assert_eq!(v.protocol_version, PROTOCOL_VERSION);
            assert_eq!(v.children.len(), 5);
            assert!(v.children.iter().all(|c| c.status == ChildStatus::Pending));
        }
        Ok(())
    }

    #[test]
    fn response_not_implemented_carries_op() -> Result<(), serde_json::Error> {
        let resp = Response::NotImplemented {
            op: "mute".into(),
        };
        let json = serde_json::to_string(&resp)?;
        assert!(json.contains(r#""kind":"not_implemented""#));
        assert!(json.contains(r#""op":"mute""#));
        let back: Response = serde_json::from_str(&json)?;
        assert_eq!(resp, back);
        Ok(())
    }

    #[test]
    fn supervisor_view_pending_for_preserves_order() {
        let view = SupervisorView::pending_for(&crate::canonical_children());
        let names: Vec<&str> = view.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd"]
        );
    }
}
