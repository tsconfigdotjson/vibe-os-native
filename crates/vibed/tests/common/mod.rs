//! Shared helpers for the vibed integration tests.

#![allow(dead_code, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;
use vibed::dispatch::DaemonInfo;
use vibed::server::{self, SocketGuard};

/// The daemon the contract examples were recorded against.
pub fn golden_info() -> DaemonInfo {
    DaemonInfo {
        version: "0.1.0".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        pid: 4242,
    }
}

pub struct Example {
    pub name: String,
    /// The line to send, without its newline.
    pub line: Vec<u8>,
    /// The expected response, or `Null` for none.
    pub response: Value,
}

pub fn examples() -> Vec<Example> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/examples");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    paths
        .into_iter()
        .map(|p| {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            let line = match v.get("request_raw") {
                Some(raw) => raw.as_str().unwrap().as_bytes().to_vec(),
                None => serde_json::to_vec(&v["request"]).unwrap(),
            };
            Example {
                name: p.file_name().unwrap().to_string_lossy().into_owned(),
                line,
                response: v["response"].clone(),
            }
        })
        .collect()
}

/// An in-process server on a private temp dir; dropping it stops it.
pub struct TestServer {
    pub path: PathBuf,
    _dir: tempfile::TempDir,
    _guard: SocketGuard,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

pub fn start(info: DaemonInfo, max_message_bytes: usize) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run/vibed.sock");
    let (listener, guard) = server::bind(&path).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let (stop, stopped) = oneshot::channel();
    tokio::spawn(server::serve(
        listener,
        Arc::new(info),
        max_message_bytes,
        async move {
            let _ = stopped.await;
        },
    ));
    TestServer {
        path,
        _dir: dir,
        _guard: guard,
        stop: Some(stop),
    }
}

pub struct Conn {
    pub read: BufReader<OwnedReadHalf>,
    pub write: OwnedWriteHalf,
}

impl Conn {
    pub async fn open(path: &Path) -> Conn {
        let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
        Conn {
            read: BufReader::new(read),
            write,
        }
    }

    pub async fn send(&mut self, bytes: &[u8]) {
        self.write.write_all(bytes).await.unwrap();
    }

    /// The next line, or `None` at EOF. Fails after 10 s.
    pub async fn line(&mut self) -> Option<String> {
        let mut s = String::new();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.read.read_line(&mut s),
        )
        .await
        .expect("timed out waiting for vibed")
        .unwrap();
        if n == 0 {
            return None;
        }
        assert!(s.ends_with('\n'), "a reply ends with a newline: {s:?}");
        Some(s)
    }

    pub async fn reply(&mut self) -> Value {
        let line = self.line().await.expect("a reply, not EOF");
        serde_json::from_str(&line).unwrap()
    }
}

pub const HELLO: &str = r#"{"jsonrpc":"2.0","id":"probe","method":"daemon.hello","params":{"client":"test","protocol":1}}"#;
