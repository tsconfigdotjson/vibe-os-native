//! `vibe project`, `vibe profile` and `vibe host`: host setup without the
//! menu bar (PRD: Architecture, `vibe` CLI). On a standalone Linux host they
//! replace Settings → Projects; on a Mac, `vibe host` adds Linux hosts as
//! Add Linux host… does, and `--host` sends the others to one of them.

use std::path::Path;
use std::time::Duration;

use vibe_core::model::{AgentKind, Environment, Mode, Model, PiTarget, Reasoning};
use vibe_core::rpc::{
    HostAddParams, HostInfo, HostInstall, HostLink, HostNameParams, HostParams, HostsAdd,
    HostsList, HostsRemove, HostsUpgrade, NoParams, PiForget, PiForgetParams, PiInstall,
    PiInstallParams, PiStatus, PiStatusGet, Profile, ProfileCreateParams, ProfileParams,
    ProfileUpdateParams, ProfilesCreate, ProfilesDelete, ProfilesList, ProfilesUpdate, Project,
    ProjectAddParams, ProjectParams, ProjectRemoveParams, ProjectsAdd, ProjectsList,
    ProjectsRemove,
};
use vibe_core::units::{format_size, parse_size};

use crate::client;

/// For quick calls.
const TIMEOUT: Duration = Duration::from_secs(60);

/// For calls that clone a repo or install over SSH, which can take minutes.
const LONG_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// A profile's settings, from flags; `None` for one not given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) environment: Option<Environment>,
    pub(crate) model: Option<Model>,
    pub(crate) reasoning: Option<Reasoning>,
    pub(crate) mode: Option<Mode>,
    pub(crate) memory: Option<u64>,
    pub(crate) shared_services: Option<bool>,
    pub(crate) agent: Option<AgentKind>,
    pub(crate) pi_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Pi {
        host: Option<String>,
    },
    ForgetPi {
        host: Option<String>,
        providers: Vec<String>,
    },
    InstallPi {
        host: Option<String>,
        target: PiTarget,
    },
    Projects {
        host: Option<String>,
    },
    AddProject {
        host: Option<String>,
        /// `owner/name`, or a path.
        what: String,
        name: Option<String>,
    },
    RemoveProject {
        host: Option<String>,
        project: String,
        /// On a Linux host, delete the clone too (`--delete`).
        delete: bool,
    },
    Profiles {
        host: Option<String>,
        project: String,
    },
    CreateProfile {
        host: Option<String>,
        project: String,
        name: String,
        settings: Settings,
    },
    UpdateProfile {
        host: Option<String>,
        project: String,
        profile: String,
        rename: Option<String>,
        settings: Settings,
    },
    DeleteProfile {
        host: Option<String>,
        project: String,
        profile: String,
    },
    Hosts,
    AddHost {
        name: String,
        address: String,
    },
    UpgradeHost(String),
    RemoveHost(String),
}

/// Words, and `--flag value` pairs, from a command line.
type Split<'a> = (Vec<&'a str>, Vec<(&'a str, &'a str)>);

/// Splits `--flag value` pairs from the other words.
fn flags<'a>(words: &'a [String], known: &[&str]) -> Result<Split<'a>, String> {
    let mut plain = Vec::new();
    let mut found = Vec::new();
    let mut iter = words.iter();
    while let Some(w) = iter.next() {
        if let Some(flag) = w.strip_prefix("--") {
            let (flag, value) = match flag.split_once('=') {
                Some((f, v)) => (f, v),
                None => (
                    flag,
                    iter.next()
                        .map(String::as_str)
                        .ok_or_else(|| format!("--{flag} needs a value"))?,
                ),
            };
            if !known.contains(&flag) {
                return Err(format!("unexpected flag --{flag}"));
            }
            found.push((flag, value));
        } else {
            plain.push(w.as_str());
        }
    }
    Ok((plain, found))
}

