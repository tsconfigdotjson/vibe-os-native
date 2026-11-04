//! The `vibe` binary against an in-process `vibed`.

// vibed's test helpers: the golden world the contract examples need.
#[path = "../../vibed/tests/common/mod.rs"]
mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use serde_json::Value;
use vibe_core::paths::Layout;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibed::core::{Config, Core, DaemonInfo};
use vibed::server::{self, SocketGuard};

const VIBE: &str = env!("CARGO_BIN_EXE_vibe");

struct Server {
    path: PathBuf,
    _guard: SocketGuard,
    _dir: tempfile::TempDir,
    // Dropped last: stops the server.
    _rt: tokio::runtime::Runtime,
}

fn start(info: DaemonInfo) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let os = info.os.clone();
    let core = Core::open(Config::new(info, Layout::new(dir.path().join("home"), &os))).unwrap();
    start_core(core, dir)
}

/// Serves `core` on a socket in `dir`, which is kept until the server goes.
fn start_core(core: Core, dir: tempfile::TempDir) -> Server {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let path = dir.path().join("run/vibed.sock");
    let (listener, guard) = server::bind(&path).unwrap();
    rt.spawn(async move {
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        server::serve(
            listener,
            Arc::new(core),
            MAX_MESSAGE_BYTES,
            std::future::pending(),
        )
        .await;
    });
    Server {
        path,
        _guard: guard,
        _dir: dir,
        _rt: rt,
    }
}

