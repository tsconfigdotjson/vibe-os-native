//! The conformance suite (`vibe_env::conformance`) against the `linux`
//! backend, with real VMs on a Mac: the base image `scripts/base-image.sh`
//! built, and the stand-in for Claude Code run from a share.
//!
//! ```text
//! VIBE_VM_BIN=build/vibe-vm VIBE_BASE_RELEASES=file://$PWD/build/base-image \
//!   cargo test -p vibe-env --test linux_conformance -- --test-threads=1
//! ```
//!
//! Without those it skips; `scripts/check-full.sh` runs it with
//! `VIBE_VM_MARKER` set, so a skip can't pass for a pass. It needs
//! `cargo build --workspace` first, for `vibe-shim`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vibe_core::limits::HostLimits;
use vibe_core::paths::Layout;
use vibe_env::conformance::{self, Harness, LimitCase};
use vibe_env::images::{self, Images};
use vibe_env::linux::{LinuxVm, Real, TokenSource};
use vibe_env::testing::STAND_IN;
use vibe_env::{AgentSpec, Limit, Limits};

const MIB: u64 = 1 << 20;

struct VmHarness {
    backend: LinuxVm,
    layout: Layout,
    dir: tempfile::TempDir,
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

impl VmHarness {
    fn new(test: &str) -> Option<VmHarness> {
        let (Some(vibe_vm), Some(base)) = (
            std::env::var_os("VIBE_VM_BIN"),
            std::env::var("VIBE_BASE_RELEASES")
                .ok()
                .filter(|v| !v.is_empty()),
        ) else {
            eprintln!("{test}: skipped; set VIBE_VM_BIN and VIBE_BASE_RELEASES to run it on a Mac");
            return None;
        };
        let dir = tempfile::Builder::new()
            .prefix("lconf")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let layout = Layout::new(&root, "macos");
        let images = Images::new(layout.linux_dir());
        images
            .install_base(
                &images::base_url(&base, images::BASE_RELEASE),
                images::BASE_RELEASE,
            )
            .unwrap();
        // The stand-in runs as Claude, from the share the project's .git
        // is at; the managed layer is there, but not what runs.
        let tools = root.join("web.git");
        fs::create_dir_all(&tools).unwrap();
        fs::write(tools.join("claude"), STAND_IN).unwrap();
        fs::set_permissions(tools.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        images
            .add_managed("0.0.1", &tools.join("claude"), "stand-in")
            .unwrap();
        fs::create_dir_all(root.join("scratch")).unwrap();
        fs::create_dir_all(layout.config_dir()).unwrap();
        // Small VMs, quick to save, and a small writable disk.
        fs::write(layout.host_config(), "vm_disk = \"2G\"\ncpu_cores = 2\n").unwrap();
        let token: TokenSource = Arc::new(|| Ok("sk-ant-oat01-unused-by-the-stand-in".into()));
        let backend = LinuxVm::with_machine(
            layout.clone(),
            Arc::new(Real {
                vibe_vm: absolute(PathBuf::from(vibe_vm)),
            }),
            token,
        )
        .with_claude(tools.join("claude").to_string_lossy().into_owned())
        .with_disk_check(Duration::from_millis(200));
        Some(VmHarness {
            backend,
            layout,
            dir,
        })
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
}

impl Harness for VmHarness {
    type B = LinuxVm;

    fn backend(&self) -> &LinuxVm {
        &self.backend
    }

    fn spec(&self, agent_ref: &str) -> AgentSpec {
        let worktree = self.root().join("worktrees").join(agent_ref);
        let agent_dir = self.layout.agent_dir(agent_ref);
        for d in [&worktree, &agent_dir] {
            fs::create_dir_all(d).unwrap();
        }
        AgentSpec {
            agent_ref: agent_ref.into(),
            project: "web".into(),
            worktree,
            git_dir: self.root().join("web.git"),
            agent_dir,
            limits: Limits::new(1024 * MIB, &HostLimits::default()),
        }
    }

    fn shim(&self) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let path = exe.parent().unwrap().parent().unwrap().join("vibe-shim");
        assert!(path.is_file(), "{} isn't built", path.display());
        path
    }

    fn scratch(&self) -> PathBuf {
        self.root().join("scratch")
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.layout.agents_dir(),
            self.layout.snapshots_dir(),
            self.layout.run_dir(),
        ]
    }

    /// A VM's memory and vCPUs are fixed at boot; its worktree is held to
    /// `disk_hard`.
    fn limit_case(&self) -> LimitCase {
        let limits = Limits {
            memory: 1024 * MIB,
            cpu_cores: 2,
            max_processes: 512,
            disk_soft: MIB,
            disk_hard: 4 * MIB,
        };
        LimitCase {
            prompt: "fill 8".into(),
            limits,
            expect: Limit::Disk,
            raised: Limits {
                disk_soft: 1 << 30,
                disk_hard: 1 << 31,
                ..limits
            },
        }
    }
}

#[test]
fn the_linux_backend_conforms() {
    let Some(h) = VmHarness::new("the_linux_backend_conforms") else {
        return;
    };
    conformance::check_all(&h);
    if let Some(dir) = std::env::var_os("VIBE_VM_MARKER") {
        let dir = absolute(PathBuf::from(dir));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("linux_conformance"), "ran\n").unwrap();
    }
}