fn value<'a>(found: &[(&str, &'a str)], flag: &str) -> Option<&'a str> {
    found
        .iter()
        .rev()
        .find(|(f, _)| *f == flag)
        .map(|(_, v)| *v)
}

fn settings(found: &[(&str, &str)]) -> Result<Settings, String> {
    let parse = |flag: &str| value(found, flag).map(str::to_owned);
    Ok(Settings {
        environment: parse("environment").map(|v| v.parse()).transpose()?,
        model: parse("model").map(|v| v.parse()).transpose()?,
        reasoning: parse("reasoning").map(|v| v.parse()).transpose()?,
        mode: parse("mode").map(|v| v.parse()).transpose()?,
        memory: parse("memory").map(|v| parse_size(&v)).transpose()?,
        shared_services: parse("shared-services")
            .map(|v| match v.as_str() {
                "on" => Ok(true),
                "off" => Ok(false),
                other => Err(format!("--shared-services is on or off, not {other}")),
            })
            .transpose()?,
        agent: parse("agent").map(|v| v.parse()).transpose()?,
        pi_model: parse("pi-model"),
    })
}

/// The words `vibe profile` takes first.
const SUBCOMMANDS: [&str; 8] = [
    "list", "ls", "create", "add", "set", "update", "delete", "rm",
];

const PROFILE_FLAGS: [&str; 9] = [
    "host",
    "environment",
    "model",
    "reasoning",
    "mode",
    "memory",
    "shared-services",
    "agent",
    "pi-model",
];

impl Command {
    /// From the words after `project`.
    pub(crate) fn parse_project(words: &[String]) -> Result<Command, String> {
        // `--delete` takes no value.
        let delete = words.iter().any(|w| w == "--delete");
        let words: Vec<String> = words.iter().filter(|w| *w != "--delete").cloned().collect();
        let (plain, found) = flags(&words, &["host", "name"])?;
        if delete && !matches!(plain.first(), Some(&("remove" | "rm"))) {
            return Err("--delete goes with project remove".into());
        }
        let host = value(&found, "host").map(str::to_owned);
        match plain.as_slice() {
            [] | ["list" | "ls"] => Ok(Command::Projects { host }),
            ["add", what] => Ok(Command::AddProject {
                host,
                what: (*what).to_owned(),
                name: value(&found, "name").map(str::to_owned),
            }),
            ["remove" | "rm", project] => Ok(Command::RemoveProject {
                host,
                project: (*project).to_owned(),
                delete,
            }),
            ["add"] => Err("project add needs a GitHub owner/name or a path".into()),
            ["remove" | "rm"] => Err("project remove needs a project's name".into()),
            _ => Err(
                "project takes list, add OWNER/NAME|PATH [--name NAME] or remove NAME [--delete]"
                    .into(),
            ),
        }
    }

    /// From the words after `profile`.
    pub(crate) fn parse_profile(words: &[String]) -> Result<Command, String> {
        let mut known = PROFILE_FLAGS.to_vec();
        known.push("name");
        let (plain, found) = flags(words, &known)?;
        let host = value(&found, "host").map(str::to_owned);
        let s = settings(&found)?;
        let no_settings = |what: &str| -> Result<(), String> {
            if s != Settings::default() || value(&found, "name").is_some() {
                Err(format!("profile {what} takes no settings"))
            } else {
                Ok(())
            }
        };
        match plain.as_slice() {
            ["list" | "ls", project] => {
                no_settings("list")?;
                Ok(Command::Profiles {
                    host,
                    project: (*project).to_owned(),
                })
            }
            [project] if !SUBCOMMANDS.contains(project) => {
                no_settings("list")?;
                Ok(Command::Profiles {
                    host,
                    project: (*project).to_owned(),
                })
            }
            ["create" | "add", project, name] => {
                if value(&found, "name").is_some() {
                    return Err("profile create takes the name as a word, not --name".into());
                }
                Ok(Command::CreateProfile {
                    host,
                    project: (*project).to_owned(),
                    name: (*name).to_owned(),
                    settings: s,
                })
            }
            ["set" | "update", project, profile] => {
                let rename = value(&found, "name").map(str::to_owned);
                if s == Settings::default() && rename.is_none() {
                    return Err(
                        "profile set needs something to change, such as --model sonnet".into(),
                    );
                }
                Ok(Command::UpdateProfile {
                    host,
                    project: (*project).to_owned(),
                    profile: (*profile).to_owned(),
                    rename,
                    settings: s,
                })
            }
            ["delete" | "rm", project, profile] => {
                no_settings("delete")?;
                Ok(Command::DeleteProfile {
                    host,
                    project: (*project).to_owned(),
                    profile: (*profile).to_owned(),
                })
            }
            _ => Err(
                "profile takes list PROJECT, create PROJECT NAME [settings], set PROJECT PROFILE \
                      [--name NEW] [settings] or delete PROJECT PROFILE"
                    .into(),
            ),
        }
    }

