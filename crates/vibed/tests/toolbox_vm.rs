//! The Done-when of #10, on a Mac with real VMs: after installing `gcloud`
//! in the toolbox, new agents have it, and an agent asleep on the old
//! template still restores. Also the Linux home (a login made in the
//! toolbox reaches agents, and See logins says which logins it holds,
//! found as the save looked in the real guest), `sudo` in the toolbox and
//! nowhere else,
//! `[update]`, and **Rebuild template** with what it doesn't cover. It
//! boots the base image `scripts/base-image.sh` built, with Claude Code's
//! managed layer downloaded the way `vibed` does it; the agents here run a
//! stand-in, as the conformance suite does, so it needs no Claude login.
//!
//! ```text
//! VIBE_VM_BIN=build/vibe-vm VIBE_BASE_RELEASES=file://$PWD/build/base-image \
//!   cargo test -p vibed --test toolbox_vm -- --nocapture
//! ```
//!
//! Without those it skips. `scripts/check-full.sh` runs it with
//! `VIBE_VM_MARKER` set, so a skip can't pass for a pass. It needs
//! `cargo build --workspace` first, for `vibe-shim`, and the network, for
//! `gcloud` and Debian's packages.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Golden, workspace_bin};
use vibe_core::limits::HostLimits;
use vibe_core::model::{Environment, LoginTool, ToolboxState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{Login, NoParams};
use vibe_core::shim::{self, Frame, FrameKind, control_hello};
use vibe_env::guest::{self, proto};
use vibe_env::images::Images;
use vibe_env::linux::{LinuxVm, Real, TokenSource};
use vibe_env::toolbox::{TOOLBOX_REF, Toolbox};
use vibe_env::{AgentSpec, Compartment, Handle, Launch, Limits, Run, Sleeper, Woke, vm};
use vibed::agents::Backends;
use vibed::core::{Config, Core, DaemonInfo};

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

fn mark(test: &str) {
    if let Some(dir) = std::env::var_os("VIBE_VM_MARKER") {
        let dir = absolute(PathBuf::from(dir));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(test), "ran\n").unwrap();
    }
}

/// The PRD's recipe line for gcloud, as root, and on `PATH` for everyone.
const GCLOUD: &str = "curl -sSL https://sdk.cloud.google.com | bash -s -- --disable-prompts \
                      --install-dir=/opt && ln -sf /opt/google-cloud-sdk/bin/gcloud \
                      /usr/local/bin/gcloud";

struct World {
    golden: Golden,
    layout: Layout,
    core: Arc<Core>,
    backend: Arc<LinuxVm>,
}

impl World {
    fn toolbox_vm(&self) -> PathBuf {
        self.layout.vm_socket(TOOLBOX_REF)
    }

