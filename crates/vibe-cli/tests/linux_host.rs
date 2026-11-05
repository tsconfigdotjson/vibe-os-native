//! Adding a Linux host, end to end, with every process real but the
//! network: a Mac's `vibed` installs `vibed` over `ssh` with its real
//! install scripts, links to it with `ssh <host> vibe rpc`, and drives its
//! agents through MCP. Here `ssh` runs each command on this machine, in a
//! home of the host's own, and `systemctl --user` starts its `vibed` as a
//! plain process. Pulling the network is a file that makes `ssh` fail and
//! drop what it carries.
//!
//! The same flow against a real server is `real_linux_host`, which
//! scripts/check-full.sh runs on the Mac with `VIBE_TEST_LINUX_HOST` set.

#[path = "../../vibed/tests/common/mod.rs"]
mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_core::paths::Layout;
use vibe_core::rpc::{KeyCreateParams, MAX_MESSAGE_BYTES};
use vibe_services::SecretStore;
use vibe_services::secrets::FileStore;
use vibe_services::testing::{FakeService, SECRET_MARK};
use vibed::core::Core;
use vibed::link::Timing;
use vibed::remote::Ssh;
use vibed::server;

const VIBE: &str = env!("CARGO_BIN_EXE_vibe");

fn write_exe(path: &Path, body: &str) {
    common::write_script(path, body);
}

/// `ssh -R`'s forward, for [`fake_ssh`]: binds the host's end of the
/// tunnel, refusing one that's there as sshd does, and relays each
/// connection to it to the socket here. It says `ready` once it's bound.
const RELAY: &str = r#"
import os, socket, sys, threading
remote, local = sys.argv[1], sys.argv[2]
if os.path.exists(remote):
    print("Error: remote port forwarding failed for listen path " + remote, flush=True)
    sys.exit(1)
srv = socket.socket(socket.AF_UNIX)
srv.bind(remote)
os.chmod(remote, 0o600)
srv.listen(16)
print("ready", flush=True)
def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except OSError:
        pass
    for s in (a, b):
        try:
            s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
while True:
    c, _ = srv.accept()
    l = socket.socket(socket.AF_UNIX)
    try:
        l.connect(local)
    except OSError:
        c.close()
        continue
    threading.Thread(target=pipe, args=(c, l), daemon=True).start()
    threading.Thread(target=pipe, args=(l, c), daemon=True).start()
"#;

/// `ssh` for the test: options are taken and ignored, and the command runs
/// here, with `$HOME` the host's own home, `<hosts>/<destination>`, and
/// the host's tools first on `PATH`. With `-R remote:local`, a relay
/// forwards the host's socket to the one here, failing as
/// `ExitOnForwardFailure` does when it can't. While
/// `<hosts>/<destination>.d/down` exists, it can't connect, and what it
/// carries is dropped.
///
/// A relay outlives whatever starts it, so this takes the test's [`Watch`], which only
/// [`bounded`] makes, and which ends it however the test goes.
fn fake_ssh(watch: &Watch, hosts: &Path) -> PathBuf {
    watch.owns(hosts);
    fs::write(hosts.join("relay.py"), RELAY).unwrap();
    let ssh = hosts.join("ssh");
    write_exe(
        &ssh,
        &format!(
            r#"#!/bin/sh
fwd=
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift 2 ;;
    -R) fwd=$2; shift 2 ;;
    --) shift; break ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest=$1; shift
home="{hosts}/$dest"
d="$home.d"
if [ ! -d "$home" ]; then
  echo "ssh: Could not resolve hostname $dest: Name or service not known" >&2; exit 255
fi
if [ -e "$d/down" ]; then
  echo "ssh: connect to host $dest port 22: Network is unreachable" >&2; exit 255
fi
relay=
if [ -n "$fwd" ]; then
  said=$(mktemp)
  python3 "{hosts}/relay.py" "${{fwd%%:*}}" "${{fwd#*:}}" >"$said" 2>&1 &
  relay=$!
  until grep -q ready "$said"; do
    if ! kill -0 "$relay" 2>/dev/null; then cat "$said" >&2; rm -f "$said"; exit 255; fi
    sleep 0.05
  done
  rm -f "$said"
fi
export HOME="$home" SHELL="$d/bin/login-shell" PATH="$d/bin:$PATH"
. "$d/env"
cd "$HOME" || exit 255
exec 3<&0
/bin/sh -c "$*" <&3 3<&- &
child=$!
exec 3<&-
while kill -0 "$child" 2>/dev/null; do
  if [ -e "$d/down" ]; then
    pkill -TERM -P "$child" 2>/dev/null
    kill "$child" 2>/dev/null
    [ -n "$relay" ] && kill "$relay" 2>/dev/null
    echo "Connection to $dest closed by remote host." >&2
    exit 255
  fi
  sleep 0.1
done
[ -n "$relay" ] && kill "$relay" 2>/dev/null
wait "$child"
"#,
            hosts = hosts.display()
        ),
    );
    ssh
}

/// A Linux host at `dest`, for [`fake_ssh`]: a home, and the tools a
/// server would have where this machine lacks them or must stand in.
struct FakeHost {
    home: PathBuf,
    d: PathBuf,
    mcp: SocketAddr,
}

