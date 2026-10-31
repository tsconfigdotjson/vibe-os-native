//! The real `vibe-guest`, run as an ordinary process with `--vsock-dir`:
//! its ports are Unix sockets, and it runs commands as whoever runs the
//! test. Everything but the boot (mounts, the network, the user switch) is
//! the code that runs as PID 1. Freezing needs a cgroup this user may
//! write to, as under a systemd user session; without one those checks
//! say they were skipped.
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_guest::proto::{
    CONTROL_PORT, Exit, Frame, FrameKind, PROTOCOL, Refused, TERMINAL_PORT, TermHello, TermSize,
    TermWelcome, read_frame, write_frame,
};

const GUEST: &str = env!("CARGO_BIN_EXE_vibe-guest");
const WAIT: Duration = Duration::from_secs(20);

/// A cgroup under this process's own, if it may make one.
struct Cgroup(PathBuf);

impl Cgroup {
    fn new(name: &str) -> Option<Cgroup> {
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let own = own.lines().find_map(|l| l.strip_prefix("0::"))?;
        let dir = Path::new("/sys/fs/cgroup")
            .join(own.trim_start_matches('/'))
            .join(format!("vibe-guest-test-{}-{name}", std::process::id()));
        std::fs::create_dir(&dir).ok()?;
        Some(Cgroup(dir))
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("cgroup.freeze"), "0");
        let _ = std::fs::write(self.0.join("cgroup.kill"), "1");
        for _ in 0..200 {
            if std::fs::remove_dir(&self.0).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct Guest {
    dir: tempfile::TempDir,
    child: Child,
    log: PathBuf,
    cgroup: Option<Cgroup>,
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Guest {
    fn start(name: &str, freezer: bool, extra: &[&str]) -> Guest {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = if freezer { Cgroup::new(name) } else { None };
        if freezer && cgroup.is_none() {
            eprintln!("{name}: no cgroup this user may write to; running without the freezer");
        }
        let log = dir.path().join("guest.log");
        let mut cmd = Command::new(GUEST);
        cmd.arg("--vsock-dir")
            .arg(dir.path())
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        if let Some(c) = &cgroup {
            cmd.arg("--cgroup").arg(&c.0);
        }
        let child = cmd.spawn().unwrap();
        let guest = Guest {
            dir,
            child,
            log,
            cgroup,
        };
        wait_until("the guest to serve", || {
            guest.socket(CONTROL_PORT).exists() && guest.socket(TERMINAL_PORT).exists()
        });
        guest
    }

    fn socket(&self, port: u32) -> PathBuf {
        self.dir.path().join(port.to_string())
    }

    fn ask(&self, request: Value) -> Value {
        let mut s = UnixStream::connect(self.socket(CONTROL_PORT)).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        s.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    fn ok(&self, request: Value) -> Value {
        let reply = self.ask(request.clone());
        assert_eq!(reply["ok"], true, "{request} -> {reply}\n{}", self.log());
        reply
    }

    fn launch(&self, command: &[&str]) -> u32 {
        self.ok(json!({"op": "launch", "command": command, "cwd": self.dir.path()}))["pid"]
            .as_u64()
            .unwrap() as u32
    }

    fn status(&self) -> Value {
        self.ok(json!({"op": "status"}))
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn attach(&self, size: Option<TermSize>) -> Term {
        self.try_attach(size)
            .unwrap_or_else(|r| panic!("refused: {}", r.message))
    }

    fn try_attach(&self, size: Option<TermSize>) -> Result<Term, Refused> {
        let mut stream = UnixStream::connect(self.socket(TERMINAL_PORT)).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let hello = TermHello {
            protocol: PROTOCOL,
            client: "test".into(),
            size,
        };
        write_frame(&mut stream, &Frame::json(FrameKind::Hello, &hello)).unwrap();
        let f = read_frame(&mut stream).unwrap().unwrap();
        if f.kind == FrameKind::Refused {
            return Err(f.decode().unwrap());
        }
        assert_eq!(f.kind, FrameKind::Welcome);
        let welcome: TermWelcome = f.decode().unwrap();
        let s = welcome.size;
        Ok(Term {
            stream,
            screen: vt100::Parser::new(nz(s.rows), nz(s.cols), 0),
            welcome,
            exit: None,
        })
    }
}

struct Term {
    stream: UnixStream,
    screen: vt100::Parser,
    welcome: TermWelcome,
    exit: Option<Exit>,
}

impl Term {
    fn send(&mut self, text: &str) {
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::Input, text.as_bytes()),
        )
        .unwrap();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let size = TermSize { rows, cols };
        write_frame(&mut self.stream, &Frame::json(FrameKind::Resize, &size)).unwrap();
        self.screen.set_size(nz(rows), nz(cols));
    }

    /// Reads one frame into the screen; false once the guest closed.
    fn pump(&mut self, timeout: Duration) -> bool {
        // Never zero, which a socket refuses.
        self.stream
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
            .unwrap();
        match read_frame(&mut self.stream) {
            Ok(Some(f)) => {
                match f.kind {
                    FrameKind::Output => self.screen.process(&f.payload),
                    FrameKind::Exit => self.exit = Some(f.decode().unwrap()),
                    other => panic!("a {other:?} frame"),
                }
                true
            }
            Ok(None) => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => false,
            Err(e) => panic!("{e}"),
        }
    }

    fn text(&self) -> String {
        self.screen.screen().contents()
    }

    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} never showed; the screen:\n{}",
                self.text()
            );
            if !self.pump(Duration::from_millis(100)) {
                assert!(
                    self.text().contains(text),
                    "closed before {text:?}:\n{}",
                    self.text()
                );
            }
        }
    }

    fn expect_close(&mut self) {
        let deadline = Instant::now() + WAIT;
        while self.pump(Duration::from_millis(100)) {
            assert!(
                Instant::now() < deadline,
                "the guest didn't close the terminal"
            );
        }
    }
}

