//! The registry's domain: hosts, projects and profiles, and the rules their
//! names and settings follow (PRD: Core concepts, Environments and
//! profiles).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::units::{GIB, MIB, TIB};

/// What every `vibed` calls itself. Other hosts get the name they were added
/// under (#16), such as `vps`, which is how `vps:quiet-amber-otter` reads.
pub const LOCAL_HOST: &str = "local";

/// Declares a closed set of names, as serialized on the socket and stored in
/// the registry, with the list `protocol/vibed-rpc.json` checks it against.
macro_rules! names {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $s)] $variant),+
        }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $s),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, String> {
                $name::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| format!("unknown {}: `{s}`", stringify!($name)))
            }
        }
    };
}

names! {
    /// Where an agent runs: a micro-VM (or a cgroup on a Linux host), or
    /// natively on the Mac.
    Environment { Linux = "linux", Macos = "macos" }
}

names! {
    /// Claude's `--model`.
    Model { Opus = "opus", Sonnet = "sonnet" }
}

names! {
    /// Claude's `--effort`.
    Reasoning { Low = "low", Medium = "medium", High = "high", Xhigh = "xhigh", Max = "max" }
}

names! {
    /// Claude's permission mode: `auto`, or `bypass`
    /// (`--dangerously-skip-permissions`).
    Mode { Auto = "auto", Bypass = "bypass" }
}

names! {
    /// How a project got onto its host.
    ProjectSource {
        /// Cloned by `owner/name` with the host's `gh` login.
        Github = "github",
        /// A local repo, registered where it is.
        Path = "path",
    }
}

names! {
    /// The result of the last `gh auth status`.
    GhState {
        /// Not checked yet.
        Unknown = "unknown",
        LoggedIn = "logged_in",
        LoggedOut = "logged_out",
        NotInstalled = "not_installed",
        /// The check itself failed, e.g. it timed out.
        Error = "error",
    }
}

names! {
    /// What an event on the stream is about.
    EventKind {
        ProjectAdded = "project.added",
        ProjectRemoved = "project.removed",
        /// A profile of `project` was created, changed or deleted.
        ProfilesChanged = "profiles.changed",
        LimitsChanged = "limits.changed",
        KeysChanged = "keys.changed",
        /// `gh auth status` changed; `state` is the new [`GhState`].
        GhStatus = "gh.status",
        /// An agent changed state; `state` is the new one (#4, #5).
        AgentState = "agent.state",
        /// An agent hit a limit; `limit` says which (#6, #11, #17).
        AgentLimit = "agent.limit",
        /// The subscriber fell behind and missed events: re-read everything.
        Lagged = "lagged",
    }
}

names! {
    /// Whether a session's shim is still holding its terminal.
    SessionState {
        /// Its shim is up, and clients can attach.
        Running = "running",
        /// Its command ended; the shim left a record of how.
        Exited = "exited",
    }
}

/// The default memory of a profile.
pub const DEFAULT_PROFILE_MEMORY: u64 = 4 * GIB;
pub const MIN_PROFILE_MEMORY: u64 = GIB;
pub const MAX_PROFILE_MEMORY: u64 = TIB;

/// A per-project preset for starting agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub environment: Environment,
    pub model: Model,
    pub reasoning: Reasoning,
    pub mode: Mode,
    /// The agent's high-water mark in bytes: a `linux` VM's RAM, or a
    /// `macos` agent's hard limit.
    pub memory: u64,
}

impl Profile {
    /// A profile with the defaults: Opus, high reasoning, `auto`, 4 GB.
    pub fn with_defaults(name: impl Into<String>, environment: Environment) -> Self {
        Profile {
            name: name.into(),
            environment,
            model: Model::Opus,
            reasoning: Reasoning::High,
            mode: Mode::Auto,
            memory: DEFAULT_PROFILE_MEMORY,
        }
    }

    /// Checks the profile can be used on a host running `host_os`.
    pub fn validate(&self, host_os: &str) -> Result<(), String> {
        validate_name("profile", &self.name)?;
        if self.environment == Environment::Macos && host_os != "macos" {
            return Err(format!(
                "`{}` uses the macos environment, which only a Mac host has",
                self.name
            ));
        }
        validate_memory(self.memory)
    }
}

