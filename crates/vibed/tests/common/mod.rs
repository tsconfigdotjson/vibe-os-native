//! Shared helpers for the vibed integration tests.

#![allow(dead_code, unreachable_pub)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;
use vibe_core::model::LoginTool;
use vibe_core::paths::Layout;
use vibe_core::rpc::Login;
use vibe_core::units::GIB;
use vibe_env::images::Images;
use vibe_env::testing::{STAND_IN, TestBackend, TestToolbox};
use vibed::agents::{Backends, HostAdmission};
use vibed::core::{Config, Core, DaemonInfo};
use vibed::remote::{BoxFuture, Channel, Installed, Remote};
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

/// What the test backend says every running agent uses: about what a real
/// Claude Code does (PRD: Spike results).
pub const AGENT_MEMORY_USED: u64 = 340 * 1024 * 1024;

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

/// A clone at `dir` of a new repo at `origin`, as a project with an
/// `origin` to fetch is.
pub fn clone_of_new_repo(origin: &Path, dir: &Path) {
    repo(origin);
    fs::create_dir_all(dir.parent().unwrap()).unwrap();
    git(&[
        "clone",
        "-q",
        origin.to_str().unwrap(),
        dir.to_str().unwrap(),
    ]);
}

/// `git -C dir args`'s output, trimmed.
pub fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Installs the stand-in for Claude Code at `bin/claude`.
pub fn stand_in(bin: &Path) -> PathBuf {
    fs::create_dir_all(bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, STAND_IN);
    claude
}

/// Writes an executable script beside `path` and renames it into place, so
/// no test thread can fork while it's open for writing, which makes running
/// it fail with "text file busy" on Linux.
pub fn write_script(path: &Path, body: &str) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".new");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, body).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, path).unwrap();
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
    write_script(&gh, &script);
    gh
}

/// The world the examples were recorded in: a Mac whose home has `src/app`
/// checked out from an `origin` beside it, a GitHub with `me/web` on it, a
/// fixed clock and fixed key randomness, 64G of memory, and the test
/// backend for every environment, with the stand-in for Claude Code.
/// The golden host's Tailscale address (protocol/README.md).
pub const GOLDEN_TAILSCALE: &str = "100.101.102.103";

pub struct Golden {
    pub dir: tempfile::TempDir,
    /// Canonical, so paths vibed canonicalizes match it (macOS's temp dir
    /// is behind a symlink).
    pub home: PathBuf,
    pub gh: PathBuf,
    pub claude: PathBuf,
    sessions: Vec<RunningShim>,
}

impl Golden {
    pub fn new() -> Golden {
        let dir = short_tempdir();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        clone_of_new_repo(&root.join("remotes/app"), &home.join("src/app"));
        repo(&root.join("remotes/web"));
        let gh = fake_gh(&root.join("bin"), &root.join("remotes"));
        let claude = stand_in(&root.join("bin"));
        Golden {
            dir,
            home,
            gh,
            claude,
            sessions: Vec::new(),
        }
    }

