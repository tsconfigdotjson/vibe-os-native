//! The conformance suite (`vibe_env::conformance`) against the test
//! backend, one test per check. Each backend's own tests run the same
//! suite.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use vibe_core::limits::HostLimits;
use vibe_env::conformance::{self, Harness};
use vibe_env::testing::{STAND_IN, TestBackend};
use vibe_env::{AgentSpec, Limits};

struct TestHarness {
    backend: TestBackend,
    /// Under /tmp, as a socket's path in it must fit in 104 bytes on macOS.
    dir: tempfile::TempDir,
}

impl TestHarness {
    fn new() -> TestHarness {
        let dir = tempfile::Builder::new()
            .prefix("conf")
            .tempdir_in("/tmp")
            .unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        // Written beside its place and renamed in, so no test thread forks
        // while it's open for writing ("text file busy" on Linux).
        let tmp = bin.join("claude.new");
        fs::write(&tmp, STAND_IN).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, bin.join("claude")).unwrap();
        fs::create_dir_all(dir.path().join("scratch")).unwrap();
        fs::set_permissions(
            dir.path().join("scratch"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        TestHarness {
            backend: TestBackend::new(bin.join("claude")),
            dir,
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
}

impl Harness for TestHarness {
    type B = TestBackend;

    fn backend(&self) -> &TestBackend {
        &self.backend
    }

    fn spec(&self, agent_ref: &str) -> AgentSpec {
        let root = self.root();
        let worktree = root.join("worktrees").join(agent_ref);
        let agent_dir = root.join("agents").join(agent_ref);
        for d in [&worktree, &agent_dir] {
            fs::create_dir_all(d).unwrap();
        }
        AgentSpec {
            agent: Default::default(),
            agent_ref: agent_ref.into(),
            project: "web".into(),
            worktree,
            git_dir: root.join("web/.git"),
            agent_dir,
            limits: Limits::new(1 << 30, &HostLimits::default()),
        }
    }

    fn shim(&self) -> PathBuf {
        // Built beside the tests by `cargo test --workspace`.
        let exe = std::env::current_exe().unwrap();
        let path = exe.parent().unwrap().parent().unwrap().join("vibe-shim");
        assert!(
            path.is_file(),
            "{} isn't built; run `cargo build --workspace` first",
            path.display()
        );
        path
    }

    fn scratch(&self) -> PathBuf {
        self.root().join("scratch")
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root().join("agents")]
    }
}

#[test]
fn start_shows_processes() {
    conformance::start_shows_processes(&TestHarness::new());
}

#[test]
fn run_in_compartment() {
    conformance::run_in_compartment(&TestHarness::new());
}

#[test]
fn limit_hit_pauses() {
    conformance::limit_hit_pauses(&TestHarness::new());
}

#[test]
fn sleep_then_wake_is_exact() {
    conformance::sleep_then_wake_is_exact(&TestHarness::new());
}

#[test]
fn reboot_then_wake() {
    conformance::reboot_then_wake(&TestHarness::new());
}

#[test]
fn delete_leaves_nothing() {
    conformance::delete_leaves_nothing(&TestHarness::new());
}

#[test]
fn every_check_in_one_go() {
    conformance::check_all(&TestHarness::new());
}

#[test]
fn a_memory_limit_pauses_too() {
    // Any limit does: here, memory well under what the stand-in uses.
    struct Memory(TestHarness);
    impl Harness for Memory {
        type B = TestBackend;
        fn backend(&self) -> &TestBackend {
            self.0.backend()
        }
        fn spec(&self, r: &str) -> AgentSpec {
            self.0.spec(r)
        }
        fn shim(&self) -> PathBuf {
            self.0.shim()
        }
        fn scratch(&self) -> PathBuf {
            self.0.scratch()
        }
        fn roots(&self) -> Vec<PathBuf> {
            self.0.roots()
        }
        fn limit_case(&self) -> conformance::LimitCase {
            let mut case = self.0.limit_case();
            case.prompt = "fork 2".into();
            case.limits.memory = 1024;
            case.expect = vibe_env::Limit::Memory;
            case.raised.memory = 1 << 40;
            case
        }
    }
    conformance::limit_hit_pauses(&Memory(TestHarness::new()));
}
