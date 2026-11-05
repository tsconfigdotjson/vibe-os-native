//! The toolbox (PRD: The toolbox shell): a VM where the Linux template and
//! the Linux home are writable, for installing tools and logging in once
//! for every `linux` agent.
//!
//! Its VM boots the base the template was built on and the managed layer,
//! read-only, with two writable disks: a clone of the newest template as
//! the root's writable layer, and a clone of the newest Linux home for
//! `/home/vibe` (`vibe.home`). `vibe` may use `sudo` there (`vibe.sudo`),
//! and the Linux Claude config is shared at `~/.claude`, so it's edited
//! there too. Pi has a private agent dir at `~/.pi/agent`, which `vibed` keeps in
//! step with the store, so `/login` there signs in every Pi agent at once and
//! nothing of it lands in the Linux home.
//! Cloning is an APFS clone on the Mac, so nothing is copied.
//!
//! When the work is done, the guest seals the disks (ends everything,
//! flushes, and leaves each filesystem clean), the VM powers off, and each
//! working disk becomes the next version in `~/VibeOS/linux/`
//! ([`Images::keep`]): a new image file, never a changed one, because a
//! sleeping agent's snapshot depends on the exact disks it was saved with.
//! New agents boot the newest; running and sleeping agents keep theirs.
//!
//! What the owner did in the shell is never thrown away. If it can't be
//! saved (the VM stopped first, as in a reboot, or a disk wouldn't seal),
//! its working disks stay, marked unsaved, and the next open boots them
//! again, where ext4's journal makes them whole, so exiting saves them.
//! Until then, no update or rebuild runs.
//!
//! Its layer order differs from an agent's on purpose: the template is the
//! root's writable top, above the managed layer, where agents stack the
//! managed layer above the template. A change under `/opt/vibeos` in the
//! toolbox is hidden in agents, as Claude Code is the daemon's to keep.
//!
//! The same VM runs, without a shell, the weekly `[update]` commands of
//! `toolbox.toml` (a new template version) and the rebuild `vibed` runs itself when
//! a new base ships, which replays the recipe into a blank template on the newest
//! base and lists what the old template has that the recipe doesn't cover. Only one runs
//! at a time, and they all use the ref [`TOOLBOX_REF`]: `run/toolbox.vm`,
//! and `~/VibeOS/agents/toolbox/` for its working disks and `vm.json`,
//! which keeps the layers it uses from being pruned while it runs.
//!
//! What `vibed` needs of it is [`ToolboxHost`], which the test backend
//! implements too ([`crate::testing::TestToolbox`]).

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use vibe_core::claude_token::TOKEN_ENV;
use vibe_core::limits::{self, HostLimits};
use vibe_core::model::DEFAULT_PROFILE_MEMORY;
use vibe_core::model::LoginTool;
use vibe_core::paths::{Layout, private_dir};
use vibe_core::rpc::{Login, ToolboxLogins};
use vibe_core::toolbox::Recipe;

use crate::claude::{CONFIG_DIR_ENV, ClaudeHome};
use crate::guest::{self, GuestState, boot, proto};
use crate::images::{Base, Images, Kind};
use crate::linux::{CMDLINE, GUEST_CLAUDE_HOME, Machine, TokenSource};
use crate::pi;
use crate::vm::{self, Disk, Share, VmConfig};
use crate::{EnvError, Result, TerminalEnd};

/// The toolbox VM's ref: its socket is `run/toolbox.vm`, its directory
/// `~/VibeOS/agents/toolbox`. No agent may take it.
pub const TOOLBOX_REF: &str = "toolbox";

/// `vibe`'s home in the guest, where the shell starts.
pub const GUEST_HOME: &str = "/home/vibe";

/// The toolbox VM's memory: a profile's default.
const MEMORY: u64 = DEFAULT_PROFILE_MEMORY;

/// How long a booting guest may take to answer.
const BOOT_PATIENCE: Duration = Duration::from_secs(60);

/// How long a booted guest may take to get its network (DHCP) before an
/// update or a rebuild, which download.
const NETWORK_PATIENCE: Duration = Duration::from_secs(60);

/// How many of a failed command's last lines its error carries; the whole
/// of what it printed is in `run.log`.
const ERROR_LINES: usize = 20;

/// How long a power-off may take before the VM is stopped outright.
const POWEROFF_PATIENCE: Duration = Duration::from_secs(30);

/// How long one recipe or update command may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long the inventory of what's installed may take.
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(120);

/// The most output the guest returns from a run: the last 32 KiB.
const RUN_OUTPUT: usize = 32 * 1024;

/// Lists what's installed system-wide, one item per line: manually
/// installed Debian packages, apt sources, what's at the top of `/opt` and
/// `/usr/local`'s directories, what `pip` put under `/usr/local` as root, added
/// certificates, and global npm packages. Run on the old template, its base and the
/// rebuilt template, it tells what was installed by hand and what the recipe doesn't
/// cover. That's all it can see: an edit in `/etc`, or a file put elsewhere, isn't
/// on it. Bounded, as a run's output is ([`RUN_OUTPUT`]).
pub const INVENTORY: &str = r#"apt-mark showmanual 2>/dev/null | sed 's/^/apt: /'
for d in /etc/apt/sources.list.d /opt /usr/local/bin /usr/local/sbin /usr/local/lib \
         /usr/local/libexec /usr/local/share /usr/local/include /usr/local/etc; do
  [ -d "$d" ] && ls -1A "$d" 2>/dev/null | sed "s|^|$d/|"
done
for d in /usr/local/share/ca-certificates /usr/local/lib/python3*/dist-packages; do
  [ -d "$d" ] && ls -1A "$d" 2>/dev/null | grep -v '^__pycache__$' | sed "s|^|$d/|"
done
npm ls -g --depth=0 --parseable 2>/dev/null | sed -n 's|.*/node_modules/||p' | sed 's/^/npm: /'
exit 0"#;

