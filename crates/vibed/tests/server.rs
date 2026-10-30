//! The socket server: binding safely, and framing.

mod common;

use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

use common::{Conn, HELLO, golden_info, start};
use tokio::io::AsyncWriteExt as _;
use vibed::server::{self, BindError};

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn bind_creates_a_private_dir_and_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("a/b/run/vibed.sock");
    let (_listener, guard) = server::bind(&path).unwrap();
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
    assert_eq!(mode(&path), 0o600);
    assert!(server::lock_path(&path).exists());
    drop(guard);
    assert!(!path.exists(), "dropping the guard removes the socket");
}

#[test]
fn bind_refuses_a_dir_open_to_others() {
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let err = server::bind(&tmp.path().join("vibed.sock")).unwrap_err();
    assert!(
        matches!(err, BindError::InsecureDir { mode: 0o755, .. }),
        "{err}"
    );
}

#[test]
fn one_daemon_per_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("run/vibed.sock");
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let (_listener, guard) = server::bind(&path).unwrap();
    let err = server::bind(&path).unwrap_err();
    assert!(matches!(err, BindError::AlreadyRunning { .. }), "{err}");
    assert!(
        path.exists(),
        "a refused bind leaves the running socket alone"
    );
    drop(guard);
    server::bind(&path).unwrap();
}

#[test]
fn stale_socket_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("run/vibed.sock");
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    // A socket left behind by a daemon that died without cleaning up.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let (listener, _guard) = server::bind(&path).unwrap();
    std::os::unix::net::UnixStream::connect(&path).unwrap();
    drop(listener);
}

#[test]
fn other_files_are_never_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("run/vibed.sock");
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&path, "keep me").unwrap();
    let err = server::bind(&path).unwrap_err();
    assert!(matches!(err, BindError::NotASocket { .. }), "{err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "keep me");
}

#[test]
fn lock_path_never_collides_with_the_socket() {
    let p = std::path::Path::new("/r/vibed.lock");
    assert_eq!(
        server::lock_path(p),
        std::path::Path::new("/r/vibed.lock.lock")
    );
}

#[tokio::test]
async fn crlf_and_blank_lines() {
    let server = start(golden_info(), 1 << 20);
    let mut conn = Conn::open(&server.path).await;
    conn.send(b"\n  \n\r\n").await;
    conn.send(HELLO.as_bytes()).await;
    conn.send(b"\r\n").await;
    assert_eq!(conn.reply().await["result"]["daemon"], "vibed");
}

#[tokio::test]
async fn last_message_without_newline_is_answered() {
    let server = start(golden_info(), 1 << 20);
    let mut conn = Conn::open(&server.path).await;
    conn.send(HELLO.as_bytes()).await;
    conn.write.shutdown().await.unwrap();
    assert_eq!(conn.reply().await["id"], "probe");
    assert_eq!(conn.line().await, None, "then the daemon closes");
}

#[tokio::test]
async fn message_at_the_limit_is_served() {
    let server = start(golden_info(), HELLO.len());
    let mut conn = Conn::open(&server.path).await;
    conn.send(HELLO.as_bytes()).await;
    conn.send(b"\n").await;
    assert_eq!(conn.reply().await["id"], "probe");
}

#[tokio::test]
async fn message_over_the_limit_is_refused_and_closed() {
    let server = start(golden_info(), HELLO.len() - 1);
    let mut conn = Conn::open(&server.path).await;
    conn.send(HELLO.as_bytes()).await;
    conn.send(b"\n").await;
    let reply = conn.reply().await;
    assert_eq!(reply["id"], serde_json::Value::Null);
    assert_eq!(reply["error"]["code"], -32600);
    let limit = HELLO.len() - 1;
    assert_eq!(
        reply["error"]["message"],
        format!("Message too large: the limit is {limit} bytes")
    );
    assert_eq!(conn.line().await, None);
}

#[tokio::test]
async fn connections_are_independent() {
    let server = start(golden_info(), 1 << 20);
    let mut idle = Conn::open(&server.path).await;
    // A half-sent message on one connection doesn't hold up another.
    idle.send(b"{\"jsonrpc\":").await;
    let mut busy = Conn::open(&server.path).await;
    busy.send(format!("{HELLO}\n").as_bytes()).await;
    assert_eq!(busy.reply().await["id"], "probe");
    idle.send(b"\"2.0\",\"id\":9,\"method\":\"daemon.nope\"}\n")
        .await;
    assert_eq!(idle.reply().await["error"]["code"], -32601);
}
