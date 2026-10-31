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
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_core::paths::Layout;
use vibe_core::rpc::{KeyCreateParams, MAX_MESSAGE_BYTES};
use vibed::core::Core;
use vibed::link::Timing;
use vibed::remote::Ssh;
use vibed::server;

const VIBE: &str = env!("CARGO_BIN_EXE_vibe");

fn write_exe(path: &Path, body: &str) {
    common::write_script(path, body);
}

/// `ssh` for the test: options are taken and ignored, and the command runs
/// here, with `$HOME` the host's own home, `<hosts>/<destination>`, and
/// the host's tools first on `PATH`. While `<hosts>/<destination>.d/down`
/// exists, it can't connect, and what it carries is dropped.
fn fake_ssh(hosts: &Path) -> PathBuf {
    let ssh = hosts.join("ssh");
    write_exe(
        &ssh,
        &format!(
            r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift 2 ;;
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
    echo "Connection to $dest closed by remote host." >&2
    exit 255
  fi
  sleep 0.1
done
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
  enable-linger) touch "$HOME.d/linger" ;;
  *) exit 1 ;;
esac
"#,
        );
        write_exe(
            &bin.join("journalctl"),
            "#!/bin/sh\ntail -n 1 \"$HOME.d/vibed.log\"\n",
        );
        if Command::new("sh")
            .args(["-c", "command -v sha256sum"])
            .output()
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
                "export VIBE_TEST_BACKEND='{}' VIBE_GH='{}'\n",
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
            format!("mcp_port = {}\nreserve_memory = \"0\"\n", mcp.port()),
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut config = golden.config();
    config.remote = Arc::new(Ssh {
        program: ssh.into(),
        bundle: Some(bundle),
    });
    config.link_timing = Timing {
        ping_every: Duration::from_millis(500),
        ping_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(20),
        min_backoff: Duration::from_millis(200),
        max_backoff: Duration::from_secs(1),
    };
    let core = Arc::new(Core::open(config).unwrap());
    {
        let _in = rt.enter();
        core.spawn_links();
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
    Command::new(VIBE)
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .unwrap()
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
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    let ssh = fake_ssh(hosts.path());
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

#[test]
fn an_unreachable_server_is_refused_with_why() {
    let golden = common::Golden::new();
    let hosts = common::short_tempdir();
    let ssh = fake_ssh(hosts.path());
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
            .output()
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