/// How long the look at the Linux home's logins may take: it reads a few
/// small files.
const LOGINS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the hang-up's `kill` may take in the guest.
const HANGUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Lists the logins in the Linux home `$1`, one `<tool>\t<whose>` per line,
/// from the files the tools keep, so nothing goes over the network and no
/// secret is printed: every account `gh` holds on each host (its `user` and
/// the `users` of gh 2.40 on), `gcloud`'s accounts and application-default
/// credentials, git's identity (includes followed), and SSH keys (`id_*`,
/// or any private key with its `.pub` beside it) with their comments. Run
/// as `vibe` just before the toolbox seals the home it saves (PRD: Core
/// concepts, Linux home). Other logins (`~/.aws`, `~/.npmrc`, …) aren't
/// looked for.
pub const LOGINS: &str = r#"h=$1
unset GIT_CONFIG_GLOBAL
if [ -f "$h/.config/gh/hosts.yml" ]; then
  awk 'function unq(s) { gsub(/^"|"$/, "", s); return s }
       function ind(s) { match(s, /^[ \t]*/); return RLENGTH }
       /^[ \t]*(#.*)?$/ { next }
       { n = ind($0); line = $0; sub(/^[ \t]+/, "", line) }
       n == 0 { host = ""; users = 0
                if (line ~ /:[ \t]*$/) { host = line; sub(/:[ \t]*$/, "", host); host = unq(host) }
                next }
       host == "" { next }
       users && n <= uind { users = 0 }
       users { if (cind == 0) cind = n
               if (n == cind && line ~ /^[^:]+:[ \t]*({[ \t]*})?[ \t]*$/) {
                 u = line; sub(/:.*$/, "", u); print "gh\t" unq(u) " on " host }
               next }
       line ~ /^users:[ \t]*$/ { users = 1; uind = n; cind = 0; next }
       line ~ /^user:[ \t]*[^ \t]/ { u = line; sub(/^user:[ \t]*/, "", u); print "gh\t" unq(u) " on " host }' \
    "$h/.config/gh/hosts.yml"
fi
for a in "$h"/.config/gcloud/legacy_credentials/*/; do
  [ -d "$a" ] && printf 'gcloud\t%s\n' "$(basename "$a")"
done
[ -f "$h/.config/gcloud/application_default_credentials.json" ] &&
  printf 'gcloud\tapplication default credentials\n'
git() { HOME=$h XDG_CONFIG_HOME= command git config --global --includes --get "$1" 2>/dev/null; }
n=$(git user.name)
e=$(git user.email)
if [ -n "$n" ] && [ -n "$e" ]; then printf 'git\t%s <%s>\n' "$n" "$e"
elif [ -n "$n$e" ]; then printf 'git\t%s\n' "$n$e"; fi
for k in "$h"/.ssh/*; do
  [ -f "$k" ] || continue
  case $k in *.pub) continue ;; esac
  case ${k##*/} in id_*) ;; *) [ -f "$k.pub" ] || continue ;; esac
  c=
  [ -f "$k.pub" ] && c=$(awk '{ $1 = ""; $2 = ""; sub(/^ +/, ""); print; exit }' "$k.pub")
  printf 'ssh\t%s%s\n' "${k##*/}" "${c:+ ($c)}"
done
exit 0"#;

/// What [`LOGINS`] printed, as logins: known tools only, each once, in
/// the order found.
pub fn parse_logins(output: &str) -> Vec<Login> {
    let mut found: Vec<Login> = Vec::new();
    for line in output.lines() {
        let Some((tool, account)) = line.split_once('\t') else {
            continue;
        };
        let tool = match tool.trim() {
            "gh" => LoginTool::Gh,
            "gcloud" => LoginTool::Gcloud,
            "git" => LoginTool::Git,
            "ssh" => LoginTool::Ssh,
            _ => continue,
        };
        let account: String = account
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect::<String>()
            .trim()
            .to_owned();
        let login = Login { tool, account };
        if !login.account.is_empty() && !found.contains(&login) {
            found.push(login);
        }
    }
    found
}

/// What `toolbox.logins` says of the toolbox in `images`: the newest Linux
/// home, the logins noted for it (none without a home), and whether linux
/// agents have a Claude login, or why not.
pub fn home_logins(images: &Images, claude: std::result::Result<(), String>) -> ToolboxLogins {
    let home = images.newest_of(Kind::Home).map(|(n, _)| n);
    ToolboxLogins {
        logins: match home {
            Some(n) => images.logins_of(n),
            None => Some(Vec::new()),
        },
        home,
        claude: claude.is_ok(),
        claude_problem: claude.err(),
    }
}

/// What one toolbox run kept: the new versions, and for a rebuild, what
/// the old template had that the recipe doesn't cover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    pub template: Option<u32>,
    pub home: Option<u32>,
    pub uncovered: Vec<String>,
}

/// The versions the toolbox has made, and the base images.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Versions {
    /// The newest template, if the toolbox has made one.
    pub template: Option<u32>,
    pub home: Option<u32>,
    /// The base release the newest template is on, or new agents boot
    /// without one.
    pub base: Option<u32>,
    /// The newest base release installed: newer than `base`, a rebuild is
    /// due (PRD: Base image updates).
    pub newest_base: Option<u32>,
}

