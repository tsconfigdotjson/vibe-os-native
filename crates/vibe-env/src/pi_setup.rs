//! Talking to Pi for the app (PRD: Profiles): its own list of providers and models.
//! `vibed` runs `pi --mode rpc` with [`EXTENSION`] and nothing else of anyone's loaded,
//! in an empty directory, and this is how it speaks to that: what to run, what to send
//! and what Pi said. Nothing here holds a provider's details, and nothing here is logged.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};
use vibe_core::model::clean_alert;
use vibe_core::rpc::{PiModel, PiProvider};

use crate::pi;

/// The extension, written beside each run for `-e`.
pub const EXTENSION: &str = include_str!("vibe-setup.ts");

/// The extension's file name.
pub const EXTENSION_NAME: &str = "vibe-setup.ts";

/// All a run may print, far past any real list of models.
pub const MAX_OUTPUT: u64 = 64 << 20;

/// The longest address VibeOS will open or show.
const MAX_URL: usize = 8 << 10;

/// Pi's arguments for a run. Nothing of the owner's or of any project's is loaded
/// and there's no session: it starts on this one extension, and answers only it.
pub fn args(extension: &Path) -> Vec<String> {
    [
        "--mode",
        "rpc",
        "--no-session",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        "--no-mcp",
        "--no-approve",
    ]
    .map(str::to_owned)
    .into_iter()
    .chain(["-e".to_owned(), extension.to_string_lossy().into_owned()])
    .collect()
}

/// The environment for a run, on what Pi's agent dir is: as for any Pi, from an
/// allowlist so no stray key is used ([`pi::allowed_env`]). `keys` is what the owner
/// set on purpose in `pi.env`, for a list to say who has one. It's offline: a look at a
/// list must not spend a rotating token or reach any network.
pub fn env(
    inherited: impl IntoIterator<Item = (String, String)>,
    agent_dir: &Path,
    keys: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut env = pi::allowed_env(inherited);
    env.extend(keys);
    env.extend(pi::quiet_env());
    env.push((
        pi::AGENT_DIR_ENV.to_owned(),
        agent_dir.to_string_lossy().into_owned(),
    ));
    env.push(("PI_OFFLINE".to_owned(), "1".to_owned()));
    env
}

fn command(message: &str) -> String {
    json!({"id": "vibe", "type": "prompt", "message": message}).to_string()
}

/// What asks for the list.
pub fn catalog_command() -> String {
    command("/vibe-catalog")
}

/// One provider as Pi told it, with whether Pi has what it needs for it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// What Pi said, in one line of its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    Catalog(Vec<CatalogProvider>),
    Failed(String),
}

/// What `line` of Pi's output says, if it's anything to do with this. Everything's
/// cleaned to show, as a host at the far end of a link may not be well.
pub fn read_line(line: &str) -> Option<Said> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value["type"].as_str()? {
        // A command Pi couldn't run says so here.
        "response" if value["success"] == false => Some(Said::Failed(clean_alert(
            value["error"].as_str().unwrap_or("Pi refused that"),
        ))),
        "extension_ui_request" if value["method"] == "notify" => {
            read_said(&serde_json::from_str(value["message"].as_str()?).ok()?)
        }
        _ => None,
    }
}

fn read_said(said: &Value) -> Option<Said> {
    match said["vibe"].as_str()? {
        "catalog" => serde_json::from_value(said["providers"].clone())
            .ok()
            .map(Said::Catalog),
        // Pi's message for what went wrong can run on into a stack, which says nothing
        // to anyone reading this.
        "failed" => {
            let message = said["message"].as_str()?;
            let message = message.split("; stack=").next().unwrap_or(message);
            Some(Said::Failed(clean_alert(message)))
        }
        _ => None,
    }
}

