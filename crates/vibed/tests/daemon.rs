//! The real `vibed` binary: starting, serving, refusing a second instance,
//! keeping its state across restarts and stopping cleanly. Each test gives
//! it a temp `$HOME`, so nothing touches the real one.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_core::paths::Layout;
use vibe_core::shim::{self, FrameKind};

const VIBED: &str = env!("CARGO_BIN_EXE_vibed");

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(cmd: &mut Command, socket: &Path) -> Daemon {
    let child = cmd.stderr(Stdio::piped()).spawn().unwrap();
    let mut daemon = Daemon { child };
    let deadline = Instant::now() + Duration::from_secs(20);
    while UnixStream::connect(socket).is_err() {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            panic!("vibed exited early: {status}");
        }
        assert!(Instant::now() < deadline, "vibed didn't start");
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

/// `vibed` with its own `$HOME` in `tmp`, serving `socket`.
fn vibed(tmp: &tempfile::TempDir, socket: &Path) -> Command {
    let mut cmd = Command::new(VIBED);
    cmd.arg("--socket")
        .arg(socket)
        .env("HOME", tmp.path().join("home"))
        .env("VIBE_GH", "/nonexistent/gh");
    cmd
}

/// One call on its own connection.
fn call(socket: &Path, method: &str, params: Value) -> Value {
    let mut s = UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    s.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn hello(socket: &Path) -> Value {
    call(
        socket,
        "daemon.hello",
        serde_json::json!({"client": "daemon-test", "protocol": 2}),
    )
}

/// `hello`, once the socket's there.
fn hello_when_up(socket: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    while UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "vibed didn't start");
        std::thread::sleep(Duration::from_millis(20));
    }
    hello(socket)
}

fn stop(mut d: Daemon) -> String {
    sigterm(&d.child);
    let out = d.child.wait().unwrap();
    assert!(out.success(), "{out}");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut d.child.stderr.take().unwrap(), &mut stderr).unwrap();
    stderr
}

fn sigterm(child: &Child) {
    let ok = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok);
}

fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("run/vibed.sock")
}

#[test]
fn serves_hello_and_stops_cleanly_on_sigterm() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = socket_in(&tmp);
    let d = spawn(&mut vibed(&tmp, &socket), &socket);
    let r = hello(&socket);
    let result = &r["result"];
    assert_eq!(result["daemon"], "vibed");
    assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["protocol"], 2);
    assert_eq!(result["os"], std::env::consts::OS);
    assert_eq!(result["arch"], std::env::consts::ARCH);
    assert_eq!(result["pid"], d.child.id());

    let stderr = stop(d);
    assert!(!socket.exists(), "the socket is removed on exit");
    assert!(stderr.contains("listening on"), "{stderr}");
    assert!(stderr.contains("hello from daemon-test"), "{stderr}");
    assert!(stderr.contains("vibed: stopped"), "{stderr}");
}

/// How `cmd` ends, in time: a `vibed` that was meant to be refused and wasn't
/// serves forever, which would hang `output()` and the whole run.
fn refused_in_time(cmd: &mut Command) -> Output {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "a second vibed wasn't refused; it was still serving: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn a_second_daemon_on_the_same_socket_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = socket_in(&tmp);
    let _first = spawn(&mut vibed(&tmp, &socket), &socket);
    let out = refused_in_time(&mut vibed(&tmp, &socket));
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already serving"), "{stderr}");
    assert_eq!(
        hello(&socket)["result"]["daemon"],
        "vibed",
        "the first keeps serving"
    );
}

#[test]
fn socket_comes_from_vibe_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = socket_in(&tmp);
    let _d = spawn(
        Command::new(VIBED)
            .env("VIBE_SOCKET", &socket)
            .env("HOME", tmp.path().join("home"))
            .env("VIBE_GH", "/nonexistent/gh"),
        &socket,
    );
    assert_eq!(hello(&socket)["result"]["daemon"], "vibed");
}

#[test]
fn default_socket_is_under_home() {
    // Under /tmp: $TMPDIR plus the macOS default path would pass the 104-byte
    // limit on Unix socket paths.
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let rel = vibe_core::paths::default_socket_relative(std::env::consts::OS);
    let socket = tmp.path().join(rel);
    let _d = spawn(
        Command::new(VIBED)
            .env_remove("VIBE_SOCKET")
            .env("HOME", tmp.path())
            .env("VIBE_GH", "/nonexistent/gh"),
        &socket,
    );
    assert_eq!(hello(&socket)["result"]["daemon"], "vibed");
}

#[test]
fn a_second_daemon_on_the_same_state_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = socket_in(&tmp);
    let _first = spawn(&mut vibed(&tmp, &socket), &socket);
    // Another socket, but the same $HOME.
    let second = tmp.path().join("run2/vibed.sock");
    let out = refused_in_time(&mut vibed(&tmp, &second));
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another vibed is already using"),
        "{stderr}"
    );
    assert_eq!(hello(&socket)["result"]["daemon"], "vibed");
    // Refused before it bound anything, with nothing to connect to, ever.
    assert!(
        !second.parent().unwrap().exists(),
        "it made {}",
        second.parent().unwrap().display()
    );
}

