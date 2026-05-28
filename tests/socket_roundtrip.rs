//! End-to-end UDS round trip for `Supervisor` ↔ `client_roundtrip`.
//!
//! Validates the iter-3 wire contract: a fresh supervisor binds a
//! tempdir socket, accepts a connection, and returns a 5-child pending
//! `SupervisorView` for `Request::Status`.

#![allow(clippy::expect_used, clippy::panic)]

use std::time::Duration;

use tokio::time::timeout;

use wintermute_platform::protocol::{ChildStatus, Request, Response};
use wintermute_platform::supervisor::{client_roundtrip, Supervisor};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_roundtrip_returns_five_pending_children() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("init.sock");

    let sup = Supervisor::new();
    let server_sock = sock.clone();
    let server_sup = sup.clone();
    let server = tokio::spawn(async move { server_sup.serve(&server_sock).await });

    wait_for_socket(&sock).await;

    let resp = timeout(Duration::from_secs(2), client_roundtrip(&sock, &Request::Status))
        .await
        .expect("client roundtrip timed out")
        .expect("client roundtrip failed");

    let Response::Status { view } = resp else {
        panic!("expected Status response, got {resp:?}");
    };
    assert_eq!(view.protocol_version, 1);
    assert_eq!(view.children.len(), 5);
    let names: Vec<&str> = view.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd"]
    );
    assert!(view.children.iter().all(|c| c.status == ChildStatus::Pending));

    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_request_acks_and_status_reports_muted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("init.sock");

    let sup = Supervisor::new();
    let server_sock = sock.clone();
    let server_sup = sup.clone();
    let server = tokio::spawn(async move { server_sup.serve(&server_sock).await });

    wait_for_socket(&sock).await;

    let resp = timeout(Duration::from_secs(2), client_roundtrip(&sock, &Request::Mute))
        .await
        .expect("client roundtrip timed out")
        .expect("client roundtrip failed");
    assert_eq!(resp, Response::Ack);

    let status = timeout(Duration::from_secs(2), client_roundtrip(&sock, &Request::Status))
        .await
        .expect("status roundtrip timed out")
        .expect("status roundtrip failed");
    let Response::Status { view } = status else {
        panic!("expected Status, got {status:?}");
    };
    assert!(view.muted, "muted flag should be set after Mute");

    let unmute = timeout(Duration::from_secs(2), client_roundtrip(&sock, &Request::Unmute))
        .await
        .expect("unmute roundtrip timed out")
        .expect("unmute roundtrip failed");
    assert_eq!(unmute, Response::Ack);

    let status2 = timeout(Duration::from_secs(2), client_roundtrip(&sock, &Request::Status))
        .await
        .expect("status2 roundtrip timed out")
        .expect("status2 roundtrip failed");
    let Response::Status { view: v2 } = status2 else {
        panic!("expected Status, got {status2:?}");
    };
    assert!(!v2.muted, "muted flag should clear after Unmute");

    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_request_yields_error_response() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("init.sock");

    let sup = Supervisor::new();
    let server_sock = sock.clone();
    let server_sup = sup.clone();
    let server = tokio::spawn(async move { server_sup.serve(&server_sock).await });

    wait_for_socket(&sock).await;

    let stream = UnixStream::connect(&sock).await.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(b"not-json\n")
        .await
        .expect("write");
    write_half.flush().await.expect("flush");

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("read timed out")
        .expect("read");
    let resp: Response = serde_json::from_str(line.trim()).expect("parse");
    assert!(matches!(resp, Response::Error { .. }), "got {resp:?}");

    server.abort();
    let _ = server.await;
}

async fn wait_for_socket(sock: &std::path::Path) {
    for _ in 0..100 {
        if sock.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("socket never appeared at {}", sock.display());
}