/// The last `count N` on a screen.
fn count(text: &str) -> Option<u32> {
    text.rsplit("count ")
        .next()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n).unwrap()
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn a_session_through_the_terminal_from_launch_to_exit() {
    let g = Guest::start("session", false, &[]);
    assert_eq!(g.status()["state"], "idle");
    // No one to attach to yet.
    let refused = g.try_attach(None).err().unwrap();
    assert!(
        refused.message.contains("nothing is running"),
        "{}",
        refused.message
    );

    let pid = g.launch(&["sh"]);
    let mut t = g.attach(Some(TermSize {
        rows: 30,
        cols: 100,
    }));
    assert_eq!(t.welcome.session.pid, pid);
    assert_eq!(
        t.welcome.size,
        TermSize {
            rows: 30,
            cols: 100
        }
    );
    t.send("stty size; echo hello from $((6 * 7))\n");
    t.expect("30 100");
    t.expect("hello from 42");
    t.resize(20, 70);
    t.send("stty size\n");
    t.expect("20 70");
    let status = g.status();
    assert_eq!(
        (&status["state"], &status["session"]["pid"]),
        (&json!("running"), &json!(pid))
    );

    // One command at a time.
    let busy = g.ask(json!({"op": "launch", "command": ["sh"]}));
    assert_eq!(busy["code"], "busy", "{busy}");

    t.send("exit 3\n");
    t.expect_close();
    assert_eq!(
        t.exit,
        Some(Exit {
            code: Some(3),
            signal: None
        })
    );
    let status = g.status();
    assert_eq!(status["state"], "exited");
    assert_eq!(status["session"]["exit"], json!({"code": 3}));

    // A shim connecting after the end still learns how it ended, and sees
    // the last screen.
    let mut late = g.attach(None);
    late.expect("hello from 42");
    late.expect_close();
    assert_eq!(late.exit.as_ref().and_then(|e| e.code), Some(3));

    // The next launch starts afresh, as a `claude --resume` would.
    let again = g.launch(&["sh", "-c", "kill -TERM $$"]);
    assert_ne!(again, pid);
    wait_until("the second command to end", || {
        g.status()["session"]["exit"] == json!({"signal": "SIGTERM"})
    });
}