/// The shared services' socket is `vibed`'s own, not any agent's: it's there for
/// as long as `vibed` is, for agents yet to start, and goes when it stops. Only
/// its lock stays, as `vibed.sock`'s does.
#[test]
fn the_services_socket_lasts_as_long_as_vibed() {
    // Under /tmp: this socket is in $HOME, and $TMPDIR plus the macOS state
    // directory would pass the 104-byte limit on Unix socket paths.
    let tmp = common::short_tempdir();
    let socket = socket_in(&tmp);
    let d = spawn(&mut vibed(&tmp, &socket), &socket);
    let run = Layout::new(tmp.path().join("home"), std::env::consts::OS).run_dir();
    let services = run.join(".services.sock");
    let deadline = Instant::now() + Duration::from_secs(20);
    while UnixStream::connect(&services).is_err() {
        assert!(Instant::now() < deadline, "it never served {services:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    stop(d);
    assert!(!services.exists(), "{services:?} outlived vibed");
    let left: Vec<_> = std::fs::read_dir(&run)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != ".services.sock.lock")
        .collect();
    assert_eq!(left, Vec::<String>::new());
}

/// As soon as its socket answers, a `vibed` holds the state: two started at once
/// can't each find the other's socket and go on to serve.
#[test]
fn two_daemons_started_together_leave_one() {
    for _ in 0..3 {
        let tmp = tempfile::tempdir().unwrap();
        let (one, two) = (
            tmp.path().join("one/vibed.sock"),
            tmp.path().join("two/vibed.sock"),
        );
        let mut a = vibed(&tmp, &one).stderr(Stdio::piped()).spawn().unwrap();
        let mut b = vibed(&tmp, &two).stderr(Stdio::piped()).spawn().unwrap();
        // The refused one exits, once the lock has had its moment.
        let deadline = Instant::now() + Duration::from_secs(30);
        let (winner, mut alive, socket) = loop {
            if a.try_wait().unwrap().is_some() {
                break (&two, b, &one);
            }
            if b.try_wait().unwrap().is_some() {
                break (&one, a, &two);
            }
            if Instant::now() >= deadline {
                let _ = a.kill();
                let _ = b.kill();
                let _ = a.wait();
                let _ = b.wait();
                panic!("both are still serving");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(hello_when_up(winner)["result"]["daemon"], "vibed");
        assert!(
            !socket.exists(),
            "the refused one left {}",
            socket.display()
        );
        let _ = alive.kill();
        let _ = alive.wait();
    }
}

/// The ticket's "done when", through the real binary: add a project, list
/// its starter profiles, edit one and change a host limit; restart; all of
/// it is still there.
#[test]
fn state_survives_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let socket = socket_in(&tmp);
    let app = root.join("home/src/app");
    common::repo(&app);
    let gh = common::fake_gh(&root.join("bin"), &root.join("remotes"));
    common::repo(&root.join("remotes/web"));
    let start = || {
        let mut cmd = vibed(&tmp, &socket);
        cmd.env("VIBE_GH", &gh);
        spawn(&mut cmd, &socket)
    };
    // What a new project starts with here: both environments on a Mac.
    let starters: &[&str] = if cfg!(target_os = "macos") {
        &["software-eng-linux", "software-eng-mac"]
    } else {
        &["software-eng-linux"]
    };

    let d = start();
    let r = call(&socket, "projects.add", serde_json::json!({"path": app}));
    assert_eq!(r["result"]["name"], "app", "{r}");
    let r = call(
        &socket,
        "projects.add",
        serde_json::json!({"repo": "me/web"}),
    );
    assert_eq!(r["result"]["source"], "github", "{r}");
    let r = call(
        &socket,
        "profiles.list",
        serde_json::json!({"project": "app"}),
    );
    let names: Vec<&str> = r["result"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, starters);
    let r = call(
        &socket,
        "profiles.update",
        serde_json::json!({"project": "app", "profile": "software-eng-linux", "memory": 8u64 << 30}),
    );
    assert_eq!(r["result"]["memory"], 8u64 << 30, "{r}");
    let r = call(
        &socket,
        "limits.set",
        serde_json::json!({"max_awake_agents": 7}),
    );
    assert_eq!(r["result"]["max_awake_agents"], 7, "{r}");
    let r = call(
        &socket,
        "keys.create",
        serde_json::json!({"name": "laptop"}),
    );
    let secret = r["result"]["secret"].as_str().unwrap().to_owned();
    let stderr = stop(d);
    assert!(!stderr.contains(&secret), "a key reached the log");

    let d = start();
    let r = call(&socket, "projects.list", serde_json::json!({}));
    let projects = r["result"]["projects"].as_array().unwrap();
    let names: Vec<&str> = projects
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["app", "web"]);
    assert_eq!(projects[0]["profiles"][0]["memory"], 8u64 << 30);
    assert_eq!(
        projects[0]["profiles"].as_array().unwrap().len(),
        starters.len()
    );
    let r = call(&socket, "limits.get", serde_json::json!({}));
    assert_eq!(r["result"]["max_awake_agents"], 7);
    let r = call(
        &socket,
        "keys.verify",
        serde_json::json!({"secret": secret}),
    );
    assert_eq!(r["result"]["name"], "laptop", "{r}");
    // VIBE_GH picks the gh that runs.
    let r = call(&socket, "gh.status", serde_json::json!({"refresh": true}));
    assert_eq!(r["result"]["state"], "logged_in", "{r}");
    stop(d);

    // State is where the PRD says, and the log too.
    let home = root.join("home");
    let (db, log) = if cfg!(target_os = "macos") {
        (
            home.join("Library/Application Support/VibeOS/vibe.db"),
            home.join("Library/Logs/VibeOS/vibed.log"),
        )
    } else {
        (
            home.join(".local/share/vibeos/vibe.db"),
            home.join(".local/share/vibeos/logs/vibed.log"),
        )
    };
    assert!(db.exists());
    assert!(home.join(".config/vibeos/host.toml").exists());
    assert!(home.join("VibeOS/projects/web/README.md").exists());
    let log = std::fs::read_to_string(log).unwrap();
    assert!(log.contains("added project app"), "{log}");
    assert!(!log.contains(&secret), "a key reached the log file");
    // Each line is timestamped.
    assert!(
        log.lines().all(|l| l.as_bytes().get(10) == Some(&b'T')),
        "{log}"
    );
}

#[test]
fn no_home_is_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(VIBED)
        .arg("--socket")
        .arg(socket_in(&tmp))
        .env_remove("HOME")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("$HOME is not set"), "{stderr}");
}