    /// From the words after `pi`.
    pub(crate) fn parse_pi(words: &[String]) -> Result<Command, String> {
        // `--vm` takes no value.
        let vm = words.iter().any(|w| w == "--vm");
        let words: Vec<String> = words.iter().filter(|w| *w != "--vm").cloned().collect();
        let (plain, found) = flags(&words, &["host"])?;
        let host = value(&found, "host").map(str::to_owned);
        match plain.as_slice() {
            [] | ["status"] if !vm => Ok(Command::Pi { host }),
            ["forget", providers @ ..] if !providers.is_empty() => Ok(Command::ForgetPi {
                host,
                providers: providers.iter().map(|p| (*p).to_owned()).collect(),
            }),
            ["install"] => Ok(Command::InstallPi {
                host,
                target: if vm { PiTarget::Vm } else { PiTarget::Native },
            }),
            _ => Err("pi takes status, install [--vm], or forget PROVIDER...".into()),
        }
    }

    /// From the words after `host`.
    pub(crate) fn parse_host(words: &[String]) -> Result<Command, String> {
        let (plain, _) = flags(words, &[])?;
        match plain.as_slice() {
            [] | ["list" | "ls"] => Ok(Command::Hosts),
            ["add", name, address] => Ok(Command::AddHost {
                name: (*name).to_owned(),
                address: (*address).to_owned(),
            }),
            ["upgrade", name] => Ok(Command::UpgradeHost((*name).to_owned())),
            ["remove" | "rm", name] => Ok(Command::RemoveHost((*name).to_owned())),
            _ => Err(
                "host takes list, add NAME SSH-DESTINATION, upgrade NAME, or remove NAME".into(),
            ),
        }
    }
}

/// A project and its profiles, as lines.
/// What `project remove` did, as it was: the clone deleted, or left.
pub(crate) fn describe_removal(p: &Project) -> String {
    if p.deleted {
        format!("removed {} and deleted its clone at {}", p.name, p.path)
    } else {
        format!("removed {}; {} is left as it was", p.name, p.path)
    }
}

pub(crate) fn describe_project(p: &Project) -> String {
    let mut out = format!("{}  {}", p.name, p.path);
    if let Some(repo) = &p.repo {
        out.push_str(&format!("  ({repo})"));
    }
    if p.host != vibe_core::model::LOCAL_HOST {
        out.push_str(&format!("  on {}", p.host));
    }
    out.push('\n');
    for profile in &p.profiles {
        out.push_str(&format!("  {}\n", describe_profile(profile)));
    }
    out
}

