//! `vibe project`, `vibe profile` and `vibe host`: host setup without the
//! menu bar (PRD: Architecture, `vibe` CLI). On a standalone Linux host they
//! replace Settings → Projects; on a Mac, `vibe host` adds Linux hosts as
//! Add Linux host… does, and `--host` sends the others to one of them.

use std::path::Path;
use std::time::Duration;

use vibe_core::model::{Environment, Mode, Model, Reasoning};
use vibe_core::rpc::{
    HostAddParams, HostExternalParams, HostInfo, HostInstall, HostLink, HostNameParams, HostParams,
    HostsAdd, HostsExternal, HostsList, HostsRemove, HostsUpgrade, NoParams, Profile,
    ProfileCreateParams, ProfileParams, ProfileUpdateParams, ProfilesCreate, ProfilesDelete,
    ProfilesList, ProfilesUpdate, Project, ProjectAddParams, ProjectParams, ProjectsAdd,
    ProjectsList, ProjectsRemove,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
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
    /// Makes a host the external address at a domain, or with none not.
    External {
        host: String,
        domain: Option<String>,
    },
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
    })
}

/// The words `vibe profile` takes first.
const SUBCOMMANDS: [&str; 8] = [
    "list", "ls", "create", "add", "set", "update", "delete", "rm",
];

const PROFILE_FLAGS: [&str; 6] = [
    "host",
    "environment",
    "model",
    "reasoning",
    "mode",
    "memory",
];

impl Command {
    /// From the words after `project`.
    pub(crate) fn parse_project(words: &[String]) -> Result<Command, String> {
        let (plain, found) = flags(words, &["host", "name"])?;
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
            }),
            ["add"] => Err("project add needs a GitHub owner/name or a path".into()),
            ["remove" | "rm"] => Err("project remove needs a project's name".into()),
            _ => Err("project takes list, add OWNER/NAME|PATH [--name NAME] or remove NAME".into()),
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
            ["external", name, "off"] => Ok(Command::External {
                host: (*name).to_owned(),
                domain: None,
            }),
            ["external", name, domain] => Ok(Command::External {
                host: (*name).to_owned(),
                domain: Some((*domain).to_owned()),
            }),
            _ => Err(
                "host takes list, add NAME SSH-DESTINATION, upgrade NAME, remove NAME, or \
                 external NAME DOMAIN|off"
                    .into(),
            ),
        }
    }
}

/// A project and its profiles, as lines.
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

pub(crate) fn describe_profile(p: &Profile) -> String {
    format!(
        "{}  {}, {}, {} reasoning, {} mode, {}",
        p.name,
        p.environment,
        p.model,
        p.reasoning,
        p.mode,
        format_size(p.memory).unwrap_or_else(|| format!("{} bytes", p.memory))
    )
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
    if h.external {
        out.push_str("  external address");
        if let Some(url) = &h.url {
            out.push_str(&format!(" {url}"));
        }
        if let Some(t) = h.tunnel {
            out.push_str(&format!(", tunnel {t}"));
        }
        if let Some(why) = &h.external_detail {
            out.push_str(&format!(": {why}"));
        }
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
            "Still needed there for its agents: {}. Install and log in to them on the server \
             itself (`gh auth login`, and `claude` then /login).\n",
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
        Command::RemoveProject { host, project } => {
            let p =
                client::call::<ProjectsRemove>(socket, &ProjectParams { host, project }, TIMEOUT)
                    .map_err(e)?;
            println!("removed {}; {} is left as it was", p.name, p.path);
        }
        Command::Profiles { host, project } => {
            let list =
                client::call::<ProfilesList>(socket, &ProjectParams { host, project }, TIMEOUT)
                    .map_err(e)?;
            for p in &list.profiles {
                println!("{}", describe_profile(p));
            }
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
        Command::External { host, domain } => {
            let params = HostExternalParams {
                host: Some(host),
                on: domain.is_some(),
                domain,
            };
            let h = client::call::<HostsExternal>(socket, &params, TIMEOUT).map_err(e)?;
            println!("{}", describe_host(&h));
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
                project: "web".into()
            })
        );
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
        };
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
            url: None,
            tunnel: None,
            external_detail: None,
        };
        assert_eq!(
            describe_host(&h),
            "vps  vps-1 (linux)  down: it stopped answering  ssh me@vps  vibed 0.1.0  missing claude"
        );
        let h = HostInfo {
            link: HostLink::Connected,
            detail: None,
            missing: vec![],
            external: true,
            url: Some("https://vps.example.com/mcp".into()),
            tunnel: Some(vibe_core::model::TunnelState::Down),
            external_detail: Some("can't listen on port 443".into()),
            ..h
        };
        assert_eq!(
            describe_host(&h),
            "vps  vps-1 (linux)  connected  ssh me@vps  vibed 0.1.0  external address \
             https://vps.example.com/mcp, tunnel down: can't listen on port 443"
        );
        assert_eq!(
            Command::parse_host(&words(&["external", "vps", "vps.example.com"])),
            Ok(Command::External {
                host: "vps".into(),
                domain: Some("vps.example.com".into())
            })
        );
        assert_eq!(
            Command::parse_host(&words(&["external", "vps", "off"])),
            Ok(Command::External {
                host: "vps".into(),
                domain: None
            })
        );
        assert!(Command::parse_host(&words(&["external", "vps"])).is_err());
    }
}