    /// Everything else the examples need: the sessions they list,
    /// `quiet-amber-otter` running and two that exited, `brave-copper-lynx`
    /// with code 0 and `calm-silver-heron` killed; and a `.vibe/setup.sh`
    /// in `app` that copies `.env.example` to `.env.local`, so a new
    /// agent's worktree has a change it hasn't committed.
    pub fn for_examples(mut self) -> Golden {
        let origin = self.dir.path().canonicalize().unwrap().join("remotes/app");
        fs::create_dir_all(origin.join(".vibe")).unwrap();
        fs::write(
            origin.join(".vibe/setup.sh"),
            "cp .env.example .env.local\n",
        )
        .unwrap();
        fs::write(
            origin.join(".env.example"),
            "API_URL=http://localhost:8080\n",
        )
        .unwrap();
        let (o, app) = (origin.to_str().unwrap(), self.home.join("src/app"));
        git(&["-C", o, "add", "."]);
        git(&["-C", o, "commit", "-q", "-m", "setup"]);
        git(&["-C", app.to_str().unwrap(), "pull", "-q"]);
        let layout = Layout::new(&self.home, "macos");
        write_record(
            &layout,
            "brave-copper-lynx",
            Some(0),
            None,
            "2026-10-30T11:00:00Z",
        );
        write_record(
            &layout,
            "calm-silver-heron",
            None,
            Some("SIGKILL"),
            "2026-10-30T11:30:00Z",
        );
        self.sessions
            .push(start_shim(&layout, "quiet-amber-otter", &["sleep", "600"]));
        // The toolbox's: base release 1, a Linux home, and a recipe whose
        // update works and whose install fails the first time only.
        let base = layout.linux_dir().join("base-1");
        fs::create_dir_all(&base).unwrap();
        for f in ["Image", "initrd", "base.img"] {
            fs::write(base.join(f), f).unwrap();
        }
        fs::write(layout.linux_dir().join("home-1.img"), "").unwrap();
        // What the toolbox found in that home when it saved it.
        Images::new(layout.linux_dir())
            .note_logins(1, &golden_logins())
            .unwrap();
        fs::create_dir_all(layout.config_dir()).unwrap();
        fs::write(
            layout.toolbox_config(),
            "[[install]]\nname = \"gcloud\"\nrun = \"test -e .tried || { touch .tried; exit 3; }\"\n\n\
             [update]\nrun = [\"true\"]\n",
        )
        .unwrap();
        self
    }