/// The profiles a new project starts with: one per environment the host
/// has (PRD: Profiles).
pub fn starter_profiles(host_os: &str) -> Vec<Profile> {
    let mut profiles = vec![Profile::with_defaults(
        "software-eng-linux",
        Environment::Linux,
    )];
    if host_os == "macos" {
        profiles.push(Profile::with_defaults(
            "software-eng-mac",
            Environment::Macos,
        ));
    }
    profiles
}

pub fn validate_memory(bytes: u64) -> Result<(), String> {
    if !bytes.is_multiple_of(MIB) {
        return Err(format!(
            "memory must be a whole number of MiB, not {bytes} bytes"
        ));
    }
    if !(MIN_PROFILE_MEMORY..=MAX_PROFILE_MEMORY).contains(&bytes) {
        return Err(format!(
            "memory must be between 1G and 1T, not {} MiB",
            bytes / MIB
        ));
    }
    Ok(())
}

/// Names of projects, profiles and API keys: up to 100 letters, digits, `.`,
/// `_` and `-`, starting with a letter or digit. Project names become
/// directory names, so nothing else is allowed.
pub fn validate_name(what: &str, name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` isn't a valid {what} name: use up to 100 letters, digits, `.`, `_` and `-`, starting with a letter or digit"
        ))
    }
}

/// The longest ref. A ref names a socket, `run/<ref>.sock`, whose path must
/// fit in 103 bytes on macOS. That leaves 26 for the home directory, such as
/// `/Users/` and a name of up to 19 letters.
pub const MAX_REF_LEN: usize = 32;

/// Names `run/` uses for itself, so no session may take them.
const RESERVED_REFS: [&str; 1] = ["vibed"];

/// An agent's ref, which also names its worktree, branch and session: like
/// a name (see [`validate_name`]) but at most [`MAX_REF_LEN`] characters,
/// and not `vibed`, whose socket is in the same directory.
pub fn validate_ref(r: &str) -> Result<(), String> {
    let ok = !r.is_empty()
        && r.len() <= MAX_REF_LEN
        && r.starts_with(|c: char| c.is_ascii_alphanumeric())
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(format!(
            "`{r}` isn't a valid ref: use up to {MAX_REF_LEN} letters, digits, `.`, `_` and `-`, starting with a letter or digit"
        ));
    }
    if RESERVED_REFS.contains(&r) {
        return Err(format!("`{r}` is reserved; pick another ref"));
    }
    Ok(())
}

/// API key names are only shown, never used as paths: anything printable up
/// to 100 characters, such as "laptop Claude Code".
pub fn validate_key_name(name: &str) -> Result<(), String> {
    let ok = !name.trim().is_empty()
        && name.trim() == name
        && name.chars().count() <= 100
        && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` isn't a valid key name: use up to 100 printable characters, not starting or ending with a space"
        ))
    }
}

/// A GitHub repo as `owner/name`. A trailing `.git` is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepo {
    pub owner: String,
    pub name: String,
}

impl GithubRepo {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("`{s}` isn't a GitHub repo; write it as owner/name");
        let (owner, name) = s.trim().split_once('/').ok_or_else(bad)?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        let owner_ok = !owner.is_empty()
            && owner.len() <= 39
            && !owner.starts_with('-')
            && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        // GitHub's rule for repo names, which unlike project names may start
        // with a dot, as in `owner/.github`.
        let name_ok = !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !owner_ok || !name_ok {
            return Err(bad());
        }
        Ok(GithubRepo {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// Whether a git remote URL points at this repo on github.com, over
    /// HTTPS or SSH, with or without `.git`. GitHub names are
    /// case-insensitive.
    pub fn matches_remote(&self, url: &str) -> bool {
        let url = url.trim().trim_end_matches('/');
        let url = url.strip_suffix(".git").unwrap_or(url);
        let path = [
            "https://github.com/",
            "git@github.com:",
            "ssh://git@github.com/",
        ]
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix));
        path.is_some_and(|p| p.eq_ignore_ascii_case(&self.to_string()))
    }
}

