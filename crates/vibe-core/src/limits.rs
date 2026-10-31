//! Host limits: everything that isn't per profile (PRD: Host limits). They
//! live in `~/.config/vibeos/host.toml`, which the owner can edit by hand and
//! `vibed` edits over the socket. A setting left out of the file has its
//! default, so defaults can improve without rewriting anyone's file.

use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Value};

use crate::units::{GIB, TIB, format_duration, format_size, parse_duration, parse_size};

/// The largest size any setting takes: far beyond any disk, and well inside
/// the 64-bit signed integers clients decode it into.
const MAX_SIZE: u64 = 1024 * TIB;

/// The longest `idle_debounce`: a year.
const MAX_DURATION: u64 = 365 * 24 * 3600;

/// Host-wide limits. On the socket, sizes are bytes and durations seconds;
/// in `host.toml` they're written like `"4G"` and `"60s"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLimits {
    /// Memory always left for macOS and `vibed`.
    pub reserve_memory: u64,
    /// Cores always left for macOS and `vibed`.
    pub reserve_cores: u32,
    pub max_awake_agents: u32,
    /// CPU cores per agent.
    pub cpu_cores: u32,
    /// Processes per `macos` agent.
    pub max_processes: u32,
    /// Per worktree: past it, a warning.
    pub disk_soft: u64,
    /// Per worktree: past it, the agent is paused.
    pub disk_hard: u64,
    /// A `linux` agent's own writable layer.
    pub vm_disk: u64,
    /// How long an agent stays idle before it sleeps.
    pub idle_debounce: u64,
    /// The disk budget shared by every sleeping `linux` agent's snapshot.
    pub snapshot_disk: u64,
    /// Whether `linux` agents share the npm, pnpm, cargo and pip caches
    /// (PRD: Mounts). It's the opt-out, so it's set in `host.toml` only,
    /// not over the socket.
    #[serde(skip, default = "yes")]
    pub package_caches: bool,
    /// The loopback port MCP is served on, `http://127.0.0.1:<port>/mcp`
    /// (PRD: MCP interface). Set in `host.toml` only, not over the socket.
    #[serde(skip, default = "mcp_port")]
    pub mcp_port: u16,
    /// Whether MCP is also served on this host's Tailscale address, on the
    /// same port. In `host.toml`, and set over the socket with `mcp.set`
    /// (Settings), not with `limits.set`.
    #[serde(skip)]
    pub mcp_tailscale: bool,
}

fn yes() -> bool {
    true
}

/// The default MCP port.
pub const MCP_PORT: u16 = 7878;

fn mcp_port() -> u16 {
    MCP_PORT
}

/// A setting that's true or false, and only in `host.toml`.
pub const PACKAGE_CACHES: &str = "package_caches";

/// The MCP settings, which are in `host.toml` only.
pub const MCP_PORT_KEY: &str = "mcp_port";
pub const MCP_TAILSCALE: &str = "mcp_tailscale";

/// The settings that aren't host limits on the socket.
const FILE_ONLY: [&str; 3] = [PACKAGE_CACHES, MCP_PORT_KEY, MCP_TAILSCALE];

impl Default for HostLimits {
    fn default() -> Self {
        HostLimits {
            reserve_memory: 4 * GIB,
            reserve_cores: 2,
            max_awake_agents: 16,
            cpu_cores: 4,
            max_processes: 512,
            disk_soft: 20 * GIB,
            disk_hard: 30 * GIB,
            vm_disk: 20 * GIB,
            idle_debounce: 60,
            snapshot_disk: 20 * GIB,
            package_caches: true,
            mcp_port: MCP_PORT,
            mcp_tailscale: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Size,
    Count,
    Duration,
}

/// Every setting, in the order the PRD lists them.
const SETTINGS: [(&str, Kind); 10] = [
    ("reserve_memory", Kind::Size),
    ("reserve_cores", Kind::Count),
    ("max_awake_agents", Kind::Count),
    ("cpu_cores", Kind::Count),
    ("max_processes", Kind::Count),
    ("disk_soft", Kind::Size),
    ("disk_hard", Kind::Size),
    ("vm_disk", Kind::Size),
    ("idle_debounce", Kind::Duration),
    ("snapshot_disk", Kind::Size),
];

impl HostLimits {
    /// The names of the settings, as in `host.toml` and on the socket.
    pub fn keys() -> impl Iterator<Item = &'static str> {
        SETTINGS.iter().map(|(k, _)| *k)
    }

    fn slot(&mut self, key: &str) -> Option<Slot<'_>> {
        Some(match key {
            "reserve_memory" => Slot::Wide(&mut self.reserve_memory),
            "reserve_cores" => Slot::Narrow(&mut self.reserve_cores),
            "max_awake_agents" => Slot::Narrow(&mut self.max_awake_agents),
            "cpu_cores" => Slot::Narrow(&mut self.cpu_cores),
            "max_processes" => Slot::Narrow(&mut self.max_processes),
            "disk_soft" => Slot::Wide(&mut self.disk_soft),
            "disk_hard" => Slot::Wide(&mut self.disk_hard),
            "vm_disk" => Slot::Wide(&mut self.vm_disk),
            "idle_debounce" => Slot::Wide(&mut self.idle_debounce),
            "snapshot_disk" => Slot::Wide(&mut self.snapshot_disk),
            _ => return None,
        })
    }