/// What `vibed` drives the toolbox through. Every call may block.
pub trait ToolboxHost: Send + Sync + fmt::Debug {
    fn versions(&self) -> Versions;
    /// Boots the toolbox with the template and home writable, and starts
    /// its shell, whose terminal a shim then holds. If the shell is open
    /// already, its terminal as it is: there's one toolbox at a time.
    fn open(&self) -> Result<TerminalEnd>;
    /// Whether a shell is open: started, and its toolbox not yet closed.
    fn is_open(&self) -> bool;
    /// Whether the open shell's command still runs.
    fn shell_running(&self) -> bool;
    /// Hangs up on the open shell, as closing its terminal does: `SIGHUP`
    /// to its process group, so the shell saves its history and ends what
    /// it started. `shell_pid` is its pid where it runs, as its shim says
    /// (`child_pid`): in the VM, for the real toolbox. Then [`close`] saves
    /// once the shell has exited.
    ///
    /// [`close`]: ToolboxHost::close
    fn hang_up(&self, shell_pid: u32) -> Result<()>;
    /// After the shell exits: seals, powers off and keeps the next version
    /// of the template and of the home. If the VM stopped first, nothing
    /// is kept and the error says so.
    fn close(&self) -> Result<Saved>;
    /// Throws away whatever an unfinished toolbox left, stopping its VM.
    fn discard(&self) -> Result<()>;
    /// Whether a shell's changes wait to be saved, which holds back any rebuild.
    fn has_unsaved(&self) -> bool {
        false
    }
    /// Runs `commands` as root on the template, then keeps it as the next
    /// version: `[update]`.
    fn update(&self, commands: &[String]) -> Result<Saved>;
    /// Replays `recipe` into a blank template on the newest base, keeps it,
    /// and lists what the old template has that the recipe doesn't cover.
    /// The Linux home carries over as it is. With `whole_only`, as nobody's there
    /// to see what's lost, a template that lacks any of that isn't kept: it says
    /// what's uncovered, with no `template`.
    fn rebuild(&self, recipe: &Recipe, whole_only: bool) -> Result<Saved>;
    /// The logins the newest Linux home holds, as noted when it was saved,
    /// and whether linux agents have a Claude login. Boots nothing.
    fn logins(&self) -> ToolboxLogins;
    /// Gives the Linux home `token`, a login to GitHub got by the owner's own browser
    /// sign-in, by `gh`'s own way of being given one, and keeps the next version of
    /// it. The template stays as it is.
    fn log_in_to_github(&self, token: &str) -> Result<Saved>;
}

/// The toolbox in a VM, on `vibe-vm` and `vibe-guest`.
pub struct Toolbox {
    layout: Layout,
    images: Images,
    machine: Arc<dyn Machine>,
    token: TokenSource,
    host_cpus: usize,
    network_patience: Duration,
    /// Held while anything boots, closes or runs in the toolbox VM.
    busy: Mutex<()>,
}

impl fmt::Debug for Toolbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Toolbox")
            .field("images", &self.images)
            .field("machine", &self.machine)
            .finish_non_exhaustive()
    }
}

/// What an open (or unsaved) shell was booted on, kept in its directory so
/// a restarted `vibed` can still close it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Opened {
    base: u32,
}

/// Where a writable disk comes from.
#[derive(Debug, Clone)]
enum Source {
    /// A clone of a version, which stays as it is.
    Clone(PathBuf),
    /// A blank sparse disk of this size, which the guest formats.
    Blank(u64),
    /// The working disk as it is: an unsaved shell's.
    Unsaved,
}

/// One boot of the toolbox VM.
#[derive(Debug, Clone)]
struct Plan {
    base: Base,
    /// Read-only layers, top first, above the base: the managed layer,
    /// and for an inventory the template being looked at.
    layers: Vec<(&'static str, PathBuf)>,
    template: Option<Source>,
    home: Option<Source>,
    /// Share the Linux Claude config at `~/.claude`, and Pi's agent dir at
    /// `~/.pi/agent`.
    claude: bool,
}

fn err(message: impl Into<String>) -> EnvError {
    EnvError::new(message.into())
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(err(format!("can't remove {}: {e}", path.display())))
        }
        _ => Ok(()),
    }
}

/// `command` as root, as `sudo` runs it in the toolbox shell: root's
/// home, root's `PATH`, and no prompt.
pub fn as_root(command: &str) -> Vec<String> {
    ["sudo", "-n", "-H", "/bin/bash", "-c", command]
        .map(String::from)
        .into()
}

/// The commands that install `recipe`'s packages, each run as it is.
pub fn apt_commands(packages: &[String]) -> Vec<Vec<String>> {
    if packages.is_empty() {
        return Vec::new();
    }
    let apt = |args: &[&str]| -> Vec<String> {
        [
            "sudo",
            "-n",
            "env",
            "DEBIAN_FRONTEND=noninteractive",
            "apt-get",
        ]
        .iter()
        .chain(args)
        .map(|s| (*s).to_owned())
        .collect()
    };
    let mut install = apt(&["install", "-y", "-q", "--no-install-recommends", "--"]);
    install.extend(packages.iter().cloned());
    vec![apt(&["update", "-q"]), install]
}

/// The last [`ERROR_LINES`] lines `output` has, for an error: on lines of
/// their own after a colon, or nothing if it printed nothing.
pub fn output_tail(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let from = lines.len().saturating_sub(ERROR_LINES);
    let tail: Vec<String> = lines[from..]
        .iter()
        .map(|l| l.chars().take(500).collect())
        .collect();
    format!(":\n{}", tail.join("\n"))
}

/// What the old template has that its base doesn't, and that the rebuilt
/// template doesn't have either: what was installed by hand and the recipe
/// doesn't cover.
pub fn uncovered(
    old: &BTreeSet<String>,
    old_base: &BTreeSet<String>,
    new: &BTreeSet<String>,
) -> Vec<String> {
    old.iter()
        .filter(|i| !old_base.contains(*i) && !new.contains(*i))
        .cloned()
        .collect()
}