    pub fn config(&self) -> Config {
        let mut config = Config::new(golden_info(), Layout::new(&self.home, "macos"));
        config.label = "mac-mini".into();
        config.gh = self.gh.clone().into();
        config.shim = workspace_bin("vibe-shim").into_os_string();
        config.backends = Backends::all(Arc::new(Seeding {
            backend: TestBackend::new(&self.claude).reporting_memory(AGENT_MEMORY_USED),
            run: Layout::new(&self.home, "macos").run_dir(),
            settled: std::sync::atomic::AtomicBool::new(false),
        }));
        config.toolbox = Some(Arc::new(
            TestToolbox::new(
                Layout::new(&self.home, "macos"),
                vec!["sleep".into(), "600".into()],
            )
            .with_uncovered(vec!["apt: htop".into()])
            .with_logins(golden_logins(), false),
        ));
        config.admission = Arc::new(HostAdmission {
            total_memory: 64 * GIB,
        });
        // Whatever this machine's memory is doing.
        config.pressure = Arc::new(vibed::pressure::NoPressure);
        config.clock = Arc::new(|| GOLDEN_NOW);
        config.remote = Arc::new(GoldenRemote);
        // One Tailscale address, whatever this machine has.
        config.tailscale = Arc::new(|| vec![GOLDEN_TAILSCALE.parse().unwrap()]);
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

impl Drop for Golden {
    /// Agents a test started and didn't delete, because it failed, end with
    /// it: every shim serving in its `run/` is killed, which hangs up its
    /// command.
    fn drop(&mut self) {
        // As a Mac lays its home out, and as a Linux host does.
        for os in ["macos", "linux"] {
            kill_shims(&Layout::new(&self.home, os).run_dir());
        }
    }
}

/// Kills every shim serving in `run`, which hangs up its command.
pub fn kill_shims(run: &Path) {
    let Ok(entries) = fs::read_dir(run) else {
        return;
    };
    for e in entries.filter_map(Result::ok) {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "sock") {
            continue;
        }
        let status = vibe_core::shim::connect(
            &path,
            &vibe_core::shim::control_hello("test cleanup"),
            Duration::from_secs(2),
        )
        .and_then(|mut c| Ok(c.status()?));
        if let Ok(status) = status
            && let Ok(pid) = i32::try_from(status.pid)
        {
            // SAFETY: a signal to a shim this test's daemon started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// The golden world's backend: the test backend, but `dark-mode-review`'s
/// Claude home has a turn in its transcript before its Claude starts, so
/// its conversation reads the same every time. And the first time its
/// memory is measured, as `agents.list` does just before the examples read
/// its screen, it waits for that screen to be the one they read: the
/// stand-in for Claude prints a moment after its session starts, and how
/// long a moment depends on the machine (a shell starts slower on macOS).
#[derive(Debug)]
pub struct Seeding {
    pub backend: TestBackend,
    /// Where its shims serve their sockets.
    pub run: PathBuf,
    settled: std::sync::atomic::AtomicBool,
}

/// What `dark-mode-review`'s stand-in for Claude shows once it's up, which
/// `83a-agents-output.json` reads.
const SETTLED_SCREEN: &str = "Ready.";

impl Seeding {
    /// Waits, once, up to 20 s, for `dark-mode-review`'s screen to show the
    /// stand-in up and waiting.
    fn settle(&self, agent: &str) {
        use std::sync::atomic::Ordering;
        if agent != "dark-mode-review" || self.settled.swap(true, Ordering::SeqCst) {
            return;
        }
        let socket = self.run.join(format!("{agent}.sock"));
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last = String::new();
        while Instant::now() < deadline {
            let read = vibe_core::shim::connect(
                &socket,
                &vibe_core::shim::control_hello("golden"),
                Duration::from_secs(2),
            )
            .and_then(|mut c| Ok(c.read_output(None)?));
            if let Ok(out) = read {
                if out.screen.contains(SETTLED_SCREEN) {
                    return;
                }
                last = out.screen;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("{agent}'s stand-in never showed {SETTLED_SCREEN:?}; its screen: {last:?}");
    }
}

/// The session `dark-mode-review` gets in the examples.
pub const SEEDED_SESSION: &str = "60616263-6465-4667-a869-6a6b6c6d6e6f";

impl vibe_env::Compartment for Seeding {
    fn create(&self, spec: &vibe_env::AgentSpec) -> vibe_env::Result<vibe_env::Handle> {
        let h = self.backend.create(spec)?;
        if spec.agent_ref == "dark-mode-review" {
            let path = h.claude.transcript(&spec.worktree, SEEDED_SESSION);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let cwd = spec.worktree.to_string_lossy();
            let entry = |kind: &str, at: &str, uuid: &str, message: &str| {
                format!(
                    "{{\"type\":\"{kind}\",\"sessionId\":\"{SEEDED_SESSION}\",\"cwd\":\"{cwd}\",\
                     \"timestamp\":\"{at}\",\"uuid\":\"{uuid}\",\"message\":{message}}}\n"
                )
            };
            let text = entry(
                "user",
                "2026-10-30T11:59:00.001Z",
                "seed-1",
                r#"{"role":"user","content":"Review the dark mode change"}"#,
            ) + &entry(
                "assistant",
                "2026-10-30T11:59:30.002Z",
                "seed-2",
                r#"{"id":"msg-seed","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"The toggle works. One nit: its label isn't localized."}]}"#,
            );
            fs::write(&path, text).unwrap();
        }
        Ok(h)
    }
    fn run(
        &self,
        h: &vibe_env::Handle,
        run: &vibe_env::Run,
    ) -> vibe_env::Result<vibe_env::RunOutput> {
        self.backend.run(h, run)
    }
    fn launch(
        &self,
        h: &vibe_env::Handle,
        l: &vibe_env::Launch,
    ) -> vibe_env::Result<vibe_env::TerminalEnd> {
        self.backend.launch(h, l)
    }
    fn processes(&self, h: &vibe_env::Handle) -> vibe_env::Result<Vec<vibe_env::Proc>> {
        self.backend.processes(h)
    }
    fn stop(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.backend.stop(h)
    }
    fn delete(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.backend.delete(h)
    }
}

impl vibe_env::Limiter for Seeding {
    fn apply(&self, h: &vibe_env::Handle, limits: &vibe_env::Limits) -> vibe_env::Result<()> {
        self.backend.apply(h, limits)
    }
    fn usage(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Usage> {
        self.settle(&h.agent_ref);
        self.backend.usage(h)
    }
}

impl vibe_env::Sleeper for Seeding {
    fn sleep(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.backend.sleep(h)
    }
    fn wake(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Woke> {
        self.backend.wake(h)
    }
}

impl vibe_env::Backend for Seeding {
    fn capabilities(&self) -> vibe_env::Capabilities {
        self.backend.capabilities()
    }
}

/// How the golden Mac reaches Linux hosts: every install finds an x86_64
/// server called `vps-1` with no `claude` yet, and its links never come up
/// (the examples don't run links).
#[derive(Debug)]
pub struct GoldenRemote;

impl Remote for GoldenRemote {
    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>> {
        Box::pin(async move { Err(format!("can't reach {address} over SSH")) })
    }

    fn install<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Installed, String>> {
        Box::pin(async move {
            Ok(Installed {
                os: "linux".into(),
                arch: "x86_64".into(),
                label: "vps-1".into(),
                version: "0.1.0".into(),
                missing: vec!["claude".into()],
                changed: true,
                notes: vec![format!(
                    "Lingering is on for you on {address}, so vibed runs when you're logged \
                     out and starts again after a reboot."
                )],
            })
        })
    }
}

/// A temp dir under /tmp, as a socket's path in it must fit in 104 bytes
/// on macOS, where the usual temp dir is long.
pub fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("vibe")
        .tempdir_in("/tmp")
        .unwrap()
}

/// A binary of the workspace's other packages, built beside the tests'.
/// `cargo test --workspace` builds them all first; a test of one package
/// needs `cargo build --workspace` before.
pub fn workspace_bin(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join(name);
    assert!(
        path.is_file(),
        "{} isn't built; run `cargo build --workspace` first",
        path.display()
    );
    path
}

/// A record like the one a shim leaves when its command exits.
pub fn write_record(
    layout: &Layout,
    agent: &str,
    code: Option<i32>,
    signal: Option<&str>,
    at: &str,
) {
    let record = vibe_core::shim::Record {
        agent: agent.into(),
        command: vec!["claude".into()],
        cwd: "/".into(),
        started_at: "2026-10-30T10:00:00Z".into(),
        exit: vibe_core::rpc::SessionExit {
            code,
            signal: signal.map(str::to_owned),
            at: at.into(),
        },
        output: Default::default(),
    };
    let dir = layout.agent_dir(agent);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(vibe_core::shim::RECORD_FILE),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

/// A `vibe-shim` started the way vibed starts one; killed when dropped.
pub struct RunningShim {
    pub pid: i32,
    pub socket: PathBuf,
}

impl Drop for RunningShim {
    fn drop(&mut self) {
        // SAFETY: a signal to the shim this test started.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
    }
}

pub fn start_shim(layout: &Layout, agent: &str, command: &[&str]) -> RunningShim {
    let socket = layout.session_socket(agent);
    let out = Command::new(workspace_bin("vibe-shim"))
        .args(["--ref", agent, "--socket"])
        .arg(&socket)
        .arg("--state")
        .arg(layout.agent_dir(agent))
        .args(["--cwd", "/", "--"])
        .args(command)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "vibe-shim: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pid = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    RunningShim { pid, socket }
}

pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Waits up to 20 s for `ok`.
pub fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
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

/// The logins the examples' Linux home holds.
pub fn golden_logins() -> Vec<Login> {
    vec![
        Login {
            tool: LoginTool::Gh,
            account: "me on github.com".into(),
        },
        Login {
            tool: LoginTool::Git,
            account: "Me <me@example.com>".into(),
        },
    ]
}