    /// One setting by name.
    pub fn get(&self, key: &str) -> u64 {
        let mut copy = *self;
        match copy.slot(key) {
            Some(Slot::Wide(v)) => *v,
            Some(Slot::Narrow(v)) => u64::from(*v),
            None => 0,
        }
    }

    /// Sets one setting by name. Counts must fit in 32 bits.
    pub fn set(&mut self, key: &str, value: u64) -> Result<(), String> {
        match self.slot(key) {
            Some(Slot::Wide(v)) => *v = value,
            Some(Slot::Narrow(v)) => {
                *v = u32::try_from(value).map_err(|_| format!("{key} is too large: {value}"))?
            }
            None => return Err(format!("unknown setting `{key}`")),
        }
        Ok(())
    }

    /// Checks the limits make sense together.
    pub fn validate(&self) -> Result<(), String> {
        let at_least_one = [
            ("max_awake_agents", u64::from(self.max_awake_agents)),
            ("cpu_cores", u64::from(self.cpu_cores)),
            ("max_processes", u64::from(self.max_processes)),
            ("disk_soft", self.disk_soft),
            ("disk_hard", self.disk_hard),
        ];
        for (key, v) in at_least_one {
            if v == 0 {
                return Err(format!("{key} must be more than 0"));
            }
        }
        for (key, kind) in SETTINGS {
            let v = self.get(key);
            let max = match kind {
                Kind::Size => MAX_SIZE,
                Kind::Duration => MAX_DURATION,
                Kind::Count => continue,
            };
            if v > max {
                return Err(format!("{key} must be at most {}", show(kind, max)));
            }
        }
        if self.disk_soft > self.disk_hard {
            return Err(format!(
                "disk_soft ({}) must not be more than disk_hard ({})",
                show(Kind::Size, self.disk_soft),
                show(Kind::Size, self.disk_hard)
            ));
        }
        if self.vm_disk < GIB {
            return Err("vm_disk must be at least 1G".into());
        }
        Ok(())
    }
}

enum Slot<'a> {
    Wide(&'a mut u64),
    Narrow(&'a mut u32),
}

fn show(kind: Kind, v: u64) -> String {
    match kind {
        Kind::Size => format_size(v).unwrap_or_else(|| format!("{v} bytes")),
        Kind::Count => v.to_string(),
        Kind::Duration => format_duration(v),
    }
}

#[derive(Debug)]
pub enum LimitsError {
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The file isn't valid: bad TOML, an unknown setting or a bad value.
    Invalid { path: PathBuf, message: String },
    /// The change asked for would leave the limits invalid.
    Rejected(String),
}

impl fmt::Display for LimitsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitsError::Io { what, path, source } => {
                write!(f, "can't {what} {}: {source}", path.display())
            }
            LimitsError::Invalid { path, message } => {
                write!(f, "{} is invalid: {message}", path.display())
            }
            LimitsError::Rejected(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LimitsError {}

/// Reads the limits from `path`. A missing file means every default.
pub fn load(path: &Path) -> Result<HostLimits, LimitsError> {
    let doc = read(path)?;
    let limits = interpret(&doc).map_err(|message| invalid(path, message))?;
    limits
        .validate()
        .map_err(|message| invalid(path, message))?;
    Ok(limits)
}

/// Sets the numeric setting `key` in `doc`.
fn set_number(doc: &mut DocumentMut, key: &str, value: u64) -> Result<(), LimitsError> {
    let kind = SETTINGS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, kind)| *kind)
        .ok_or_else(|| LimitsError::Rejected(format!("unknown setting `{key}`")))?;
    set_value(doc, key, encode(kind, value));
    Ok(())
}

