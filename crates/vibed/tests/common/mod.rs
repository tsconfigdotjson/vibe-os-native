//! Shared helpers for the vibed integration tests.

#![allow(dead_code, unreachable_pub)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;
use vibe_core::paths::Layout;
use vibed::core::{Config, Core, DaemonInfo};
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

/// The home directory the examples are written with; replaced by the real
/// one when they're replayed.
pub const GOLDEN_HOME: &str = "/Users/me";

/// 2026-10-30T12:00:00Z: the time in every example.
pub const GOLDEN_NOW: i64 = 1_793_361_600;

pub struct Example {
    pub name: String,
    /// The line to send, without its newline; `None` for a notification
    /// example, which isn't replayed.
    pub line: Option<Vec<u8>>,
    /// The expected response, or `Null` for none.
    pub response: Value,
    /// The notifications expected after the response.
    pub events: Vec<Value>,
}

/// The examples, with [`GOLDEN_HOME`] replaced by `home`.
pub fn examples(home: &Path) -> Vec<Example> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/examples");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    let home = home.to_str().unwrap();
    paths
        .into_iter()
        .map(|p| {
            let text = fs::read_to_string(&p).unwrap().replace(GOLDEN_HOME, home);
            let v: Value = serde_json::from_str(&text).unwrap();
            let line = if v.get("notification").is_some() {
                None
            } else {
                Some(match v.get("request_raw") {
                    Some(raw) => raw.as_str().unwrap().as_bytes().to_vec(),
                    None => serde_json::to_vec(&v["request"]).unwrap(),
                })
            };
            Example {
                name: p.file_name().unwrap().to_string_lossy().into_owned(),
                line,
                response: v["response"].clone(),
                events: v
                    .get("events")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            }
        })
        .collect()
}

pub fn git(args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A git repo with one commit.
pub fn repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let d = dir.to_str().unwrap();
    git(&["init", "-q", d]);
    fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(&["-C", d, "add", "."]);
    git(&["-C", d, "commit", "-q", "-m", "first"]);
}

/// A stand-in for `gh`: `auth status` says it's logged in as `me`, and
/// `repo clone OWNER/NAME DEST` clones `<remotes>/NAME` if it exists, with
/// origin pointing at GitHub as a real clone's would.
pub fn fake_gh(bin: &Path, remotes: &Path) -> PathBuf {
    fs::create_dir_all(bin).unwrap();
    let gh = bin.join("gh");
    let script = format!(
        r#"#!/bin/sh
case "$1 $2" in
"auth status")
  echo "github.com"
  echo "  ✓ Logged in to github.com account me (keyring)"
  echo "  - Token: gho_************************************"
  ;;
"repo clone")
  name=${{3#*/}}
  if [ -d "{remotes}/$name" ]; then
    git clone -q "{remotes}/$name" "$4" &&
      git -C "$4" remote set-url origin "https://github.com/$3.git"
  else
    echo "GraphQL: Could not resolve to a Repository with the name '$3'. (repository)" >&2
    exit 1
  fi
  ;;
*)
  echo "fake gh: unexpected $*" >&2
  exit 2
  ;;
esac
"#,
        remotes = remotes.display()
    );
    fs::write(&gh, script).unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    gh
}

/// The world the examples were recorded in: a Mac whose home has
/// `src/app` checked out, a GitHub with `me/web` on it, a fixed clock and
/// fixed key randomness.
pub struct Golden {
    pub dir: tempfile::TempDir,
    /// Canonical, so paths vibed canonicalizes match it (macOS's temp dir
    /// is behind a symlink).
    pub home: PathBuf,
    pub gh: PathBuf,
}

impl Golden {
    pub fn new() -> Golden {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        repo(&home.join("src/app"));
        repo(&root.join("remotes/web"));
        let gh = fake_gh(&root.join("bin"), &root.join("remotes"));
        Golden { dir, home, gh }
    }

    pub fn config(&self) -> Config {
        let mut config = Config::new(golden_info(), Layout::new(&self.home, "macos"));
        config.label = "mac-mini".into();
        config.gh = self.gh.clone().into();
        config.clock = Arc::new(|| GOLDEN_NOW);
        // Bytes 0, 1, 2, … for the first key, 32, 33, … for the next.
        let calls = AtomicU8::new(0);
        config.entropy = Arc::new(move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Ok(std::array::from_fn(|i| {
                (i as u8).wrapping_add(n.wrapping_mul(32))
            }))
        });
        config
    }

    pub fn core(&self) -> Arc<Core> {
        Arc::new(Core::open(self.config()).unwrap())
    }
}

/// An in-process server on a private temp dir; dropping it stops it.
pub struct TestServer {
    pub path: PathBuf,
    pub core: Arc<Core>,
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

/// A server for `info`, with its state in a temp home.
pub fn start(info: DaemonInfo, max_message_bytes: usize) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let os = info.os.clone();
    let core = Core::open(Config::new(info, Layout::new(dir.path().join("home"), &os))).unwrap();
    serve(Arc::new(core), dir, max_message_bytes)
}

/// A server for `core`. `dir` is kept until the server is dropped.
pub fn serve(core: Arc<Core>, dir: tempfile::TempDir, max_message_bytes: usize) -> TestServer {
    let path = dir.path().join("run/vibed.sock");
    let (listener, guard) = server::bind(&path).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let (stop, stopped) = oneshot::channel();
    tokio::spawn(server::serve(
        listener,
        core.clone(),
        max_message_bytes,
        async move {
            let _ = stopped.await;
        },
    ));
    TestServer {
        path,
        core,
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

    /// Sends one request and returns its reply.
    pub async fn call(&mut self, method: &str, params: Value) -> Value {
        let req =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        self.send(format!("{req}\n").as_bytes()).await;
        self.reply().await
    }
}

pub const HELLO: &str = r#"{"jsonrpc":"2.0","id":"probe","method":"daemon.hello","params":{"client":"test","protocol":1}}"#;
