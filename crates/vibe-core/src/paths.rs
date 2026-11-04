//! Where `vibed` keeps things. Clients and the daemon resolve the socket the
//! same way; `protocol/vibed-rpc.json` records its defaults for the Swift
//! side. [`Layout`] has the rest (PRD: Where state lives).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Overrides the socket path for the daemon and every client.
pub const SOCKET_ENV: &str = "VIBE_SOCKET";

/// The temp directory's name in a shim's state directory, which is
/// `~/VibeOS/agents/<ref>` for an agent's session.
pub const TMP_DIR: &str = "tmp";

/// The daemon's socket's name, in the run directory.
pub const SOCKET_NAME: &str = "vibed.sock";

/// The socket path relative to `$HOME` on macOS.
pub const MACOS_SOCKET: &str = "Library/Application Support/VibeOS/run/vibed.sock";

/// The socket path relative to `$HOME` on a Linux host.
pub const LINUX_SOCKET: &str = ".local/share/vibeos/run/vibed.sock";

/// The default socket path for `os` (as in `std::env::consts::OS`), relative
/// to `$HOME`. Every Unix other than macOS uses the Linux layout.
pub fn default_socket_relative(os: &str) -> &'static str {
    if os == "macos" {
        MACOS_SOCKET
    } else {
        LINUX_SOCKET
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoHomeError;

impl fmt::Display for NoHomeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "$HOME is not set, so there's no default for vibed's files (or set {SOCKET_ENV} for the socket alone)"
        )
    }
}

impl std::error::Error for NoHomeError {}

/// Resolves the socket path: an explicit path wins, then `$VIBE_SOCKET`, then
/// the platform default under `$HOME`. Empty values count as unset.
pub fn resolve_socket(
    explicit: Option<PathBuf>,
    env_socket: Option<OsString>,
    home: Option<OsString>,
    os: &str,
) -> Result<PathBuf, NoHomeError> {
    let nonempty = |v: OsString| (!v.is_empty()).then_some(v);
    if let Some(path) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return Ok(path);
    }
    if let Some(path) = env_socket.and_then(nonempty) {
        return Ok(PathBuf::from(path));
    }
    let home = home.and_then(nonempty).ok_or(NoHomeError)?;
    Ok(PathBuf::from(home).join(default_socket_relative(os)))
}

/// Every path `vibed` uses for one user, on one OS. macOS keeps state under
/// `~/Library`; every other Unix uses the Linux layout under
/// `~/.local/share/vibeos/`. Projects and host config are in the same place on
/// both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    home: PathBuf,
    mac: bool,
}

impl Layout {
    /// The layout for `home` on `os` (as in `std::env::consts::OS`).
    pub fn new(home: impl Into<PathBuf>, os: &str) -> Self {
        Layout {
            home: home.into(),
            mac: os == "macos",
        }
    }

    /// The layout for this process: `$HOME` (non-empty) on this OS.
    pub fn current() -> Result<Self, NoHomeError> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or(NoHomeError)?;
        Ok(Layout::new(home, std::env::consts::OS))
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `vibe.db`, the run directory and, on Linux, the logs.
    pub fn data_dir(&self) -> PathBuf {
        if self.mac {
            self.home.join("Library/Application Support/VibeOS")
        } else {
            self.home.join(".local/share/vibeos")
        }
    }

    /// The SQLite registry.
    pub fn db(&self) -> PathBuf {
        self.data_dir().join("vibe.db")
    }

    /// Sockets: `vibed.sock`, an external-access tunnel's (`mac.sock` on a
    /// Linux host, `tunnel.sock` on a Mac), and `<ref>.sock` for each
    /// running session.
    pub fn run_dir(&self) -> PathBuf {
        self.data_dir().join("run")
    }

    /// The socket of the session `agent_ref`'s shim serves.
    pub fn session_socket(&self, agent_ref: &str) -> PathBuf {
        self.run_dir().join(format!("{agent_ref}.sock"))
    }

    /// The control socket of a `linux` agent's `vibe-vm`, while its VM runs.
    /// It's `<ref>.vm`, not `.sock`, so a scan for sessions never takes it
    /// for a shim's, and it's no longer than a session's socket.
    pub fn vm_socket(&self, agent_ref: &str) -> PathBuf {
        self.run_dir().join(format!("{agent_ref}.vm"))
    }

    /// The default socket, the same path [`resolve_socket`] gives without
    /// overrides.
    pub fn socket(&self) -> PathBuf {
        self.run_dir().join(SOCKET_NAME)
    }

    pub fn logs_dir(&self) -> PathBuf {
        if self.mac {
            self.home.join("Library/Logs/VibeOS")
        } else {
            self.data_dir().join("logs")
        }
    }

    /// `~/.config/vibeos`, on every OS.
    pub fn config_dir(&self) -> PathBuf {
        self.home.join(".config/vibeos")
    }

    /// Host limits, which the owner may also edit by hand.
    pub fn host_config(&self) -> PathBuf {
        self.config_dir().join("host.toml")
    }