/// Writes `changes` (setting, value) into `path`, keeping everything else in
/// the file, comments included, and returns the limits that result. Nothing
/// is written if they'd be invalid.
pub fn update(path: &Path, changes: &[(&str, u64)]) -> Result<HostLimits, LimitsError> {
    edit(path, |doc| {
        for &(key, value) in changes {
            set_number(doc, key, value)?;
        }
        Ok(())
    })
}

/// Turns MCP on this host's Tailscale address on or off in `path`
/// (PRD: MCP interface, Transport), keeping everything else in the file, and
/// returns the settings that result.
pub fn set_mcp_tailscale(path: &Path, on: bool) -> Result<HostLimits, LimitsError> {
    edit(path, |doc| {
        set_value(doc, MCP_TAILSCALE, Value::from(on));
        Ok(())
    })
}

/// Sets `key` to `new` in `doc`, keeping the comment after the old value,
/// e.g. `# per agent`.
fn set_value(doc: &mut DocumentMut, key: &str, mut new: Value) {
    match doc.get_mut(key).and_then(Item::as_value_mut) {
        Some(old) => {
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
        None => doc[key] = Item::Value(new),
    }
}

/// Reads `path`, changes it with `change`, and writes it back if what
/// results is valid; nothing is written if it isn't.
fn edit(
    path: &Path,
    change: impl FnOnce(&mut DocumentMut) -> Result<(), LimitsError>,
) -> Result<HostLimits, LimitsError> {
    let existed = path.exists();
    let mut doc = read(path)?;
    change(&mut doc)?;
    let limits = interpret(&doc).map_err(|message| invalid(path, message))?;
    limits.validate().map_err(LimitsError::Rejected)?;
    let mut text = doc.to_string();
    if !existed {
        text.insert_str(0, HEADER);
    }
    write_atomically(path, text.as_bytes())?;
    Ok(limits)
}

const HEADER: &str = "\
# VibeOS host limits. vibed edits this file too; a setting left out has its
# default. Sizes are like \"4G\" or \"512M\", durations like \"60s\" or \"5m\".
";

fn invalid(path: &Path, message: String) -> LimitsError {
    LimitsError::Invalid {
        path: path.to_owned(),
        message,
    }
}

fn read(path: &Path) -> Result<DocumentMut, LimitsError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(LimitsError::Io {
                what: "read",
                path: path.to_owned(),
                source,
            });
        }
    };
    text.parse::<DocumentMut>().map_err(|e| {
        let message = e.to_string();
        invalid(path, message.trim_end().to_owned())
    })
}

fn encode(kind: Kind, v: u64) -> Value {
    let int = |v: u64| match i64::try_from(v) {
        Ok(i) => Value::from(i),
        Err(_) => Value::from(v.to_string()),
    };
    match kind {
        Kind::Size => format_size(v).map(Value::from).unwrap_or_else(|| int(v)),
        Kind::Count => int(v),
        Kind::Duration => Value::from(format_duration(v)),
    }
}