#[test]
fn the_command_runs_on_when_the_shim_goes_and_the_next_one_sees_the_screen() {
    let g = Guest::start("reconnect", false, &[]);
    let pid = g.launch(&["sh"]);
    let mut first = g.attach(None);
    first.send("i=0; while :; do i=$((i+1)); printf '\\rcount %d ' $i; sleep 0.05; done\n");
    first.expect("count 5");
    let before = count(&first.text()).unwrap();
    // The vsock link drops, as when a shim dies.
    drop(first);
    assert!(alive(pid));
    // The redraw shows where it got to without anyone watching: further on
    // than when the last shim left, however slow this machine.
    let deadline = Instant::now() + WAIT;
    let mut second = loop {
        let mut next = g.attach(None);
        next.expect("count ");
        if count(&next.text()).is_some_and(|c| c > before) {
            break next;
        }
        assert!(Instant::now() < deadline, "the count stopped at {before}");
        drop(next);
        std::thread::sleep(Duration::from_millis(100));
    };

    // A third shim takes over from the second, which is closed.
    let mut third = g.attach(None);
    second.expect_close();
    assert!(second.exit.is_none(), "that's a takeover, not an exit");
    third.send("\x03");
    third.send("echo still here\n");
    third.expect("still here");
    assert!(alive(pid));
}

#[test]
fn the_environment_reaches_the_command_and_never_the_log() {
    let g = Guest::start("env", false, &[]);
    let secret = "sk-ant-oat01-never-in-the-log";
    let reply = g.ok(json!({
        "op": "launch",
        "command": ["sh", "-c", "echo token=$CLAUDE_CODE_OAUTH_TOKEN term=$TERM; sleep 30"],
        "env": {"CLAUDE_CODE_OAUTH_TOKEN": secret},
        "size": {"rows": 10, "cols": 120},
    }));
    let mut t = g.attach(None);
    t.expect(&format!("token={secret} term=xterm-256color"));
    assert!(reply["pid"].is_u64());
    assert!(!g.log().contains(secret), "{}", g.log());
    assert!(g.log().contains("started"), "{}", g.log());
}

#[test]
fn bad_requests_and_launches_are_refused_with_a_reason() {
    let g = Guest::start("bad", false, &[]);
    let cases = [
        (json!({"op": "dance"}), "bad_request"),
        (json!({"op": "launch", "command": []}), "bad_request"),
        (
            json!({"op": "launch", "command": ["sh"], "cwd": "relative"}),
            "bad_request",
        ),
        (
            json!({"op": "launch", "command": ["/no/such/program"]}),
            "launch_failed",
        ),
        (
            json!({"op": "launch", "command": ["sh"], "cwd": "/no/such/dir"}),
            "launch_failed",
        ),
    ];
    // A directory the user can't enter, though it's there.
    let locked = g.dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    // SAFETY: getuid has no memory effects.
    let cases: Vec<(Value, &str)> = if unsafe { libc::getuid() } == 0 {
        cases.to_vec()
    } else {
        let mut all = cases.to_vec();
        all.push((
            json!({"op": "launch", "command": ["sh"], "cwd": locked}),
            "launch_failed",
        ));
        all
    };
    for (request, code) in cases {
        let reply = g.ask(request.clone());
        assert_eq!(reply["code"], code, "{request} -> {reply}");
        assert!(reply["error"].is_string());
    }
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // Garbage on the terminal port is refused too.
    let mut s = UnixStream::connect(g.socket(TERMINAL_PORT)).unwrap();
    write_frame(&mut s, &Frame::new(FrameKind::Input, b"x".to_vec())).unwrap();
    let f = read_frame(&mut s).unwrap().unwrap();
    assert_eq!(f.kind, FrameKind::Refused);
    let mut s = UnixStream::connect(g.socket(TERMINAL_PORT)).unwrap();
    let newer = json!({"protocol": PROTOCOL + 1, "client": "x", "colour": "red"});
    write_frame(&mut s, &Frame::json(FrameKind::Hello, &newer)).unwrap();
    let refused: Refused = read_frame(&mut s).unwrap().unwrap().decode().unwrap();
    assert_eq!(refused.protocol, PROTOCOL);
    assert!(refused.message.contains("protocol"), "{}", refused.message);
    // Nothing went wrong for the guest itself.
    assert_eq!(g.status()["state"], "idle");
    // Without a cgroup it can't freeze, and says so.
    let reply = g.ask(json!({"op": "freeze"}));
    assert_eq!(reply["code"], "freeze_failed", "{reply}");
}

