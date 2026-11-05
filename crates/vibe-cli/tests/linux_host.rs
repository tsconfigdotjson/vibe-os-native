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
    for b in ["vibed", "vibe", "vibe-shim"] {
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
    for b in ["vibed", "vibe", "vibe-shim"] {
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
        let started = mcp.call(
            "vibe_start_agent",
            json!({"project": project, "profile": "software-eng-linux", "prompt": "Say hi",
                   "host": "testhost"}),
        );
        eprintln!("{started}");
        assert!(started.contains("testhost:"), "{started}");
        let all = mcp.call("vibe_list_agents", json!({"host": "testhost"}));
        assert!(
            all.contains(&format!("ssh -t {dest} .local/lib/vibeos/vibe attach")),
            "{all}"
        );
    }
    // A hung connection: stop the link's ssh, as a pulled cable would.
    let pids = String::from_utf8(
        Command::new("pgrep")
            .args(["-f", &format!("ssh .*{dest} .local/lib/vibeos/vibe rpc")])
            .in_files()
            .unwrap()
            .stdout,
    )
    .unwrap();
    for pid in pids.split_whitespace() {
        let _ = Command::new("kill").args(["-STOP", pid]).status();
    }
    wait_for("the link to go down", Duration::from_secs(60), || {
        link_is("  down")
    });
    for pid in pids.split_whitespace() {
        let _ = Command::new("kill").args(["-KILL", pid]).status();
    }
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
}

impl Drop for Relays {
    fn drop(&mut self) {
        self.signal("-KILL");
    }
}

/// One round of every tool a host's own agents need, as a caller makes them, or the first
/// that took 5 s or more. `name` is this round's agent.
fn one_round_of_local_tools(c: &mut Outside, name: &str) -> Result<(), String> {
    let mut timed = |tool: &str, args: Value| {
        let begun = Instant::now();
        let said = c.call(tool, args);
        let took = begun.elapsed();
        if took < Duration::from_secs(5) {
            Ok(said)
        } else {
            Err(format!("{tool} took {took:?}: {said}"))
        }
    };
    let listed = timed("vibe_list_agents", json!({}))?;
    assert!(listed.contains("on-vps"), "{listed}");
    let projects = timed("vibe_list_projects", json!({}))?;
    assert!(projects.contains("host-only (on local)"), "{projects}");
    // A project only this host has, with no host named.
    let started = timed(
        "vibe_start_agent",
        json!({"project": "host-only", "profile": "small", "prompt": "talk", "name": name}),
    )?;
    assert!(started.contains(&format!("started {name}")), "{started}");
    // A bare ref on this host, through all of it.
    let sent = timed("vibe_send", json!({"agent": name, "text": "hello"}))?;
    assert!(sent.contains("sent to"), "{sent}");
    timed("vibe_wait", json!({"agents": name, "timeout": 4}))?;
    timed("vibe_read_output", json!({"agent": name}))?;
    timed("vibe_read_messages", json!({"agent": name, "last": 1}))?;
    let deleted = timed("vibe_delete_agent", json!({"agent": name, "force": true}))?;
    assert!(deleted.contains("deleted"), "{deleted}");
    Ok(())
}

