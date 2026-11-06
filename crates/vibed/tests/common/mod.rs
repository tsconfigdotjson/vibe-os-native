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
use vibe_env::testing::{PI_STAND_IN, STAND_IN, TestBackend, TestToolbox};
use vibe_services::secrets::MemoryStore;
use vibe_services::testing::FakeService;
use vibe_services::{Secret, SecretStore};
use vibed::agents::Backends;
use vibed::core::{Config, Core, DaemonInfo};
use vibed::remote::{BoxFuture, Channel, Installed, Remote};
use vibed::server::{self, SocketGuard};
use vibed::services::ServicesConfig;

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

/// The golden Mac's memory: 64G, 58G of it in use, and 3G of 4G of swap,
/// with macOS warning of memory pressure. Nothing is refused for it (#47).
pub fn golden_memory() -> vibe_core::rpc::MemoryUse {
    vibe_core::rpc::MemoryUse {
        total: 64 * GIB,
        used: 58 * GIB,
        swap_total: 4 * GIB,
        swap_used: 3 * GIB,
        pressure: None,
    }
}

/// macOS's memory pressure at warn, as the golden Mac has it: worth a
/// warning, and short of what makes vibed act.
#[derive(Debug)]
pub struct Warned;

impl vibed::pressure::MemoryPressure for Warned {
    fn read(&self) -> Option<vibed::pressure::Reading> {
        Some(vibed::pressure::Level::Warn.into())
    }
}

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

/// The examples' name for the stand-in for a service, which is really on a port
/// of its own.
pub const GOLDEN_SERVICE: &str = "http://services.example";

/// Puts the stand-in's real address in place of [`GOLDEN_SERVICE`], or the other
/// way round, whether it's plain or as it is in a query.
pub fn swap_service(text: &str, from: &str, to: &str) -> String {
    let encoded = |u: &str| u.replace(':', "%3A").replace('/', "%2F");
    text.replace(&encoded(from), &encoded(to)).replace(from, to)
}