impl FakeHost {
    fn new(hosts: &Path, dest: &str, claude: &Path) -> FakeHost {
        let home = hosts.join(dest);
        let d = hosts.join(format!("{dest}.d"));
        let bin = d.join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        // It's Linux, whatever this machine is.
        write_exe(
            &bin.join("uname"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo {} ;; *) exec /usr/bin/uname \"$@\" ;; esac\n",
                std::env::consts::ARCH
            ),
        );
        // Only Linux has `/proc`: see the test that makes this file.
        if !cfg!(target_os = "linux") {
            write_exe(
                &bin.join("readlink"),
                r#"#!/bin/sh
case "$1" in
/proc/*/exe)
  if [ -e "$HOME.d/exe-deleted" ]; then
    rm -f "$HOME.d/exe-deleted"
    echo "$HOME/.local/lib/vibeos/vibed (deleted)"
  fi
  exit 0 ;;
esac
exec /usr/bin/readlink "$@"
"#,
            );
        }
        // `systemctl --user`: the service runs as a plain process, with the
        // unit's PATH, and restarting it stops only vibed (KillMode=process).
        write_exe(
            &bin.join("systemctl"),
            r#"#!/bin/sh
[ "$1" = --user ] && shift
unit="$HOME/.config/systemd/user/vibed.service"
pidfile="$HOME.d/vibed.pid"
start() {
  path=$(sed -n 's/^Environment="PATH=\(.*\)"$/\1/p' "$unit")
  grep -q '^KillMode=process$' "$unit" || { echo "the unit must keep sessions" >&2; exit 1; }
  PATH="$path" "$HOME/.local/lib/vibeos/vibed" >>"$HOME.d/vibed.log" 2>&1 </dev/null &
  echo $! >"$pidfile"
}
stop() {
  if [ -f "$pidfile" ]; then
    pid=$(cat "$pidfile"); kill "$pid" 2>/dev/null
    i=0; while kill -0 "$pid" 2>/dev/null && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done
    rm -f "$pidfile"
  fi
}
case "$1" in
  show-environment) echo "HOME=$HOME" ;;
  daemon-reload) ;;
  enable) touch "$HOME.d/enabled" ;;
  start) if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then :; else start; fi ;;
  show) cat "$pidfile" 2>/dev/null ;;
  restart) stop; start ;;
  stop) stop ;;
  *) echo "fake systemctl: $*" >&2; exit 1 ;;
esac
"#,
        );
        write_exe(
            &bin.join("loginctl"),
            r#"#!/bin/sh
case "$1" in
  show-user) if [ -e "$HOME.d/linger" ]; then echo Linger=yes; else echo Linger=no; fi ;;
  enable-linger)
    # As logind does without polkit: refused, but for root.
    if [ -e "$HOME.d/refuse-linger" ] && [ -z "$AS_ROOT" ]; then
      echo 'Could not enable linger: Access denied' >&2
      exit 1
    fi
    touch "$HOME.d/linger" ;;
  *) exit 1 ;;
esac
"#,
        );
        // `-n`, so it never asks: it lets this user run what it's given, or doesn't.
        write_exe(
            &bin.join("sudo"),
            r#"#!/bin/sh
[ "$1" = -n ] || { echo "sudo was asked without -n: $*" >>"$HOME.d/sudo-prompted"; exit 1; }
shift
[ -e "$HOME.d/sudo-ok" ] || { echo 'sudo: a password is required' >&2; exit 1; }
AS_ROOT=1 exec "$@"
"#,
        );
        write_exe(
            &bin.join("journalctl"),
            "#!/bin/sh\ntail -n 1 \"$HOME.d/vibed.log\"\n",
        );
        if Command::new("sh")
            .args(["-c", "command -v sha256sum"])
            .in_files()
            .is_ok_and(|o| !o.status.success())
        {
            write_exe(
                &bin.join("sha256sum"),
                "#!/bin/sh\nexec shasum -a 256 \"$@\"\n",
            );
        }
        // The owner's login shell finds `gh` here, and no `claude`.
        let tools = d.join("tools");
        fs::create_dir_all(&tools).unwrap();
        write_exe(
            &tools.join("gh"),
            "#!/bin/sh\necho 'not logged in' >&2\nexit 1\n",
        );
        write_exe(
            &bin.join("login-shell"),
            &format!(
                "#!/bin/sh\nshift\nPATH={}:/usr/bin:/bin exec /bin/sh -c \"$1\"\n",
                tools.display()
            ),
        );
        // Its vibed runs agents under the test backend: a Linux host's
        // cgroups backend has tests of its own (vibe-env's tests/cgroups.rs),
        // and isn't needed to show the link.
        fs::write(
            d.join("env"),
            format!(
                "export VIBE_TEST_HOST_BROKER=1 VIBE_TEST_BACKEND='{}' VIBE_GH='{}'\n",
                claude.display(),
                tools.join("gh").display()
            ),
        )
        .unwrap();
        let mcp = free_port();
        let config = home.join(".config/vibeos");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("host.toml"),
            format!("mcp_port = {}\n", mcp.port()),
        )
        .unwrap();
        common::repo(&home.join("src/app"));
        FakeHost { home, d, mcp }
    }

    fn socket(&self) -> PathBuf {
        Layout::new(&self.home, std::env::consts::OS)
            .data_dir()
            .join("run/vibed.sock")
    }

    fn pid(&self) -> Option<i32> {
        fs::read_to_string(self.d.join("vibed.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn pull(&self) {
        fs::write(self.d.join("down"), "").unwrap();
    }

    fn plug_in(&self) {
        fs::remove_file(self.d.join("down")).unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(self.d.join("vibed.log")).unwrap_or_default()
    }

    /// Its public endpoint, once it's the external address: on `port`
    /// (as 443 isn't the test's), with a certificate for `domain` made
    /// here, as Let's Encrypt would give. Returns the certificate's file.
    fn serve_publicly(&self, port: u16, domain: &str) -> PathBuf {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![domain.to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let (c, k) = (self.d.join("cert.pem"), self.d.join("key.pem"));
        fs::write(&c, cert.pem()).unwrap();
        fs::write(&k, key.serialize_pem()).unwrap();
        let mut env = fs::OpenOptions::new()
            .append(true)
            .open(self.d.join("env"))
            .unwrap();
        writeln!(
            env,
            "export VIBE_TEST_SERVE_EXTERNAL=1 VIBE_EXTERNAL_PORT={port} VIBE_EXTERNAL_LISTEN=127.0.0.1 \
             VIBE_EXTERNAL_CERT='{}' VIBE_EXTERNAL_KEY='{}'",
            c.display(),
            k.display()
        )
        .unwrap();
        c
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        common::kill_shims(&Layout::new(&self.home, std::env::consts::OS).run_dir());
        if let Some(pid) = self.pid() {
            // SAFETY: the vibed this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn free_port() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// The Linux builds a Mac would carry: this build's own binaries, for this
/// machine's CPU.
fn bundle(dir: &Path) -> PathBuf {
    let arch = dir.join("bundle").join(std::env::consts::ARCH);
    fs::create_dir_all(&arch).unwrap();
    for b in ["vibed", "vibe", "vibe-shim", "vibe-bind"] {
        let src = if b == "vibe" {
            PathBuf::from(VIBE)
        } else {
            common::workspace_bin(b)
        };
        std::os::unix::fs::symlink(src, arch.join(b)).unwrap();
    }
    dir.join("bundle")
}

/// The Mac: a vibed in this process, on a socket and serving MCP.
struct Mac {
    core: Arc<Core>,
    socket: PathBuf,
    mcp: SocketAddr,
    _guard: server::SocketGuard,
    rt: tokio::runtime::Runtime,
}

fn mac(golden: &common::Golden, ssh: &Path, bundle: PathBuf) -> Mac {
    mac_with(golden, ssh, bundle, None)
}

/// A Mac whose broker goes to `fake`, keeping what it's given in `logins`.
fn mac_with_services(
    golden: &common::Golden,
    ssh: &Path,
    bundle: PathBuf,
    fake: &FakeService,
    logins: &FileStore,
) -> Mac {
    let services = vibed::services::ServicesConfig {
        services: vec![fake.service()],
        store: Arc::new(logins.clone()),
        place: vibe_services::Place::Mac,
        token_wait: Duration::from_secs(1),
    };
    mac_with(golden, ssh, bundle, Some(services))
}

fn mac_with(
    golden: &common::Golden,
    ssh: &Path,
    bundle: PathBuf,
    services: Option<vibed::services::ServicesConfig>,
) -> Mac {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut config = golden.config();
    // What a real Mac is: on the build its bundle holds.
    config.info.version = env!("CARGO_PKG_VERSION").into();
    // Sharing a connection per host as the app's vibed does: with real ssh
    // (the real-host tests), that's what's tested.
    config.remote = Arc::new(Ssh {
        program: ssh.into(),
        bundle: Some(bundle),
        shared: Some(config.layout.run_dir()),
    });
    config.link_timing = Timing {
        ping_every: Duration::from_millis(500),
        ping_timeout: Duration::from_secs(6),
        connect_timeout: Duration::from_secs(20),
        min_backoff: Duration::from_millis(200),
        max_backoff: Duration::from_secs(1),
        upgrade_gap: Duration::from_millis(500),
        ..Timing::default()
    };
    if services.is_some() {
        config.services = services;
    }
    let core = Arc::new(Core::open(config).unwrap());
    {
        let _in = rt.enter();
        core.spawn_links();
        core.spawn_external();
        drop(core.spawn_services());
    }
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let mcp = listener.local_addr().unwrap();
    rt.spawn(vibe_mcp::http::serve(listener, Arc::clone(&core), true));
    let socket = golden.dir.path().join("run/vibed.sock");
    let (unix, guard) = server::bind(&socket).unwrap();
    let served = Arc::clone(&core);
    rt.spawn(async move {
        let unix = tokio::net::UnixListener::from_std(unix).unwrap();
        server::serve(unix, served, MAX_MESSAGE_BYTES, std::future::pending()).await;
    });
    Mac {
        core,
        socket,
        mcp,
        _guard: guard,
        rt,
    }
}

fn vibe(socket: &Path, args: &[&str]) -> Output {
    common::output_in_files(Command::new(VIBE).arg("--socket").arg(socket).args(args)).unwrap()
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// [`common::output_in_files`], for what isn't `vibe` itself.
trait InFiles {
    fn in_files(&mut self) -> std::io::Result<Output>;
}

impl InFiles for Command {
    fn in_files(&mut self) -> std::io::Result<Output> {
        common::output_in_files(self)
    }
}

/// How long a test here may take in all. They take under a minute on a quiet
/// machine; a call they make can wait far longer than that, and a run that stops
/// here stops a whole `check-full.sh` with it.
const DEADLINE: Duration = Duration::from_secs(6 * 60);

/// What a test here tells [`bounded`] about itself.
#[derive(Clone)]
struct Watch {
    /// The test's temporary directories, by name, which are in the command line of
    /// what it starts that lasts: a shim, a relay, a host's `vibed`. What's left goes
    /// with those.
    dirs: Arc<Mutex<Vec<String>>>,
    at: Arc<Mutex<String>>,
}

impl Watch {
    fn owns(&self, dir: &Path) {
        self.dirs.lock().unwrap().push(unique_name(dir));
    }

    /// Where the test has got to, for when it stops there.
    fn at(&self, what: &str) {
        what.clone_into(&mut self.at.lock().unwrap());
    }

    /// Ends whatever the test started that's still running: a shim, a relay or a
    /// host's `vibed` it left, and any call it's stuck in. They'd otherwise outlive
    /// it, holding on to the pipes of whatever's collecting its output.
    fn end_what_it_started(&self) {
        for dir in self.dirs.lock().unwrap().iter() {
            let _ = Command::new("pkill")
                .args(["-KILL", "-f"])
                .arg(format!("{dir}/"))
                .status();
        }
    }
}

/// A temporary directory's own name: unique, and the same whether or not its
/// path has been through a link, as `/tmp` is on a Mac.
fn unique_name(dir: &Path) -> String {
    dir.file_name().unwrap().to_str().unwrap().to_owned()
}

/// Runs `test`, failing it if it takes longer than `within` rather than waiting on
/// it forever, and says how far it got.
fn bounded(within: Duration, test: impl FnOnce(&Watch) + Send + 'static) {
    let watch = Watch {
        dirs: Arc::default(),
        at: Arc::default(),
    };
    let (finished, ended) = mpsc::channel::<()>();
    let running = {
        let watch = watch.clone();
        std::thread::spawn(move || {
            // Gone however the test ends, a panic included.
            let _finished = finished;
            test(&watch);
        })
    };
    if ended
        .recv_timeout(within)
        .is_err_and(|e| e == mpsc::RecvTimeoutError::Timeout)
    {
        let at = watch.at.lock().unwrap().clone();
        watch.end_what_it_started();
        // What it was stuck in has ended, so it can clean up after itself, if
        // that's all it was stuck in.
        let _ = ended.recv_timeout(Duration::from_secs(30));
        watch.end_what_it_started();
        let at = if at.is_empty() {
            "somewhere it doesn't say"
        } else {
            at.as_str()
        };
        panic!("past its deadline of {within:?}, at: {at}");
    }
    let result = running.join();
    watch.end_what_it_started();
    if let Err(panicked) = result {
        std::panic::resume_unwind(panicked);
    }
}

/// A test that's stuck fails by its deadline, says where it was, and takes what it
/// started with it. And a test that isn't stuck ends as it would have.
#[test]
fn a_stuck_test_fails_by_its_deadline() {
    let dir = common::short_tempdir();
    // In a directory of the test's own, as what a test here starts always is.
    fs::write(dir.path().join("nap"), "import time\ntime.sleep(600)\n").unwrap();
    let path = dir.path().to_owned();
    let begun = Instant::now();
    let failed = std::panic::catch_unwind(|| {
        bounded(Duration::from_secs(1), move |watch| {
            watch.owns(&path);
            watch.at("its nap");
            // Blocks for far longer than the test may, unless it's ended.
            let _ = Command::new("python3").arg(path.join("nap")).in_files();
        });
    })
    .unwrap_err();
    assert!(
        begun.elapsed() < Duration::from_secs(30),
        "{:?}",
        begun.elapsed()
    );
    let said = failed.downcast_ref::<String>().unwrap();
    assert!(said.contains("deadline"), "{said}");
    assert!(said.contains("at: its nap"), "{said}");
    let left = Command::new("pgrep")
        .args(["-f", &format!("{}/nap", unique_name(dir.path()))])
        .in_files()
        .unwrap();
    assert!(left.stdout.is_empty(), "{left:?}");

    let passed = Arc::new(Mutex::new(false));
    let set = Arc::clone(&passed);
    bounded(Duration::from_secs(30), move |_| {
        *set.lock().unwrap() = true
    });
    assert!(*passed.lock().unwrap());
    let panicked = std::panic::catch_unwind(|| {
        bounded(Duration::from_secs(30), |_| panic!("its own failure"));
    })
    .unwrap_err();
    assert_eq!(panicked.downcast_ref::<&str>(), Some(&"its own failure"));
}

/// The relays running out of `hosts`.
fn relays_of(hosts: &str) -> Vec<String> {
    let found = Command::new("pgrep")
        .args(["-f", &format!("{hosts}/relay.py")])
        .in_files()
        .unwrap();
    String::from_utf8_lossy(&found.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A real relay from `ssh -R`, started the way a tunnel starts one, is gone when the
/// test that made it is, whether it passed or failed (#67). The relay's seen running
/// first, so this can't pass by not finding one.
#[test]
fn a_test_leaves_no_relay_behind_however_it_ends() {
    for fails in [false, true] {
        let hosts = common::short_tempdir();
        let name = unique_name(hosts.path());
        let path = hosts.path().to_owned();
        let looking_for = name.clone();
        let ended = std::panic::catch_unwind(move || {
            bounded(Duration::from_secs(60), move |watch| {
                let ssh = fake_ssh(watch, &path);
                fs::create_dir_all(path.join("me@vps")).unwrap();
                fs::create_dir_all(path.join("me@vps.d/bin")).unwrap();
                fs::write(path.join("me@vps.d/env"), "").unwrap();
                let end = path.join("forwarded.sock");
                // Past the relay's start, and the stand-in's tidying up after it.
                let up = path.join("up");
                let mut started = Command::new(&ssh);
                started
                    .arg("-R")
                    .arg(format!(
                        "{}:{}",
                        end.display(),
                        path.join("here.sock").display()
                    ))
                    .args(["me@vps", &format!("touch '{}'; sleep 30", up.display())])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                // Left running on purpose, as a tunnel is, and reaped when it goes.
                let mut tunnel = started.spawn().unwrap();
                std::thread::spawn(move || tunnel.wait());
                wait_for("the relay", Duration::from_secs(20), || up.exists());
                assert!(!relays_of(&looking_for).is_empty(), "no relay to end");
                assert!(!fails, "forced, with the relay up");
            });
        });
        match ended {
            Ok(()) => assert!(!fails),
            // This failure, with the relay up, and not one before it started.
            Err(panicked) => {
                let said = panicked
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or(panicked.downcast_ref::<&str>().copied())
                    .unwrap_or_default();
                assert!(
                    fails && said.contains("forced, with the relay up"),
                    "{said}"
                );
            }
        }
        wait_for("the relay to go", Duration::from_secs(5), || {
            relays_of(&name).is_empty()
        });
    }
}

fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `vibe mcp` with `key`, to the MCP at `addr`.
struct Bridge {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Bridge {
    fn start(addr: SocketAddr, key: &str) -> Bridge {
        let mut child = Command::new(VIBE)
            .args(["mcp", "--url", &format!("http://{addr}/mcp")])
            .env("VIBE_API_KEY", key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take().unwrap();
        let mut b = Bridge {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        b.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "linux-host-test", "version": "1"}}),
        );
        writeln!(
            b.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        b
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        self.stdin.flush().unwrap();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(60))
                .expect("an answer from vibe mcp");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    /// A tool's text.
    fn call(&mut self, tool: &str, arguments: Value) -> String {
        let r = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_owned()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_linux_host_added_over_ssh_runs_agents_the_mac_drives() {
    bounded(
        DEADLINE,
        a_linux_host_added_over_ssh_runs_agents_the_mac_drives_body,
    );
}

fn a_linux_host_added_over_ssh_runs_agents_the_mac_drives_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));

    // Menu → Add Linux host…, as the CLI does it.
    let added = ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    assert!(added.contains(", just installed."), "{added}");
    assert!(
        added.contains("Lingering is on for you on me@vps"),
        "{added}"
    );
    // Its login shell's PATH has git and gh: those aren't missing.
    let needed = added
        .lines()
        .find_map(|l| l.strip_prefix("Still needed there for its agents: "))
        .and_then(|l| l.split('.').next())
        .unwrap_or("");
    assert!(!needed.contains("gh") && !needed.contains("git"), "{added}");
    let lib = host.home.join(".local/lib/vibeos");
    for b in ["vibed", "vibe", "vibe-shim", "vibe-bind"] {
        let mode = fs::metadata(lib.join(b)).unwrap().permissions().mode();
        assert_eq!(mode & 0o755, 0o755, "{b}");
    }
    assert_eq!(
        fs::read_link(host.home.join(".local/bin/vibe")).unwrap(),
        Path::new("../lib/vibeos/vibe")
    );
    let unit = fs::read_to_string(host.home.join(".config/systemd/user/vibed.service")).unwrap();
    assert!(unit.contains("KillMode=process"), "{unit}");
    assert!(
        host.d.join("enabled").exists(),
        "enabled, so it starts at boot"
    );
    let pid = host.pid().expect("vibed runs on the host");

    let link_is = |want: &str| {
        let hosts = ok(vibe(&mac.socket, &["host"]));
        hosts
            .lines()
            .any(|l| l.starts_with("vps  ") && l.contains(want))
    };
    wait_for("the link", Duration::from_secs(30), || {
        link_is("  connected")
    });

    // Installing again changes nothing: same binaries, same service.
    let again = ok(vibe(&mac.socket, &["host", "upgrade", "vps"]));
    assert!(again.contains(", as it did."), "{again}");
    assert_eq!(host.pid(), Some(pid), "vibed wasn't restarted");

    // A project on the host, set up from the Mac.
    let app = host.home.join("src/app");
    ok(vibe(
        &mac.socket,
        &["project", "add", "--host", "vps", app.to_str().unwrap()],
    ));
    // A small profile, so the agent fits beside whatever else this machine
    // runs.
    let made = ok(vibe(
        &mac.socket,
        &[
            "profile", "create", "app", "small", "--memory", "1G", "--host", "vps",
        ],
    ));
    assert_eq!(
        made,
        "created small  linux, opus, high reasoning, auto mode, 1G\n"
    );

    // An agent started there through the Mac's MCP.
    let key = mac
        .core
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap()
        .secret;
    let mut mcp = Bridge::start(mac.mcp, &key);
    let started = mcp.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "small", "prompt": "hello",
               "host": "vps", "name": "remote-one"}),
    );
    assert!(started.contains("vps:remote-one"), "{started}");

    // It shows in the menu's listing, with the line that attaches to it.
    let listed: Value = serde_json::from_str(&ok(Command::new(VIBE)
        .arg("--socket")
        .arg(&mac.socket)
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map(|mut c| {
            writeln!(
                c.stdin.take().unwrap(),
                "{}",
                json!({"jsonrpc": "2.0", "id": 1, "method": "agents.list", "params": {"host": "vps"}})
            )
            .unwrap();
            c.wait_with_output().unwrap()
        })
        .unwrap()))
    .unwrap();
    let agent = &listed["result"]["agents"][0];
    assert_eq!(agent["host"], "vps", "{listed}");
    assert_eq!(agent["ref"], "remote-one");
    assert_eq!(
        agent["attach"],
        "ssh -t me@vps .local/lib/vibeos/vibe attach remote-one"
    );
    let all = mcp.call("vibe_list_agents", json!({}));
    assert!(all.contains("vps:remote-one"), "{all}");
    // A wait goes over the link too, as every call for it does.
    let waited = mcp.call(
        "vibe_wait",
        json!({"agents": "vps:remote-one", "timeout": 1}),
    );
    assert!(!waited.contains("unavailable"), "{waited}");

    // The Mac's key reached the host, whose own MCP serves its own agents:
    // standalone, it needs no Mac.
    let socket = host.socket();
    wait_for(
        "the key pushed to the host",
        Duration::from_secs(20),
        || ok(vibe(&socket, &["key"])).contains("laptop"),
    );
    let mut own = Bridge::start(host.mcp, &key);
    let there = own.call("vibe_list_agents", json!({}));
    assert!(there.contains("remote-one"), "{there}");
    assert!(!there.contains("vps:"), "on the host it's its own: {there}");

    // Pull the network: the host is down, its agent unavailable, and
    // nothing stops on the host.
    host.pull();
    wait_for("the link to go down", Duration::from_secs(30), || {
        link_is("  down")
    });
    let all = mcp.call("vibe_list_agents", json!({}));
    assert!(
        all.contains("vps:remote-one") && all.contains("unavailable"),
        "{all}"
    );
    let there = own.call("vibe_list_agents", json!({}));
    assert!(there.contains("remote-one"), "{there}");
    // Back again: it reconnects on its own.
    host.plug_in();
    wait_for("the link to come back", Duration::from_secs(30), || {
        link_is("  connected")
    });
    let all = mcp.call("vibe_list_agents", json!({}));
    assert!(
        all.contains("vps:remote-one") && !all.contains("unavailable"),
        "{all}"
    );

    // Deleting it there, from the Mac.
    let deleted = mcp.call(
        "vibe_delete_agent",
        json!({"agent": "vps:remote-one", "force": true}),
    );
    assert!(deleted.contains("remote-one"), "{deleted}");
    ok(vibe(&mac.socket, &["host", "remove", "vps"]));
    assert!(
        host.pid().is_some_and(common::alive),
        "it runs on, on its own: {}",
        host.log()
    );
    drop(mac.rt);
}

/// A real host's state is only safe under `scripts/keep-host-state.sh`, as a test's Mac
/// changes it: check-full.sh runs these under it, and they refuse to run otherwise.
fn the_real_hosts_state_is_kept() {
    assert!(
        std::env::var_os("VIBE_TEST_HOST_STATE_KEPT").is_some(),
        "run this through scripts/keep-host-state.sh, which puts the real host back as it was"
    );
}

/// This process's `ssh`es matching `pattern`: never the real Mac's to the same host,
/// which may be running beside this test.
fn my_ssh(pattern: &str) -> String {
    let found = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string(), "-f", pattern])
        .in_files()
        .unwrap();
    String::from_utf8(found.stdout).unwrap()
}

/// The process of the connection `golden`'s Mac shares to `dest`, from what it says itself.
/// That's the one to stop to hang the connection: the link's own `ssh` hands its ends over
/// to it, so stopping that stops nothing, and answers go on coming.
fn shared_connection(golden: &common::Golden, dest: &str) -> String {
    let remote = vibed::remote::Ssh {
        program: "ssh".into(),
        bundle: None,
        shared: Some(golden.config().layout.run_dir()),
    };
    let check = Command::new("ssh")
        .args(remote.sharing(dest))
        .args(["-O", "check", "--", dest])
        .in_files()
        .unwrap();
    let said = String::from_utf8_lossy(&check.stderr).into_owned();
    connection_pid(&said).unwrap_or_else(|| panic!("no shared connection to {dest}: {said}"))
}

/// The pid in what `ssh -O check` says: `Master running (pid=1234)`.
fn connection_pid(said: &str) -> Option<String> {
    let pid = said.split("(pid=").nth(1)?.split(')').next()?;
    (!pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())).then(|| pid.to_owned())
}

#[test]
fn a_shared_connections_pid_is_read_from_what_ssh_says() {
    assert_eq!(
        connection_pid("Master running (pid=1234)\r\n").as_deref(),
        Some("1234")
    );
    assert_eq!(connection_pid("Control socket connect: No such file"), None);
}

/// Waits for `name`'s link to go down, seen as it happens.
fn wait_for_link_down(
    mac: &Mac,
    mut events: tokio::sync::broadcast::Receiver<vibe_core::rpc::Event>,
    name: &str,
    within: Duration,
) {
    use tokio::sync::broadcast::error::RecvError;
    mac.rt.block_on(async {
        tokio::time::timeout(within, async {
            loop {
                match events.recv().await {
                    Ok(e)
                        if e.kind == vibe_core::model::EventKind::HostLink
                            && e.host == name
                            && e.state.as_deref() == Some("down") =>
                    {
                        return;
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => panic!("the Mac's events ended"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {name}'s link to go down"));
    });
}

/// One JSON-RPC call on the Mac, as the app makes it.
fn on_mac(mac: &Mac, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let answer = mac.rt.block_on(vibed::dispatch::handle_message(
        &mac.core,
        &mut vibed::dispatch::Session::default(),
        line.as_bytes(),
    ));
    serde_json::from_str(&answer.expect("an answer")).unwrap()
}

/// A process on the host, as its own user runs it.
fn on_host(host: &FakeHost) -> Command {
    let mut c = Command::new(VIBE);
    c.env("HOME", &host.home)
        .env_remove("VIBE_SOCKET")
        .env_remove("VIBE_SERVICES_SOCKET")
        .env_remove("XDG_CACHE_HOME");
    c
}

/// `vibe mcp-proxy linear` on the host, as an agent's MCP client starts it.
struct HostShim {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next: i64,
    read: String,
}

impl Drop for HostShim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl HostShim {
    fn start(host: &FakeHost, agent: &str) -> HostShim {
        let mut child = on_host(host)
            .args(["mcp-proxy", "linear"])
            .env("VIBE_AGENT", agent)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let mut shim = HostShim {
            child,
            stdin,
            lines,
            next: 10,
            read: String::new(),
        };
        let init = shim.ask(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "vibe-linear");
        writeln!(
            shim.stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        shim
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let m = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{m}").unwrap();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect("the shim answers");
            self.read.push_str(&line);
            self.read.push('\n');
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn call(&mut self, tool: &str) -> String {
        let v = self.ask("tools/call", json!({"name": tool, "arguments": {}}));
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn has_tool(&mut self, tool: &str) -> bool {
        for _ in 0..100 {
            let v = self.ask("tools/list", json!({}));
            if v["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == tool)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

/// Whether `text` is in any file under `dir`.
fn any_file_has(dir: &Path, text: &str) -> Option<PathBuf> {
    for e in fs::read_dir(dir).ok()?.flatten() {
        let path = e.path();
        if path.is_dir() {
            if let Some(found) = any_file_has(&path, text) {
                return Some(found);
            }
        } else if fs::read(&path).is_ok_and(|b| b.windows(text.len()).any(|w| w == text.as_bytes()))
        {
            return Some(path);
        }
    }
    None
}

/// The design in one run, with every process real: the app connects a host to
/// a service from the Mac, the host holds the login, and when the Mac goes away
/// the host's agents carry on, refresh by themselves, and can be mended with a
/// key in the host's own shell. `ssh` and the service are the only stand-ins.
#[test]
fn a_linux_host_that_owns_a_login_serves_and_refreshes_alone_with_the_mac_gone() {
    bounded(
        DEADLINE,
        a_linux_host_that_owns_a_login_serves_and_refreshes_alone_with_the_mac_gone_body,
    );
}

fn a_linux_host_that_owns_a_login_serves_and_refreshes_alone_with_the_mac_gone_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let fake = FakeService::start_detached();
    // Its vibed finds its stand-in for Linear; a debug build's only.
    let env = host.d.join("env");
    let mut setup = fs::read_to_string(&env).unwrap();
    setup.push_str(&format!(
        "export VIBE_TEST_SERVICES_UPSTREAM='{}'\n",
        fake.base
    ));
    fs::write(&env, setup).unwrap();
    let on_the_mac = FileStore(golden.dir.path().join("mac-services"));
    let carried = bundle(golden.dir.path());
    let mac = mac_with_services(&golden, &ssh, carried.clone(), &fake, &on_the_mac);

    ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    wait_for("the link", Duration::from_secs(30), || {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .any(|l| l.starts_with("vps  ") && l.contains("  connected"))
    });

    // Settings → Services → Connect: the host is its owner, as a linked Linux
    // host is the default. Its vibed makes the verifier and the exchange, and all
    // the Mac does is carry a code.
    let overview = on_mac(&mac, "services.overview", json!({}));
    assert_eq!(
        overview["result"]["services"][0]["owner"], "vps",
        "{overview}"
    );
    assert_eq!(overview["result"]["services"][0]["signed_in"], false);
    let begun = on_mac(
        &mac,
        "services.connect",
        json!({"host": "vps", "service": "linear"}),
    );
    let url = begun["result"]["url"].as_str().expect("a page").to_owned();
    assert_eq!(begun["result"]["callback_scheme"], "vibeos");
    let back = mac.rt.block_on(fake.sign_in(&url));
    let done = on_mac(
        &mac,
        "services.complete",
        json!({"host": "vps", "service": "linear", "callback_url": back}),
    );
    assert_eq!(done["result"]["state"], "connected", "{done}");

    // It's on the host, private; the Mac has an access token and nothing more.
    let dir = Layout::new(&host.home, std::env::consts::OS)
        .data_dir()
        .join("services");
    let login = dir.join("linear.login");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&login), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(
        any_file_has(golden.dir.path(), SECRET_MARK),
        None,
        "nothing of a token in the clear on the Mac"
    );
    wait_for("the Mac to follow", Duration::from_secs(30), || {
        ok(vibe(&mac.socket, &["service"])).starts_with("linear  connected  login on vps")
    });
    assert_eq!(on_the_mac.load("linear").unwrap().unwrap().refresh, None);

    // Its own CLI sees it's here.
    let status = ok(on_host(&host).arg("service").in_files().unwrap());
    assert!(
        status.starts_with("linear  connected  login here"),
        "{status}"
    );

    // An agent on the host, on the real shim.
    let mut shim = HostShim::start(&host, "quiet-amber-otter");
    assert!(shim.has_tool("list_issues"));
    assert_eq!(shim.call("list_issues"), "issues on /mcp");
    let before = fs::read(&login).unwrap();

    // The Mac goes: the network's pulled, and its vibed stops.
    fs::write(host.d.join("down"), "").unwrap();
    drop(mac);

    assert_eq!(shim.call("list_issues"), "issues on /mcp");
    assert!(shim.has_tool("create_issue"));

    // Its token runs out, and the host refreshes alone, and keeps what it got.
    fake.expire_access();
    assert_eq!(shim.call("list_issues"), "issues on /mcp");
    assert_eq!(fake.refreshes(), 1);
    assert_ne!(
        fs::read(&login).unwrap(),
        before,
        "the rotated login was kept"
    );
    assert_eq!(mode(&login), 0o600);

    // Its login is lost: the agent is told how it's mended, and tools list.
    fake.expire_access();
    fake.refuse_refresh("invalid_grant");
    let lost = shim.call("list_issues");
    assert!(lost.contains("needs reconnecting on the Mac"), "{lost}");
    assert!(shim.has_tool("list_issues"));
    let status = ok(on_host(&host).arg("service").in_files().unwrap());
    assert!(status.starts_with("linear  needs_reconnect"), "{status}");
    assert!(status.contains("invalid_grant"), "{status}");

    // There's no way to mend it from here: signing in takes the Mac's browser.
    let refused = on_host(&host)
        .args(["service", "set-key", "linear"])
        .in_files()
        .unwrap();
    assert!(!refused.status.success());

    // Turned off, with no Mac to do it from: the agent's told how to turn it back
    // on. That comes first, before what's wrong with the login.
    let off = ok(on_host(&host)
        .args(["service", "off", "linear"])
        .in_files()
        .unwrap());
    assert!(off.contains("turned off"), "{off}");
    let told = shim.call("list_issues");
    assert!(told.contains("`vibe service on linear`"), "{told}");
    let on = ok(on_host(&host)
        .args(["service", "on", "linear"])
        .in_files()
        .unwrap());
    assert!(!on.contains("turned off"), "{on}");

    // The Mac's back, and Reconnect there lands at the owner, in the browser and
    // back: the agent goes on with no restart.
    fs::remove_file(host.d.join("down")).unwrap();
    let mac = mac_with_services(&golden, &ssh, carried, &fake, &on_the_mac);
    wait_for("the link again", Duration::from_secs(30), || {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .any(|l| l.starts_with("vps  ") && l.contains("  connected"))
    });
    let begun = on_mac(
        &mac,
        "services.connect",
        json!({"host": "vps", "service": "linear"}),
    );
    let url = begun["result"]["url"].as_str().expect("a page").to_owned();
    let back = mac.rt.block_on(fake.sign_in(&url));
    let done = on_mac(
        &mac,
        "services.complete",
        json!({"host": "vps", "service": "linear", "callback_url": back}),
    );
    assert_eq!(done["result"]["state"], "connected", "{done}");
    assert_eq!(shim.call("list_issues"), "issues on /mcp");

    // And in all of that, what's readable on the host's disk or in its logs
    // holds no token: only the login file does, encoded, private.
    let shown = shim.read.clone();
    assert!(!shown.contains(SECRET_MARK), "{shown}");
    let found = any_file_has(hosts.path(), SECRET_MARK);
    assert_eq!(found, None, "a secret in the clear on the host");
}

#[test]
fn an_unreachable_server_is_refused_with_why() {
    bounded(DEADLINE, an_unreachable_server_is_refused_with_why_body);
}

fn an_unreachable_server_is_refused_with_why_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    let out = vibe(&mac.socket, &["host", "add", "vps", "me@nowhere"]);
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(
        why.contains("can't reach me@nowhere over SSH: ssh: Could not resolve hostname"),
        "{why}"
    );
    assert!(!ok(vibe(&mac.socket, &["host"])).contains("vps"));
}

/// A real Linux server, from `VIBE_TEST_LINUX_HOST` (an SSH destination that
/// logs in with a key), with the Linux builds in `VIBE_HOST_BUNDLE`:
/// install, link, an agent through the Mac's MCP, and the link noticing a
/// hung connection. scripts/check-full.sh runs it on the Mac.
#[test]
#[ignore = "needs a real Linux server: VIBE_TEST_LINUX_HOST and VIBE_HOST_BUNDLE"]
fn real_linux_host() {
    the_real_hosts_state_is_kept();
    let dest = std::env::var("VIBE_TEST_LINUX_HOST")
        .ok()
        .filter(|d| !d.is_empty())
        .expect("VIBE_TEST_LINUX_HOST: an SSH destination that logs in with a key");
    let bundle = PathBuf::from(
        std::env::var_os("VIBE_HOST_BUNDLE")
            .expect("VIBE_HOST_BUNDLE: the Linux builds to install"),
    );
    let golden = common::Golden::new();
    let mac = mac(&golden, Path::new("ssh"), bundle);
    let added = ok(vibe(&mac.socket, &["host", "add", "testhost", &dest]));
    eprintln!("{added}");
    let link_is = |want: &str| {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .any(|l| l.starts_with("testhost  ") && l.contains(want))
    };
    wait_for("the link", Duration::from_secs(60), || {
        link_is("  connected")
    });
    let key = mac
        .core
        .create_key(KeyCreateParams {
            name: "real-host-test".into(),
        })
        .unwrap()
        .secret;
    let mut mcp = Bridge::start(mac.mcp, &key);
    let projects = mcp.call("vibe_list_projects", json!({"host": "testhost"}));
    eprintln!("{projects}");
    // An agent needs a project there; when the server has a project named
    // by VIBE_TEST_LINUX_PROJECT, start one, in a cgroup of its own, and
    // see it in the menu's listing.
    if let Ok(project) = std::env::var("VIBE_TEST_LINUX_PROJECT") {
        // Unique to the run, as a delete keeps its agent's branch, and a name that has one
        // is taken.
        let name = format!("real-host-test-{}", std::process::id());
        let started = mcp.call(
            "vibe_start_agent",
            json!({"project": project, "profile": "software-eng-linux", "prompt": "Say hi",
                   "host": "testhost", "name": name}),
        );
        eprintln!("{started}");
        assert!(started.contains("testhost:"), "{started}");
        let all = mcp.call("vibe_list_agents", json!({"host": "testhost"}));
        assert!(
            all.contains(&format!("ssh -t {dest} .local/lib/vibeos/vibe attach")),
            "{all}"
        );
        mcp.call(
            "vibe_delete_agent",
            json!({"agent": format!("testhost:{name}"), "force": true}),
        );
        drop_test_branches(&dest, &project, &[&name]);
    }
    // A hung connection, as a pulled cable would make it.
    let (_, events) = mac.core.subscribe();
    let connection = shared_connection(&golden, &dest);
    let _ = Command::new("kill").args(["-STOP", &connection]).status();
    wait_for_link_down(&mac, events, "testhost", Duration::from_secs(60));
    let _ = Command::new("kill").args(["-KILL", &connection]).status();
    wait_for("the link to come back", Duration::from_secs(90), || {
        link_is("  connected")
    });
    ok(vibe(&mac.socket, &["host", "remove", "testhost"]));
    drop(mac.rt);
}

/// An MCP client on the internet: `curl` over HTTPS to the external
/// address, with an API key, trusting the certificate `ca` (none: the
/// system's, as for Let's Encrypt's), and reaching `domain` at `resolve`
/// when it's given (`host:port:addr`, as `curl --resolve` takes it).
struct Outside {
    url: String,
    ca: Option<PathBuf>,
    resolve: Option<String>,
    key: Option<String>,
    next: u64,
}

impl Outside {
    /// Posts a JSON-RPC request, and gives its HTTP status and body. The
    /// key goes to curl on its stdin, never on its command line.
    fn request(&mut self, method: &str, params: Value) -> (u16, String) {
        self.next += 1;
        let body = json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        let mut config = String::from(
            "header = \"Content-Type: application/json\"\n\
             header = \"Accept: application/json, text/event-stream\"\n\
             header = \"MCP-Protocol-Version: 2025-06-18\"\n",
        );
        if let Some(key) = &self.key {
            config.push_str(&format!("header = \"Authorization: Bearer {key}\"\n"));
        }
        let mut said = tempfile::NamedTempFile::new().expect("curl's config");
        said.write_all(config.as_bytes()).unwrap();
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "--max-time",
            "300",
            "-o",
            "-",
            "-w",
            "\n%{http_code}",
        ]);
        if let Some(ca) = &self.ca {
            cmd.arg("--cacert").arg(ca);
        }
        if let Some(r) = &self.resolve {
            cmd.args(["--resolve", r]);
        }
        cmd.arg("--config")
            .arg(said.path())
            .args(["--data-binary", &body.to_string(), &self.url]);
        let out = common::output_in_files(&mut cmd).expect("curl");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        let code = code.trim().parse().unwrap_or_else(|_| {
            panic!(
                "curl {method}: {text} {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (code, body.to_owned())
    }

    fn tool(&mut self, name: &str, arguments: Value) -> (bool, String) {
        let (code, body) =
            self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(code, 200, "{name}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
        let r = &v["result"];
        (
            r["isError"] == true,
            r["content"][0]["text"].as_str().unwrap_or("").to_owned(),
        )
    }

    fn call(&mut self, name: &str, arguments: Value) -> String {
        let (error, text) = self.tool(name, arguments.clone());
        assert!(!error, "{name} {arguments}: {text}");
        text
    }

    /// Starts nothing new: sends a line to `agent`, waits for its answer,
    /// and reads it back, as an orchestrator's round does.
    fn talk(&mut self, agent: &str) {
        let waited = self.call("vibe_wait", json!({"agents": agent, "timeout": 120}));
        assert!(waited.contains("has answered"), "{agent}: {waited}");
        let line = format!("hello from outside to {agent}");
        let sent = self.call("vibe_send", json!({"agent": agent, "text": line}));
        assert!(sent.contains("submitted"), "{sent}");
        let waited = self.call("vibe_wait", json!({"agents": agent, "timeout": 120}));
        assert!(waited.contains("has answered"), "{agent}: {waited}");
        let read = self.call("vibe_read_messages", json!({"agent": agent, "last": 2}));
        assert!(read.contains(&line), "{agent}: {read}");
    }
}

/// The TCP ports process `pid` listens on beyond loopback, as `lsof` sees
/// them: none, for a Mac with external access on.
fn public_listeners(pid: u32) -> Vec<String> {
    let out = Command::new("lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"])
        .in_files()
        .expect("lsof");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(8).map(str::to_owned))
        .filter(|addr| {
            !(addr.starts_with("127.")
                || addr.starts_with("[::1]")
                || addr.starts_with("localhost"))
        })
        .collect()
}

#[test]
fn external_access_through_a_linux_host() {
    bounded(DEADLINE, external_access_through_a_linux_host_body);
}

fn external_access_through_a_linux_host_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let port = free_port().port();
    let ca = host.serve_publicly(port, "vps.test");
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    let vps_line = || {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .find(|l| l.starts_with("vps  "))
            .unwrap_or_default()
            .to_owned()
    };
    wait_for("the link", Duration::from_secs(30), || {
        vps_line().contains("  connected")
    });
    // `app` on each: a small profile on the host, so its agent fits beside
    // whatever this machine runs.
    ok(vibe(
        &mac.socket,
        &[
            "project",
            "add",
            golden.home.join("src/app").to_str().unwrap(),
        ],
    ));
    ok(vibe(
        &mac.socket,
        &[
            "project",
            "add",
            "--host",
            "vps",
            host.home.join("src/app").to_str().unwrap(),
        ],
    ));
    ok(vibe(
        &mac.socket,
        &[
            "profile", "create", "app", "small", "--memory", "1G", "--host", "vps",
        ],
    ));

    // Settings → Hosts → Use as external address, as the CLI does it.
    let on = ok(vibe(&mac.socket, &["host", "external", "vps", "vps.test"]));
    assert!(on.contains("external address https://vps.test/mcp"), "{on}");
    wait_for("the tunnel", Duration::from_secs(60), || {
        vps_line().contains("tunnel up")
    });
    let key = mac
        .core
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap()
        .secret;
    let socket = host.socket();
    wait_for("the key pushed", Duration::from_secs(20), || {
        ok(vibe(&socket, &["key"])).contains("laptop")
    });

    let outside = |key: Option<&str>| Outside {
        url: format!("https://vps.test:{port}/mcp"),
        ca: Some(ca.clone()),
        resolve: Some(format!("vps.test:{port}:127.0.0.1")),
        key: key.map(str::to_owned),
        next: 0,
    };
    // Nothing without a key.
    let (code, _) = outside(None).request("tools/list", json!({}));
    assert_eq!(code, 401);
    let mut c = outside(Some(&key));
    let (code, body) = c.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "outside", "version": "1"}}),
    );
    assert_eq!(code, 200, "{body}");
    // An agent on the VPS and one on the Mac, through the one URL.
    let started = c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "small", "prompt": "talk", "host": "local",
               "name": "on-vps"}),
    );
    assert!(started.contains("started on-vps"), "{started}");
    let started = c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk", "host": "mac",
               "name": "on-mac"}),
    );
    assert!(started.contains("started mac:on-mac"), "{started}");
    c.talk("on-vps");
    c.talk("mac:on-mac");
    let listed = c.call("vibe_list_agents", json!({}));
    assert!(
        listed.contains("on-vps") && listed.contains("mac:on-mac"),
        "{listed}"
    );

    // The public endpoint logs every call, by the key's name; the key
    // itself is in no log.
    let log = host.log();
    assert!(
        log.contains("public: 127.0.0.1 key \"laptop\" tools/call vibe_start_agent"),
        "{log}"
    );
    assert!(
        log.contains("public: 127.0.0.1 POST: refused, no key"),
        "{log}"
    );
    assert!(!log.contains(&key), "the key is in the host's log");

    // The Mac goes offline: the host still answers for its own agents, and
    // the Mac's are unavailable.
    host.pull();
    wait_for("the tunnel down", Duration::from_secs(60), || {
        !vps_line().contains("tunnel up")
    });
    wait_for("the Mac to go", Duration::from_secs(30), || {
        ok(vibe(&socket, &["host"])).contains("tunnel down")
    });
    let listed = c.call("vibe_list_agents", json!({}));
    assert!(listed.contains("on-vps"), "{listed}");
    let mac_row = listed
        .lines()
        .find(|l| l.starts_with("mac:on-mac"))
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(mac_row.contains("unavailable"), "{listed}");
    let read = c.call("vibe_read_messages", json!({"agent": "on-vps", "last": 1}));
    assert!(read.contains("hello from outside"), "{read}");
    host.plug_in();
    wait_for("the tunnel back", Duration::from_secs(60), || {
        vps_line().contains("tunnel up")
    });
    for agent in ["mac:on-mac", "on-vps"] {
        let deleted = c.call("vibe_delete_agent", json!({"agent": agent, "force": true}));
        assert!(deleted.contains("deleted"), "{deleted}");
    }

    // Revoked on the Mac: refused on the host too.
    ok(vibe(&mac.socket, &["key", "revoke", "laptop"]));
    wait_for(
        "the revoke to reach the host",
        Duration::from_secs(20),
        || c.request("tools/list", json!({})).0 == 401,
    );

    // Nothing listens publicly on the Mac, external access or not.
    assert_eq!(public_listeners(std::process::id()), Vec::<String>::new());
    let off = ok(vibe(&mac.socket, &["host", "external", "vps", "off"]));
    assert!(!off.contains("external address"), "{off}");
    drop(mac.rt);
}

/// The tunnel's relays, which don't outlast the test stopped, whatever becomes of it.
struct Relays(Vec<String>);

impl Relays {
    fn signal(&self, which: &str) {
        for pid in &self.0 {
            // It may have gone by itself, once the Mac has.
            let _ = Command::new("kill").args([which, pid]).status();
        }
    }

    /// Lets them go on, and forgets them: a pid kept past now could be anyone's by the
    /// end of the test.
    fn resume(mut self) {
        self.signal("-CONT");
        self.0.clear();
    }
}

impl Drop for Relays {
    fn drop(&mut self) {
        self.signal("-KILL");
    }
}

/// The most a tool for a host's own agents may take, whatever's become of the Mac (#59).
const PROMPTLY: Duration = Duration::from_secs(5);

/// `tool`, on the stopwatch, with what it took said whether it passes or not.
fn timed_within(
    c: &mut Outside,
    limit: Duration,
    when: &str,
    tool: &str,
    args: Value,
) -> Result<String, String> {
    let begun = Instant::now();
    let (error, said) = c.tool(tool, args);
    let took = begun.elapsed();
    eprintln!("{when}: {tool} took {took:?}");
    if error {
        Err(format!("{when}: {tool} failed: {said}"))
    } else if took >= limit {
        Err(format!("{when}: {tool} took {took:?}: {said}"))
    } else {
        Ok(said)
    }
}

/// The first listing after the Mac goes, before the host's next look at its tunnel has
/// told it so. It's timed, and it says the Mac's agent is unavailable.
fn first_listing_with_the_mac_away(c: &mut Outside, when: &str, mac_agent: &str) -> String {
    let listed = timed_within(c, PROMPTLY, when, "vibe_list_agents", json!({}))
        .unwrap_or_else(|why| panic!("{why}"));
    let mac_row = listed
        .lines()
        .find(|l| l.starts_with(mac_agent))
        .unwrap_or_else(|| panic!("{when}: no {mac_agent} in {listed}"));
    assert!(mac_row.contains("unavailable"), "{when}: {listed}");
    listed
}

/// What a round of tools starts its agent in.
struct Round<'a> {
    project: &'a str,
    profile: &'a str,
    prompt: &'a str,
    /// The host to name, when it's not to be found from the project.
    host: Option<&'a str>,
    /// The agent's name, and so its branch, which a delete keeps.
    name: String,
}

/// One round of every tool a host's own agents need, as a caller makes them, each on the
/// stopwatch, once: a second go would hide what waits on the Mac only some of the time.
/// What it started doesn't outlast it, however it ends.
fn every_local_tool_answers_promptly(c: &mut Outside, when: &str, round: &Round) {
    if let Err(why) = one_round_of_local_tools(c, when, round) {
        let _ = c.tool(
            "vibe_delete_agent",
            json!({"agent": round.name, "force": true}),
        );
        panic!("{why}");
    }
}

fn one_round_of_local_tools(c: &mut Outside, when: &str, round: &Round) -> Result<(), String> {
    let name = round.name.as_str();
    timed_within(c, PROMPTLY, when, "vibe_list_agents", json!({}))?;
    let projects = timed_within(c, PROMPTLY, when, "vibe_list_projects", json!({}))?;
    if !projects.contains(&format!("{} (on local)", round.project)) {
        return Err(format!("{when}: {projects}"));
    }
    let mut start = json!({"project": round.project, "profile": round.profile,
                           "prompt": round.prompt, "name": name});
    if let Some(host) = round.host {
        start["host"] = json!(host);
    }
    let started = timed_within(c, PROMPTLY, when, "vibe_start_agent", start)?;
    if !started.contains(&format!("started {name}")) {
        return Err(format!("{when}: {started}"));
    }
    // A bare ref on this host, through all of it.
    let sent = timed_within(
        c,
        PROMPTLY,
        when,
        "vibe_send",
        json!({"agent": name, "text": "hello"}),
    )?;
    if !sent.contains("sent to") {
        return Err(format!("{when}: {sent}"));
    }
    // It's the answer that's timed here, not how long it's let wait.
    let wait = json!({"agents": name, "timeout": 2});
    timed_within(c, PROMPTLY, when, "vibe_wait", wait)?;
    let read = json!({"agent": name});
    timed_within(c, PROMPTLY, when, "vibe_read_output", read)?;
    let read = json!({"agent": name, "last": 1});
    timed_within(c, PROMPTLY, when, "vibe_read_messages", read)?;
    let delete = json!({"agent": name, "force": true});
    let deleted = timed_within(c, PROMPTLY, when, "vibe_delete_agent", delete)?;
    if !deleted.contains("deleted") {
        return Err(format!("{when}: {deleted}"));
    }
    Ok(())
}

/// Takes the branches a real test's agents kept out of `project` on `dest`: a delete keeps
/// its agent's branch, and they're no one's work. It's said if they can't be.
fn drop_test_branches(dest: &str, project: &str, branches: &[&str]) {
    let script = format!(
        "dir=$(.local/lib/vibeos/vibe project | awk -v p='{project}' '$1 == p {{ print $2 }}') \
         && git -C \"$dir\" branch -q -D {}",
        branches.join(" ")
    );
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", dest, &script])
        .in_files()
        .unwrap();
    if !out.status.success() {
        eprintln!(
            "left the branches {branches:?} in {project} on {dest}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// #59's integration, in real processes: a Mac and a Linux host's `vibed`, with the
/// tunnel's stand-in relay stopped, as with a Mac asleep, so its end on the host goes on
/// accepting with nothing behind it; then resumed, merging back; then the Mac stopped.
#[test]
fn external_access_answers_with_the_mac_asleep_or_stopped() {
    bounded(
        DEADLINE,
        external_access_answers_with_the_mac_asleep_or_stopped_body,
    );
}

fn external_access_answers_with_the_mac_asleep_or_stopped_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let port = free_port().port();
    let ca = host.serve_publicly(port, "vps.test");
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    let vps_line = |socket: &Path| {
        ok(vibe(socket, &["host"]))
            .lines()
            .find(|l| l.starts_with("vps  "))
            .unwrap_or_default()
            .to_owned()
    };
    watch.at("the link");
    wait_for("the link", Duration::from_secs(30), || {
        vps_line(&mac.socket).contains("  connected")
    });
    let app = golden.home.join("src/app");
    ok(vibe(
        &mac.socket,
        &["project", "add", app.to_str().unwrap()],
    ));
    let host_only = host.home.join("src/host-only");
    common::repo(&host_only);
    for path in [host.home.join("src/app"), host_only] {
        ok(vibe(
            &mac.socket,
            &["project", "add", "--host", "vps", path.to_str().unwrap()],
        ));
    }
    for project in ["app", "host-only"] {
        ok(vibe(
            &mac.socket,
            &[
                "profile", "create", project, "small", "--memory", "1G", "--host", "vps",
            ],
        ));
    }
    watch.at("the tunnel");
    let on = ok(vibe(&mac.socket, &["host", "external", "vps", "vps.test"]));
    assert!(on.contains("external address https://vps.test/mcp"), "{on}");
    wait_for("the tunnel", Duration::from_secs(60), || {
        vps_line(&mac.socket).contains("tunnel up")
    });
    let key = mac
        .core
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap()
        .secret;
    let socket = host.socket();
    wait_for("the key pushed", Duration::from_secs(20), || {
        ok(vibe(&socket, &["key"])).contains("laptop")
    });
    let mut c = Outside {
        url: format!("https://vps.test:{port}/mcp"),
        ca: Some(ca.clone()),
        resolve: Some(format!("vps.test:{port}:127.0.0.1")),
        key: Some(key),
        next: 0,
    };
    watch.at("the first calls from outside");
    let (code, body) = c.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "outside", "version": "1"}}),
    );
    assert_eq!(code, 200, "{body}");
    for (profile, whose, name) in [
        ("small", "local", "on-vps"),
        ("software-eng-mac", "mac", "on-mac"),
    ] {
        let started = c.call(
            "vibe_start_agent",
            json!({"project": "app", "profile": profile, "prompt": "talk", "host": whose,
                   "name": name}),
        );
        assert!(started.contains("started"), "{started}");
    }
    watch.at("talking to both agents");
    c.talk("on-vps");
    c.talk("mac:on-mac");
    watch.at("both in one listing");
    let merged = |c: &mut Outside| {
        let listed = c.call("vibe_list_agents", json!({}));
        listed.contains("on-vps")
            && listed.contains("mac:on-mac")
            && !listed.contains("unavailable")
    };
    wait_for("both in one listing", Duration::from_secs(30), || {
        merged(&mut c)
    });

    // The Mac sleeps: the tunnel's relay stops, with its socket on the host still there.
    watch.at("the Mac asleep");
    // By its directory's own name, as `/tmp` is a link on a Mac: the relay runs as
    // the path it was given, not the resolved one.
    let relay = format!("{}/relay.py", unique_name(hosts.path()));
    let found = Command::new("pgrep")
        .args(["-f", &relay])
        .in_files()
        .unwrap();
    let relays: Vec<String> = String::from_utf8_lossy(&found.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(!relays.is_empty(), "the tunnel's relay isn't running");
    let relays = Relays(relays);
    relays.signal("-STOP");
    // First, as a Mac still up restarts a tunnel gone quiet for a minute, which forgets
    // what it listed, and that's not what's being shown here.
    // What's this host's own comes first of all, before anything's found the Mac gone,
    // and in less than a call that's waiting on the Mac gets given up on.
    let read = json!({"agent": "on-vps", "last": 1});
    let limit = Duration::from_secs(vibe_core::limits::PEER_READ);
    timed_within(&mut c, limit, "asleep", "vibe_read_messages", read)
        .unwrap_or_else(|why| panic!("{why}"));
    let round = |when: &str| Round {
        project: "host-only",
        profile: "small",
        prompt: "talk",
        host: None,
        name: format!("while-{when}"),
    };
    let listed = first_listing_with_the_mac_away(&mut c, "asleep", "mac:on-mac");
    assert!(listed.contains("on-vps"), "{listed}");
    every_local_tool_answers_promptly(&mut c, "asleep", &round("asleep"));

    // It wakes, and it all merges back with nothing done.
    watch.at("the Mac waking");
    relays.resume();
    let woke = Instant::now();
    wait_for("the Mac back", Duration::from_secs(60), || merged(&mut c));
    eprintln!("the Mac was back {:?} after it woke", woke.elapsed());
    let read = c.call(
        "vibe_read_messages",
        json!({"agent": "mac:on-mac", "last": 1}),
    );
    assert!(!read.contains("unavailable"), "{read}");

    // The Mac stops altogether: the same.
    watch.at("the Mac stopping");
    drop(mac.rt);
    watch.at("the Mac stopped");
    let listed = first_listing_with_the_mac_away(&mut c, "stopped", "mac:on-mac");
    assert!(listed.contains("on-vps"), "{listed}");
    every_local_tool_answers_promptly(&mut c, "stopped", &round("stopped"));
}

/// `scripts/keep-host-state.sh`, against a real `vibed` on a stand-in host, doing what a
/// real-host test does to it: however what it runs ends, the external address is as it
/// was, and it says so if it can't be; and a key gone is a failure. Issue #100.
#[test]
fn a_hosts_state_is_kept_however_the_test_ends() {
    bounded(DEADLINE, a_hosts_state_is_kept_body);
}

fn a_hosts_state_is_kept_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let port = free_port().port();
    let ca = host.serve_publicly(port, "vps.test");
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    wait_for("the link", Duration::from_secs(30), || {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .any(|l| l.starts_with("vps  ") && l.contains("  connected"))
    });
    let socket = host.socket();
    let key = mac
        .core
        .create_key(KeyCreateParams {
            name: "real".into(),
        })
        .unwrap();
    wait_for("the key pushed", Duration::from_secs(20), || {
        ok(vibe(&socket, &["key"])).contains("real")
    });
    let prefix = key.key.prefix.clone();
    let host_vibe = host.home.join(".local/lib/vibeos/vibe");
    // Serving, as the host we mustn't break is, and asked for on the host itself.
    let on_there = |args: &[&str]| ok(on_host(&host).args(args).output().unwrap());
    on_there(&["host", "external", "local", "vps.test"]);
    let domain_there = || {
        ok(on_host(&host).arg("host").output().unwrap())
            .lines()
            .find(|l| l.starts_with("local "))
            .unwrap()
            .contains("external address https://vps.test/mcp")
    };
    assert!(domain_there());
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/keep-host-state.sh");
    let keep = || {
        let mut c = Command::new(&script);
        c.arg("me@vps")
            .arg("--")
            .env("VIBE_SSH", &ssh)
            .env("KEEP_HOST_URL", format!("https://vps.test:{port}/mcp"))
            .env(
                "KEEP_HOST_CURL_ARGS",
                format!(
                    "--cacert {} --resolve vps.test:{port}:127.0.0.1",
                    ca.display()
                ),
            )
            .env_remove("VIBE_SOCKET");
        c
    };
    let off = format!(
        "HOME={} {} host external local off",
        host.home.display(),
        host_vibe.display()
    );
    let run = |command: &str| keep().args(["/bin/sh", "-c", command]).in_files().unwrap();
    let said = |o: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };

    // It's switched off, and all's well otherwise: it's put back.
    let out = run(&format!("[ -n \"$VIBE_TEST_HOST_STATE_KEPT\" ] && {off}"));
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("is as it was"), "{}", said(&out));
    assert!(domain_there(), "{}", said(&out));

    // It's switched off, and what's run fails: put back, and its failure is kept.
    let out = run(&format!("{off}; exit 7"));
    assert_eq!(out.status.code(), Some(7), "{}", said(&out));
    assert!(domain_there(), "{}", said(&out));

    // It's stopped midway, by a signal, as a person would, and then again, impatiently.
    // The second, during the putting back, doesn't stop that.
    let marker = hosts.path().join("off-now");
    let stopped_said = hosts.path().join("stopped-said");
    let mut stopped = keep();
    stopped
        .args([
            "/bin/sh",
            "-c",
            &format!("{off}; touch {}; sleep 600", marker.display()),
        ])
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&stopped_said).unwrap());
    let mut child = stopped.spawn().unwrap();
    // Nobody reading what it says, as with a `| tee` that has gone.
    drop(child.stdout.take());
    wait_for("it to switch it off", Duration::from_secs(30), || {
        marker.exists()
    });
    assert!(!domain_there());
    // SAFETY: signals to the script this test started.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    std::thread::sleep(Duration::from_millis(50));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let ended = child.wait().unwrap();
    assert_eq!(
        ended.code(),
        Some(143),
        "it said: {}",
        fs::read_to_string(&stopped_said).unwrap()
    );
    assert!(domain_there());

    // What a real test leaves behind of its own, if it failed before tidying up, goes.
    let on_host_vibe = format!("HOME={} {}", host.home.display(), host_vibe.display());
    let out = run(&format!("{on_host_vibe} key create external-test"));
    assert!(out.status.success(), "{}", said(&out));
    assert!(!ok(vibe(&socket, &["key"])).contains("external-test"));

    // A key it doesn't recognize is left, as it could be anyone's, and said.
    let out = run(&format!("{on_host_vibe} key create someone-else"));
    assert!(!out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("someone-else"), "{}", said(&out));
    ok(vibe(&socket, &["key", "revoke", "someone-else"]));

    // A key gone can't be put back, but it's not let pass.
    let out = run(&format!("{on_host_vibe} key revoke real"));
    assert!(!out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains(&format!("lost the API key {prefix}")),
        "{}",
        said(&out)
    );

    // If it can't tell how the host is, it runs nothing.
    let ran = hosts.path().join("ran");
    let out = Command::new(&script)
        .args(["me@nowhere", "--", "touch"])
        .arg(&ran)
        .env("VIBE_SSH", &ssh)
        .in_files()
        .unwrap();
    assert!(!out.status.success());
    assert!(said(&out).contains("nothing was run"), "{}", said(&out));
    assert!(!ran.exists());
    drop(mac.rt);
}

/// External access through a real server (`VIBE_TEST_LINUX_HOST`, with the
/// Linux builds in `VIBE_HOST_BUNDLE`) at a public name pointing at it
/// (`VIBE_TEST_EXTERNAL_DOMAIN`), with a real certificate from Let's
/// Encrypt: a client on the internet with the URL and a key starts and
/// talks to an agent on this Mac, and one on the server when
/// `VIBE_TEST_LINUX_PROJECT` names a project there; it gets nothing
/// without a key; and nothing listens publicly on the Mac. With its `ssh -R` stopped, as
/// with a Mac asleep, the first listing and, with that project, every tool for the
/// server's own agents answer in under 5 s, and the Mac merges back on its own.
/// scripts/check-full.sh runs it on the Mac.
///
/// A host has one tunnel's end. So beside a real Mac linked to the same host, this test's
/// tunnel takes it for as long as it runs, and the real one has it back about a minute
/// after that began. Measured, a run takes 11 s, or 35 s with that project. One that takes
/// over a minute loses it to the real Mac partway, and fails as that refuses its key.
#[test]
#[ignore = "needs a real Linux server with a public name: VIBE_TEST_LINUX_HOST, VIBE_HOST_BUNDLE and VIBE_TEST_EXTERNAL_DOMAIN"]
fn real_external_access() {
    the_real_hosts_state_is_kept();
    let need = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| panic!("{name} is needed"))
    };
    let dest = need("VIBE_TEST_LINUX_HOST");
    let domain = need("VIBE_TEST_EXTERNAL_DOMAIN");
    let bundle = PathBuf::from(need("VIBE_HOST_BUNDLE"));
    let golden = common::Golden::new();
    let mac = mac(&golden, Path::new("ssh"), bundle);
    eprintln!(
        "{}",
        ok(vibe(&mac.socket, &["host", "add", "testhost", &dest]))
    );
    let line = || {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .find(|l| l.starts_with("testhost  "))
            .unwrap_or_default()
            .to_owned()
    };
    wait_for("the link", Duration::from_secs(60), || {
        line().contains("  connected")
    });
    ok(vibe(
        &mac.socket,
        &[
            "project",
            "add",
            golden.home.join("src/app").to_str().unwrap(),
        ],
    ));
    eprintln!(
        "{}",
        ok(vibe(
            &mac.socket,
            &["host", "external", "testhost", &domain]
        ))
    );
    // A certificate from Let's Encrypt, and the tunnel. It's kept on the
    // server, so a second run doesn't ask for another.
    wait_for("the external address", Duration::from_secs(300), || {
        let l = line();
        eprintln!("{l}");
        l.contains("tunnel up") && !l.contains("tunnel up:")
    });
    let key = mac
        .core
        .create_key(KeyCreateParams {
            name: "external-test".into(),
        })
        .unwrap()
        .secret;
    let mut c = Outside {
        url: format!("https://{domain}/mcp"),
        ca: None,
        resolve: None,
        key: Some(key.clone()),
        next: 0,
    };
    // The key reaches the server with the next push.
    wait_for("the key pushed", Duration::from_secs(60), || {
        c.request("tools/list", json!({})).0 == 200
    });
    let mut nobody = Outside {
        key: None,
        url: c.url.clone(),
        ca: None,
        resolve: None,
        next: 0,
    };
    assert_eq!(nobody.request("tools/list", json!({})).0, 401);
    let started = c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk", "host": "mac",
               "name": "external-mac"}),
    );
    assert!(started.contains("started mac:external-mac"), "{started}");
    c.talk("mac:external-mac");
    // The Mac's real `ssh -R` stops passing anything with its ends still open, as
    // with a Mac asleep: the server goes on answering, with the Mac's agents as last
    // seen, and has them back when it resumes.
    c.call("vibe_list_agents", json!({}));
    let tunnel = Relays(
        my_ssh("ssh .*-R .*mac\\.sock")
            .lines()
            .map(str::to_owned)
            .collect(),
    );
    assert!(!tunnel.0.is_empty(), "the Mac's ssh -R isn't running");
    tunnel.signal("-STOP");
    first_listing_with_the_mac_away(&mut c, "asleep", "mac:external-mac");
    // Unique to the run, as a delete keeps its agent's branch, and a name that has one is
    // taken.
    let names = (
        format!("while-asleep-{}", std::process::id()),
        format!("external-vps-{}", std::process::id()),
    );
    let project = std::env::var("VIBE_TEST_LINUX_PROJECT").ok();
    match &project {
        // Named, as a real project may be on the Mac too.
        Some(project) => every_local_tool_answers_promptly(
            &mut c,
            "asleep",
            &Round {
                project,
                profile: "software-eng-linux",
                prompt: "Say hi",
                host: Some("local"),
                name: names.0.clone(),
            },
        ),
        None => eprintln!(
            "asleep: only that listing's timed: VIBE_TEST_LINUX_PROJECT names no project for the rest"
        ),
    }
    tunnel.resume();
    let woke = Instant::now();
    wait_for("the Mac back", Duration::from_secs(60), || {
        let listed = c.call("vibe_list_agents", json!({}));
        listed.contains("mac:external-mac") && !listed.contains("unavailable")
    });
    eprintln!("the Mac was back {:?} after it woke", woke.elapsed());
    if let Some(project) = &project {
        let vps = names.1.as_str();
        let started = c.call(
            "vibe_start_agent",
            json!({"project": project, "profile": "software-eng-linux", "prompt": "Say hi",
                   "host": "local", "name": vps}),
        );
        assert!(started.contains(&format!("started {vps}")), "{started}");
        let waited = c.call("vibe_wait", json!({"agents": vps, "timeout": 240}));
        eprintln!("{waited}");
        let read = c.call("vibe_read_messages", json!({"agent": vps, "last": 2}));
        eprintln!("{read}");
        c.call("vibe_delete_agent", json!({"agent": vps, "force": true}));
        drop_test_branches(&dest, project, &[&names.0, vps]);
    }
    c.call(
        "vibe_delete_agent",
        json!({"agent": "mac:external-mac", "force": true}),
    );
    // Nothing listens publicly on the Mac.
    assert_eq!(public_listeners(std::process::id()), Vec::<String>::new());
    ok(vibe(&mac.socket, &["key", "revoke", "external-test"]));
    wait_for("the revoke", Duration::from_secs(60), || {
        c.request("tools/list", json!({})).0 == 401
    });
    // Forgetting the host, and then the Mac being gone altogether, leaves it serving.
    // A revoked key is still an answer from it, over real TLS, so needs no key to work.
    // This test never turns it off: a real Mac may be using it, and if it was off before,
    // keep-host-state.sh turns it off again. (A renewal can't be asked for on a real
    // server: Let's Encrypt's certificates renew at day 60.)
    ok(vibe(&mac.socket, &["host", "remove", "testhost"]));
    drop(mac.rt);
    assert_eq!(
        c.request("tools/list", json!({})).0,
        401,
        "it stopped serving"
    );
    let restart = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &dest])
        .arg("systemctl --user restart vibed.service")
        .in_files()
        .unwrap();
    assert!(restart.status.success(), "{restart:?}");
    wait_for(
        "it to serve after a restart",
        Duration::from_secs(120),
        || c.request("tools/list", json!({})).0 == 401,
    );
}

/// #74: lingering is VibeOS's to turn on: as the user, then with passwordless sudo,
/// and only then one button on the Mac. Never a command to copy.
#[test]
fn lingering_is_turned_on_any_way_that_needs_no_password() {
    bounded(
        DEADLINE,
        lingering_is_turned_on_any_way_that_needs_no_password_body,
    );
}

fn lingering_is_turned_on_any_way_that_needs_no_password_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let state = PathBuf::from(format!("{}.d", host.home.display()));
    fs::write(state.join("refuse-linger"), "").unwrap();
    fs::write(state.join("sudo-ok"), "").unwrap();
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    let added = ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    assert!(added.contains("Lingering is on for you"), "{added}");
    assert!(state.join("linger").exists());
    assert!(!state.join("sudo-prompted").exists(), "sudo must never ask");
}

#[test]
fn a_host_that_refuses_lingering_gets_a_button_not_a_command() {
    bounded(
        DEADLINE,
        a_host_that_refuses_lingering_gets_a_button_not_a_command_body,
    );
}

fn a_host_that_refuses_lingering_gets_a_button_not_a_command_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let state = PathBuf::from(format!("{}.d", host.home.display()));
    fs::write(state.join("refuse-linger"), "").unwrap();
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    let added = ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    assert!(added.contains("Lingering couldn't be turned on"), "{added}");
    assert!(added.contains("Settings → Hosts"), "{added}");
    assert!(!added.contains("sudo loginctl"), "{added}");
    assert!(!state.join("linger").exists());
}

/// #74: a Mac that's moved on upgrades a host behind it, with nobody asked and the
/// host offline at first, and the host's agent keeps running through the restart.
/// All real processes: the Mac's `vibed`, the install's script, the host's `vibed`
/// under its unit's `KillMode=process`, and the agent's shim.
#[test]
fn a_host_behind_the_mac_upgrades_itself_and_its_agent_keeps_running() {
    bounded(
        DEADLINE,
        a_host_behind_the_mac_upgrades_itself_and_its_agent_keeps_running_body,
    );
}

fn a_host_behind_the_mac_upgrades_itself_and_its_agent_keeps_running_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    // It says it runs what an older build would, however often it's restarted.
    let env = host.d.join("env");
    let mut setup = fs::read_to_string(&env).unwrap();
    setup.push_str("export VIBE_TEST_VERSION=0.0.1\n");
    fs::write(&env, setup).unwrap();
    let bundled = bundle(golden.dir.path());
    let mac = mac(&golden, &ssh, bundled.clone());
    ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    let link_is = |want: &str| {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .any(|l| l.starts_with("vps  ") && l.contains(want))
    };
    wait_for("the link", Duration::from_secs(30), || {
        link_is("  connected")
    });
    // It's behind, so it's upgraded at once: to the same build as yet, which changes
    // nothing there, and it says so.
    wait_for("the first upgrade", Duration::from_secs(60), || {
        link_is(&format!("vibed {}", env!("CARGO_PKG_VERSION")))
    });

    // An agent running there.
    let app = host.home.join("src/app");
    ok(vibe(
        &mac.socket,
        &["project", "add", "--host", "vps", app.to_str().unwrap()],
    ));
    ok(vibe(
        &mac.socket,
        &[
            "profile", "create", "app", "small", "--memory", "1G", "--host", "vps",
        ],
    ));
    let key = mac
        .core
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap()
        .secret;
    let mut mcp = Bridge::start(mac.mcp, &key);
    let started = mcp.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "small", "prompt": "hello",
               "host": "vps", "name": "stays-up"}),
    );
    assert!(started.contains("vps:stays-up"), "{started}");
    let socket = host.socket();
    let sessions = ok(vibe(&socket, &["ls"]));
    assert!(sessions.contains("stays-up"), "{sessions}");
    let before = host.pid().unwrap();

    // The app's updated: this Mac now has a new build for it, and the host's away
    // for the moment.
    let arch = bundled.join(std::env::consts::ARCH);
    let mut new_build = fs::read(arch.join("vibed")).unwrap();
    new_build.extend_from_slice(b"\n# a new release\n");
    fs::remove_file(arch.join("vibed")).unwrap();
    fs::write(arch.join("vibed"), &new_build).unwrap();
    fs::set_permissions(arch.join("vibed"), fs::Permissions::from_mode(0o755)).unwrap();
    host.pull();
    wait_for("the link to go down", Duration::from_secs(30), || {
        link_is("  down")
    });
    assert_eq!(host.pid(), Some(before), "nothing's reached it yet");

    // Back: it gets the new build, and restarts on it, with nobody asked.
    host.plug_in();
    let installed = host.home.join(".local/lib/vibeos/vibed");
    wait_for("the new build there", Duration::from_secs(60), || {
        fs::read(&installed).is_ok_and(|b| b == new_build)
    });
    wait_for("its vibed to restart", Duration::from_secs(30), || {
        host.pid().is_some_and(|p| p != before)
    });
    wait_for("the link to come back", Duration::from_secs(30), || {
        link_is("  connected")
    });

    // Its agent carried on through that, and the new vibed found it.
    wait_for("the agent found again", Duration::from_secs(30), || {
        ok(vibe(&socket, &["ls"])).contains("stays-up")
    });
    let all = mcp.call("vibe_list_agents", json!({}));
    assert!(all.contains("vps:stays-up"), "{all}");
    assert!(!all.contains("exited"), "{all}");
    assert!(!ok(vibe(&mac.socket, &["host"])).contains("upgrading it failed"));
}

/// B2 of the review. An upgrade cut short once its files are in has a `vibed` still
/// on the build before, with nothing new to go up. The next try still restarts it.
#[test]
fn an_upgrade_cut_short_after_its_files_went_in_is_finished_by_the_next() {
    bounded(
        DEADLINE,
        an_upgrade_cut_short_after_its_files_went_in_is_finished_by_the_next_body,
    );
}

fn an_upgrade_cut_short_after_its_files_went_in_is_finished_by_the_next_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    let before = host.pid().unwrap();
    // Only Linux has `/proc`, which the install asks. This host is a stand-in, so
    // elsewhere it's told what Linux would say. On Linux this asks the real thing.
    if !cfg!(target_os = "linux") {
        fs::write(host.d.join("exe-deleted"), "").unwrap();
    }
    // As the upload does: the same bytes, a new file, the old one gone from under
    // the vibed that's running.
    let installed = host.home.join(".local/lib/vibeos/vibed");
    let moved = installed.with_extension("moved");
    fs::copy(&installed, &moved).unwrap();
    fs::rename(&moved, &installed).unwrap();
    let again = ok(vibe(&mac.socket, &["host", "upgrade", "vps"]));
    assert!(again.contains("just installed"), "{again}");
    assert_ne!(host.pid(), Some(before), "the old build is still running");
}

/// S6 of the review: a build that won't start, now that nobody's there to see it,
/// isn't left to cut the host off. It goes back, says so, and agents never knew.
#[test]
fn a_build_that_wont_start_goes_back_to_the_one_before_and_says_so() {
    bounded(
        DEADLINE,
        a_build_that_wont_start_goes_back_to_the_one_before_and_says_so_body,
    );
}

fn a_build_that_wont_start_goes_back_to_the_one_before_and_says_so_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let bundled = bundle(golden.dir.path());
    let mac = mac(&golden, &ssh, bundled.clone());
    ok(vibe(&mac.socket, &["host", "add", "vps", "me@vps"]));
    let installed = host.home.join(".local/lib/vibeos/vibed");
    let good = fs::read(&installed).unwrap();
    let arch = bundled.join(std::env::consts::ARCH);
    fs::remove_file(arch.join("vibed")).unwrap();
    write_exe(
        &arch.join("vibed"),
        &format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo vibed {}; exit 0; }}\nexit 3\n",
            env!("CARGO_PKG_VERSION")
        ),
    );
    let out = vibe(&mac.socket, &["host", "upgrade", "vps"]);
    assert!(!out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(said.contains("went back to the one before"), "{said}");
    assert_eq!(
        fs::read(&installed).unwrap(),
        good,
        "the good build is back"
    );
    let lib = host.home.join(".local/lib/vibeos");
    assert!(!lib.join(".vibed.prev").exists());
    // It's running on it again, and the Mac reaches it.
    let row = || {
        ok(vibe(&mac.socket, &["host"]))
            .lines()
            .find(|l| l.starts_with("vps  "))
            .unwrap_or_default()
            .to_owned()
    };
    wait_for("the link", Duration::from_secs(30), || {
        row().contains("  connected")
    });
    // That it's back isn't that it's fixed: it still says what happened.
    assert!(row().contains("upgrading it failed"), "{}", row());
    // And a later look that finds no `vibed` doesn't put the same bad build back, which
    // would cut the host off all over again, with nobody having asked. It was this,
    // straight after going back, that kept the link away.
    let stopped = Command::new(host.d.join("bin/systemctl"))
        .args(["--user", "stop"])
        .env("HOME", &host.home)
        .in_files()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    wait_for("the link to go", Duration::from_secs(30), || {
        !row().contains("  connected")
    });
    let quiet = Instant::now() + Duration::from_secs(10);
    while Instant::now() < quiet {
        assert_eq!(
            fs::read(&installed).unwrap(),
            good,
            "it put the bad build back"
        );
        assert!(
            !lib.join(".vibed.new").exists(),
            "it's putting the bad build back"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(row().contains("upgrading it failed"), "{}", row());
}

/// B1 of the review, by the real install's own look at what's there.
#[test]
fn the_install_never_goes_over_a_newer_vibed() {
    bounded(DEADLINE, the_install_never_goes_over_a_newer_vibed_body);
}

fn the_install_never_goes_over_a_newer_vibed_body(watch: &Watch) {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    watch.owns(golden.dir.path());
    let ssh = fake_ssh(watch, hosts.path());
    let host = FakeHost::new(hosts.path(), "me@vps", &golden.claude);
    let lib = host.home.join(".local/lib/vibeos");
    fs::create_dir_all(&lib).unwrap();
    let newer = "#!/bin/sh\necho vibed 99.0.0\n";
    write_exe(&lib.join("vibed"), newer);
    let mac = mac(&golden, &ssh, bundle(golden.dir.path()));
    let out = vibe(&mac.socket, &["host", "add", "vps", "me@vps"]);
    assert!(!out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(said.contains("newer than this VibeOS"), "{said}");
    assert_eq!(fs::read_to_string(lib.join("vibed")).unwrap(), newer);
}