/// What's installed and signed in to, a line each. Only providers' names: a
/// login's contents are Pi's own.
pub(crate) fn describe_pi(p: &PiStatus) -> String {
    let logins = |l: &[String]| {
        if l.is_empty() {
            "not signed in".to_owned()
        } else {
            format!("signed in to {}", l.join(", "))
        }
    };
    let native = match &p.native {
        Some(n) => format!("Pi {} at {}", n.version, n.path),
        None => match &p.error {
            Some(why) => format!("Pi isn't installed yet: {why}"),
            None => "Pi isn't installed (VibeOS installs it once a profile runs it)".to_owned(),
        },
    };
    let mut out = format!("{native}, {}", logins(&p.logins));
    if let Some(vms) = &p.vms {
        let layer = match &vms.version {
            Some(v) => format!("Pi {v}"),
            None => "Pi isn't downloaded (VibeOS fetches it once a profile runs it)".to_owned(),
        };
        out.push_str(&format!(
            "\nlinux agents: {layer}, {}",
            match logins(&vms.logins) {
                l if vms.logins.is_empty() => {
                    format!("{l}. {}", vibe_core::model::PI_TOOLBOX_SIGN_IN)
                }
                l => l,
            }
        ));
        // What a daemon of any version says goes to this terminal only as text.
        for alert in &vms.alerts {
            let alert = vibe_core::model::clean_alert(alert);
            out.push_str(&format!("\n! {alert}"));
        }
        for o in &vms.offered {
            let (provider, agent) = (
                vibe_core::model::clean_alert(&o.provider),
                vibe_core::model::clean_alert(&o.agent),
            );
            out.push_str(&format!(
                "\n{provider}'s login came from {agent}, not you: `vibe pi forget {provider}`, \
                 and stop that agent if it wasn't you"
            ));
        }
    }
    out
}

pub(crate) fn describe_profile(p: &Profile) -> String {
    if p.agent == AgentKind::Pi {
        return format!(
            "{}  pi, {}, {}, {} thinking, no approvals, {}",
            p.name,
            p.environment,
            p.pi_model.as_deref().unwrap_or("Pi's default model"),
            p.reasoning,
            format_size(p.memory).unwrap_or_else(|| format!("{} bytes", p.memory))
        ) + if p.shared_services {
            ""
        } else {
            ", shared services off"
        };
    }
    format!(
        "{}  {}, {}, {} reasoning, {} mode, {}",
        p.name,
        p.environment,
        p.model,
        p.reasoning,
        p.mode,
        format_size(p.memory).unwrap_or_else(|| format!("{} bytes", p.memory))
    ) + if p.shared_services {
        ""
    } else {
        ", shared services off"
    }
}

pub(crate) fn describe_host(h: &HostInfo) -> String {
    let mut out = format!("{}  {} ({})", h.name, h.label, h.os);
    match h.link {
        HostLink::Local => out.push_str("  this host"),
        HostLink::Connected => out.push_str("  connected"),
        HostLink::Down => {
            out.push_str("  down");
            if let Some(why) = &h.detail {
                out.push_str(&format!(": {why}"));
            }
        }
    }
    if let Some(address) = &h.address {
        out.push_str(&format!("  ssh {address}"));
    }
    if let Some(v) = &h.version {
        out.push_str(&format!("  vibed {v}"));
    }
    if !h.missing.is_empty() {
        out.push_str(&format!("  missing {}", h.missing.join(", ")));
    }
    if !h.needs_admin.is_empty() {
        out.push_str("  needs sudo there once: Settings → Hosts has the button");
    }
    if let Some(why) = &h.upgrade_error {
        out.push_str(&format!("  upgrading it failed: {why}"));
    }
    out
}

fn describe_install(i: &HostInstall) -> String {
    let h = &i.host;
    let mut out = format!(
        "{} ({}) runs vibed {}{}.\n",
        h.name,
        h.label,
        h.version.as_deref().unwrap_or("?"),
        if i.changed {
            ", just installed"
        } else {
            ", as it did"
        }
    );
    if !h.missing.is_empty() {
        out.push_str(&format!(
            "Still needed there for its agents: {}. VibeOS installs them by itself, and \
             Settings → Hosts signs in with one click.\n",
            h.missing.join(", ")
        ));
    }
    for note in &i.notes {
        out.push_str(note);
        out.push('\n');
    }
    out
}