impl fmt::Display for GithubRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        assert_eq!(validate_ref("quiet-amber-otter"), Ok(()));
        assert_eq!(validate_ref(&"a".repeat(MAX_REF_LEN)), Ok(()));
        for bad in ["", "-x", "a/b", "a b", "vibed", "..", ".hidden"] {
            assert!(validate_ref(bad).is_err(), "{bad:?}");
        }
        let long = validate_ref(&"a".repeat(MAX_REF_LEN + 1)).unwrap_err();
        assert!(long.contains("up to 32"), "{long}");
        assert_eq!(SessionState::Exited.as_str(), "exited");
    }

    #[test]
    fn names_round_trip() {
        for e in Reasoning::ALL {
            assert_eq!(e.as_str().parse::<Reasoning>(), Ok(*e));
            assert_eq!(serde_json::to_value(e).unwrap(), e.as_str());
        }
        assert_eq!(EventKind::AgentState.to_string(), "agent.state");
        assert!("Linux".parse::<Environment>().is_err());
        assert!(serde_json::from_str::<Mode>("\"plan\"").is_err());
    }

    #[test]
    fn starters_per_host() {
        let mac = starter_profiles("macos");
        assert_eq!(
            mac.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["software-eng-linux", "software-eng-mac"]
        );
        assert_eq!(mac[1].environment, Environment::Macos);
        for p in &mac {
            assert_eq!(
                (p.model, p.reasoning, p.mode, p.memory),
                (Model::Opus, Reasoning::High, Mode::Auto, 4 * GIB)
            );
            assert_eq!(p.validate("macos"), Ok(()));
        }
        let linux = starter_profiles("linux");
        assert_eq!(linux.len(), 1);
        assert_eq!(linux[0].environment, Environment::Linux);
    }

    #[test]
    fn macos_profiles_need_a_mac() {
        let p = Profile::with_defaults("m", Environment::Macos);
        assert!(p.validate("linux").unwrap_err().contains("only a Mac"));
    }

    #[test]
    fn memory_bounds() {
        assert_eq!(validate_memory(GIB), Ok(()));
        assert_eq!(validate_memory(TIB), Ok(()));
        assert_eq!(validate_memory(1536 * MIB), Ok(()));
        assert!(validate_memory(GIB - MIB).is_err());
        assert!(validate_memory(TIB + MIB).is_err());
        assert!(validate_memory(GIB + 1).is_err());
    }

    #[test]
    fn names() {
        for ok in ["a", "web", "Vibe.OS_2-x", &"a".repeat(100)] {
            assert_eq!(validate_name("project", ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "-x",
            "a/b",
            "a b",
            "é",
            &"a".repeat(101),
        ] {
            assert!(validate_name("project", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_names() {
        for ok in [
            "laptop",
            "laptop Claude Code",
            "CI (staging) ✓",
            &"k".repeat(100),
        ] {
            assert_eq!(validate_key_name(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            " ",
            " laptop",
            "laptop ",
            "a\nb",
            "a\tb",
            &"k".repeat(101),
        ] {
            assert!(validate_key_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn github_repos() {
        let r = GithubRepo::parse("tsconfigdotjson/vibe-os-native.git").unwrap();
        assert_eq!(r.to_string(), "tsconfigdotjson/vibe-os-native");
        assert_eq!(GithubRepo::parse("me/.github").unwrap().name, ".github");
        for bad in [
            "", "x", "/x", "x/", "a/b/c", "a b/c", "-a/b", "a/.git", "a/..", "a/.", "a/b c",
        ] {
            assert!(GithubRepo::parse(bad).is_err(), "{bad}");
        }
        for url in [
            "https://github.com/Me/Web.git",
            "https://github.com/me/web",
            "git@github.com:me/web.git",
            "ssh://git@github.com/me/web/",
        ] {
            assert!(
                GithubRepo::parse("me/web").unwrap().matches_remote(url),
                "{url}"
            );
        }
        for url in [
            "https://github.com/me/web2",
            "https://gitlab.com/me/web",
            "/tmp/web",
        ] {
            assert!(
                !GithubRepo::parse("me/web").unwrap().matches_remote(url),
                "{url}"
            );
        }
    }
}