/// Every local tool under 5 s, whatever's become of the Mac. A busy machine can stall
/// any of them, with a healthy Mac too, so a slow round gets another go. Anything waiting
/// on the Mac would be slow every time, and so fail.
fn every_local_tool_answers_promptly(c: &mut Outside, when: &str) {
    let mut last = String::new();
    for round in 0..3 {
        let name = format!("while-{when}-{round}");
        match one_round_of_local_tools(c, &name) {
            Ok(()) => return,
            Err(why) => {
                eprintln!("{when}, round {round}: {why}");
                last = why;
                // What it started may be left: it's only in the way of nothing.
                // Or it was never started, if a listing was what was slow.
                let _ = c.tool("vibe_delete_agent", json!({"agent": name, "force": true}));
            }
        }
    }
    panic!("{when}: every round had something slow, the last: {last}");
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
    let listed = c.call("vibe_list_agents", json!({}));
    let mac_row = listed
        .lines()
        .find(|l| l.starts_with("mac:on-mac"))
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(mac_row.contains("unavailable"), "{listed}");
    every_local_tool_answers_promptly(&mut c, "asleep");

    // It wakes, and it all merges back with nothing done.
    watch.at("the Mac waking");
    relays.signal("-CONT");
    wait_for("the Mac back", Duration::from_secs(60), || merged(&mut c));
    let read = c.call(
        "vibe_read_messages",
        json!({"agent": "mac:on-mac", "last": 1}),
    );
    assert!(!read.contains("unavailable"), "{read}");

    // The Mac stops altogether: the same.
    watch.at("the Mac stopping");
    drop(mac.rt);
    watch.at("the Mac stopped");
    let listed = c.call("vibe_list_agents", json!({}));
    let mac_row = listed
        .lines()
        .find(|l| l.starts_with("mac:on-mac"))
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(mac_row.contains("unavailable"), "{listed}");
    every_local_tool_answers_promptly(&mut c, "stopped");
}

/// External access through a real server (`VIBE_TEST_LINUX_HOST`, with the
/// Linux builds in `VIBE_HOST_BUNDLE`) at a public name pointing at it
/// (`VIBE_TEST_EXTERNAL_DOMAIN`), with a real certificate from Let's
/// Encrypt: a client on the internet with the URL and a key starts and
/// talks to an agent on this Mac, and one on the server when
/// `VIBE_TEST_LINUX_PROJECT` names a project there; it gets nothing
/// without a key; and nothing listens publicly on the Mac.
/// scripts/check-full.sh runs it on the Mac.
#[test]
#[ignore = "needs a real Linux server with a public name: VIBE_TEST_LINUX_HOST, VIBE_HOST_BUNDLE and VIBE_TEST_EXTERNAL_DOMAIN"]
fn real_external_access() {
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
    let pgrep = Command::new("pgrep")
        .args(["-f", "ssh .*-R .*mac\\.sock"])
        .in_files()
        .unwrap();
    let tunnel = String::from_utf8_lossy(&pgrep.stdout).trim().to_owned();
    assert!(!tunnel.is_empty(), "the Mac's ssh -R isn't running");
    let signal = |which: &str| {
        for pid in tunnel.lines() {
            assert!(
                Command::new("kill")
                    .args([which, pid])
                    .status()
                    .unwrap()
                    .success()
            );
        }
    };
    signal("-STOP");
    let begun = Instant::now();
    let listed = c.call("vibe_list_agents", json!({}));
    let took = begun.elapsed();
    signal("-CONT");
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert!(listed.contains("mac:external-mac"), "{listed}");
    assert!(listed.contains("unavailable"), "{listed}");
    wait_for("the Mac back", Duration::from_secs(60), || {
        !c.call("vibe_list_agents", json!({}))
            .contains("unavailable")
    });
    if let Ok(project) = std::env::var("VIBE_TEST_LINUX_PROJECT") {
        let started = c.call(
            "vibe_start_agent",
            json!({"project": project, "profile": "software-eng-linux", "prompt": "Say hi",
                   "host": "local", "name": "external-vps"}),
        );
        assert!(started.contains("started external-vps"), "{started}");
        let waited = c.call(
            "vibe_wait",
            json!({"agents": "external-vps", "timeout": 240}),
        );
        eprintln!("{waited}");
        let read = c.call(
            "vibe_read_messages",
            json!({"agent": "external-vps", "last": 2}),
        );
        eprintln!("{read}");
        c.call(
            "vibe_delete_agent",
            json!({"agent": "external-vps", "force": true}),
        );
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
    ok(vibe(&mac.socket, &["host", "external", "testhost", "off"]));
    ok(vibe(&mac.socket, &["host", "remove", "testhost"]));
    drop(mac.rt);
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