    /// The toolbox's optional recipe, which the owner writes by hand.
    pub fn toolbox_config(&self) -> PathBuf {
        self.config_dir().join("toolbox.toml")
    }

    /// Where GitHub projects are cloned.
    pub fn projects_dir(&self) -> PathBuf {
        self.home.join("VibeOS/projects")
    }

    /// Agents' worktrees, one directory per project.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.home.join("VibeOS/worktrees")
    }

    /// The worktree of the agent `agent_ref` of `project`, which is also
    /// where its branch is checked out.
    pub fn worktree(&self, project: &str, agent_ref: &str) -> PathBuf {
        self.worktrees_dir().join(project).join(agent_ref)
    }

    /// One directory per agent, for what VibeOS keeps of it outside its
    /// worktree: its temp dir and its session's log and record.
    pub fn agents_dir(&self) -> PathBuf {
        self.home.join("VibeOS/agents")
    }

    pub fn agent_dir(&self, agent_ref: &str) -> PathBuf {
        self.agents_dir().join(agent_ref)
    }

    /// The agent's `TMPDIR`, which its shim points it at.
    pub fn agent_tmp(&self, agent_ref: &str) -> PathBuf {
        self.agent_dir(agent_ref).join(TMP_DIR)
    }

    /// A `linux` agent's VM configuration, fixed for its life.
    pub fn vm_config(&self, agent_ref: &str) -> PathBuf {
        self.agent_dir(agent_ref).join("vm.json")
    }

    /// Where a `linux` agent's guest console goes.
    pub fn vm_console_log(&self, agent_ref: &str) -> PathBuf {
        self.agent_dir(agent_ref).join("console.log")
    }

    /// A `linux` agent's `vibe-vm` log.
    pub fn vm_log(&self, agent_ref: &str) -> PathBuf {
        self.agent_dir(agent_ref).join("vibe-vm.log")
    }

    /// The disk images `linux` agents boot: the base, the managed layer,
    /// the template and the Linux home (`vibe_env::images`).
    pub fn linux_dir(&self) -> PathBuf {
        self.home.join("VibeOS/linux")
    }

    /// The Linux Claude config, made in the toolbox and copied into each
    /// `linux` agent's own Claude home when it starts.
    pub fn linux_claude_dir(&self) -> PathBuf {
        self.home.join("VibeOS/linux-claude")
    }

    /// What the owner keeps for Pi in `linux` agents on the Mac: settings,
    /// models, extensions. Every Pi VM has it as a read-only share, so nothing it
    /// does can reach another through it (PRD: Exceptions).
    pub fn linux_pi_dir(&self) -> PathBuf {
        self.home.join("VibeOS/linux-pi")
    }

    /// The owner's one Pi login for `linux` agents: `auth.json`, which no guest
    /// mounts and only `vibed` writes, from which each gets a private copy.
    pub fn linux_pi_logins_dir(&self) -> PathBuf {
        self.home.join("VibeOS/linux-pi-logins")
    }

    /// The toolbox's own agent dir for Pi: a private copy of the login like a
    /// VM's, and the owner's, so a `/login` there always wins.
    pub fn linux_pi_signin_dir(&self) -> PathBuf {
        self.home.join("VibeOS/linux-pi-signin")
    }

    /// Where the owner names what Pi agents get in their environment, on
    /// purpose.
    pub fn pi_env_file(&self) -> PathBuf {
        self.config_dir().join("pi.env")
    }

    /// The package caches `linux` agents share, one directory per tool.
    pub fn cache_dir(&self) -> PathBuf {
        self.home.join("VibeOS/cache")
    }

    /// Sleeping `linux` agents' snapshots, which share the `snapshot_disk`
    /// budget.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.home.join("VibeOS/snapshots")
    }

    /// A sleeping `linux` agent's whole VM, RAM included.
    pub fn snapshot(&self, agent_ref: &str) -> PathBuf {
        self.snapshots_dir().join(agent_ref)
    }
}