fn vibe(args: &[&str], socket: &Path, stdin: &[u8]) -> Output {
    let mut child = Command::new(VIBE)
        .args(args)
        .env("VIBE_SOCKET", socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn hello_prints_what_vibed_reports() {
    let s = start(DaemonInfo::current());
    let out = vibe(&["hello"], &s.path, b"");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let expected = format!(
        "vibed {} · protocol 1 · {}/{} · pid {} · {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
        s.path.display()
    );
    assert_eq!(text(&out.stdout), expected);
}

#[test]
fn socket_flag_beats_the_environment() {
    let s = start(DaemonInfo::current());
    let path = s.path.to_str().unwrap();
    let out = vibe(
        &["--socket", path, "hello"],
        Path::new("/nonexistent.sock"),
        b"",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn hello_without_a_daemon_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("none.sock");
    for cmd in ["hello", "rpc"] {
        let out = vibe(&[cmd], &missing, b"");
        assert_eq!(out.status.code(), Some(1), "{cmd}");
        let stderr = text(&out.stderr);
        assert!(stderr.contains("can't reach vibed at"), "{cmd}: {stderr}");
        assert!(stderr.contains("is vibed running?"), "{cmd}: {stderr}");
    }
}

#[test]
fn rpc_bridges_the_whole_contract() {
    // The golden daemon of protocol/examples, so every reply is exact.
    let golden = common::Golden::new().for_examples();
    let core = Core::open(golden.config()).unwrap();
    let s = start_core(core, tempfile::tempdir().unwrap());
    let mut input = Vec::new();
    let mut expected: Vec<Value> = Vec::new();
    let mut said_link = false;
    for ex in common::examples(&golden) {
        // What only another host's link can call: this connection says it's one
        // from there on, as a link's would, with the hello it would use.
        if !said_link && (ex.name.contains("services-token") || ex.name.contains("services-follow"))
        {
            said_link = true;
            let hello = serde_json::json!({"jsonrpc": "2.0", "id": "link", "method": "daemon.hello",
                "params": {"client": "vibed link calls from tests",
                           "protocol": vibe_core::rpc::PROTOCOL_VERSION}});
            input.extend(format!("{hello}\n").into_bytes());
            let mut answer = expected[0].clone();
            answer["id"] = "link".into();
            expected.push(answer);
        }
        let Some(line) = ex.line else { continue };
        input.extend(line);
        input.push(b'\n');
        if !ex.response.is_null() {
            expected.push(ex.response);
        }
        expected.extend(ex.events);
    }
    assert!(said_link, "the examples have the link's calls");
    let out = vibe(&["rpc"], &s.path, &input);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let got: Vec<Value> = text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(got, expected);
}

/// `vibe doctor` on the golden host: a line a check, what to do about each
/// that needs it, and exit 1 while one does. `--fix` has vibed run `gh auth
/// setup-git`; the Mac's sleep setting is the owner's to change, so it
/// still fails.
#[test]
fn doctor_lists_every_check_and_fixes_what_it_can() {
    let golden = common::Golden::new().for_examples();
    let core = Core::open(golden.config()).unwrap();
    let s = start_core(core, tempfile::tempdir().unwrap());
    let out = vibe(&["doctor"], &s.path, b"");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let report = text(&out.stdout);
    assert_eq!(
        report
            .lines()
            .filter(|l| ["ok  ", "!!  ", "--  ", "..  "]
                .iter()
                .any(|m| l.starts_with(m)))
            .count(),
        8,
        "a line a check:\n{report}"
    );
    assert!(
        report.contains("ok  git             git 2.50.1 is installed.\n"),
        "{report}"
    );
    assert!(
        report.contains("!!  git through gh  git doesn't ask gh"),
        "{report}"
    );
    assert!(
        report.contains("`vibe doctor --fix` can do it."),
        "{report}"
    );
    assert!(
        report.contains("!!  system sleep    This Mac sleeps after 10 minutes"),
        "{report}"
    );
    assert!(
        report.contains("..  Linux images    Downloading the Linux base image (40%)."),
        "{report}"
    );
    assert!(report.ends_with("2 checks need you.\n"), "{report}");

    let out = vibe(&["doctor", "--fix"], &s.path, b"");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let report = text(&out.stdout);
    assert!(report.starts_with("fixed: git through gh\n"), "{report}");
    assert!(
        report.contains("ok  git through gh  git asks gh for GitHub's credentials."),
        "{report}"
    );
    assert!(report.ends_with("1 check needs you.\n"), "{report}");

    // Without vibed, it says so, as every command does.
    let dir = tempfile::tempdir().unwrap();
    let out = vibe(&["doctor"], &dir.path().join("none.sock"), b"");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("can't reach vibed at"));
}

/// `ssh <host> vibe rpc` is a conversation: each reply comes as soon as
/// it's made, while stdin is still open, not when it ends.
#[test]
fn rpc_answers_while_stdin_stays_open() {
    let s = start(DaemonInfo {
        version: "9.9.9".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        pid: 1,
    });
    let mut child = Command::new(VIBE)
        .arg("rpc")
        .env("VIBE_SOCKET", &s.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    for id in 1..=3 {
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":{id},"method":"daemon.hello","params":{{"client":"t","protocol":1}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut out, &mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id, "{line}");
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn version_and_usage() {
    let out = Command::new(VIBE).arg("--version").output().unwrap();
    assert_eq!(
        text(&out.stdout),
        format!("vibe {}\n", env!("CARGO_PKG_VERSION"))
    );
    let out = Command::new(VIBE).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("usage: vibe"));
}

const TOKEN: &str = "sk-ant-oat01-a-test-token-from-a-stand-in-for-claude-0123456789";

/// `vibe setup-token` with a stand-in for `claude` that prints `body`.
fn setup_token(dir: &Path, body: &str) -> (Output, PathBuf) {
    let claude = dir.join("claude");
    fs::write(&claude, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let token = dir.join("token");
    let out = Command::new(VIBE)
        .arg("setup-token")
        .env("VIBE_CLAUDE", &claude)
        .env("VIBE_TEST_TOKEN_FILE", &token)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (out, token)
}

#[test]
fn setup_token_keeps_the_token_claude_printed() {
    let tmp = tempfile::tempdir().unwrap();
    // Run on a terminal, with escapes around the token, as Claude's TUI
    // prints it; and asked for nothing but `setup-token`.
    let (out, token) = setup_token(
        tmp.path(),
        &format!(
            "[ \"$1\" = setup-token ] || exit 9\n\
             [ -t 1 ] || {{ echo 'not a terminal'; exit 8; }}\n\
             printf '\\033[1mYour OAuth token (valid for 1 year):\\033[22m\\r\\n\\r\\n\\033[32m{TOKEN}\\033[39m\\r\\n'"
        ),
    );
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).contains("Your OAuth token"),
        "it's shown as it runs"
    );
    assert_eq!(fs::read_to_string(&token).unwrap().trim(), TOKEN);
    let mode = fs::metadata(&token).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn setup_token_without_a_token_fails_and_keeps_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, token) = setup_token(tmp.path(), "echo 'Login cancelled'");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("printed no token"),
        "{}",
        text(&out.stderr)
    );
    assert!(!token.exists());
    let (out, token) = setup_token(tmp.path(), "echo 'error'; exit 3");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("didn't finish"),
        "{}",
        text(&out.stderr)
    );
    assert!(!token.exists());
    // No claude at all.
    let out = Command::new(VIBE)
        .arg("setup-token")
        .env("VIBE_CLAUDE", tmp.path().join("missing"))
        .env("VIBE_TEST_TOKEN_FILE", &token)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("can't run"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn setup_token_asks_for_a_paste_on_a_terminal_when_it_saw_none() {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path().join("claude");
    fs::write(&claude, "#!/bin/sh\necho 'Opened your browser.'\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let token = tmp.path().join("token");
    // A terminal for vibe's stdin, stdout and stderr, as a person's is.
    // SAFETY: each call is checked; the descriptors are owned at once.
    let (master, slave) = unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(m)).to_owned();
        let s = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        assert!(s >= 0);
        (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
    };
    let mut child = Command::new(VIBE)
        .arg("setup-token")
        .env("VIBE_CLAUDE", &claude)
        .env("VIBE_TEST_TOKEN_FILE", &token)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()))
        .spawn()
        .unwrap();
    drop(slave);
    let mut master = fs::File::from(master);
    let mut reader = master.try_clone().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            let _ = tx.send(String::from_utf8_lossy(&seen).into_owned());
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let seen = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("vibe asked for nothing");
        if seen.contains("paste the token") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{seen}");
    }
    master.write_all(format!("{TOKEN}\n").as_bytes()).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(&token).unwrap().trim(), TOKEN);
}