/// Whether the app may open `url` for the owner: `https`, and only an address.
pub fn url_can_be_opened(url: &str) -> bool {
    url.len() <= MAX_URL
        && url
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        && url.len() > 8
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A catalog as the app is told it: only names that are safe to give Pi back and to
/// show, signed-in providers first. `signed_in` says who has a login that Pi itself
/// can't see from where this list was made.
pub fn providers_of(
    catalog: Vec<CatalogProvider>,
    signed_in: impl Fn(&str) -> bool,
) -> Vec<PiProvider> {
    let mut providers: Vec<PiProvider> = catalog
        .into_iter()
        .filter(|p| pi::provider_name_ok(&p.id))
        .map(|p| {
            let models = p
                .models
                .into_iter()
                .filter(|m| {
                    vibe_core::model::validate_pi_model(&format!("{}/{}", p.id, m.id)).is_ok()
                })
                .map(|m| PiModel {
                    name: m.name.as_deref().map(clean_alert).filter(|n| *n != m.id),
                    id: m.id,
                })
                .collect();
            PiProvider {
                signed_in: p.configured || signed_in(&p.id),
                ambient: pi::can_sign_in_from_around(&p.id),
                name: clean_alert(&p.name),
                models,
                id: p.id,
            }
        })
        .collect();
    providers.sort_by(|a, b| {
        (!a.signed_in, a.name.to_lowercase()).cmp(&(!b.signed_in, b.name.to_lowercase()))
    });
    providers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify(said: Value) -> String {
        json!({"type": "extension_ui_request", "id": "n", "method": "notify",
               "message": said.to_string(), "notifyType": "info"})
        .to_string()
    }

    #[test]
    fn nothing_of_anyones_loads_for_a_run() {
        let args = args(Path::new("/s/vibe-setup.ts"));
        for flag in [
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-mcp",
            "--no-approve",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag}");
        }
        assert_eq!(args[args.len() - 2..], ["-e", "/s/vibe-setup.ts"]);
    }

    #[test]
    fn a_run_gets_no_stray_key_and_is_offline() {
        let inherited = [
            ("PATH", "/bin"),
            ("OPENROUTER_API_KEY", "stray"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "stray"),
            ("PI_CODING_AGENT_DIR", "/elsewhere"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let keys = vec![("OPENCODE_API_KEY".to_owned(), "mine".to_owned())];
        let on = env(inherited, Path::new("/a"), keys);
        let has =
            |e: &[(String, String)], k: &str, v: &str| e.iter().any(|(ek, ev)| ek == k && ev == v);
        assert!(has(&on, "PATH", "/bin"));
        assert!(has(&on, "OPENCODE_API_KEY", "mine"));
        assert!(has(&on, "PI_CODING_AGENT_DIR", "/a"));
        assert!(has(&on, "PI_OFFLINE", "1"));
        assert!(!on.iter().any(|(k, _)| k == "OPENROUTER_API_KEY"));
        assert!(!on.iter().any(|(k, _)| k == "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!has(&on, "PI_CODING_AGENT_DIR", "/elsewhere"));
    }

    #[test]
    fn what_pi_says_is_read() {
        assert_eq!(read_line("not json"), None);
        assert_eq!(read_line(r#"{"type":"agent_start"}"#), None);
        assert_eq!(
            read_line(&notify(json!({"vibe": "catalog", "providers": []}))),
            Some(Said::Catalog(Vec::new()))
        );
        // Some other extension's, or Pi's own, isn't this.
        assert_eq!(read_line(&notify(json!({"note": "hi"}))), None);
        assert_eq!(
            read_line(&notify(json!({"vibe": "failed", "message": "no\u{1b}[2J"})))
                .map(|s| matches!(s, Said::Failed(m) if !m.contains('\u{1b}'))),
            Some(true)
        );
        assert_eq!(
            read_line(
                r#"{"type":"response","command":"prompt","success":false,"error":"Unknown"}"#
            ),
            Some(Said::Failed("Unknown".to_owned()))
        );
        assert_eq!(
            read_line(r#"{"type":"response","command":"prompt","success":true}"#),
            None
        );
    }

    /// The app's own sign-ins, to Claude and GitHub, go by this (`vibed`'s `signin`).
    #[test]
    fn a_page_that_isnt_https_is_never_passed_on_to_open() {
        assert!(url_can_be_opened("https://claude.ai/oauth?x=1"));
        assert!(url_can_be_opened("HTTPS://x.example"));
        for bad in [
            "http://evil.example",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "x-apple.systempreferences:com",
            "https://a b",
            "https://",
        ] {
            assert!(!url_can_be_opened(bad), "{bad}");
        }
        assert!(!url_can_be_opened(&format!(
            "https://x/{}",
            "a".repeat(MAX_URL)
        )));
    }

    /// VibeOS signs nobody in to Pi: what it loads into Pi can only read its list, and
    /// a question from Pi, which would need an answer, isn't heard.
    #[test]
    fn what_vibeos_loads_into_pi_only_reads_its_list() {
        assert!(EXTENSION.contains("vibe-catalog"));
        for word in [
            "vibe-login",
            "runtime.login",
            "runtime.logout",
            "ctx.ui.input",
            "writeFile",
        ] {
            assert!(!EXTENSION.contains(word), "{word}");
        }
        let asked = json!({"type": "extension_ui_request", "id": "p1", "method": "input",
                           "title": json!({"vibe": "prompt", "message": "API key"}).to_string()});
        assert_eq!(read_line(&asked.to_string()), None);
    }

    #[test]
    fn a_failure_is_said_without_its_stack() {
        assert_eq!(
            read_line(&notify(json!({
                "vibe": "failed",
                "message": "Token exchange failed. url=https://x; stack=Error: at y\n at z",
            }))),
            Some(Said::Failed("Token exchange failed. url=https://x".into()))
        );
    }

    #[test]
    fn a_list_has_only_what_can_be_given_back_and_those_signed_in_lead() {
        let model = |id: &str, name: Option<&str>| CatalogModel {
            id: id.to_owned(),
            name: name.map(str::to_owned),
        };
        let provider = |id: &str, name: &str, configured, models| CatalogProvider {
            id: id.to_owned(),
            name: name.to_owned(),
            configured,
            models,
        };
        let listed = providers_of(
            vec![
                provider(
                    "zeta",
                    "Zeta",
                    true,
                    vec![
                        model("z-1", Some("Z One")),
                        model("z-1b", Some("z-1b")),
                        model("has space", None),
                        model("quo\"te", None),
                    ],
                ),
                provider("alpha", "Alpha", false, vec![model("a-1", None)]),
                provider("beta", "Beta", false, Vec::new()),
                provider("bad name!", "Bad", true, Vec::new()),
                provider("amazon-bedrock", "Amazon Bedrock", false, Vec::new()),
            ],
            |p| p == "beta",
        );
        let ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["beta", "zeta", "alpha", "amazon-bedrock"]);
        assert!(listed[0].signed_in && listed[1].signed_in && !listed[2].signed_in);
        assert!(listed[3].ambient && !listed[3].signed_in && !listed[0].ambient);
        let zeta = &listed[1];
        assert_eq!(zeta.models.len(), 2);
        assert_eq!(zeta.models[0].name.as_deref(), Some("Z One"));
        // A name that only says the id again says nothing.
        assert_eq!(zeta.models[1].name, None);
    }
}