/// Reads every setting in `doc` over the defaults.
fn interpret(doc: &DocumentMut) -> Result<HostLimits, String> {
    let mut limits = HostLimits::default();
    for (key, item) in doc.iter() {
        if key == PACKAGE_CACHES {
            limits.package_caches = item
                .as_value()
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("{key} must be true or false"))?;
            continue;
        }
        if key == MCP_TAILSCALE {
            limits.mcp_tailscale = item
                .as_value()
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("{key} must be true or false"))?;
            continue;
        }
        if key == MCP_PORT_KEY {
            limits.mcp_port = item
                .as_value()
                .and_then(Value::as_integer)
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("{key} must be a port, from 1 to 65535"))?;
            continue;
        }
        let kind = SETTINGS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, kind)| *kind)
            .ok_or_else(|| {
                let known: Vec<&str> = HostLimits::keys().chain(FILE_ONLY).collect();
                format!(
                    "unknown setting `{key}`; the settings are {}",
                    known.join(", ")
                )
            })?;
        let value = item
            .as_value()
            .ok_or_else(|| format!("{key} must be a single value"))?;
        let n = match (kind, value) {
            (_, Value::Integer(i)) => {
                u64::try_from(*i.value()).map_err(|_| format!("{key} must not be negative"))?
            }
            (Kind::Size, Value::String(s)) => {
                parse_size(s.value()).map_err(|e| format!("{key}: {e}"))?
            }
            (Kind::Duration, Value::String(s)) => {
                parse_duration(s.value()).map_err(|e| format!("{key}: {e}"))?
            }
            (Kind::Count, _) => return Err(format!("{key} must be a whole number")),
            (Kind::Size, _) => return Err(format!("{key} must be a size, like \"4G\"")),
            (Kind::Duration, _) => return Err(format!("{key} must be a duration, like \"60s\"")),
        };
        limits.set(key, n).map_err(|e| format!("{key}: {e}"))?;
    }
    Ok(limits)
}

/// Replaces `path` in one step, so a crash leaves the old file or the new
/// one, never half of either. An existing file keeps its permissions, and a
/// symlink (to a dotfiles repo, say) stays one: its target is replaced.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), LimitsError> {
    let is_link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let target = if is_link {
        link_target(path)
    } else {
        path.to_owned()
    };
    let path = target.as_path();
    let io_err = |what| {
        let path = path.to_owned();
        move |source| LimitsError::Io { what, path, source }
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_err("create the directory for"))?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = dir.join(name);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        if let Ok(meta) = fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(io_err("write"))
}