#[test]
fn the_process_report_covers_everything_the_command_started() {
    let g = Guest::start("procs", true, &[]);
    let pid = g.launch(&["sh", "-c", "setsid sleep 300 & sleep 300 & exec sleep 301"]);
    wait_until("three sleeps", || {
        let report = g.ok(json!({"op": "processes"}));
        report["processes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["name"] == "sleep")
            .count()
            == 3
    });
    let report = g.ok(json!({"op": "processes"}));
    let procs = report["processes"].as_array().unwrap();
    let main = procs.iter().find(|p| p["pid"] == pid).expect("the command");
    assert_eq!(main["args"], json!(["sleep", "301"]));
    for p in procs {
        assert!(p["started_ms"].as_u64().unwrap() <= report["uptime_ms"].as_u64().unwrap());
        assert!(p["rss"].as_u64().unwrap() > 0, "{p}");
        assert!(p["state"].is_string() && p["cpu_ms"].is_u64() && p["ppid"].is_u64());
    }
    // The one that left the session is still the agent's, with a cgroup.
    if g.cgroup.is_some() {
        assert_eq!(procs.len(), 3, "{report}");
    }
}

#[test]
fn freeze_stops_everything_and_thaw_after_a_restore_waits_for_the_shares() {
    let share = tempfile::tempdir().unwrap();
    let missing = format!("gone:{}/not-there", share.path().display());
    let shared = format!("work:{}", share.path().display());
    std::fs::write(share.path().join("probe"), "x").unwrap();
    let g = Guest::start("freeze", true, &["--share", &shared]);
    let Some(cgroup) = &g.cgroup else {
        eprintln!("freeze: skipped; no cgroup this user may write to");
        return;
    };
    let pid = g.launch(&["sh"]);
    let mut t = g.attach(None);
    t.send("i=0; while :; do i=$((i+1)); printf '\\rcount %d ' $i; sleep 0.02; done\n");
    t.expect("count 3");
    let frozen = g.ok(json!({"op": "freeze"}));
    assert!(frozen["processes"].as_u64().unwrap() >= 2, "{frozen}");
    assert!(frozen["memory"]["total"].as_u64().unwrap() > 0);
    assert!(frozen["sync_ms"].is_u64() && frozen["freeze_ms"].is_u64());
    assert_eq!(g.status()["frozen"], true);
    let events = std::fs::read_to_string(cgroup.0.join("cgroup.events")).unwrap();
    assert!(events.contains("frozen 1"), "{events}");
    // Nothing moves while frozen.
    while t.pump(Duration::from_millis(200)) && t.screen.screen().contents().is_empty() {}
    let before = t.text();
    for _ in 0..5 {
        t.pump(Duration::from_millis(100));
    }
    assert_eq!(t.text(), before, "the count moved while frozen");
    // Freezing again is harmless, and a launch waits for the thaw.
    g.ok(json!({"op": "freeze"}));
    assert_eq!(
        g.ask(json!({"op": "launch", "command": ["sh"]}))["code"],
        "busy"
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let thawed = g.ok(json!({"op": "thaw", "time_ms": now_ms}));
    assert_eq!(thawed["unanswered"], Value::Null, "{thawed}");
    assert!(thawed["shares_ms"].as_u64().unwrap() < 1000, "{thawed}");
    // An ordinary user can't set the clock: the thaw goes ahead and says so.
    // SAFETY: getuid has no memory effects.
    if unsafe { libc::getuid() } != 0 {
        assert_eq!(thawed["clock_step_ms"], Value::Null, "{thawed}");
        assert!(
            thawed["problems"][0].as_str().unwrap().contains("clock"),
            "{thawed}"
        );
        assert_eq!(
            g.ask(json!({"op": "clock", "time_ms": now_ms}))["code"],
            "clock_failed"
        );
    }
    assert_eq!(g.status()["frozen"], false);
    t.expect_change(&before);
    assert!(alive(pid));
    drop(t);

    // A share that doesn't answer is waited for until the timeout, then
    // reported; the agent is thawed all the same.
    drop(g);
    let g = Guest::start("freeze2", true, &["--share", &shared, "--share", &missing]);
    g.launch(&["sleep", "300"]);
    g.ok(json!({"op": "freeze"}));
    let t0 = Instant::now();
    let thawed = g.ok(json!({"op": "thaw", "time_ms": now_ms, "share_timeout_ms": 300}));
    assert!(t0.elapsed() >= Duration::from_millis(300));
    assert_eq!(thawed["unanswered"], json!(["gone"]), "{thawed}");
    assert_eq!(g.status()["frozen"], false);
}

#[test]
fn a_launch_clears_out_what_the_last_command_left_behind() {
    let g = Guest::start("leftovers", true, &[]);
    if g.cgroup.is_none() {
        eprintln!("leftovers: skipped; no cgroup this user may write to");
        return;
    }
    // `nohup … &`: it outlives the command, as a detached job does.
    g.launch(&["sh", "-c", "nohup sleep 300 >/dev/null 2>&1 & sleep 0.2"]);
    wait_until("the command to exit", || g.status()["state"] == "exited");
    let left: Vec<Value> = g.ok(json!({"op": "processes"}))["processes"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        left.len(),
        1,
        "the orphaned sleep stays the agent's: {left:?}"
    );
    let orphan = left[0]["pid"].as_u64().unwrap() as u32;
    assert!(alive(orphan));
    let next = g.launch(&["sleep", "300"]);
    let now: Vec<u64> = g.ok(json!({"op": "processes"}))["processes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["pid"].as_u64().unwrap())
        .collect();
    assert_eq!(now, [u64::from(next)]);
    wait_until("the orphan to be reaped", || !alive(orphan));
}

#[test]
fn poweroff_ends_the_command_and_the_guest() {
    let mut g = Guest::start("poweroff", true, &[]);
    let pid = g.launch(&["sleep", "300"]);
    g.ok(json!({"op": "poweroff"}));
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = g.child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "the guest didn't exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_until("the command to end", || !alive(pid));
}

impl Term {
    /// Waits for the screen to differ from `before`.
    fn expect_change(&mut self, before: &str) {
        let deadline = Instant::now() + WAIT;
        while self.text() == before {
            assert!(Instant::now() < deadline, "the screen never changed");
            self.pump(Duration::from_millis(100));
        }
    }
}

#[test]
fn a_run_goes_to_completion_beside_the_command() {
    let g = Guest::start("run", false, &[]);
    // Claude, as it were, runs; a run goes ahead beside it.
    g.launch(&["sh", "-c", "echo agent; exec cat"]);
    let cwd = g.dir.path().canonicalize().unwrap();
    let ran = g.ok(json!({
        "op": "run",
        "command": ["sh", "-c", "echo \"$VIBE_X in $(pwd -P)\"; echo oops >&2; exit 3"],
        "env": {"VIBE_X": "x"},
        "cwd": cwd,
        "timeout_ms": 20_000,
    }));
    assert_eq!(ran["code"], 3, "{ran}");
    assert_eq!(ran["timed_out"], false);
    assert_eq!(
        ran["output"],
        format!("x in {}\noops\n", cwd.display()),
        "{ran}"
    );
    assert_eq!(
        g.status()["state"],
        "running",
        "the agent's command goes on"
    );

    // Out of time: killed, with what it started, and the reply says so.
    let started = Instant::now();
    let ran = g.ok(json!({
        "op": "run",
        "command": ["sh", "-c", "sleep 30 & echo started; wait"],
        "timeout_ms": 500,
    }));
    assert_eq!(ran["timed_out"], true, "{ran}");
    assert_eq!(ran["signal"], "SIGKILL", "{ran}");
    assert_eq!(ran["output"], "started\n");
    assert!(started.elapsed() < Duration::from_secs(10));

    // What it leaves running when it ends is killed too.
    let ran = g.ok(json!({
        "op": "run",
        "command": ["sh", "-c", "sleep 30 & echo $! >left; echo done"],
        "cwd": cwd,
        "timeout_ms": 20_000,
    }));
    assert_eq!(ran["code"], 0, "{ran}");
    let left: u32 = std::fs::read_to_string(cwd.join("left"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_until("what it left to be killed", || !alive(left));

    // Only the end of a lot of output comes back.
    let ran = g.ok(json!({
        "op": "run",
        "command": ["sh", "-c", "head -c 100000 /dev/zero | tr '\\0' a; echo; echo last"],
        "timeout_ms": 20_000,
    }));
    let out = ran["output"].as_str().unwrap();
    assert_eq!(
        out.len(),
        vibe_guest::proto::MAX_RUN_OUTPUT,
        "{}",
        out.len()
    );
    assert!(out.ends_with("a\nlast\n"));

    // A command that can't start, or a run without a timeout, is refused.
    let r = g.ask(json!({"op": "run", "command": ["/nonexistent/x"], "timeout_ms": 1000}));
    assert_eq!(r["code"], "launch_failed", "{r}");
    let r = g.ask(json!({"op": "run", "command": ["true"], "timeout_ms": 0}));
    assert_eq!(r["code"], "bad_request", "{r}");
}