/// An inventory's output, as a set of items.
pub fn parse_inventory(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

impl Toolbox {
    pub fn new(layout: Layout, machine: Arc<dyn Machine>, token: TokenSource) -> Toolbox {
        Toolbox {
            images: Images::new(layout.linux_dir()),
            layout,
            machine,
            token,
            host_cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            network_patience: NETWORK_PATIENCE,
            busy: Mutex::new(()),
        }
    }

    /// Waits this long for the guest's network instead of a minute.
    pub fn with_network_patience(mut self, patience: Duration) -> Toolbox {
        self.network_patience = patience;
        self
    }

    pub fn images(&self) -> &Images {
        &self.images
    }

    fn dir(&self) -> PathBuf {
        self.layout.agent_dir(TOOLBOX_REF)
    }

    fn socket(&self) -> PathBuf {
        self.layout.vm_socket(TOOLBOX_REF)
    }

    fn config_path(&self) -> PathBuf {
        self.dir().join("vm.json")
    }

    fn working(&self, kind: Kind) -> PathBuf {
        self.dir().join(match kind {
            Kind::Template => "template.img",
            Kind::Home => "home.img",
        })
    }

    /// What the commands of the last update or rebuild printed, whole.
    fn run_log(&self) -> PathBuf {
        self.dir().join("run.log")
    }

    fn opened_path(&self) -> PathBuf {
        self.dir().join("open.json")
    }

    fn unsaved_path(&self) -> PathBuf {
        self.dir().join("unsaved.json")
    }

    fn opened(&self) -> Option<Opened> {
        let text = fs::read_to_string(self.opened_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// What an unsaved shell was booted on, if one is waiting.
    fn unsaved(&self) -> Option<Opened> {
        let text = fs::read_to_string(self.unsaved_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Stops the VM if it runs and keeps an open shell's working disks,
    /// unsaved, for the next open.
    fn keep_unsaved(&self) -> Result<()> {
        let socket = self.socket();
        if self.machine.running(&socket) {
            self.machine.stop(&socket)?;
        }
        if self.opened_path().exists() {
            fs::rename(self.opened_path(), self.unsaved_path())
                .map_err(|e| err(format!("can't mark the toolbox unsaved: {e}")))?;
        }
        remove_file(&self.config_path())
    }

    /// Refuses while an unsaved shell waits: its disks come first.
    fn refuse_if_unsaved(&self, what: &str) -> Result<()> {
        if self.unsaved().is_some() {
            return Err(err(format!(
                "the toolbox has unsaved changes: open it with `vibe toolbox` and exit to save \
                 them, and the {what} can run"
            )));
        }
        Ok(())
    }

    fn host_limits(&self) -> Result<HostLimits> {
        limits::load(&self.layout.host_config()).map_err(|e| err(e.to_string()))
    }

    /// Takes the toolbox for one boot, close or run, or says it's busy.
    fn take(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        match self.busy.try_lock() {
            Ok(g) => Ok(g),
            Err(TryLockError::Poisoned(p)) => Ok(p.into_inner()),
            Err(TryLockError::WouldBlock) => Err(err(
                "the toolbox is busy: it's starting, saving, updating or rebuilding the template",
            )),
        }
    }

    /// The base the newest template is on, or the newest base without one.
    fn current_base(&self) -> Result<Base> {
        let none = || err("no Linux base image yet: vibed downloads it when it starts");
        match self.images.newest_of(Kind::Template) {
            Some((_, t)) => self.images.base_of(&t).ok_or_else(none),
            None => self.images.base().ok_or_else(none),
        }
    }

    fn managed_layer(&self) -> Vec<(&'static str, PathBuf)> {
        let mut layers: Vec<(&'static str, PathBuf)> = Vec::new();
        if let Some((_, p)) = self.images.managed() {
            layers.push(("managed", p));
        }
        if let Some((_, p)) = self.images.pi() {
            layers.push(("pi", p));
        }
        layers
    }

    /// Makes a writable disk from `source` at `path`.
    fn make_disk(path: &Path, source: &Source) -> Result<()> {
        if let Source::Unsaved = source {
            return if path.is_file() {
                Ok(())
            } else {
                Err(err(format!("the unsaved disk {} is gone", path.display())))
            };
        }
        remove_file(path)?;
        match source {
            Source::Clone(from) => crate::sparse::clone_file(from, path).map_err(|e| {
                err(format!(
                    "can't clone {} for the toolbox: {e}",
                    from.display()
                ))
            }),
            Source::Blank(size) => {
                vm::create_disk(path, *size).map_err(|e| err(format!("the toolbox's disk: {e}")))
            }
            Source::Unsaved => unreachable!("handled above"),
        }
    }

    /// Writes the VM's configuration for `plan`, its disks included, and
    /// boots it; once the guest answers with everything stacked and
    /// mounted, it's up. If anything fails, the VM is stopped.
    fn boot(&self, plan: &Plan, host: &HostLimits) -> Result<()> {
        let dir = self.dir();
        private_dir(&dir).map_err(|e| err(format!("can't create {}: {e}", dir.display())))?;
        let mut disks = vec![Disk {
            id: "base".into(),
            path: plan.base.disk.clone(),
            read_only: true,
        }];
        for (id, path) in &plan.layers {
            disks.push(Disk {
                id: (*id).into(),
                path: path.clone(),
                read_only: true,
            });
        }
        let mut writable = Vec::new();
        for (kind, source) in [(Kind::Template, &plan.template), (Kind::Home, &plan.home)] {
            if let Some(source) = source {
                let path = self.working(kind);
                Toolbox::make_disk(&path, source)?;
                let id = match kind {
                    Kind::Template => "template",
                    Kind::Home => "home",
                };
                disks.push(Disk {
                    id: id.into(),
                    path,
                    read_only: false,
                });
                writable.push(id);
            }
        }
        let mut shares = Vec::new();
        let mut mounts = Vec::new();
        if plan.claude {
            let config = self.layout.linux_claude_dir();
            private_dir(&config)
                .map_err(|e| err(format!("can't create {}: {e}", config.display())))?;
            // Claude opens straight to its prompt there too, and doesn't
            // try to update itself: the managed layer does.
            let home = ClaudeHome::own(config.clone());
            home.mark_managed()
                .and_then(|()| home.prefill(Path::new(GUEST_HOME), false))
                .map_err(|e| err(format!("can't set up the Linux Claude config: {e}")))?;
            shares.push(Share {
                tag: "claude".into(),
                path: config,
                read_only: false,
            });
            mounts.push(boot::Share {
                tag: "claude".into(),
                path: GUEST_CLAUDE_HOME.into(),
            });
            // A private agent dir for Pi here, the owner's, which `vibed` keeps
            // in step with the store as it does a VM's. Not the owner's share of
            // settings and extensions, which would run with `sudo` and the Claude
            // token here.
            let dir = self.layout.linux_pi_signin_dir();
            private_dir(&dir).map_err(|e| err(format!("can't create {}: {e}", dir.display())))?;
            shares.push(Share {
                tag: "pi".into(),
                path: dir,
                read_only: false,
            });
            mounts.push(boot::Share {
                tag: "pi".into(),
                path: pi::GUEST_AGENT_DIR.into(),
            });
        }
        let mut layers: Vec<String> = plan.layers.iter().map(|(id, _)| (*id).to_owned()).collect();
        layers.push("base".into());
        let line = boot::Boot {
            layers,
            upper: plan.template.as_ref().map(|_| "template".into()),
            home: plan.home.as_ref().map(|_| "home".into()),
            sudo: plan.template.is_some(),
            shares: mounts,
            // SAFETY: getuid and getgid have no memory effects.
            uid: Some(unsafe { libc::getuid() }),
            gid: Some(unsafe { libc::getgid() }),
        }
        .append_to(CMDLINE)
        .map_err(|e| err(format!("the toolbox's command line: {e}")))?;
        let config = VmConfig {
            version: vm::CONFIG_VERSION,
            agent: TOOLBOX_REF.into(),
            cpus: vm::vcpus(host.cpu_cores, self.host_cpus),
            memory: MEMORY,
            kernel: plan.base.kernel.clone(),
            initrd: Some(plan.base.initrd.clone()),
            cmdline: line,
            machine_id: None,
            mac: None,
            disks,
            shares,
            console_log: Some(dir.join("console.log")),
            snapshot: self.layout.snapshot(TOOLBOX_REF),
        };
        let path = self.config_path();
        config
            .save(&path)
            .map_err(|e| err(format!("can't write {}: {e}", path.display())))?;
        self.machine.create(&path)?;
        let socket = self.socket();
        self.machine
            .boot(&path, &socket, &dir.join("vibe-vm.log"))
            .map_err(|e| err(format!("the toolbox VM won't boot: {e}")))?;
        let up = (|| -> Result<()> {
            let status = self.machine.ready(&socket, BOOT_PATIENCE)?;
            // Without its layers the guest runs from its initrd, and would
            // write nothing to the disks it was given.
            if status.layers.is_empty() {
                return Err(err(format!(
                    "the toolbox VM couldn't stack its disks: {}",
                    status.problems.join("; ")
                )));
            }
            // A guest from before the toolbox ignores what it can't read,
            // and would put the home's writes in the template, with no sudo
            // and no seal.
            if let Some(p) = status
                .problems
                .iter()
                .find(|p| p.contains("unknown setting"))
            {
                return Err(err(format!(
                    "base image {}'s vibe-guest is too old for the toolbox ({p}), so it can't \
                     open until the template moves to the newest base image",
                    plan.base.version
                )));
            }
            let missing: Vec<&str> = config
                .shares
                .iter()
                .map(|s| s.tag.as_str())
                .filter(|tag| !status.shares.iter().any(|m| m.tag == *tag))
                .collect();
            if !missing.is_empty() {
                return Err(err(format!(
                    "the toolbox VM couldn't mount {}: {}",
                    missing.join(", "),
                    status.problems.join("; ")
                )));
            }
            // A VM has no clock of its own; TLS and apt's signatures need it.
            self.machine.set_clock(&socket, SystemTime::now())
        })();
        if up.is_err() {
            let _ = self.machine.stop(&socket);
        }
        up
    }

    /// Seals the VM's writable disks and powers it off. If the guest can't
    /// seal them all, or the VM is gone, they mustn't be kept as versions.
    fn seal_and_power_off(&self, expect: &[&str]) -> Result<()> {
        let socket = self.socket();
        if !self.machine.running(&socket) {
            return Err(err("the toolbox VM stopped before its disks were sealed"));
        }
        let sealed = self.machine.seal(&socket);
        let powered_off = self.machine.poweroff(&socket).is_ok()
            && self.machine.wait_gone(&socket, POWEROFF_PATIENCE);
        if !powered_off {
            self.machine.stop(&socket)?;
        }
        let sealed = sealed?;
        if let Some(missing) = expect.iter().find(|id| !sealed.iter().any(|s| s == *id)) {
            return Err(err(format!("the toolbox couldn't seal its {missing} disk")));
        }
        Ok(())
    }

    /// Removes what a toolbox VM leaves in its directory.
    fn clean(&self) -> Result<()> {
        for path in [
            self.working(Kind::Template),
            self.working(Kind::Home),
            self.config_path(),
            self.config_path().with_extension("json.tmp"),
            self.opened_path(),
        ] {
            remove_file(&path)?;
        }
        Ok(())
    }

    /// Stops the VM if it runs and cleans up after it.
    fn stop_and_clean(&self) -> Result<()> {
        let socket = self.socket();
        if self.machine.running(&socket) {
            self.machine.stop(&socket)?;
        }
        self.clean()
    }

    /// Runs `argv` as `vibe` in the VM. What it printed is added to
    /// `run.log`; an error names `what`, says how it ended, and carries the
    /// last lines it printed.
    fn run(&self, what: &str, argv: Vec<String>, timeout: Duration) -> Result<String> {
        let ran = self.machine.run(
            &self.socket(),
            &proto::Run {
                command: argv.clone(),
                env: Default::default(),
                cwd: Some("/".into()),
                timeout_ms: timeout.as_millis() as u64,
            },
        )?;
        let log = self.run_log();
        let logged = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "$ {}\n{}", argv.join(" "), ran.output.trim_end())
            });
        if let Err(e) = logged {
            return Err(err(format!("can't write {}: {e}", log.display())));
        }
        if ran.code == Some(0) {
            return Ok(ran.output);
        }
        let how = match (ran.timed_out, ran.code, ran.signal) {
            (true, _, _) if timeout.as_secs() < 60 => {
                format!("didn't finish in {} seconds", timeout.as_secs())
            }
            (true, _, _) => format!("didn't finish in {} minutes", timeout.as_secs() / 60),
            (_, Some(code), _) => format!("exited with code {code}"),
            (_, None, Some(signal)) => format!("was ended by {signal}"),
            _ => "failed".into(),
        };
        Err(err(format!(
            "{what} {how}{}\n(all it printed is in {})",
            output_tail(&ran.output),
            log.display()
        )))
    }

    /// Waits for the guest's network, as an update or a rebuild downloads.
    fn wait_network(&self) -> Result<()> {
        let socket = self.socket();
        let deadline = Instant::now() + self.network_patience;
        loop {
            let status = self.machine.status(&socket)?;
            if status.network.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(err(format!(
                    "the toolbox VM got no network in {} s: {}",
                    self.network_patience.as_secs(),
                    status.problems.join("; ")
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn inventory(&self) -> Result<BTreeSet<String>> {
        let out = self.run(
            "the inventory of what's installed",
            vec!["/bin/bash".into(), "-c".into(), INVENTORY.into()],
            INVENTORY_TIMEOUT,
        )?;
        // The guest gives back only the end of a long output.
        if out.len() >= RUN_OUTPUT - 64 {
            return Err(err(
                "the inventory of what's installed is too long to compare",
            ));
        }
        Ok(parse_inventory(&out))
    }

    /// Boots `plan` read-only (writes go to memory), takes the inventory,
    /// and powers off.
    fn inventory_of(&self, plan: &Plan, host: &HostLimits) -> Result<BTreeSet<String>> {
        if let Err(e) = self.boot(plan, host) {
            let _ = self.clean();
            return Err(e);
        }
        let found = self.inventory();
        let socket = self.socket();
        let off = self.machine.poweroff(&socket).is_ok()
            && self.machine.wait_gone(&socket, POWEROFF_PATIENCE);
        if !off {
            self.machine.stop(&socket)?;
        }
        self.clean()?;
        found
    }

    /// The template's next version from what `work` does in a toolbox VM
    /// booted on `plan`; nothing is kept if any of it fails.
    /// Puts back what was installed by hand that a machine can: Debian packages and
    /// global npm packages, by the names the inventory has. As far as it can, and
    /// quietly: what's still missing afterwards is what's said to the owner, so
    /// nothing else is asked of them for it.
    fn reinstall(&self, by_hand: &BTreeSet<String>, recipe: &Recipe) {
        let plain = |name: &&str| {
            !name.is_empty()
                && !name.starts_with('-')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-._:@/".contains(c))
        };
        let apt: Vec<String> = by_hand
            .iter()
            .filter_map(|item| item.strip_prefix("apt: "))
            .filter(plain)
            .filter(|name| !recipe.packages.iter().any(|p| p == name))
            .map(str::to_owned)
            .collect();
        let quietly = |what: &str, argv: Vec<String>| self.run(what, argv, COMMAND_TIMEOUT).is_ok();
        let all_at_once = apt_commands(&apt)
            .into_iter()
            .all(|argv| quietly("putting back what was installed by hand", argv));
        // One that's gone from the new base shouldn't cost the others.
        if !all_at_once {
            for name in &apt {
                if let Some(install) = apt_commands(std::slice::from_ref(name)).pop() {
                    quietly("putting back what was installed by hand", install);
                }
            }
        }
        for name in by_hand
            .iter()
            .filter_map(|item| item.strip_prefix("npm: "))
            .filter(plain)
        {
            quietly(
                "putting back what was installed by hand",
                as_root(&format!("npm install -g -- {name}")),
            );
        }
    }

    fn build_template(
        &self,
        plan: &Plan,
        host: &HostLimits,
        work: impl FnOnce() -> Result<()>,
    ) -> Result<u32> {
        let result = (|| -> Result<u32> {
            self.boot(plan, host)?;
            self.wait_network()?;
            work()?;
            self.seal_and_power_off(&["template"])?;
            let (n, _) = self.images.keep(
                Kind::Template,
                &self.working(Kind::Template),
                Some(plan.base.version),
            )?;
            Ok(n)
        })();
        if result.is_err() {
            let _ = self.stop_and_clean();
        } else {
            self.clean()?;
        }
        result.map_err(|e| err(format!("{e}; nothing was kept")))
    }
}

impl ToolboxHost for Toolbox {
    fn has_unsaved(&self) -> bool {
        self.unsaved().is_some()
    }

    fn versions(&self) -> Versions {
        let template = self.images.newest_of(Kind::Template);
        Versions {
            base: match &template {
                Some((_, t)) => self.images.base_of(t).map(|b| b.version),
                None => self.images.base().map(|b| b.version),
            },
            template: template.map(|(n, _)| n),
            home: self.images.newest_of(Kind::Home).map(|(n, _)| n),
            newest_base: self.images.base().map(|b| b.version),
        }
    }

    fn open(&self) -> Result<TerminalEnd> {
        let socket = self.socket();
        let _busy = self.take()?;
        if self.machine.running(&socket) {
            if self.opened().is_some() {
                return Ok(TerminalEnd::Vm { socket });
            }
            return Err(err(
                "the toolbox is busy updating or rebuilding the template",
            ));
        }
        let host = self.host_limits()?;
        // An unsaved shell's disks, where there are, on the base it was on;
        // otherwise clones of the newest versions.
        let unsaved = self.unsaved();
        let base = match &unsaved {
            Some(u) => self.images.installed(u.base).ok_or_else(|| {
                err(format!(
                    "the unsaved toolbox was on base image {}, which is gone",
                    u.base
                ))
            })?,
            None => {
                self.clean()?;
                self.current_base()?
            }
        };
        let source = |kind: Kind| match self.images.newest_of(kind) {
            _ if unsaved.is_some() && self.working(kind).is_file() => Source::Unsaved,
            Some((_, path)) => Source::Clone(path),
            None => Source::Blank(host.vm_disk),
        };
        let plan = Plan {
            layers: self.managed_layer(),
            template: Some(source(Kind::Template)),
            home: Some(source(Kind::Home)),
            claude: true,
            base: base.clone(),
        };
        let started = (|| -> Result<()> {
            self.boot(&plan, &host)?;
            let opened = Opened { base: base.version };
            fs::write(
                self.opened_path(),
                serde_json::to_string(&opened).expect("it encodes"),
            )
            .map_err(|e| err(format!("can't write {}: {e}", self.opened_path().display())))?;
            remove_file(&self.unsaved_path())?;
            let mut env = std::collections::BTreeMap::new();
            env.insert(CONFIG_DIR_ENV.into(), GUEST_CLAUDE_HOME.into());
            env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
            env.insert("DISABLE_INSTALLATION_CHECKS".into(), "1".into());
            env.extend(pi::quiet_env());
            // Claude works in the toolbox once it has a login; the shell
            // works without one.
            if let Ok(token) = (self.token)() {
                env.insert(TOKEN_ENV.into(), token);
            }
            self.machine.launch(
                &socket,
                &guest::Launch {
                    command: vec!["/bin/bash".into(), "-l".into()],
                    env,
                    cwd: Some(GUEST_HOME.into()),
                    size: None,
                    time_ms: Some(guest::millis(SystemTime::now())),
                },
            )
        })();
        if let Err(e) = started {
            // Unsaved disks stay unsaved; fresh clones go.
            if unsaved.is_some() {
                let _ = self.keep_unsaved();
                if self.unsaved().is_none() {
                    let opened = Opened { base: base.version };
                    let _ = fs::write(
                        self.unsaved_path(),
                        serde_json::to_string(&opened).expect("it encodes"),
                    );
                }
            } else {
                let _ = self.stop_and_clean();
            }
            return Err(e);
        }
        Ok(TerminalEnd::Vm { socket })
    }

    fn is_open(&self) -> bool {
        self.opened().is_some()
    }

    fn shell_running(&self) -> bool {
        let socket = self.socket();
        self.opened().is_some()
            && self.machine.running(&socket)
            && self
                .machine
                .status(&socket)
                .is_ok_and(|s| s.state == GuestState::Running)
    }

    fn hang_up(&self, shell_pid: u32) -> Result<()> {
        if self.opened().is_none() {
            return Err(err("no toolbox shell is open"));
        }
        if shell_pid <= 1 {
            return Err(err(format!("{shell_pid} isn't the toolbox shell's pid")));
        }
        // The guest started the shell leading its own session and group;
        // its `kill` names the group, as the shim's hang-up does here.
        self.run(
            "hanging up on the toolbox shell",
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "kill -s HUP -- \"-$1\"".into(),
                "hang-up".into(),
                shell_pid.to_string(),
            ],
            HANGUP_TIMEOUT,
        )
        .map(drop)
    }

    fn close(&self) -> Result<Saved> {
        let _busy = self.take()?;
        let opened = self
            .opened()
            .ok_or_else(|| err("no toolbox shell is open"))?;
        let mut saved = Saved::default();
        // What the home holds, while the guest still runs. A look that
        // fails leaves it unknown, and says why in the log; it never stops
        // or slows the save.
        let logins = if self.machine.running(&self.socket()) {
            self.run(
                "the look at the Linux home's logins",
                vec![
                    "/bin/bash".into(),
                    "-c".into(),
                    LOGINS.into(),
                    "logins".into(),
                    GUEST_HOME.into(),
                ],
                LOGINS_TIMEOUT,
            )
            .map(|out| parse_logins(&out))
            .map_err(|e| eprintln!("vibe-env: the toolbox's logins are unknown: {e}"))
            .ok()
        } else {
            None
        };
        let kept = (|| -> Result<()> {
            self.seal_and_power_off(&["template", "home"])?;
            // Each as it's kept: one that fails stays unsaved alone.
            let (template, _) = self.images.keep(
                Kind::Template,
                &self.working(Kind::Template),
                Some(opened.base),
            )?;
            saved.template = Some(template);
            let (home, _) = self
                .images
                .keep_home(&self.working(Kind::Home), logins.as_deref())?;
            saved.home = Some(home);
            Ok(())
        })();
        if let Err(e) = kept {
            self.keep_unsaved()?;
            let done = match saved.template {
                Some(t) => format!(" (template {t} was saved)"),
                None => String::new(),
            };
            return Err(err(format!(
                "{e}; the changes are kept unsaved{done}: `vibe toolbox` opens them again, and \
                 exiting saves them"
            )));
        }
        self.clean()?;
        Ok(saved)
    }

    /// Stops a toolbox VM nothing waits on and removes what an interrupted
    /// update or rebuild left. An open shell's disks are kept, unsaved.
    fn discard(&self) -> Result<()> {
        let _busy = self.take()?;
        if self.opened().is_some() || self.unsaved().is_some() {
            return self.keep_unsaved();
        }
        self.stop_and_clean()
    }

    fn update(&self, commands: &[String]) -> Result<Saved> {
        if commands.is_empty() {
            return Err(err("toolbox.toml has no [update] commands to run"));
        }
        let _busy = self.take()?;
        if self.machine.running(&self.socket()) || self.opened().is_some() {
            return Err(err(
                "the toolbox shell is open: exit it first, and the update can run",
            ));
        }
        self.refuse_if_unsaved("update")?;
        remove_file(&self.run_log())?;
        let host = self.host_limits()?;
        let base = self.current_base()?;
        let plan = Plan {
            layers: self.managed_layer(),
            template: Some(match self.images.newest_of(Kind::Template) {
                Some((_, path)) => Source::Clone(path),
                None => Source::Blank(host.vm_disk),
            }),
            home: None,
            claude: false,
            base,
        };
        let template = self.build_template(&plan, &host, || {
            for command in commands {
                self.run(&format!("`{command}`"), as_root(command), COMMAND_TIMEOUT)?;
            }
            Ok(())
        })?;
        Ok(Saved {
            template: Some(template),
            ..Saved::default()
        })
    }

    fn rebuild(&self, recipe: &Recipe, whole_only: bool) -> Result<Saved> {
        let _busy = self.take()?;
        if self.machine.running(&self.socket()) || self.opened().is_some() {
            return Err(err(
                "the toolbox shell is open: exit it first, and the rebuild can run",
            ));
        }
        self.refuse_if_unsaved("rebuild")?;
        remove_file(&self.run_log())?;
        let host = self.host_limits()?;
        let newest = self
            .images
            .base()
            .ok_or_else(|| err("no Linux base image yet: vibed downloads it when it starts"))?;
        let managed = self.managed_layer();
        // What the old template has that its own base doesn't: what was
        // installed by hand, by the recipe or not.
        let by_hand = match self.images.newest_of(Kind::Template) {
            None => BTreeSet::new(),
            Some((_, old)) => {
                let old_base = self.images.base_of(&old).ok_or_else(|| {
                    err("the template's base image is gone, so it can't be compared")
                })?;
                let mut layers = managed.clone();
                layers.push(("template", old));
                let read_only = |layers| Plan {
                    base: old_base.clone(),
                    layers,
                    template: None,
                    home: None,
                    claude: false,
                };
                let with = self.inventory_of(&read_only(layers), &host)?;
                let without = self.inventory_of(&read_only(managed.clone()), &host)?;
                with.difference(&without).cloned().collect()
            }
        };
        let plan = Plan {
            base: newest,
            layers: managed,
            template: Some(Source::Blank(host.vm_disk)),
            home: None,
            claude: false,
        };
        let mut after = BTreeSet::new();
        let mut held = false;
        let template = self.build_template(&plan, &host, || {
            for argv in apt_commands(&recipe.packages) {
                self.run("installing the recipe's packages", argv, COMMAND_TIMEOUT)?;
            }
            for install in &recipe.install {
                self.run(
                    &format!("installing {}", install.name),
                    as_root(&install.run),
                    COMMAND_TIMEOUT,
                )?;
            }
            self.reinstall(&by_hand, recipe);
            after = self.inventory()?;
            if whole_only && !uncovered(&by_hand, &BTreeSet::new(), &after).is_empty() {
                held = true;
                return Err(err("it wouldn't have all the old template has"));
            }
            Ok(())
        });
        let uncovered = uncovered(&by_hand, &BTreeSet::new(), &after);
        if held {
            return Ok(Saved {
                template: None,
                home: None,
                uncovered,
            });
        }
        Ok(Saved {
            template: Some(template?),
            home: None,
            uncovered,
        })
    }

    fn log_in_to_github(&self, token: &str) -> Result<Saved> {
        let _busy = self.take()?;
        if self.machine.running(&self.socket()) || self.opened().is_some() {
            return Err(err(
                "the toolbox shell is open: exit it first, and you can sign in from here",
            ));
        }
        self.refuse_if_unsaved("sign-in")?;
        remove_file(&self.run_log())?;
        let host = self.host_limits()?;
        let base = self.current_base()?;
        let source = |kind: Kind| match self.images.newest_of(kind) {
            Some((_, path)) => Source::Clone(path),
            None => Source::Blank(host.vm_disk),
        };
        let plan = Plan {
            layers: self.managed_layer(),
            template: Some(source(Kind::Template)),
            home: Some(source(Kind::Home)),
            claude: false,
            base,
        };
        let result = (|| -> Result<u32> {
            self.boot(&plan, &host)?;
            let ran = self.machine.run(
                &self.socket(),
                &proto::Run {
                    command: vec![
                        "/bin/bash".into(),
                        "-c".into(),
                        "printf %s \"$VIBE_GH_TOKEN\" | gh auth login --hostname github.com \
                         --with-token && gh auth setup-git --hostname github.com"
                            .into(),
                    ],
                    env: [("VIBE_GH_TOKEN".to_owned(), token.to_owned())]
                        .into_iter()
                        .collect(),
                    cwd: Some(GUEST_HOME.into()),
                    timeout_ms: LOGINS_TIMEOUT.as_millis() as u64 * 12,
                },
            )?;
            if ran.code != Some(0) {
                return Err(err(format!(
                    "gh in the toolbox wouldn't take it: {}",
                    output_tail(&ran.output)
                )));
            }
            let logins = self
                .run(
                    "the look at the Linux home's logins",
                    vec![
                        "/bin/bash".into(),
                        "-c".into(),
                        LOGINS.into(),
                        "logins".into(),
                        GUEST_HOME.into(),
                    ],
                    LOGINS_TIMEOUT,
                )
                .map(|out| parse_logins(&out))
                .ok();
            self.seal_and_power_off(&["template", "home"])?;
            let (home, _) = self
                .images
                .keep_home(&self.working(Kind::Home), logins.as_deref())?;
            Ok(home)
        })();
        match &result {
            Ok(_) => self.clean()?,
            Err(_) => {
                let _ = self.stop_and_clean();
            }
        }
        Ok(Saved {
            home: Some(result.map_err(|e| err(format!("{e}; nothing was kept")))?),
            ..Saved::default()
        })
    }

    fn logins(&self) -> ToolboxLogins {
        home_logins(
            &self.images,
            (self.token)().map(drop).map_err(|e| e.message().to_owned()),
        )
    }
}

#[cfg(test)]
mod tests;
