//! The toolbox's policy, on the fake `vibe-vm` and guest the `linux`
//! backend's tests use. The real thing runs on a Mac:
//! `crates/vibed/tests/linux_vm.rs`.

use super::*;
use crate::linux::LinuxVm;
use crate::linux::fake::Fake;
use crate::{AgentSpec, Compartment, Handle, Launch, Limits, Sleeper, Woke};
use std::os::unix::fs::{FileExt, PermissionsExt};

const TOKEN: &str = "sk-ant-oat01-test-token-for-the-toolbox-0123456789";

struct Setup {
    tmp: tempfile::TempDir,
    layout: Layout,
    fake: Arc<Fake>,
    toolbox: Toolbox,
}

fn add_base(layout: &Layout, n: u32) {
    let base = layout.linux_dir().join(format!("base-{n}"));
    fs::create_dir_all(&base).unwrap();
    for f in ["Image", "initrd", "base.img"] {
        fs::write(base.join(f), format!("{f} {n}")).unwrap();
    }
}

fn token() -> TokenSource {
    Arc::new(|| Ok(TOKEN.to_owned()))
}

impl Setup {
    fn new() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path(), "macos");
        add_base(&layout, 1);
        let bin = tmp.path().join("claude-bin");
        fs::write(&bin, "claude").unwrap();
        Images::new(layout.linux_dir())
            .add_managed("2.1.308", &bin, "x")
            .unwrap();
        let fake = Arc::new(Fake::default());
        let toolbox = Toolbox::new(layout.clone(), fake.clone(), token());
        Setup {
            tmp,
            layout,
            fake,
            toolbox,
        }
    }

    fn socket(&self) -> PathBuf {
        self.layout.vm_socket(TOOLBOX_REF)
    }

    fn config(&self) -> VmConfig {
        VmConfig::load(&self.layout.agent_dir(TOOLBOX_REF).join("vm.json")).unwrap()
    }

    fn working(&self, name: &str) -> PathBuf {
        self.layout.agent_dir(TOOLBOX_REF).join(name)
    }

    fn version(&self, name: &str) -> PathBuf {
        self.layout.linux_dir().join(name)
    }

    /// What the owner does in the shell: writes `what` into the disk
    /// that takes it, at `at`, as an install or a login would.
    fn write(&self, disk: &str, at: u64, what: &[u8]) {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(self.working(disk))
            .unwrap();
        f.write_all_at(what, at).unwrap();
    }

    /// One shell session: open, `work`, exit, close.
    fn session(&self, work: impl FnOnce()) -> Saved {
        self.toolbox.open().unwrap();
        work();
        self.fake.exit(&self.socket(), 0);
        assert!(!self.toolbox.shell_running());
        self.toolbox.close().unwrap()
    }

    fn calls(&self) -> Vec<String> {
        self.fake.calls()
    }
}

fn read_at(path: &Path, at: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    fs::File::open(path)
        .unwrap()
        .read_exact_at(&mut buf, at)
        .unwrap();
    buf
}