/// [`resolve_socket`] for this process: its environment and its OS.
/// Makes `dir`, and the directories on the way, if they're missing, and
/// leaves `dir` itself `0700` whatever the umask: for what only this user
/// may read, such as sockets, snapshots and session state.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn socket_path(explicit: Option<PathBuf>) -> Result<PathBuf, NoHomeError> {
    resolve_socket(
        explicit,
        std::env::var_os(SOCKET_ENV),
        std::env::var_os("HOME"),
        std::env::consts::OS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn private_dirs_are_0700_whatever_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        // One made open, as a umask of 022 or 002 makes it, is closed.
        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o775)).unwrap();
        private_dir(&open).unwrap();
        assert_eq!(mode(&open), 0o700);
        // One made here is private, however deep.
        let deep = tmp.path().join("a/b/scratch");
        private_dir(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
        // One too closed is opened to its owner.
        std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o500)).unwrap();
        private_dir(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
    }

    #[test]
    fn explicit_path_wins() {
        let p = resolve_socket(
            Some("/a.sock".into()),
            os("/b.sock"),
            os("/home/u"),
            "linux",
        );
        assert_eq!(p, Ok(PathBuf::from("/a.sock")));
    }

    #[test]
    fn env_beats_default() {
        let p = resolve_socket(None, os("/b.sock"), os("/home/u"), "macos");
        assert_eq!(p, Ok(PathBuf::from("/b.sock")));
    }

    #[test]
    fn defaults_per_os() {
        assert_eq!(
            resolve_socket(None, None, os("/Users/u"), "macos"),
            Ok(PathBuf::from(
                "/Users/u/Library/Application Support/VibeOS/run/vibed.sock"
            ))
        );
        assert_eq!(
            resolve_socket(None, None, os("/home/u"), "linux"),
            Ok(PathBuf::from("/home/u/.local/share/vibeos/run/vibed.sock"))
        );
        assert_eq!(default_socket_relative("freebsd"), LINUX_SOCKET);
        for s in [MACOS_SOCKET, LINUX_SOCKET] {
            assert!(s.ends_with(&format!("/run/{SOCKET_NAME}")), "{s}");
        }
    }

    #[test]
    fn empty_values_count_as_unset() {
        let p = resolve_socket(Some(PathBuf::new()), os(""), os("/home/u"), "linux");
        assert_eq!(p, Ok(PathBuf::from("/home/u").join(LINUX_SOCKET)));
        assert_eq!(
            resolve_socket(None, os(""), os(""), "linux"),
            Err(NoHomeError)
        );
        assert_eq!(resolve_socket(None, None, None, "linux"), Err(NoHomeError));
    }

    #[test]
    fn mac_layout() {
        let l = Layout::new("/Users/u", "macos");
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(l.home(), Path::new("/Users/u"));
        assert_eq!(
            l.db(),
            p("/Users/u/Library/Application Support/VibeOS/vibe.db")
        );
        assert_eq!(
            l.run_dir(),
            p("/Users/u/Library/Application Support/VibeOS/run")
        );
        assert_eq!(l.logs_dir(), p("/Users/u/Library/Logs/VibeOS"));
        assert_eq!(l.host_config(), p("/Users/u/.config/vibeos/host.toml"));
        assert_eq!(l.projects_dir(), p("/Users/u/VibeOS/projects"));
        assert_eq!(l.agent_dir("x"), p("/Users/u/VibeOS/agents/x"));
        assert_eq!(l.agent_tmp("x"), p("/Users/u/VibeOS/agents/x/tmp"));
        assert_eq!(l.worktree("web", "x"), p("/Users/u/VibeOS/worktrees/web/x"));
        assert_eq!(
            l.session_socket("x"),
            p("/Users/u/Library/Application Support/VibeOS/run/x.sock")
        );
        assert_eq!(
            l.vm_socket("x"),
            p("/Users/u/Library/Application Support/VibeOS/run/x.vm")
        );
        assert_eq!(l.vm_config("x"), p("/Users/u/VibeOS/agents/x/vm.json"));
        assert_eq!(
            l.vm_console_log("x"),
            p("/Users/u/VibeOS/agents/x/console.log")
        );
        assert_eq!(l.vm_log("x"), p("/Users/u/VibeOS/agents/x/vibe-vm.log"));
        assert_eq!(l.snapshot("x"), p("/Users/u/VibeOS/snapshots/x"));
        assert_eq!(l.linux_dir(), p("/Users/u/VibeOS/linux"));
        assert_eq!(l.linux_claude_dir(), p("/Users/u/VibeOS/linux-claude"));
        assert_eq!(l.cache_dir(), p("/Users/u/VibeOS/cache"));
    }

    #[test]
    fn linux_layout() {
        let l = Layout::new("/home/u", "linux");
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(l.db(), p("/home/u/.local/share/vibeos/vibe.db"));
        assert_eq!(l.logs_dir(), p("/home/u/.local/share/vibeos/logs"));
        assert_eq!(l.host_config(), p("/home/u/.config/vibeos/host.toml"));
        assert_eq!(l.projects_dir(), p("/home/u/VibeOS/projects"));
        assert_eq!(l.agent_dir("x"), p("/home/u/VibeOS/agents/x"));
        assert_eq!(
            l.session_socket("x"),
            p("/home/u/.local/share/vibeos/run/x.sock")
        );
    }

    #[test]
    fn vm_sockets_never_look_like_sessions() {
        // A session scan takes every `<ref>.sock` in run/; a VM's socket must
        // not be one, whatever the ref, and must fit wherever a session's
        // does.
        let l = Layout::new("/h", "macos");
        for r in ["a", "a.vm", "quiet-amber-otter"] {
            let vm = l.vm_socket(r);
            assert!(!vm.to_string_lossy().ends_with(".sock"), "{vm:?}");
            assert_ne!(vm, l.session_socket(r));
            assert!(vm.as_os_str().len() <= l.session_socket(r).as_os_str().len());
        }
    }

    #[test]
    fn layout_socket_is_the_default_socket() {
        for os in ["macos", "linux"] {
            assert_eq!(
                Ok(Layout::new("/h", os).socket()),
                resolve_socket(None, None, Some(OsString::from("/h")), os),
                "{os}"
            );
        }
    }
}