#[test]
fn project_and_profile_set_up_a_host_without_the_menu() {
    // A standalone Linux host: its projects start with one linux profile.
    let s = start(DaemonInfo {
        version: "9.9.9".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        pid: 1,
    });
    let repo = s._dir.path().join("src/web");
    common::repo(&repo);
    let run = |args: &[&str]| {
        let out = vibe(args, &s.path, b"");
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    let fails = |args: &[&str]| {
        let out = vibe(args, &s.path, b"");
        assert!(!out.status.success(), "{args:?} should fail");
        text(&out.stderr)
    };
    assert!(run(&["project"]).starts_with("no projects"));
    // A relative path is the checkout under this directory.
    let out = Command::new(VIBE)
        .args(["project", "add", "./web"])
        .current_dir(repo.parent().unwrap())
        .env("VIBE_SOCKET", &s.path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let added = text(&out.stdout);
    assert!(added.starts_with("added web  /"), "{added}");
    assert!(
        added.contains("software-eng-linux  linux, opus, high reasoning, auto mode, 4G"),
        "{added}"
    );
    assert!(!added.contains("software-eng-mac"), "{added}");

    let made = run(&[
        "profile", "create", "web", "reviewer", "--model", "sonnet", "--memory", "2G",
    ]);
    assert_eq!(
        made,
        "created reviewer  linux, sonnet, high reasoning, auto mode, 2G\n"
    );
    let set = run(&[
        "profile", "set", "web", "reviewer", "--name", "critic", "--mode", "bypass",
    ]);
    assert_eq!(
        set,
        "critic  linux, sonnet, high reasoning, bypass mode, 2G\n"
    );
    let listed = run(&["profile", "list", "web"]);
    assert_eq!(
        listed,
        "software-eng-linux  linux, opus, high reasoning, auto mode, 4G\n\
         critic  linux, sonnet, high reasoning, bypass mode, 2G\n"
    );
    // A Linux host has no macos environment.
    let why = fails(&["profile", "create", "web", "mac", "--environment", "macos"]);
    assert!(why.contains("only a Mac host has"), "{why}");
    assert_eq!(
        run(&["profile", "delete", "web", "critic"]),
        "deleted critic\n"
    );
    assert!(run(&["project"]).starts_with("web  /"));
    let why = fails(&["project", "--host", "vps"]);
    assert!(why.contains("no host named vps"), "{why}");
    let removed = run(&["project", "remove", "web"]);
    assert!(removed.starts_with("removed web; "), "{removed}");

    let hosts = run(&["host"]);
    assert!(hosts.starts_with("local  "), "{hosts}");
    assert!(hosts.contains("(linux)  this host  vibed 9.9.9"), "{hosts}");
    let why = fails(&["host", "remove", "local"]);
    assert!(why.contains("local is this host"), "{why}");
}