/// Where a symlink ends up, even if that file doesn't exist yet.
fn link_target(link: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(link) {
        return real;
    }
    match fs::read_link(link) {
        Ok(t) => link.parent().unwrap_or(Path::new(".")).join(t),
        Err(_) => link.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::MIB;

    fn file(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.toml");
        fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn defaults_match_the_prd() {
        let prd = r#"
reserve_memory   = "4G"    # always left for macOS and vibed
reserve_cores    = 2
max_awake_agents = 16
cpu_cores        = 4       # per agent
max_processes    = 512     # per macos agent
disk_soft        = "20G"   # per worktree
disk_hard        = "30G"
vm_disk          = "20G"   # a linux agent's own writable layer
idle_debounce    = "60s"
snapshot_disk    = "20G"   # total for all sleeping linux agents
package_caches   = true    # linux agents share npm, pnpm, cargo and pip caches
"#;
        let (_d, path) = file(prd);
        assert_eq!(load(&path).unwrap(), HostLimits::default());
        assert_eq!(HostLimits::keys().count(), 10);
    }

    #[test]
    fn missing_file_is_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&dir.path().join("none.toml")).unwrap(),
            HostLimits::default()
        );
    }

    #[test]
    fn values_override_defaults() {
        let (_d, path) =
            file("cpu_cores = 8\nreserve_memory = 6442450944\nidle_debounce = \"5m\"\n");
        let l = load(&path).unwrap();
        assert_eq!(l.cpu_cores, 8);
        assert_eq!(l.reserve_memory, 6 * GIB);
        assert_eq!(l.idle_debounce, 300);
        assert_eq!(l.max_awake_agents, 16);
        assert!(l.package_caches);
    }

    #[test]
    fn package_caches_are_opted_out_of_in_the_file_only() {
        let (_d, path) = file("package_caches = false\ncpu_cores = 2\n");
        let l = load(&path).unwrap();
        assert!(!l.package_caches);
        // An edit over the socket keeps it, and it isn't on the socket.
        let l = update(&path, &[("cpu_cores", 3)]).unwrap();
        assert!(!l.package_caches);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("package_caches = false")
        );
        let json = serde_json::to_value(l).unwrap();
        assert!(json.get("package_caches").is_none(), "{json}");
        let back: HostLimits = serde_json::from_value(json).unwrap();
        assert!(back.package_caches, "the socket's limits say nothing of it");
        assert!(update(&path, &[("package_caches", 1)]).is_err());
    }

    #[test]
    fn mcp_settings_are_in_the_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let l = load(&dir.path().join("host.toml")).unwrap();
        assert_eq!((l.mcp_port, l.mcp_tailscale), (MCP_PORT, false));
        let (_d, path) = file("mcp_port = 9000\nmcp_tailscale = true\ncpu_cores = 2\n");
        let l = load(&path).unwrap();
        assert_eq!((l.mcp_port, l.mcp_tailscale), (9000, true));
        // A limits edit keeps them, and they aren't among the limits.
        let l = update(&path, &[("cpu_cores", 3)]).unwrap();
        assert_eq!((l.mcp_port, l.mcp_tailscale), (9000, true));
        let json = serde_json::to_value(l).unwrap();
        assert!(json.get("mcp_port").is_none(), "{json}");
        assert!(json.get("mcp_tailscale").is_none(), "{json}");
        assert!(update(&path, &[("mcp_port", 1)]).is_err());
        for (text, expect) in [
            ("mcp_port = 0", "mcp_port must be a port"),
            ("mcp_port = 70000", "mcp_port must be a port"),
            ("mcp_port = \"7878\"", "mcp_port must be a port"),
            ("mcp_tailscale = 1", "mcp_tailscale must be true or false"),
        ] {
            let (_d, path) = file(text);
            let e = load(&path).unwrap_err().to_string();
            assert!(e.contains(expect), "{text}: {e}");
        }
        let (_d, path) = file("mcp_ports = 1");
        let e = load(&path).unwrap_err().to_string();
        assert!(e.contains("mcp_port, mcp_tailscale"), "{e}");
    }

    #[test]
    fn tailscale_is_set_in_the_file_keeping_the_rest() {
        let (_d, path) = file("cpu_cores = 2   # per agent\nmcp_tailscale = false # Settings\n");
        let l = set_mcp_tailscale(&path, true).unwrap();
        assert!(l.mcp_tailscale);
        assert_eq!(l.cpu_cores, 2);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("cpu_cores = 2   # per agent"), "{text}");
        assert!(text.contains("mcp_tailscale = true # Settings"), "{text}");
        assert!(load(&path).unwrap().mcp_tailscale);
        assert!(!set_mcp_tailscale(&path, false).unwrap().mcp_tailscale);
        // A new file gets the header, and only what was set.
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("host.toml");
        set_mcp_tailscale(&new, true).unwrap();
        let text = fs::read_to_string(&new).unwrap();
        assert!(text.starts_with("# VibeOS host limits."), "{text}");
        assert!(text.ends_with("mcp_tailscale = true\n"), "{text}");
        // A file that's broken already is left alone.
        let (_d, bad) = file("cpu_core = 4\n");
        assert!(matches!(
            set_mcp_tailscale(&bad, true),
            Err(LimitsError::Invalid { .. })
        ));
        assert_eq!(fs::read_to_string(&bad).unwrap(), "cpu_core = 4\n");
    }

    #[test]
    fn bad_files_say_what_is_wrong() {
        for (text, expect) in [
            ("cpu_core = 4", "unknown setting `cpu_core`"),
            ("cpu_cores = \"4\"", "cpu_cores must be a whole number"),
            ("cpu_cores = -1", "must not be negative"),
            ("cpu_cores = 99999999999", "too large"),
            ("vm_disk = \"lots\"", "vm_disk: `lots` isn't a size"),
            ("idle_debounce = 1.5", "must be a duration"),
            ("[disk]\nsoft = 1", "unknown setting `disk`"),
            ("[cpu_cores]\nx = 1", "cpu_cores must be a single value"),
            ("cpu_cores = ", "invalid"),
            ("package_caches = 1", "package_caches must be true or false"),
            (
                "max_awake_agents = 0",
                "max_awake_agents must be more than 0",
            ),
            (
                "disk_soft = \"40G\"",
                "disk_soft (40G) must not be more than disk_hard (30G)",
            ),
        ] {
            let (_d, path) = file(text);
            let err = load(&path).unwrap_err();
            assert!(matches!(err, LimitsError::Invalid { .. }), "{text}: {err}");
            assert!(err.to_string().contains(expect), "{text}: {err}");
            assert!(err.to_string().contains("host.toml"), "{err}");
        }
    }

    #[test]
    fn update_keeps_comments_and_other_settings() {
        let (_d, path) = file("# mine\ncpu_cores = 4 # per agent\n\nvm_disk = \"20G\"\n");
        let l = update(&path, &[("cpu_cores", 6), ("snapshot_disk", 40 * GIB)]).unwrap();
        assert_eq!((l.cpu_cores, l.snapshot_disk), (6, 40 * GIB));
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# mine\ncpu_cores = 6 # per agent\n\nvm_disk = \"20G\"\nsnapshot_disk = \"40G\"\n"
        );
        assert_eq!(load(&path).unwrap(), l);
    }

    #[test]
    fn update_creates_the_file_with_a_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vibeos/host.toml");
        update(
            &path,
            &[("idle_debounce", 0), ("reserve_memory", 1500 * MIB + 1)],
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# VibeOS host limits."), "{text}");
        assert!(
            text.ends_with("idle_debounce = \"0s\"\nreserve_memory = 1572864001\n"),
            "{text}"
        );
        let l = load(&path).unwrap();
        assert_eq!((l.idle_debounce, l.reserve_memory), (0, 1500 * MIB + 1));
    }

    #[test]
    fn rejected_updates_change_nothing() {
        let (_d, path) = file("disk_hard = \"30G\"\n");
        let err = update(&path, &[("disk_soft", 31 * GIB)]).unwrap_err();
        assert!(matches!(err, LimitsError::Rejected(_)), "{err}");
        let err = update(&path, &[("max_processes", 0)]).unwrap_err();
        assert!(matches!(err, LimitsError::Rejected(_)), "{err}");
        let err = update(&path, &[("cpu_cores", u64::from(u32::MAX) + 1)]).unwrap_err();
        assert!(matches!(err, LimitsError::Invalid { .. }), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "disk_hard = \"30G\"\n");
        // Raising both together is fine.
        update(&path, &[("disk_soft", 40 * GIB), ("disk_hard", 50 * GIB)]).unwrap();
    }

    #[test]
    fn update_can_fix_a_bad_value_but_not_bad_toml() {
        let (_d, path) = file("disk_soft = \"40G\"\n");
        assert!(load(&path).is_err());
        let l = update(&path, &[("disk_soft", 10 * GIB)]).unwrap();
        assert_eq!(l.disk_soft, 10 * GIB);

        let (_d, path) = file("cpu_cores = = 4\n");
        let err = update(&path, &[("cpu_cores", 2)]).unwrap_err();
        assert!(matches!(err, LimitsError::Invalid { .. }), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "cpu_cores = = 4\n");
    }

    #[test]
    fn sizes_and_durations_have_a_ceiling() {
        let (_d, path) = file("snapshot_disk = \"16000000T\"\n");
        let err = load(&path).unwrap_err();
        assert!(
            err.to_string()
                .contains("snapshot_disk must be at most 1024T"),
            "{err}"
        );
        let err = update(&path, &[("snapshot_disk", u64::MAX)]).unwrap_err();
        assert!(matches!(err, LimitsError::Rejected(_)), "{err}");
        let err = update(
            &path,
            &[("snapshot_disk", 20 * GIB), ("idle_debounce", 400 * 86400)],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("idle_debounce must be at most"),
            "{err}"
        );
        update(
            &path,
            &[
                ("snapshot_disk", 1024 * TIB),
                ("idle_debounce", 365 * 86400),
            ],
        )
        .unwrap();
    }

    #[test]
    fn a_symlinked_file_stays_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        fs::write(dotfiles.join("host.toml"), "cpu_cores = 4 # mine\n").unwrap();
        let path = dir.path().join("host.toml");
        std::os::unix::fs::symlink("dotfiles/host.toml", &path).unwrap();
        update(&path, &[("cpu_cores", 2)]).unwrap();
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(dotfiles.join("host.toml")).unwrap(),
            "cpu_cores = 2 # mine\n"
        );
        // A link to a file that doesn't exist yet creates the file.
        let dangling = dir.path().join("other.toml");
        std::os::unix::fs::symlink("dotfiles/new.toml", &dangling).unwrap();
        update(&dangling, &[("cpu_cores", 3)]).unwrap();
        assert!(
            fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(load(&dotfiles.join("new.toml")).unwrap().cpu_cores, 3);
    }

    #[test]
    fn update_keeps_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, path) = file("cpu_cores = 4\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        update(&path, &[("cpu_cores", 2)]).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["host.toml"]);
    }
}