/// Enough of an image to tell it hasn't changed: its size, when it was
/// last written, and its first blocks, where the tests write. (Reading a
/// whole sparse image would read gigabytes of zeros.)
fn fingerprint(path: &Path) -> (u64, std::time::SystemTime, Vec<u8>) {
    let meta = fs::metadata(path).unwrap();
    (
        meta.len(),
        meta.modified().unwrap(),
        read_at(path, 0, 16384),
    )
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_shell_boots_the_template_and_home_writable() {
    let s = Setup::new();
    assert_eq!(
        s.toolbox.versions(),
        Versions {
            template: None,
            home: None,
            base: Some(1),
            newest_base: Some(1),
        }
    );
    assert!(!s.toolbox.is_open());
    let end = s.toolbox.open().unwrap();
    assert_eq!(end, TerminalEnd::Vm { socket: s.socket() });
    assert!(s.toolbox.is_open() && s.toolbox.shell_running());
    let c = s.config();
    assert_eq!(c.agent, "toolbox");
    assert_eq!(c.memory, 4 << 30);
    assert_eq!(c.kernel, s.version("base-1/Image"));
    let disks: Vec<_> = c
        .disks
        .iter()
        .map(|d| (d.id.as_str(), d.path.clone(), d.read_only))
        .collect();
    assert_eq!(
        disks,
        [
            ("base", s.version("base-1/base.img"), true),
            ("managed", s.version("managed-2.1.308.img"), true),
            ("template", s.working("template.img"), false),
            ("home", s.working("home.img"), false),
        ]
    );
    // The first ones are blank, for the guest to format.
    let host = HostLimits::default();
    for disk in ["template.img", "home.img"] {
        assert_eq!(fs::metadata(s.working(disk)).unwrap().len(), host.vm_disk);
    }
    let (boot, problems) = boot::Boot::parse(&c.cmdline);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(boot.layers, ["managed", "base"]);
    assert_eq!(boot.upper.as_deref(), Some("template"));
    assert_eq!(boot.home.as_deref(), Some("home"));
    assert!(boot.sudo, "the toolbox shell may use sudo");
    // The Linux Claude config is edited there too.
    assert_eq!(c.shares.len(), 1);
    assert_eq!(c.shares[0].path, s.layout.linux_claude_dir());
    assert_eq!(boot.shares[0].path, "/home/vibe/.claude");
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(s.layout.linux_claude_dir().join(".claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["hasCompletedOnboarding"], true);
    assert_eq!(state["autoUpdates"], false);
    let launch = s.fake.state.lock().unwrap().launches[0].clone();
    assert_eq!(launch.command, ["/bin/bash", "-l"]);
    assert_eq!(launch.cwd.as_deref(), Some("/home/vibe"));
    assert_eq!(launch.env["CLAUDE_CONFIG_DIR"], "/home/vibe/.claude");
    assert_eq!(launch.env[TOKEN_ENV], TOKEN);
    assert_eq!(launch.env["DISABLE_AUTOUPDATER"], "1");

    // One toolbox at a time: opening again is the same shell.
    assert_eq!(s.toolbox.open().unwrap(), end);
    assert_eq!(s.calls().iter().filter(|c| *c == "boot").count(), 1);
    // Nor do an update or a rebuild run beside it.
    let e = s.toolbox.update(&["true".into()]).unwrap_err();
    assert!(e.message().contains("shell is open"), "{e}");
    let e = s.toolbox.rebuild(&Recipe::default()).unwrap_err();
    assert!(e.message().contains("shell is open"), "{e}");
    // Closing while the shell runs ends it; what it did is kept.
    assert_eq!(
        s.toolbox.close().unwrap(),
        Saved {
            template: Some(1),
            home: Some(1),
            uncovered: vec![],
        }
    );
}

#[test]
fn exiting_keeps_a_new_version_of_each_and_never_changes_an_old_one() {
    let s = Setup::new();
    let saved = s.session(|| {
        s.write("template.img", 4096, b"gcloud");
        s.write("home.img", 4096, b"gh login");
    });
    assert_eq!((saved.template, saved.home), (Some(1), Some(1)));
    let calls = s.calls();
    let seal = calls.iter().position(|c| c == "seal").unwrap();
    let off = calls.iter().position(|c| c == "poweroff").unwrap();
    assert!(seal < off, "sealed before the power-off: {calls:?}");
    let (t1, h1) = (s.version("template-1.img"), s.version("home-1.img"));
    assert_eq!(read_at(&t1, 4096, 6), b"gcloud");
    assert_eq!(read_at(&h1, 4096, 8), b"gh login");
    assert_eq!((mode(&t1), mode(&h1)), (0o444, 0o444));
    assert_eq!(
        fs::read_to_string(s.version("template-1.base")).unwrap(),
        "1\n"
    );
    // The working disks became the versions; nothing else is left.
    for gone in ["template.img", "home.img", "vm.json", "open.json"] {
        assert!(!s.working(gone).exists(), "{gone}");
    }
    assert!(!s.toolbox.is_open());
    assert_eq!(
        s.toolbox.versions(),
        Versions {
            template: Some(1),
            home: Some(1),
            base: Some(1),
            newest_base: Some(1),
        }
    );

    // The next shell starts from clones of the last versions, which stay
    // exactly as they were.
    let before = fingerprint(&t1);
    let saved = s.session(|| {
        assert_eq!(read_at(&s.working("template.img"), 4096, 6), b"gcloud");
        assert_eq!(mode(&s.working("template.img")), 0o600);
        s.write("template.img", 8192, b"kubectl");
    });
    assert_eq!((saved.template, saved.home), (Some(2), Some(2)));
    let t2 = s.version("template-2.img");
    assert_eq!(read_at(&t2, 4096, 6), b"gcloud");
    assert_eq!(read_at(&t2, 8192, 7), b"kubectl");
    assert_eq!(
        fingerprint(&t1),
        before,
        "an image is never changed in place"
    );
    assert_eq!(read_at(&s.version("home-2.img"), 4096, 8), b"gh login");
}

#[test]
fn a_shell_that_cant_be_saved_is_kept_unsaved_and_saved_next_time() {
    let s = Setup::new();
    // The VM stops on its own before the shell is done, as in a reboot.
    s.toolbox.open().unwrap();
    s.write("template.img", 4096, b"gcloud");
    s.write("home.img", 4096, b"gh login");
    s.fake.kill(&s.socket());
    assert!(!s.toolbox.shell_running());
    let e = s.toolbox.close().unwrap_err();
    assert!(e.message().contains("kept unsaved"), "{e}");
    assert!(
        e.message().contains("`vibe toolbox` opens them again"),
        "{e}"
    );
    assert_eq!(s.toolbox.versions().template, None);
    assert!(!s.toolbox.is_open());
    assert!(s.working("template.img").exists() && s.working("home.img").exists());
    // Nothing else runs on the toolbox until they're saved...
    for e in [
        s.toolbox.update(&["true".into()]).unwrap_err(),
        s.toolbox.rebuild(&Recipe::default()).unwrap_err(),
    ] {
        assert!(e.message().contains("unsaved changes"), "{e}");
    }
    // ...and a restarted vibed's tidying keeps them.
    s.toolbox.discard().unwrap();
    assert!(s.working("template.img").exists());

    // A disk the guest can't leave clean is kept unsaved too.
    s.toolbox.open().unwrap();
    assert_eq!(read_at(&s.working("template.img"), 4096, 6), b"gcloud");
    s.fake.state.lock().unwrap().seal_fails = true;
    s.fake.exit(&s.socket(), 0);
    let e = s.toolbox.close().unwrap_err();
    assert!(e.message().contains("kept unsaved"), "{e}");
    assert!(!s.fake.vm(&s.socket()).running, "the VM is stopped");
    assert_eq!(s.toolbox.versions().template, None);
    assert!(
        s.toolbox
            .close()
            .unwrap_err()
            .message()
            .contains("no toolbox shell")
    );

    // The next open boots the same disks, not fresh clones, and exiting
    // saves them.
    let saved = s.session(|| {
        assert_eq!(read_at(&s.working("template.img"), 4096, 6), b"gcloud");
        assert_eq!(read_at(&s.working("home.img"), 4096, 8), b"gh login");
        s.write("template.img", 8192, b"more");
    });
    assert_eq!((saved.template, saved.home), (Some(1), Some(1)));
    let t1 = s.version("template-1.img");
    assert_eq!(read_at(&t1, 4096, 6), b"gcloud");
    assert_eq!(read_at(&t1, 8192, 4), b"more");
    assert_eq!(read_at(&s.version("home-1.img"), 4096, 8), b"gh login");
    for gone in [
        "template.img",
        "home.img",
        "vm.json",
        "open.json",
        "unsaved.json",
    ] {
        assert!(!s.working(gone).exists(), "{gone}");
    }
    // Then the next shell is a fresh clone again.
    s.toolbox.open().unwrap();
    assert_eq!(read_at(&s.working("template.img"), 8192, 4), b"more");
    s.toolbox.discard().unwrap();
    assert!(!s.fake.vm(&s.socket()).running);
    assert!(
        s.working("template.img").exists(),
        "an open shell's disks are kept"
    );
}

#[test]
fn a_base_whose_guest_predates_the_toolbox_is_refused() {
    let s = Setup::new();
    s.fake.state.lock().unwrap().problems =
        vec!["the kernel command line: `vibe.home=home`: unknown setting `vibe.home`".into()];
    let e = s.toolbox.open().unwrap_err();
    assert!(e.message().contains("too old for the toolbox"), "{e}");
    assert!(e.message().contains("vibe toolbox rebuild"), "{e}");
    assert!(!s.fake.vm(&s.socket()).running);
    assert!(!s.toolbox.is_open() && !s.working("template.img").exists());
}

/// The PRD's "Done when": after installing `gcloud` in the toolbox, new
/// agents have it; an agent asleep on the old version still restores.
#[test]
fn new_agents_boot_the_new_versions_and_a_sleeping_agent_keeps_its_own() {
    let s = Setup::new();
    s.session(|| s.write("template.img", 4096, b"gcloud 1"));
    let linux = LinuxVm::with_machine(s.layout.clone(), s.fake.clone(), token());
    let spec = |r: &str| {
        let worktree = s.tmp.path().join("VibeOS/worktrees/app").join(r);
        let git_dir = s.tmp.path().join("VibeOS/projects/app/.git");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        let agent_dir = s.layout.agent_dir(r);
        fs::create_dir_all(&agent_dir).unwrap();
        AgentSpec {
            agent_ref: r.into(),
            project: "app".into(),
            worktree,
            git_dir,
            agent_dir,
            limits: Limits::new(4 << 30, &HostLimits::default()),
        }
    };
    let disks = |h: &Handle| -> Vec<PathBuf> {
        VmConfig::load(&h.agent_dir.join("vm.json"))
            .unwrap()
            .disks
            .into_iter()
            .map(|d| d.path)
            .collect()
    };
    let launch = |h: &Handle| {
        linux
            .launch(
                h,
                &Launch {
                    args: vec![],
                    env: vec![],
                    cwd: h.worktree.clone(),
                },
            )
            .unwrap();
    };
    let (t1, h1) = (s.version("template-1.img"), s.version("home-1.img"));
    let old = linux.create(&spec("quiet-amber-otter")).unwrap();
    assert!(disks(&old).contains(&t1) && disks(&old).contains(&h1));
    let line = VmConfig::load(&old.agent_dir.join("vm.json"))
        .unwrap()
        .cmdline;
    assert_eq!(
        boot::Boot::parse(&line).0.layers,
        ["managed", "home", "template", "base"]
    );
    launch(&old);
    linux.sleep(&old).unwrap();
    assert!(s.layout.snapshot("quiet-amber-otter").exists());

    // gcloud's next version, in a new template; the images the sleeping
    // agent was saved with stay as they were, and aren't pruned.
    let before = fingerprint(&t1);
    s.session(|| s.write("template.img", 4096, b"gcloud 2"));
    let images = Images::new(s.layout.linux_dir());
    let gone = images.prune_unused_after(&s.layout.agents_dir(), Duration::ZERO);
    assert!(gone.is_empty(), "{gone:?}");
    assert_eq!(fingerprint(&t1), before);

    let new = linux.create(&spec("brave-copper-lynx")).unwrap();
    let t2 = s.version("template-2.img");
    assert!(disks(&new).contains(&t2) && disks(&new).contains(&s.version("home-2.img")));
    assert_eq!(read_at(&t2, 4096, 8), b"gcloud 2");

    // The sleeping agent wakes exactly where it was, on its own disks.
    assert!(matches!(linux.wake(&old).unwrap(), Woke::Reattach(_)));
    assert!(disks(&old).contains(&t1));
    assert!(t1.exists());

    // Once no agent uses them, the old versions go.
    linux.delete(&old).unwrap();
    fs::remove_dir_all(&old.agent_dir).unwrap();
    let gone = images.prune_unused_after(&s.layout.agents_dir(), Duration::ZERO);
    assert!(gone.contains(&t1) && gone.contains(&h1), "{gone:?}");
    assert!(!s.version("template-1.base").exists());
    assert!(t2.exists());
}

/// A run handler that answers every command `ok`, and the inventory with
/// `inventory` of the VM's configuration.
fn answer(
    s: &Setup,
    inventory: impl Fn(&VmConfig) -> String + Send + Sync + 'static,
    fails: Option<&'static str>,
) {
    s.fake.state.lock().unwrap().run_handler = Some(Arc::new(move |run, config| {
        let text = run.command.join(" ");
        let (code, output) = if run.command.last().map(String::as_str) == Some(INVENTORY) {
            (0, inventory(config))
        } else if fails.is_some_and(|f| text.contains(f)) {
            (1, "E: Unable to locate package nope\n".to_owned())
        } else {
            (0, format!("ran {text}\n"))
        };
        proto::Ran {
            code: Some(code),
            signal: None,
            timed_out: false,
            output,
        }
    }));
}

#[test]
fn updates_run_as_root_and_keep_only_a_template() {
    let s = Setup::new();
    s.session(|| s.write("template.img", 4096, b"gcloud 1"));
    answer(&s, |_| String::new(), None);
    let saved = s
        .toolbox
        .update(&["gcloud components update --quiet".into()])
        .unwrap();
    assert_eq!(
        saved,
        Saved {
            template: Some(2),
            home: None,
            uncovered: vec![],
        }
    );
    // The shell's save looked at the home's logins first.
    let runs: Vec<_> = s
        .fake
        .state
        .lock()
        .unwrap()
        .runs
        .iter()
        .filter(|r| !r.command.iter().any(|a| a == LOGINS))
        .cloned()
        .collect();
    assert_eq!(
        runs[0].command,
        [
            "sudo",
            "-n",
            "-H",
            "/bin/bash",
            "-c",
            "gcloud components update --quiet"
        ]
    );
    // On a clone of the template, with no home and no Claude config.
    assert_eq!(read_at(&s.version("template-2.img"), 4096, 8), b"gcloud 1");
    assert_eq!(s.toolbox.versions().home, Some(1));
    assert!(!s.toolbox.is_open() && !s.working("vm.json").exists());

    // A command that fails keeps nothing, and says why.
    answer(&s, |_| String::new(), Some("nope"));
    let e = s
        .toolbox
        .update(&["true".into(), "apt-get install -y nope".into()])
        .unwrap_err();
    assert!(e.message().contains("exited with code 1"), "{e}");
    assert!(e.message().contains("Unable to locate package nope"), "{e}");
    assert_eq!(s.toolbox.versions().template, Some(2));
    assert!(!s.fake.vm(&s.socket()).running);
    assert!(!s.working("template.img").exists());
    assert!(
        s.toolbox
            .update(&[])
            .unwrap_err()
            .message()
            .contains("no [update]")
    );
}

#[test]
fn a_rebuild_replays_the_recipe_on_the_new_base_and_lists_what_it_doesnt_cover() {
    let s = Setup::new();
    s.session(|| s.write("template.img", 4096, b"by hand"));
    add_base(&s.layout, 2);
    assert_eq!(
        s.toolbox.versions(),
        Versions {
            template: Some(1),
            home: Some(1),
            base: Some(1),
            newest_base: Some(2),
        }
    );
    // New agents stay on the base the template was built on.
    let images = Images::new(s.layout.linux_dir());
    assert_eq!(images.layers().unwrap().base.version, 1);
    let booted = Arc::new(Mutex::new(Vec::new()));
    let seen = booted.clone();
    answer(
        &s,
        move |c| {
            let ids: Vec<&str> = c.disks.iter().map(|d| d.id.as_str()).collect();
            let writable = c.disks.iter().any(|d| !d.read_only);
            seen.lock().unwrap().push((c.kernel.clone(), ids.join(",")));
            let mut items = vec!["apt: git", "/usr/local/bin/node"];
            if ids.contains(&"template") && !writable {
                // The old template: what's installed by hand.
                items.extend(["apt: kubectl", "apt: htop", "/opt/google-cloud-sdk"]);
            }
            if writable {
                // The rebuilt one: what the recipe installed, on a base
                // that has something new of its own.
                items.extend(["apt: kubectl", "/opt/google-cloud-sdk", "apt: ripgrep"]);
            }
            items.join("\n") + "\n"
        },
        None,
    );
    let recipe = vibe_core::toolbox::parse(
        r#"packages = ["kubectl"]
[[install]]
name = "gcloud"
run = "curl -sSL https://sdk.cloud.google.com | bash -s -- --disable-prompts --install-dir=/opt"
"#,
    )
    .unwrap();
    let saved = s.toolbox.rebuild(&recipe).unwrap();
    assert_eq!(
        saved,
        Saved {
            template: Some(2),
            home: None,
            uncovered: vec!["apt: htop".into()],
        }
    );
    // Old template on its base, the old base alone, then the new base.
    let booted = booted.lock().unwrap().clone();
    assert_eq!(
        booted,
        [
            (s.version("base-1/Image"), "base,managed,template".into()),
            (s.version("base-1/Image"), "base,managed".into()),
            (s.version("base-2/Image"), "base,managed,template".into()),
        ]
    );
    let runs: Vec<Vec<String>> = s
        .fake
        .state
        .lock()
        .unwrap()
        .runs
        .iter()
        .map(|r| r.command.clone())
        .filter(|c| {
            c.last().map(String::as_str) != Some(INVENTORY) && !c.iter().any(|a| a == LOGINS)
        })
        .collect();
    assert_eq!(
        runs,
        [
            apt_commands(&["kubectl".into()])[0].clone(),
            apt_commands(&["kubectl".into()])[1].clone(),
            as_root(&recipe.install[0].run),
        ]
    );
    assert!(runs[1].ends_with(&["--".into(), "kubectl".into()]));
    // The rebuilt template is blank but for the recipe, on base 2, which
    // new agents now boot; the Linux home carries over as it is.
    let t2 = s.version("template-2.img");
    assert_eq!(read_at(&t2, 4096, 7), vec![0u8; 7]);
    assert_eq!(
        fs::read_to_string(s.version("template-2.base")).unwrap(),
        "2\n"
    );
    assert_eq!(images.layers().unwrap().base.version, 2);
    assert_eq!(images.layers().unwrap().home, Some(s.version("home-1.img")));
    assert_eq!(s.toolbox.versions().base, Some(2));

    // A recipe step that fails keeps nothing.
    answer(&s, |_| String::new(), Some("apt-get install"));
    let e = s.toolbox.rebuild(&recipe).unwrap_err();
    assert!(
        e.message().contains("installing the recipe's packages"),
        "{e}"
    );
    assert_eq!(s.toolbox.versions().template, Some(2));
}

#[test]
fn a_rebuild_with_no_template_yet_has_nothing_to_compare() {
    let s = Setup::new();
    answer(&s, |_| "apt: git\n".into(), None);
    let saved = s.toolbox.rebuild(&Recipe::default()).unwrap();
    assert_eq!(saved.template, Some(1));
    assert!(saved.uncovered.is_empty());
    // Only the new template was booted: one inventory.
    let inventories = s
        .fake
        .state
        .lock()
        .unwrap()
        .runs
        .iter()
        .filter(|r| r.command.last().map(String::as_str) == Some(INVENTORY))
        .count();
    assert_eq!(inventories, 1);
}

#[test]
fn recipe_commands_and_inventories() {
    assert!(apt_commands(&[]).is_empty());
    let apt = apt_commands(&["kubectl".into(), "postgresql-client".into()]);
    assert_eq!(
        apt[0],
        [
            "sudo",
            "-n",
            "env",
            "DEBIAN_FRONTEND=noninteractive",
            "apt-get",
            "update",
            "-q"
        ]
    );
    assert_eq!(
        apt[1][5..],
        [
            "install",
            "-y",
            "-q",
            "--no-install-recommends",
            "--",
            "kubectl",
            "postgresql-client"
        ]
    );
    let set =
        |items: &[&str]| -> BTreeSet<String> { items.iter().map(|s| s.to_string()).collect() };
    assert_eq!(
        parse_inventory("apt: a\n\n  /opt/x  \napt: a\n"),
        set(&["apt: a", "/opt/x"])
    );
    assert_eq!(
        uncovered(
            &set(&["apt: git", "apt: htop", "/opt/sdk", "npm: tsx"]),
            &set(&["apt: git"]),
            &set(&["/opt/sdk"]),
        ),
        ["apt: htop", "npm: tsx"]
    );
}

#[test]
fn the_clock_is_set_before_anything_runs() {
    // Without it, apt finds Debian's signatures "not live" yet, and says
    // the repository isn't signed (the fake guest fails runs the same).
    let s = Setup::new();
    answer(&s, |_| String::new(), None);
    s.toolbox.update(&["apt-get update".into()]).unwrap();
    let calls = s.calls();
    let boot = calls.iter().position(|c| c == "boot").unwrap();
    let clock = calls.iter().position(|c| c == "clock").unwrap();
    let run = calls.iter().position(|c| c == "run").unwrap();
    assert!(boot < clock && clock < run, "{calls:?}");
    // The shell's VM too, and the rebuild's every boot.
    s.session(|| {});
    s.toolbox.rebuild(&Recipe::default()).unwrap();
    let calls = s.calls();
    let boots = calls.iter().filter(|c| *c == "boot").count();
    let clocks = calls.iter().filter(|c| *c == "clock").count();
    assert_eq!(boots, clocks, "{calls:?}");
}

#[test]
fn updates_wait_for_the_network_and_give_up_saying_so() {
    let s = Setup::new();
    answer(&s, |_| String::new(), None);
    // DHCP answers a few polls after the guest does.
    s.fake.state.lock().unwrap().network_after = 3;
    s.toolbox.update(&["true".into()]).unwrap();
    assert_eq!(s.fake.state.lock().unwrap().network_after, 0);

    let toolbox = Toolbox::new(s.layout.clone(), s.fake.clone(), token())
        .with_network_patience(Duration::from_millis(300));
    s.fake.state.lock().unwrap().network_after = u32::MAX;
    let e = toolbox.update(&["true".into()]).unwrap_err();
    assert!(e.message().contains("got no network"), "{e}");
    assert!(e.message().contains("nothing was kept"), "{e}");
    assert_eq!(toolbox.versions().template, Some(1));
    assert!(!s.fake.vm(&s.socket()).running);
}

#[test]
fn a_failed_command_says_what_it_printed() {
    let s = Setup::new();
    s.fake.state.lock().unwrap().run_handler = Some(Arc::new(|run, _| {
        let output = (1..=25)
            .map(|i| format!("line {i} of {}", run.command.join(" ")))
            .collect::<Vec<_>>()
            .join("\n");
        proto::Ran {
            code: Some(100),
            signal: None,
            timed_out: false,
            output,
        }
    }));
    let e = s.toolbox.update(&["apt-get update".into()]).unwrap_err();
    let text = e.message();
    assert!(
        text.contains("`apt-get update` exited with code 100:\n"),
        "{text}"
    );
    assert!(
        text.contains("line 25 of") && text.contains("line 6 of"),
        "{text}"
    );
    assert!(!text.contains("line 5 of"), "the last 20 lines: {text}");
    let log = s.working("run.log");
    assert!(text.contains(&log.display().to_string()), "{text}");
    let logged = fs::read_to_string(&log).unwrap();
    assert!(
        logged.starts_with("$ sudo -n -H /bin/bash -c apt-get update\n"),
        "{logged}"
    );
    assert!(logged.contains("line 1 of") && logged.contains("line 25 of"));
    // The next run's log starts afresh.
    answer(&s, |_| String::new(), None);
    s.toolbox.update(&["true".into()]).unwrap();
    assert!(!fs::read_to_string(&log).unwrap().contains("line 1 of"));

    assert_eq!(output_tail(""), "");
    assert_eq!(output_tail("\n  \nE: x\n"), ":\nE: x");
}

fn login(tool: LoginTool, account: &str) -> Login {
    Login {
        tool,
        account: account.into(),
    }
}

#[test]
fn saving_the_home_notes_its_logins() {
    let s = Setup::new();
    // No home yet: nothing to log in with.
    assert_eq!(
        s.toolbox.logins(),
        ToolboxLogins {
            home: None,
            logins: Some(vec![]),
            claude: true,
            claude_problem: None,
        }
    );
    s.fake.state.lock().unwrap().run_handler = Some(Arc::new(|run, _| proto::Ran {
        code: Some(0),
        signal: None,
        timed_out: false,
        output: if run.command.iter().any(|a| a == LOGINS) {
            "gh\toctocat on github.com\ngit\tOcto Cat <octo@example.com>\n".into()
        } else {
            String::new()
        },
    }));
    assert_eq!(s.session(|| {}).home, Some(1));
    assert_eq!(
        s.toolbox.logins(),
        ToolboxLogins {
            home: Some(1),
            logins: Some(vec![
                login(LoginTool::Gh, "octocat on github.com"),
                login(LoginTool::Git, "Octo Cat <octo@example.com>"),
            ]),
            claude: true,
            claude_problem: None,
        }
    );
    // It looked in vibe's home while the guest ran, before the seal.
    let runs = s.fake.state.lock().unwrap().runs.clone();
    let look = runs
        .iter()
        .find(|r| r.command.iter().any(|a| a == LOGINS))
        .unwrap();
    assert_eq!(look.command.last().map(String::as_str), Some(GUEST_HOME));
    let calls = s.calls();
    let looked = calls.iter().rposition(|c| c == "run").unwrap();
    let seal = calls.iter().rposition(|c| c == "seal").unwrap();
    assert!(looked < seal, "{calls:?}");

    // A look that fails still saves; that home's logins are unknown, even
    // if a note was left under its name (a home deleted by hand).
    s.images_note(2, &[login(LoginTool::Gh, "someone else")]);
    s.fake.state.lock().unwrap().run_handler = None;
    assert_eq!(s.session(|| {}).home, Some(2));
    let l = s.toolbox.logins();
    assert_eq!((l.home, l.logins), (Some(2), None));

    // Without the Claude token, linux agents have no Claude login, and why.
    let none: TokenSource = Arc::new(|| Err(EnvError::new("no Claude login yet")));
    let t = Toolbox::new(s.layout.clone(), s.fake.clone(), none);
    let l = t.logins();
    assert!(!l.claude);
    assert_eq!(l.claude_problem.as_deref(), Some("no Claude login yet"));
}

impl Setup {
    fn images_note(&self, home: u32, logins: &[Login]) {
        Images::new(self.layout.linux_dir())
            .note_logins(home, logins)
            .unwrap();
    }
}

/// The awks on this machine, as `awk` in a directory of its own each: the
/// guest's Debian has mawk, a Mac its BSD awk.
fn awks(tmp: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for (i, awk) in ["mawk", "gawk", "awk", "original-awk"].iter().enumerate() {
        let Some(found) = ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"]
            .iter()
            .map(|d| Path::new(d).join(awk))
            .find(|p| p.exists())
        else {
            continue;
        };
        let dir = tmp.join(format!("awk-{i}"));
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(found, dir.join("awk")).unwrap();
        dirs.push(dir);
    }
    assert!(!dirs.is_empty(), "no awk");
    dirs
}

#[test]
fn the_look_at_the_logins_reads_what_the_tools_keep() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path().join("home");
    fs::create_dir_all(&h).unwrap();
    let write = |path: &str, text: &str| {
        let p = h.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    };
    let awks = awks(tmp.path());
    // The same with every awk there is.
    let look = || {
        let mut outs = Vec::new();
        for dir in &awks {
            let path = format!(
                "{}:{}",
                dir.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let out = std::process::Command::new("/bin/bash")
                .args(["-c", LOGINS, "logins"])
                .arg(&h)
                .env("PATH", path)
                .env_remove("GIT_CONFIG_GLOBAL")
                .output()
                .unwrap();
            assert!(out.status.success(), "{dir:?}: {out:?}");
            outs.push(String::from_utf8(out.stdout).unwrap());
        }
        assert!(
            outs.windows(2).all(|w| w[0] == w[1]),
            "awks differ: {outs:#?}"
        );
        outs.remove(0)
    };
    // An empty home holds nothing.
    assert_eq!(parse_logins(&look()), vec![]);

    // gh 2.40 on, with two accounts on github.com, one kept in a keyring
    // (`{}`); an older host entry; a GitHub Enterprise host with a port;
    // and a quoted name.
    write(
        ".config/gh/hosts.yml",
        "# gh's hosts\n\
         github.com:\n    users:\n        octocat:\n            oauth_token: gho_secret\n        \
         hubot: {}\n    git_protocol: https\n    oauth_token: gho_secret\n    user: octocat\n\
         ghe.example.com:\n    oauth_token: other_secret\n    user: \"ada\"\n\
         git.corp.example:8443:\n    user: grace\n",
    );
    fs::create_dir_all(h.join(".config/gcloud/legacy_credentials/me@example.com")).unwrap();
    write(".config/gcloud/application_default_credentials.json", "{}");
    // The identity in a file the .gitconfig includes, as dotfiles often do.
    write(".gitconfig", "[include]\n\tpath = ~/.gitconfig.local\n");
    write(
        ".gitconfig.local",
        "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n",
    );
    write(".ssh/id_ed25519", "PRIVATE");
    write(".ssh/id_ed25519.pub", "ssh-ed25519 AAAAC3Nz me@laptop\n");
    write(".ssh/id_rsa", "PRIVATE");
    write(".ssh/github_key", "PRIVATE");
    write(".ssh/github_key.pub", "ssh-ed25519 AAAAC3Nz work key\n");
    write(".ssh/id_old.pub", "ssh-rsa AAAAB3 public only\n");
    write(".ssh/known_hosts", "github.com ssh-ed25519 AAAA\n");
    write(".ssh/config", "Host github.com\n");
    let out = look();
    assert!(!out.contains("secret") && !out.contains("PRIVATE"), "{out}");
    assert_eq!(
        parse_logins(&out),
        vec![
            login(LoginTool::Gh, "octocat on github.com"),
            login(LoginTool::Gh, "hubot on github.com"),
            login(LoginTool::Gh, "ada on ghe.example.com"),
            login(LoginTool::Gh, "grace on git.corp.example:8443"),
            login(LoginTool::Gcloud, "me@example.com"),
            login(LoginTool::Gcloud, "application default credentials"),
            login(LoginTool::Git, "Ada Lovelace <ada@example.com>"),
            login(LoginTool::Ssh, "github_key (work key)"),
            login(LoginTool::Ssh, "id_ed25519 (me@laptop)"),
            login(LoginTool::Ssh, "id_rsa"),
        ]
    );

    // git's identity in its XDG file, and only half of one.
    fs::remove_file(h.join(".gitconfig")).unwrap();
    write(".config/git/config", "[user]\n\temail = ada@example.com\n");
    assert!(parse_logins(&look()).contains(&login(LoginTool::Git, "ada@example.com")));
}

#[test]
fn logins_parse_known_tools_once_each() {
    assert_eq!(
        parse_logins(
            "gh\tme on github.com\nnope\nnpm\tme\ngh\tme on github.com\ngit\t  \nssh\tk\x07ey\n"
        ),
        vec![
            login(LoginTool::Gh, "me on github.com"),
            login(LoginTool::Ssh, "key")
        ]
    );
}

/// Save & Close (`toolbox.close`) hangs up on the shell before it saves:
/// a `SIGHUP` to its group in the guest, as closing its terminal sends.
#[test]
fn hanging_up_signals_the_shells_group_in_the_guest() {
    let s = Setup::new();
    assert!(
        s.toolbox.hang_up(42).is_err(),
        "nothing to hang up on while it's closed"
    );
    s.toolbox.open().unwrap();
    s.fake.state.lock().unwrap().run_handler = Some(Arc::new(|_, _| proto::Ran {
        code: Some(0),
        signal: None,
        timed_out: false,
        output: String::new(),
    }));
    s.toolbox.hang_up(42).unwrap();
    let runs = s.fake.state.lock().unwrap().runs.clone();
    let hup = runs.last().unwrap();
    assert_eq!(
        hup.command,
        ["/bin/sh", "-c", "kill -s HUP -- \"-$1\"", "hang-up", "42"]
    );
    assert!(s.toolbox.hang_up(1).is_err(), "never init's group");
    // A kill the guest refuses says so.
    s.fake.state.lock().unwrap().run_handler = Some(Arc::new(|_, _| proto::Ran {
        code: Some(1),
        signal: None,
        timed_out: false,
        output: "kill: No such process\n".into(),
    }));
    let e = s.toolbox.hang_up(42).unwrap_err().to_string();
    assert!(
        e.contains("hanging up on the toolbox shell exited with code 1"),
        "{e}"
    );
    // One that runs out of time says so, in the units it has.
    s.fake.state.lock().unwrap().run_handler = Some(Arc::new(|_, _| proto::Ran {
        code: None,
        signal: Some("SIGKILL".into()),
        timed_out: true,
        output: String::new(),
    }));
    let e = s.toolbox.hang_up(42).unwrap_err().to_string();
    assert!(e.contains("didn't finish in 5 seconds"), "{e}");
    s.fake.state.lock().unwrap().run_handler = None;
    // Then the shell exits, and the save goes on as for an exit.
    s.fake.exit(&s.socket(), 129);
    let saved = s.toolbox.close().unwrap();
    assert_eq!((saved.template, saved.home), (Some(1), Some(1)));
}