#[test]
fn version_and_bad_args() {
    let out = Command::new(VIBED).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("vibed {}", env!("CARGO_PKG_VERSION"))
    );
    let out = Command::new(VIBED).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// An attach client of a shim, collecting what it shows.
struct Terminal {
    stream: UnixStream,
    seen: String,
}

impl Terminal {
    fn attach(socket: &str) -> Terminal {
        let hello = shim::Hello {
            protocol: shim::PROTOCOL,
            role: shim::Role::Attach,
            client: "daemon-test".into(),
            tty: Some("/dev/ttys042".into()),
            size: None,
        };
        let conn = shim::connect(Path::new(socket), &hello, Duration::from_secs(10)).unwrap();
        Terminal {
            stream: conn.stream,
            seen: String::new(),
        }
    }

    /// Types `line` and waits to see `answer`.
    fn run(&mut self, line: &str, answer: &str) {
        let input = shim::Frame::new(FrameKind::Input, format!("{line}\r"));
        shim::write_frame(&mut self.stream, &input).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.seen.contains(answer) {
            assert!(
                Instant::now() < deadline,
                "no {answer:?} in {:?}",
                self.seen
            );
            let frame = shim::read_frame(&mut self.stream)
                .unwrap()
                .expect("the shim closed");
            if frame.kind == FrameKind::Output {
                self.seen.push_str(&String::from_utf8_lossy(&frame.payload));
            }
        }
    }
}

#[test]
fn a_session_outlives_a_killed_vibed_and_is_adopted_again() {
    let tmp = common::short_tempdir();
    let socket = socket_in(&tmp);
    let layout = Layout::new(tmp.path().join("home"), std::env::consts::OS);
    let mut first = spawn(&mut vibed(&tmp, &socket), &socket);
    let shim = common::start_shim(&layout, "quiet-amber-otter", &["sh"]);

    // vibed says where the session is; the terminal talks to its shim.
    let found = call(
        &socket,
        "sessions.attach",
        json!({"ref": "quiet-amber-otter"}),
    );
    let path = found["result"]["socket"].as_str().unwrap().to_owned();
    let mut terminal = Terminal::attach(&path);
    terminal.run("echo before-$((1+1))", "before-2");

    // Killed mid-session, without a chance to clean up.
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    terminal.run("echo during-$((2+2))", "during-4");
    assert!(common::alive(shim.pid));

    // A new vibed finds the session again, attached client and all.
    let second = spawn(&mut vibed(&tmp, &socket), &socket);
    let list = call(&socket, "sessions.list", json!({}));
    let session = &list["result"]["sessions"][0];
    assert_eq!(session["ref"], "quiet-amber-otter");
    assert_eq!(session["state"], "running");
    assert_eq!(session["clients"], 1);
    assert_eq!(session["ttys"], json!(["/dev/ttys042"]));
    terminal.run("echo after-$((3+3))", "after-6");
    let found = call(
        &socket,
        "sessions.attach",
        json!({"ref": "quiet-amber-otter"}),
    );
    assert_eq!(found["result"]["socket"], path.as_str());
    let stderr = stop(second);
    assert!(
        stderr.contains("re-adopted session quiet-amber-otter"),
        "{stderr}"
    );
    assert!(stderr.contains("1 running session found"), "{stderr}");
}