/// A path as vibed takes it: absolute, as vibed's working directory isn't
/// this one's.
fn absolute(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("can't tell where {path} is: {e}"))?
            .join(p)
    };
    Ok(abs.to_string_lossy().into_owned())
}

/// Whether `what` names a local checkout rather than a GitHub repo.
fn is_path(what: &str) -> bool {
    what.starts_with(['/', '.', '~']) || Path::new(what).is_dir()
}

pub(crate) fn run(socket: &Path, command: Command) -> Result<(), String> {
    let e = |e: client::ClientError| e.to_string();
    match command {
        Command::Projects { host } => {
            let list =
                client::call::<ProjectsList>(socket, &HostParams { host }, TIMEOUT).map_err(e)?;
            if list.projects.is_empty() {
                println!("no projects; add one with vibe project add OWNER/NAME or a path");
            }
            for p in &list.projects {
                print!("{}", describe_project(p));
            }
        }
        Command::AddProject { host, what, name } => {
            let params = if is_path(&what) {
                let remote = host
                    .as_deref()
                    .is_some_and(|h| h != vibe_core::model::LOCAL_HOST);
                if remote && !what.starts_with('/') {
                    return Err(format!(
                        "{what} is a path here; give the path on {} in full, from /",
                        host.as_deref().unwrap_or_default()
                    ));
                }
                let path = match what.strip_prefix("~/") {
                    Some(rest) => std::env::var("HOME")
                        .map(|h| format!("{h}/{rest}"))
                        .map_err(|_| "HOME isn't set".to_owned())?,
                    None => absolute(&what)?,
                };
                ProjectAddParams {
                    host,
                    repo: None,
                    path: Some(path),
                    name,
                }
            } else {
                ProjectAddParams {
                    host,
                    repo: Some(what),
                    path: None,
                    name,
                }
            };
            let p = client::call::<ProjectsAdd>(socket, &params, LONG_TIMEOUT).map_err(e)?;
            print!("added {}", describe_project(&p));
        }
        Command::RemoveProject {
            host,
            project,
            delete,
        } => {
            let params = ProjectRemoveParams {
                host,
                project,
                delete,
            };
            // Deleting a big clone takes a while.
            let p = client::call::<ProjectsRemove>(socket, &params, LONG_TIMEOUT).map_err(e)?;
            println!("{}", describe_removal(&p));
        }
        Command::Profiles { host, project } => {
            let list =
                client::call::<ProfilesList>(socket, &ProjectParams { host, project }, TIMEOUT)
                    .map_err(e)?;
            for p in &list.profiles {
                println!("{}", describe_profile(p));
            }
        }
        Command::Pi { host } => {
            let status =
                client::call::<PiStatusGet>(socket, &HostParams { host }, TIMEOUT).map_err(e)?;
            println!("{}", describe_pi(&status));
        }
        Command::ForgetPi { host, providers } => {
            let status =
                client::call::<PiForget>(socket, &PiForgetParams { host, providers }, TIMEOUT)
                    .map_err(e)?;
            println!("{}", describe_pi(&status));
        }
        Command::InstallPi { host, target } => {
            println!("Downloading Pi…");
            let status =
                client::call::<PiInstall>(socket, &PiInstallParams { host, target }, LONG_TIMEOUT)
                    .map_err(e)?;
            println!("{}", describe_pi(&status));
        }
        Command::CreateProfile {
            host,
            project,
            name,
            settings: s,
        } => {
            let params = ProfileCreateParams {
                host,
                project,
                name,
                environment: s.environment,
                model: s.model,
                reasoning: s.reasoning,
                mode: s.mode,
                memory: s.memory,
                shared_services: s.shared_services,
                agent: s.agent,
                pi_model: s.pi_model,
            };
            let p = client::call::<ProfilesCreate>(socket, &params, TIMEOUT).map_err(e)?;
            println!("created {}", describe_profile(&p));
        }
        Command::UpdateProfile {
            host,
            project,
            profile,
            rename,
            settings: s,
        } => {
            let params = ProfileUpdateParams {
                host,
                project,
                profile,
                name: rename,
                environment: s.environment,
                model: s.model,
                reasoning: s.reasoning,
                mode: s.mode,
                memory: s.memory,
                shared_services: s.shared_services,
                agent: s.agent,
                pi_model: s.pi_model,
            };
            let p = client::call::<ProfilesUpdate>(socket, &params, TIMEOUT).map_err(e)?;
            println!("{}", describe_profile(&p));
        }
        Command::DeleteProfile {
            host,
            project,
            profile,
        } => {
            let p = client::call::<ProfilesDelete>(
                socket,
                &ProfileParams {
                    host,
                    project,
                    profile,
                },
                TIMEOUT,
            )
            .map_err(e)?;
            println!("deleted {}", p.name);
        }
        Command::Hosts => {
            let list = client::call::<HostsList>(socket, &NoParams {}, TIMEOUT).map_err(e)?;
            for h in &list.hosts {
                println!("{}", describe_host(h));
            }
        }
        Command::AddHost { name, address } => {
            eprintln!("Installing vibed on {address} over SSH…");
            let i =
                client::call::<HostsAdd>(socket, &HostAddParams { name, address }, LONG_TIMEOUT)
                    .map_err(e)?;
            print!("{}", describe_install(&i));
        }
        Command::UpgradeHost(host) => {
            let i = client::call::<HostsUpgrade>(socket, &HostNameParams { host }, LONG_TIMEOUT)
                .map_err(e)?;
            print!("{}", describe_install(&i));
        }
        Command::RemoveHost(host) => {
            let h = client::call::<HostsRemove>(socket, &HostNameParams { host }, TIMEOUT)
                .map_err(e)?;
            println!(
                "removed {}; vibed and its agents run on there, on their own",
                h.name
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn projects() {
        assert_eq!(
            Command::parse_project(&words(&[])),
            Ok(Command::Projects { host: None })
        );
        assert_eq!(
            Command::parse_project(&words(&["ls", "--host", "vps"])),
            Ok(Command::Projects {
                host: Some("vps".into())
            })
        );
        assert_eq!(
            Command::parse_project(&words(&["add", "me/web", "--name=site"])),
            Ok(Command::AddProject {
                host: None,
                what: "me/web".into(),
                name: Some("site".into())
            })
        );
        assert_eq!(
            Command::parse_project(&words(&["remove", "web"])),
            Ok(Command::RemoveProject {
                host: None,
                project: "web".into(),
                delete: false
            })
        );
        assert_eq!(
            Command::parse_project(&words(&["rm", "--delete", "web", "--host", "vps"])),
            Ok(Command::RemoveProject {
                host: Some("vps".into()),
                project: "web".into(),
                delete: true
            })
        );
        assert!(Command::parse_project(&words(&["add", "me/web", "--delete"])).is_err());
        assert!(Command::parse_project(&words(&["add"])).is_err());
        assert!(Command::parse_project(&words(&["add", "a", "b"])).is_err());
        assert!(Command::parse_project(&words(&["add", "a", "--nmae", "b"])).is_err());
        assert!(Command::parse_project(&words(&["add", "a", "--name"])).is_err());
        assert!(is_path("./app") && is_path("/src/app") && is_path("~/app"));
        assert!(!is_path("me/no-such-dir-here"));
    }

    #[test]
    fn profiles() {
        assert_eq!(
            Command::parse_profile(&words(&["list", "web"])),
            Ok(Command::Profiles {
                host: None,
                project: "web".into()
            })
        );
        assert_eq!(
            Command::parse_profile(&words(&["web"])),
            Ok(Command::Profiles {
                host: None,
                project: "web".into()
            })
        );
        assert_eq!(
            Command::parse_profile(&words(&[
                "create", "web", "reviewer", "--model", "sonnet", "--memory", "8G", "--mode",
                "bypass"
            ])),
            Ok(Command::CreateProfile {
                host: None,
                project: "web".into(),
                name: "reviewer".into(),
                settings: Settings {
                    model: Some(Model::Sonnet),
                    memory: Some(8 << 30),
                    mode: Some(Mode::Bypass),
                    ..Default::default()
                }
            })
        );
        assert_eq!(
            Command::parse_profile(&words(&[
                "set",
                "web",
                "reviewer",
                "--name",
                "critic",
                "--reasoning",
                "max",
                "--host",
                "vps"
            ])),
            Ok(Command::UpdateProfile {
                host: Some("vps".into()),
                project: "web".into(),
                profile: "reviewer".into(),
                rename: Some("critic".into()),
                settings: Settings {
                    reasoning: Some(Reasoning::Max),
                    ..Default::default()
                }
            })
        );
        assert_eq!(
            Command::parse_profile(&words(&["delete", "web", "critic"])),
            Ok(Command::DeleteProfile {
                host: None,
                project: "web".into(),
                profile: "critic".into()
            })
        );
        for bad in [
            &["set", "web", "reviewer"][..],
            &["create", "web", "x", "--model", "gpt"],
            &["create", "web", "x", "--memory", "lots"],
            &["create", "web", "x", "--name", "y"],
            &["delete", "web", "x", "--model", "opus"],
            &["list"],
            &["frobnicate", "a", "b"],
        ] {
            assert!(Command::parse_profile(&words(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hosts() {
        assert_eq!(Command::parse_host(&words(&[])), Ok(Command::Hosts));
        assert_eq!(
            Command::parse_host(&words(&["add", "vps", "me@vps.example.com"])),
            Ok(Command::AddHost {
                name: "vps".into(),
                address: "me@vps.example.com".into()
            })
        );
        assert_eq!(
            Command::parse_host(&words(&["upgrade", "vps"])),
            Ok(Command::UpgradeHost("vps".into()))
        );
        assert_eq!(
            Command::parse_host(&words(&["rm", "vps"])),
            Ok(Command::RemoveHost("vps".into()))
        );
        assert!(Command::parse_host(&words(&["add", "vps"])).is_err());
    }

    #[test]
    fn lines() {
        let p = Project {
            host: "vps".into(),
            name: "web".into(),
            source: vibe_core::model::ProjectSource::Github,
            repo: Some("me/web".into()),
            path: "/home/me/VibeOS/projects/web".into(),
            added_at: "t".into(),
            profiles: vibe_core::model::starter_profiles("linux"),
            deleted: false,
        };
        assert_eq!(
            describe_removal(&p),
            "removed web; /home/me/VibeOS/projects/web is left as it was"
        );
        let gone = Project {
            deleted: true,
            ..p.clone()
        };
        assert_eq!(
            describe_removal(&gone),
            "removed web and deleted its clone at /home/me/VibeOS/projects/web"
        );
        assert_eq!(
            describe_project(&p),
            "web  /home/me/VibeOS/projects/web  (me/web)  on vps\n  \
             software-eng-linux  linux, opus, high reasoning, auto mode, 4G\n"
        );
        let h = HostInfo {
            name: "vps".into(),
            os: "linux".into(),
            label: "vps-1".into(),
            link: HostLink::Down,
            external: false,
            mcp: false,
            address: Some("me@vps".into()),
            version: Some("0.1.0".into()),
            missing: vec!["claude".into()],
            detail: Some("it stopped answering".into()),
            memory: None,
            needs_admin: vec![],
            upgrade_error: None,
            sign_ins: vec![],
        };
        assert_eq!(
            describe_host(&h),
            "vps  vps-1 (linux)  down: it stopped answering  ssh me@vps  vibed 0.1.0  missing claude"
        );
        // A retired command is an error, not a way back in.
        assert!(Command::parse_host(&words(&["external", "vps", "vps.example.com"])).is_err());
        assert!(Command::parse_host(&words(&["external", "vps", "off"])).is_err());
    }

    #[test]
    fn pi_commands() {
        assert_eq!(
            Command::parse_pi(&words(&["forget", "junk-a", "junk-b"])),
            Ok(Command::ForgetPi {
                host: None,
                providers: vec!["junk-a".into(), "junk-b".into()]
            })
        );
        assert!(Command::parse_pi(&words(&["forget"])).is_err());
        assert_eq!(
            Command::parse_pi(&words(&[])),
            Ok(Command::Pi { host: None })
        );
        assert_eq!(
            Command::parse_pi(&words(&["status", "--host", "vps"])),
            Ok(Command::Pi {
                host: Some("vps".into())
            })
        );
        assert_eq!(
            Command::parse_pi(&words(&["install"])),
            Ok(Command::InstallPi {
                host: None,
                target: PiTarget::Native
            })
        );
        assert_eq!(
            Command::parse_pi(&words(&["install", "--vm"])),
            Ok(Command::InstallPi {
                host: None,
                target: PiTarget::Vm
            })
        );
        assert!(Command::parse_pi(&words(&["--vm"])).is_err());
        assert!(Command::parse_pi(&words(&["login"])).is_err());
    }

    #[test]
    fn pi_profiles_from_flags() {
        let c = Command::parse_profile(&words(&[
            "create",
            "web",
            "pi-linux",
            "--agent",
            "pi",
            "--pi-model",
            "openai/gpt-5",
        ]))
        .unwrap();
        let Command::CreateProfile { settings, .. } = c else {
            panic!("{c:?}");
        };
        assert_eq!(settings.agent, Some(AgentKind::Pi));
        assert_eq!(settings.pi_model.as_deref(), Some("openai/gpt-5"));
        assert!(Command::parse_profile(&words(&["create", "web", "x", "--agent", "gpt"])).is_err());
    }

    #[test]
    fn what_pi_status_says() {
        let mut status = PiStatus {
            host: "local".into(),
            native: None,
            error: None,
            logins: Vec::new(),
            vms: Some(vibe_core::rpc::PiVms::default()),
        };
        let none = describe_pi(&status);
        assert!(
            none.contains("installs it once a profile runs it"),
            "{none}"
        );
        assert!(none.contains("fetches it once a profile runs it"), "{none}");
        assert!(none.contains("Copy code login (headless)"), "{none}");
        assert!(none.contains("Don't choose Browser login"), "{none}");
        status.vms = Some(vibe_core::rpc::PiVms {
            alerts: vec!["quiet-otter signed in to xai".into()],
            ..Default::default()
        });
        assert!(
            describe_pi(&status).contains("\n! quiet-otter signed in to xai"),
            "the owner hears here too"
        );
        // R3: nothing of what a daemon sends reaches this terminal as more than
        // text, nor much of it.
        status.vms = Some(vibe_core::rpc::PiVms {
            alerts: vec![format!("\x1b[2Jphish{}", "x".repeat(100_000))],
            offered: vec![vibe_core::rpc::PiOffered {
                provider: "\x1b]0;evil\x07xai".into(),
                agent: "quiet-otter".into(),
            }],
            ..Default::default()
        });
        let said = describe_pi(&status);
        assert!(!said.chars().any(|c| c == '\x1b' || c == '\x07'), "{said}");
        assert!(said.len() < 2000, "{}", said.len());
        assert!(said.contains("vibe pi forget"), "{said}");
        status.native = Some(vibe_core::rpc::PiBinary {
            managed: false,
            path: "/bin/pi".into(),
            version: "1.5.0".into(),
        });
        status.logins = vec!["openai".into()];
        status.vms = None;
        assert_eq!(
            describe_pi(&status),
            "Pi 1.5.0 at /bin/pi, signed in to openai"
        );
    }
}