/// The examples, with [`GOLDEN_HOME`] replaced by the world's home.
pub fn examples(golden: &Golden) -> Vec<Example> {
    let home = golden.home.as_path();
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
            let mut text = fs::read_to_string(&p).unwrap().replace(GOLDEN_HOME, home);
            if let Some(fake) = &golden.service {
                text = swap_service(&text, GOLDEN_SERVICE, &fake.base);
            }
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

/// [`Command::output`], with what it says in files rather than pipes. A Mac makes a
/// pipe in two steps, so a program another thread starts between them inherits its
/// write end: `output` then waits for as long as that one lives, which for a shim or
/// a tunnel is the rest of the test. A test starting anything long-lived alongside
/// other calls needs this. It only waits for the program itself.
pub fn output_in_files(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    use std::io::{Read, Seek, SeekFrom};
    let (mut out, mut err) = (tempfile::tempfile()?, tempfile::tempfile()?);
    let status = cmd
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(err.try_clone()?)
        .status()?;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    out.seek(SeekFrom::Start(0))?;
    out.read_to_end(&mut stdout)?;
    err.seek(SeekFrom::Start(0))?;
    err.read_to_end(&mut stderr)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Who a test's commits are by: the machine running it may have nobody set up,
/// as a CI runner and a fresh host don't.
const GIT_IDENTITY: [&str; 4] = ["-c", "user.name=t", "-c", "user.email=t@example.com"];

pub fn git(args: &[&str]) {
    let out = output_in_files(Command::new("git").args(GIT_IDENTITY).args(args)).unwrap();
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
    let out = output_in_files(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(GIT_IDENTITY)
            .args(args),
    )
    .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Installs the stand-in for Claude Code at `bin/claude`.
pub fn stand_in(bin: &Path) -> PathBuf {
    fs::create_dir_all(bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, STAND_IN);
    write_script(&bin.join("pi"), PI_STAND_IN);
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
"auth login")
  # Waiting for a person, for as long as an example takes.
  exec sleep 30
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

/// The doctor's answers on the golden host.
pub fn golden_checks() -> vibed::doctor::FixedChecks {
    vibed::doctor::FixedChecks {
        git: Ok("git version 2.50.1".into()),
        git_uses_gh: std::sync::Mutex::new(Ok(false)),
        claude: Ok((
            "/usr/local/bin/claude".into(),
            "2.1.314 (Claude Code)".into(),
        )),
        claude_login: Ok("Claude Code is signed in, with its login in your Keychain".into()),
        pi: Ok(("/usr/local/bin/pi".into(), "1.5.0".into())),
        system_sleep: Ok(Some(10)),
        linger: std::sync::Mutex::new(Ok(None)),
        enable_linger: Ok(()),
        installs_tools: false,
    }
}

/// Publishes a build of Pi as its releases do, under `root`, as the latest: a
/// `file://` directory `vibe_env::pi_release` reads. Its `pi` is a script.
pub fn publish_pi(root: &Path, version: &str, platform: &str) {
    let release = root.join(format!("download/v{version}"));
    let staging = root.join(format!("staging-{version}"));
    fs::create_dir_all(staging.join("pi/theme")).unwrap();
    fs::create_dir_all(&release).unwrap();
    let binary = staging.join("pi/pi");
    fs::write(&binary, format!("#!/bin/sh\necho {version}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(staging.join("pi/theme/dark.json"), "{}").unwrap();
    let name = format!("pi-{platform}.tar.gz");
    let tarball = release.join(&name);
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .arg("pi")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let sum = vibe_env::images::sha256_file(&tarball).unwrap();
    fs::write(release.join("SHA256SUMS"), format!("{sum}  {name}\n")).unwrap();
    // And the `fd` that goes in a layer.
    let target = format!(
        "fd-v{}-aarch64-unknown-linux-gnu",
        vibe_env::pi_release::FD_VERSION
    );
    let staging = root.join("staging-fd");
    fs::create_dir_all(staging.join(&target)).unwrap();
    fs::write(staging.join(&target).join("fd"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        staging.join(&target).join("fd"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::create_dir_all(root.join("fd")).unwrap();
    let tarball = root.join("fd").join(format!("{target}.tar.gz"));
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let sum = vibe_env::images::sha256_file(&tarball).unwrap();
    fs::write(root.join("fd").join(format!("{target}.tar.gz.sha256")), sum).unwrap();
}

pub struct Golden {
    pub dir: tempfile::TempDir,
    /// Canonical, so paths vibed canonicalizes match it (macOS's temp dir
    /// is behind a symlink).
    pub home: PathBuf,
    pub gh: PathBuf,
    pub claude: PathBuf,
    sessions: Vec<RunningShim>,
    /// A stand-in for Linear, for the examples. No other test gets a broker.
    pub service: Option<FakeService>,
    logins: Arc<MemoryStore>,
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
            service: None,
            logins: Arc::default(),
        }
    }

    /// Everything else the examples need: the sessions they list,
    /// `quiet-amber-otter` running and two that exited, `brave-copper-lynx`
    /// with code 0 and `calm-silver-heron` killed; and a `.vibe/setup.sh`
    /// in `app` that copies `.env.example` to `.env.local`, so a new
    /// agent's worktree has a change it hasn't committed.
    pub fn for_examples(mut self) -> Golden {
        self.seed_services();
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
        // Pi: the owner's own login, the store for linux agents, and a release to
        // install the layer they boot from. Never a provider's real login.
        let own = self.home.join(".pi/agent");
        fs::create_dir_all(&own).unwrap();
        fs::write(
            own.join("auth.json"),
            r#"{"openai":{"type":"api_key","key":"sk-fake"}}"#,
        )
        .unwrap();
        let vms = layout.linux_pi_logins_dir();
        fs::create_dir_all(&vms).unwrap();
        fs::write(
            vms.join("auth.json"),
            r#"{"anthropic":{"type":"oauth","access":"a","refresh":"r","expires":1},
                "xai":{"type":"api_key","key":"fake"}}"#,
        )
        .unwrap();
        // What the owner is told of what an agent did with a login, not a week old,
        // and that it did.
        fs::write(
            vms.join(vibe_env::pi_logins::OFFERED_FILE_NAME),
            r#"{"xai":"brave-copper-lynx"}"#,
        )
        .unwrap();
        fs::write(
            vms.join(vibe_env::pi_logins::NOTICES_FILE),
            serde_json::json!([{
                "provider": "xai",
                "text": "brave-copper-lynx signed in to xai, and every Pi agent here now uses \
                         that login. If that wasn't you, sign in to it in Settings → Agents to \
                         replace it.",
                "at_ms": vibe_env::pi_logins::now_ms(),
                "first": true,
            }])
            .to_string(),
        )
        .unwrap();
        publish_pi(&self.home.join("pi-releases"), "1.5.0", "linux-arm64");
        // And for the doctor's linux check: Claude Code's managed layer, and
        // base release 2 being downloaded, 400 bytes of 1000 in so far.
        fs::write(layout.linux_dir().join("managed-2.1.314.img"), "").unwrap();
        let downloads = layout.linux_dir().join("downloads");
        fs::create_dir_all(&downloads).unwrap();
        fs::write(
            downloads.join("base.json"),
            r#"{"version": 2, "files": {
                "Image": {"sha256": "00", "size": 100},
                "initrd": {"sha256": "00", "size": 100},
                "base.img.gz": {"sha256": "00", "size": 800}}}"#,
        )
        .unwrap();
        fs::write(downloads.join("Image"), [0u8; 100]).unwrap();
        fs::write(downloads.join("base.img.gz"), [0u8; 300]).unwrap();
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
        config.services = self.service.as_ref().map(|fake| ServicesConfig {
            services: vec![fake.service()],
            store: self.logins.clone(),
            place: vibe_services::Place::Mac,
            token_wait: std::time::Duration::from_millis(100),
        });
        config.label = "mac-mini".into();
        config.gh = self.gh.clone().into();
        config.shim = workspace_bin("vibe-shim").into_os_string();
        config.pi_driver = Some(self.claude.with_file_name("pi"));
        config.backends = Backends::all(Arc::new(Seeding {
            backend: TestBackend::new(&self.claude)
                .with_pi_copies(
                    self.claude.with_file_name("pi"),
                    &Layout::new(&self.home, "macos"),
                )
                .reporting_memory(AGENT_MEMORY_USED),
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
        // Whatever this machine's memory and disk are doing, and without
        // holding it awake.
        config.memory = Arc::new(|| Some(golden_memory()));
        config.pressure = Arc::new(Warned);
        config.free_disk = Arc::new(|_: &Path| Some(1 << 40));
        config.power = Arc::new(vibed::power::NoPower);
        config.clock = Arc::new(|| GOLDEN_NOW);
        config.remote = Arc::new(GoldenRemote);
        // One Tailscale address, whatever this machine has.
        config.tailscale = Arc::new(|| vec![GOLDEN_TAILSCALE.parse().unwrap()]);
        // What vibe doctor finds: everything there but git using gh, and a
        // Mac that sleeps after 10 minutes; and the Linux images, with base
        // release 2 on its way (for_examples has it 40% downloaded).
        config.checks = Arc::new(golden_checks());
        config.pi_releases = format!("file://{}", self.home.join("pi-releases").display());
        config.linux_images = Some(vibed::linux::Watch::new(
            &Layout::new(&self.home, "macos"),
            vibed::linux::Sources {
                base_releases: "file:///nowhere".into(),
                base_release: 2,
                claude_releases: "file:///nowhere".into(),
                pi_releases: "file:///nowhere".into(),
            },
        ));
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

impl Golden {
    /// A stand-in for Linear, with no tools so that nothing it does in the
    /// background adds to the examples' events, and a login to it that needs
    /// reconnecting because its client was forgotten.
    fn seed_services(&mut self) {
        let fake = FakeService::start_detached();
        fake.set_tools(Vec::new());
        fake.accept_code("golden-code");
        let secret = Secret {
            access: "FAKESECRET-golden-access".into(),
            refresh: Some("FAKESECRET-golden-refresh".into()),
            obtained_at: None,
            expires_at: None,
            client_id: Some("forgotten-client".into()),
            token_endpoint: Some(format!("{}/token", fake.base)),
            revocation_endpoint: None,
            scopes: vec!["read".into(), "write".into()],
        };
        self.logins.store("linear", &secret).unwrap();
        let dir = Layout::new(&self.home, "macos").data_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("services.json"),
            serde_json::json!({"linear": {
                "state": "needs_reconnect",
                "role": "owner",
                "detail": "Linear refused the refresh (invalid_client)",
                "detail_at": GOLDEN_NOW,
                "client_stale": true,
            }})
            .to_string(),
        )
        .unwrap();
        fs::write(
            dir.join("services-audit.log"),
            format!(
                "{}\n",
                serde_json::json!({
                    "at": "2026-10-30T11:59:00Z",
                    "agent": "dark-mode-review",
                    "service": "linear",
                    "tool": "list_issues",
                    "outcome": "ok",
                    "ms": 42,
                })
            ),
        )
        .unwrap();
        self.service = Some(fake);
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
            let path = h
                .claude()
                .unwrap()
                .transcript(&spec.worktree, SEEDED_SESSION);
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
            // A server with no polkit and no passwordless sudo.
            let refuses = address.starts_with("refuses@");
            Ok(Installed {
                os: "linux".into(),
                arch: "x86_64".into(),
                label: "vps-1".into(),
                version: "0.1.0".into(),
                missing: vec!["claude".into()],
                needs_admin: if refuses {
                    vec![vibe_core::model::AdminNeed::Linger]
                } else {
                    Vec::new()
                },
                changed: true,
                notes: vec![if refuses {
                    format!(
                        "VibeOS couldn't be set to keep running on {address}, so it stops when you log out. Settings → Hosts has a button that does it, asking for the \
                         password there once."
                    )
                } else {
                    format!(
                        "VibeOS keeps running on {address} when you're logged out, and starts again after a reboot."
                    )
                }],
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
        cols: None,
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

pub const HELLO: &str = r#"{"jsonrpc":"2.0","id":"probe","method":"daemon.hello","params":{"client":"test","protocol":2}}"#;

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