    fn logs(&self, agent: &str) -> String {
        ["console.log", "vibe-vm.log"]
            .iter()
            .map(|f| {
                format!(
                    "--- {agent}/{f}\n{}",
                    fs::read_to_string(self.layout.agent_dir(agent).join(f)).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Runs `command` in the open toolbox, as `vibe`, beside its shell.
    fn in_toolbox(&self, command: &str) -> String {
        let ran = guest::run(
            &self.toolbox_vm(),
            &proto::Run {
                command: vec!["/bin/bash".into(), "-lc".into(), command.into()],
                env: Default::default(),
                cwd: Some("/home/vibe".into()),
                timeout_ms: 20 * 60 * 1000,
            },
        )
        .unwrap_or_else(|e| panic!("{command}: {e}\n{}", self.logs(TOOLBOX_REF)));
        assert_eq!(
            ran.code,
            Some(0),
            "{command}:\n{}\n{}",
            ran.output,
            self.logs(TOOLBOX_REF)
        );
        ran.output
    }

    /// Opens the toolbox shell, lets `work` change the template and home,
    /// then exits the shell by typing `exit`, and waits for vibed to save.
    async fn toolbox_session(&self, work: impl FnOnce()) -> (u32, u32) {
        let before = self
            .core
            .toolbox_status(NoParams {})
            .await
            .unwrap()
            .template;
        let attach = self
            .core
            .open_toolbox(NoParams {})
            .await
            .unwrap_or_else(|e| panic!("open: {e}\n{}", self.logs(TOOLBOX_REF)));
        let status = guest::status(&self.toolbox_vm()).unwrap();
        assert!(status.problems.is_empty(), "{:?}", status.problems);
        work();
        // The shell itself works, and exiting it saves.
        let mut c = shim::connect(
            Path::new(&attach.socket),
            &control_hello("toolbox_vm"),
            Duration::from_secs(10),
        )
        .unwrap();
        let typed = "echo typed-in-the-toolbox > ~/typed; exit\r";
        shim::write_frame(&mut c.stream, &Frame::new(FrameKind::Input, typed)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let s = self.core.toolbox_status(NoParams {}).await.unwrap();
            if s.state == ToolboxState::Closed && s.template != before {
                let last = s.last.unwrap();
                assert_eq!(last.error, None, "{}", self.logs(TOOLBOX_REF));
                return (s.template.unwrap(), s.home.unwrap());
            }
            assert!(
                Instant::now() < deadline,
                "the toolbox wasn't saved: {s:?}\n{}",
                self.logs(TOOLBOX_REF)
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    fn spec(&self, r: &str) -> AgentSpec {
        let worktree = self.golden.home.join("VibeOS/worktrees/app").join(r);
        let git_dir = self.golden.home.join("src/app/.git");
        fs::create_dir_all(&worktree).unwrap();
        let agent_dir = self.layout.agent_dir(r);
        fs::create_dir_all(&agent_dir).unwrap();
        AgentSpec {
            agent_ref: r.into(),
            project: "app".into(),
            worktree,
            git_dir,
            agent_dir,
            limits: Limits::new(2 << 30, &HostLimits::default()),
        }
    }

    /// A new agent, with a stand-in for Claude running.
    fn agent(&self, r: &str) -> Handle {
        let h = self.backend.create(&self.spec(r)).unwrap();
        self.backend
            .launch(
                &h,
                &Launch {
                    args: vec!["-c".into(), "sleep 100000".into()],
                    env: vec![],
                    cwd: h.worktree.clone(),
                },
            )
            .unwrap_or_else(|e| panic!("launch {r}: {e}\n{}", self.logs(r)));
        h
    }

    /// Runs `command` in agent `h`'s VM; its output, and whether it worked.
    fn in_agent(&self, h: &Handle, command: &str) -> (bool, String) {
        let out = self
            .backend
            .run(
                h,
                &Run {
                    argv: vec!["/bin/bash".into(), "-lc".into(), command.into()],
                    env: vec![],
                    cwd: h.worktree.clone(),
                    timeout: Duration::from_secs(120),
                },
            )
            .unwrap_or_else(|e| panic!("{command}: {e}\n{}", self.logs(&h.agent_ref)));
        (
            out.success(),
            String::from_utf8_lossy(&out.output).into_owned(),
        )
    }

    fn template_of(&self, h: &Handle) -> PathBuf {
        vm::VmConfig::load(&h.agent_dir.join("vm.json"))
            .unwrap()
            .disks
            .into_iter()
            .find(|d| d.id == "template")
            .expect("the agent boots a template")
            .path
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn new_agents_get_what_the_toolbox_installed_and_sleeping_ones_keep_theirs() {
    let (Some(vibe_vm), Some(base)) = (
        std::env::var_os("VIBE_VM_BIN"),
        std::env::var("VIBE_BASE_RELEASES")
            .ok()
            .filter(|v| !v.is_empty()),
    ) else {
        eprintln!("toolbox_vm: skipped; set VIBE_VM_BIN and VIBE_BASE_RELEASES to run it on a Mac");
        return;
    };
    let vibe_vm = absolute(PathBuf::from(vibe_vm));
    let golden = Golden::new();
    let layout = Layout::new(&golden.home, "macos");
    let sources = vibed::linux::Sources {
        base_releases: base,
        ..vibed::linux::Sources::from_env()
    };
    let images = Images::new(layout.linux_dir());
    vibed::linux::refresh(&images, &layout.agents_dir(), &sources);
    images.layers().expect("the base image installs");

    let token: TokenSource = Arc::new(|| Ok("sk-ant-oat01-unused-by-the-stand-in".into()));
    let machine = Arc::new(Real { vibe_vm });
    let backend = Arc::new(
        LinuxVm::with_machine(layout.clone(), machine.clone(), token.clone())
            .with_claude("/bin/sh"),
    );
    let mut config = Config::new(DaemonInfo::current(), layout.clone());
    config.gh = golden.gh.clone().into();
    config.shim = workspace_bin("vibe-shim").into_os_string();
    config.backends = Backends::new().with(Environment::Linux, backend.clone());
    config.toolbox = Some(Arc::new(Toolbox::new(layout.clone(), machine, token)));
    let core = Arc::new(Core::open(config).unwrap());
    let supervisor = core
        .spawn_toolbox(Duration::from_millis(200), Duration::from_secs(3600))
        .unwrap();
    let w = World {
        golden,
        layout: layout.clone(),
        core,
        backend,
    };

    // 1. gcloud and a login, in the toolbox. The toolbox may sudo.
    let (t1, h1) = w
        .toolbox_session(|| {
            // Its mode and owner first, so a failure says whether the image or
            // the mounts lost setuid.
            w.in_toolbox("stat -c 'sudo is %a %U:%G' /usr/bin/sudo; sudo -n true");
            w.in_toolbox(&format!("sudo -n -H bash -c '{GCLOUD}'"));
            w.in_toolbox("git config --global user.name 'Toolbox Test'");
            // Logins as gh and ssh-keygen leave them, for See logins.
            w.in_toolbox(concat!(
                "mkdir -p ~/.config/gh ~/.ssh && ",
                "printf 'github.com:\\n    oauth_token: gho_not_a_real_token\\n",
                "    user: toolbox-test\\n' > ~/.config/gh/hosts.yml && ",
                "echo not-a-real-key > ~/.ssh/id_ed25519 && ",
                "echo 'ssh-ed25519 AAAAC3Nz toolbox-test@vibeos' > ~/.ssh/id_ed25519.pub",
            ));
        })
        .await;
    assert_eq!((t1, h1), (1, 1));
    // See logins: what the save found in the real guest, with nothing booted.
    let logins = w.core.toolbox_logins(NoParams {}).await.unwrap();
    let login = |tool, account: &str| Login {
        tool,
        account: account.into(),
    };
    assert_eq!(
        (logins.home, logins.logins, logins.claude),
        (
            Some(1),
            Some(vec![
                login(LoginTool::Gh, "toolbox-test on github.com"),
                login(LoginTool::Git, "Toolbox Test"),
                login(LoginTool::Ssh, "id_ed25519 (toolbox-test@vibeos)"),
            ]),
            true
        ),
        "{}",
        w.logs(TOOLBOX_REF)
    );
    let template_1 = layout.linux_dir().join("template-1.img");

    // 2. A new agent has them; it may not sudo.
    let old = w.agent("quiet-amber-otter");
    assert_eq!(w.template_of(&old), template_1);
    let (ok, out) = w.in_agent(&old, "gcloud --version");
    assert!(
        ok,
        "gcloud in a new agent:\n{out}\n{}",
        w.logs("quiet-amber-otter")
    );
    assert!(out.contains("Google Cloud SDK"), "{out}");
    let (ok, out) = w.in_agent(&old, "git config user.name && cat ~/typed");
    assert!(ok, "{out}");
    assert!(
        out.contains("Toolbox Test") && out.contains("typed-in-the-toolbox"),
        "{out}"
    );
    let (ok, _) = w.in_agent(&old, "sudo -n true");
    assert!(!ok, "agents never sudo");

    // 3. It goes to sleep on template 1.
    let boot_id = guest::status(&layout.vm_socket("quiet-amber-otter"))
        .unwrap()
        .boot_id;
    let (b, h) = (w.backend.clone(), old.clone());
    tokio::task::spawn_blocking(move || b.sleep(&h))
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("sleep: {e}\n{}", w.logs("quiet-amber-otter")));
    assert!(layout.snapshot("quiet-amber-otter").is_file());

    // 4. A second toolbox session: template 2, which only new agents boot.
    let (t2, h2) = w
        .toolbox_session(|| {
            w.in_toolbox("sudo -n touch /usr/local/bin/second-version");
        })
        .await;
    assert_eq!((t2, h2), (2, 2));
    let gone = images.prune_unused_after(&layout.agents_dir(), Duration::ZERO);
    assert!(
        !gone.contains(&template_1),
        "pruned the sleeping agent's template"
    );
    assert!(template_1.exists());
    let new = w.agent("brave-copper-lynx");
    let (ok, out) = w.in_agent(
        &new,
        "test -e /usr/local/bin/second-version && gcloud --version",
    );
    assert!(ok, "{out}");

    // 5. The sleeping agent restores exactly, on its own template.
    let (b, h) = (w.backend.clone(), old.clone());
    let woke = tokio::task::spawn_blocking(move || b.wake(&h))
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("wake: {e}\n{}", w.logs("quiet-amber-otter")));
    assert!(matches!(woke, Woke::Reattach(_)), "{woke:?}");
    assert_eq!(
        guest::status(&layout.vm_socket("quiet-amber-otter"))
            .unwrap()
            .boot_id,
        boot_id,
        "restored, not booted afresh"
    );
    assert_eq!(w.template_of(&old), template_1);
    let (ok, out) = w.in_agent(
        &old,
        "test ! -e /usr/local/bin/second-version && gcloud --version",
    );
    assert!(ok, "the old agent kept its template:\n{out}");

    // 6. [update] makes the next template; a rebuild replays the recipe on
    // the base and says what the recipe doesn't cover.
    fs::create_dir_all(layout.config_dir()).unwrap();
    fs::write(
        layout.toolbox_config(),
        // apt in the update's VM: its clock and network, which signatures
        // and downloads need.
        "packages = [\"tree\"]\n\n[update]\nrun = [\"apt-get update -q\", \
         \"touch /usr/local/bin/updated\"]\n",
    )
    .unwrap();
    let run = w
        .core
        .update_toolbox(NoParams {})
        .await
        .unwrap_or_else(|e| panic!("update: {e}\n{}", w.logs(TOOLBOX_REF)));
    assert_eq!(run.template, Some(3));
    let run = w
        .core
        .rebuild_toolbox(NoParams {})
        .await
        .unwrap_or_else(|e| panic!("rebuild: {e}\n{}", w.logs(TOOLBOX_REF)));
    assert_eq!(run.template, Some(4));
    eprintln!("not covered by the recipe: {:?}", run.uncovered);
    for item in [
        "/opt/google-cloud-sdk",
        "/usr/local/bin/gcloud",
        "/usr/local/bin/second-version",
        "/usr/local/bin/updated",
    ] {
        assert!(
            run.uncovered.iter().any(|u| u == item),
            "{item}: {:?}",
            run.uncovered
        );
    }
    assert!(
        !run.uncovered.iter().any(|u| u == "apt: tree"),
        "{:?}",
        run.uncovered
    );
    let rebuilt = w.agent("calm-silver-heron");
    let (ok, out) = w.in_agent(
        &rebuilt,
        "command -v tree && ! command -v gcloud && git config user.name",
    );
    assert!(
        ok,
        "the rebuilt template has the recipe, and the home carried over:\n{out}"
    );

    // Nothing is left behind.
    for h in [&old, &new, &rebuilt] {
        w.backend.delete(h).unwrap();
    }
    supervisor.abort();
    assert!(!vm::lock_held(&w.toolbox_vm()));
    for r in [
        "quiet-amber-otter",
        "brave-copper-lynx",
        "calm-silver-heron",
    ] {
        assert!(!vm::lock_held(&layout.vm_socket(r)));
    }
    mark("toolbox_vm");
}
