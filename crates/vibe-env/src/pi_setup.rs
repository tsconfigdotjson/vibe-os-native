//! Talking to Pi for the app, with no terminal (PRD: Who gets which credentials;
//! Profiles): its own list of providers and models, and its own sign-in. `vibed` runs
//! `pi --mode rpc` with [`EXTENSION`] and nothing else of anyone's loaded, in an
//! empty directory, and this is how it speaks to that: what to run, what to send and
//! what Pi said. Pi's own code does the signing in and writes the login, as `/login`
//! does. Nothing here holds a provider's details, and nothing here is logged.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};
use vibe_core::model::{PiLoginMethod, PiPromptKind, clean_alert};
use vibe_core::rpc::{PiLoginOption, PiLoginPrompt, PiModel, PiProvider};

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
/// set on purpose in `pi.env`, for a list to say who has one. `offline` is for that
/// list too: a look at it must not spend a rotating token or reach any network.
pub fn env(
    inherited: impl IntoIterator<Item = (String, String)>,
    agent_dir: &Path,
    keys: Vec<(String, String)>,
    offline: bool,
) -> Vec<(String, String)> {
    let mut env = pi::allowed_env(inherited);
    env.extend(keys);
    env.extend(pi::quiet_env());
    env.push((
        pi::AGENT_DIR_ENV.to_owned(),
        agent_dir.to_string_lossy().into_owned(),
    ));
    if offline {
        env.push(("PI_OFFLINE".to_owned(), "1".to_owned()));
    }
    env
}

fn command(message: &str) -> String {
    json!({"id": "vibe", "type": "prompt", "message": message}).to_string()
}

/// What asks for the list.
pub fn catalog_command() -> String {
    command("/vibe-catalog")
}

/// What starts a sign-in. `provider` has been through [`pi::provider_name_ok`], so
/// it's one word.
pub fn login_command(provider: &str, method: PiLoginMethod) -> String {
    command(&format!("/vibe-login {provider} {method}"))
}

/// What answers prompt `id`.
pub fn answer_line(id: &str, value: &str) -> String {
    json!({"type": "extension_ui_response", "id": id, "value": value}).to_string()
}

/// One provider as Pi told it, with whether Pi has what it needs for it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub oauth: bool,
    #[serde(default)]
    pub oauth_label: Option<String>,
    #[serde(default)]
    pub api_key: bool,
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
    /// A page for the owner to open, with what to do there.
    Url {
        url: String,
        instructions: Option<String>,
    },
    /// A code for the owner to enter at a page.
    DeviceCode {
        code: String,
        url: Option<String>,
    },
    /// How it's going.
    Progress(String),
    Prompt(PiLoginPrompt),
    /// The last prompt doesn't want its answer any more: it got one, or something
    /// else answered it.
    PromptOver,
    Done,
    Failed(String),
}

/// What `line` of Pi's output says, if it's anything to do with this. A value in it
/// can be read, but only `https` is ever to be opened, and everything's cleaned to
/// show, as a host at the far end of a link may not be well.
pub fn read_line(line: &str) -> Option<Said> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value["type"].as_str()? {
        // A command Pi couldn't run says so here.
        "response" if value["success"] == false => Some(Said::Failed(clean_alert(
            value["error"].as_str().unwrap_or("Pi refused that"),
        ))),
        "extension_ui_request" => match value["method"].as_str()? {
            "notify" => read_said(&serde_json::from_str(value["message"].as_str()?).ok()?),
            "input" => {
                let asked: Value = serde_json::from_str(value["title"].as_str()?).ok()?;
                read_prompt(value["id"].as_str()?, &asked)
            }
            _ => None,
        },
        _ => None,
    }
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(clean_alert)
}

fn read_said(said: &Value) -> Option<Said> {
    match said["vibe"].as_str()? {
        "catalog" => serde_json::from_value(said["providers"].clone())
            .ok()
            .map(Said::Catalog),
        "prompt_over" => Some(Said::PromptOver),
        "done" => Some(Said::Done),
        "failed" => Some(Said::Failed(text(&said["message"])?)),
        "event" => match said["type"].as_str()? {
            "auth_url" => {
                let url = said["url"].as_str()?;
                Some(if url_can_be_opened(url) {
                    Said::Url {
                        url: url.to_owned(),
                        instructions: text(&said["instructions"]),
                    }
                } else {
                    Said::Failed("Pi gave a page VibeOS won't open: it isn't https".to_owned())
                })
            }
            "device_code" => Some(Said::DeviceCode {
                code: text(&said["userCode"])?,
                url: said["verificationUri"]
                    .as_str()
                    .filter(|u| url_can_be_opened(u))
                    .map(str::to_owned),
            }),
            "progress" | "info" => Some(Said::Progress(text(&said["message"])?)),
            _ => None,
        },
        _ => None,
    }
}

fn read_prompt(id: &str, asked: &Value) -> Option<Said> {
    if asked["vibe"] != "prompt" {
        return None;
    }
    let kind = match asked["type"].as_str()? {
        "text" => PiPromptKind::Text,
        "select" => PiPromptKind::Select,
        "manual_code" => PiPromptKind::ManualCode,
        // What Pi hasn't said is safe to show is treated as what isn't.
        _ => PiPromptKind::Secret,
    };
    let options = asked["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            Some(PiLoginOption {
                id: o["id"].as_str()?.to_owned(),
                label: text(&o["label"])?,
            })
        })
        .collect();
    Some(Said::Prompt(PiLoginPrompt {
        id: id.to_owned(),
        kind,
        message: text(&asked["message"]).unwrap_or_default(),
        placeholder: text(&asked["placeholder"]).filter(|p| !p.is_empty()),
        options,
    }))
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

/// Where a browser login means to come back to on the machine the browser is on: a
/// loopback address and a path. Pi's listening for it on the machine it runs on,
/// which is only the same one on a Mac.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// The [`Redirect`] a sign-in page's address names, if it names one on loopback.
pub fn redirect_of(page: &str) -> Option<Redirect> {
    let (_, query) = page.split_once('?')?;
    let query = query.split('#').next()?;
    let target = query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        matches!(name, "redirect_uri" | "callback_url").then_some(value)
    })?;
    let target = percent_decoded(target)?;
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    let (host, port) = authority.rsplit_once(':')?;
    if !matches!(host, "localhost" | "127.0.0.1") {
        return None;
    }
    Some(Redirect {
        host: host.to_owned(),
        port: port.parse().ok().filter(|p| *p != 0)?,
        path: path.split('?').next()?.to_owned(),
    })
}

fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(if b == b'+' { b' ' } else { b });
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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
                oauth: p.oauth,
                oauth_label: p.oauth_label.as_deref().map(clean_alert),
                api_key: p.api_key,
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
    fn a_run_gets_no_stray_key_and_only_a_list_is_offline() {
        let inherited = [
            ("PATH", "/bin"),
            ("OPENROUTER_API_KEY", "stray"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "stray"),
            ("PI_CODING_AGENT_DIR", "/elsewhere"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let keys = vec![("OPENCODE_API_KEY".to_owned(), "mine".to_owned())];
        let on = env(inherited.clone(), Path::new("/a"), keys, true);
        let has =
            |e: &[(String, String)], k: &str, v: &str| e.iter().any(|(ek, ev)| ek == k && ev == v);
        assert!(has(&on, "PATH", "/bin"));
        assert!(has(&on, "OPENCODE_API_KEY", "mine"));
        assert!(has(&on, "PI_CODING_AGENT_DIR", "/a"));
        assert!(has(&on, "PI_OFFLINE", "1"));
        assert!(!on.iter().any(|(k, _)| k == "OPENROUTER_API_KEY"));
        assert!(!on.iter().any(|(k, _)| k == "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!has(&on, "PI_CODING_AGENT_DIR", "/elsewhere"));
        let off = env(inherited, Path::new("/a"), Vec::new(), false);
        assert!(!off.iter().any(|(k, _)| k == "PI_OFFLINE"));
    }

    #[test]
    fn what_pi_says_is_read() {
        assert_eq!(read_line("not json"), None);
        assert_eq!(read_line(r#"{"type":"agent_start"}"#), None);
        assert_eq!(
            read_line(&notify(json!({"vibe": "done"}))),
            Some(Said::Done)
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

    #[test]
    fn a_page_that_isnt_https_is_never_passed_on_to_open() {
        let event = |url: &str| notify(json!({"vibe": "event", "type": "auth_url", "url": url}));
        assert_eq!(
            read_line(&event("https://claude.ai/oauth?x=1")),
            Some(Said::Url {
                url: "https://claude.ai/oauth?x=1".into(),
                instructions: None,
            })
        );
        for bad in [
            "http://evil.example",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "x-apple.systempreferences:com",
            "https://a b",
            "https://",
        ] {
            assert!(!url_can_be_opened(bad), "{bad}");
            assert!(
                matches!(read_line(&event(bad)), Some(Said::Failed(_))),
                "{bad}"
            );
        }
        assert!(url_can_be_opened("HTTPS://x.example"));
        assert!(!url_can_be_opened(&format!(
            "https://x/{}",
            "a".repeat(MAX_URL)
        )));
        // A code stands on its own, whatever its page.
        assert_eq!(
            read_line(&notify(json!({
                "vibe": "event", "type": "device_code", "userCode": "AB-12",
                "verificationUri": "file:///x",
            }))),
            Some(Said::DeviceCode {
                code: "AB-12".into(),
                url: None,
            })
        );
    }

    #[test]
    fn a_prompt_says_what_it_wants_and_unknown_ones_are_masked() {
        let input = |asked: Value| {
            json!({"type": "extension_ui_request", "id": "p1", "method": "input",
                   "title": asked.to_string(), "placeholder": ""})
            .to_string()
        };
        let Some(Said::Prompt(p)) = read_line(&input(json!({
            "vibe": "prompt", "type": "select", "message": "How?",
            "options": [{"id": "browser", "label": "Browser"}, {"nope": 1}],
        }))) else {
            panic!("a prompt");
        };
        assert_eq!(p.id, "p1");
        assert_eq!(p.kind, PiPromptKind::Select);
        assert_eq!(p.options.len(), 1);
        let Some(Said::Prompt(p)) = read_line(&input(json!({
            "vibe": "prompt", "type": "something_new", "message": "Key",
        }))) else {
            panic!("a prompt");
        };
        assert_eq!(p.kind, PiPromptKind::Secret);
        // Another extension's input isn't one of these.
        assert_eq!(read_line(&input(json!({"message": "x"}))), None);
    }

    #[test]
    fn a_reply_carries_any_text_as_one_line() {
        let line = answer_line("p1", "a\"b\nc");
        assert!(!line.contains('\n'));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["value"], "a\"b\nc");
        assert_eq!(value["id"], "p1");
    }

    #[test]
    fn a_redirect_is_only_ever_on_loopback() {
        assert_eq!(
            redirect_of(
                "https://claude.ai/oauth/authorize?code=true&redirect_uri=\
                 http%3A%2F%2Flocalhost%3A53692%2Fcallback&state=s"
            ),
            Some(Redirect {
                host: "localhost".into(),
                port: 53692,
                path: "/callback".into(),
            })
        );
        assert_eq!(
            redirect_of(
                "https://openrouter.ai/auth?callback_url=\
                 http%3A%2F%2F127.0.0.1%3A38517%2Foauth%2Fcallback%2F1ad3&code_challenge=x"
            ),
            Some(Redirect {
                host: "127.0.0.1".into(),
                port: 38517,
                path: "/oauth/callback/1ad3".into(),
            })
        );
        for page in [
            "https://x.example/auth",
            "https://x.example/auth?state=s",
            "https://x.example/auth?redirect_uri=https%3A%2F%2Fapp.example%2Fcb",
            "https://x.example/auth?redirect_uri=http%3A%2F%2Fevil.example%3A80%2Fcb",
            "https://x.example/auth?redirect_uri=http%3A%2F%2Flocalhost%2Fcb",
            "https://x.example/auth?redirect_uri=http%3A%2F%2Flocalhost%3A0%2Fcb",
            "https://x.example/auth?redirect_uri=http%3A%2F%2Flocalhost%3Azz%2Fcb",
            "https://x.example/auth?redirect_uri=%ZZ",
        ] {
            assert_eq!(redirect_of(page), None, "{page}");
        }
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
            oauth: false,
            oauth_label: None,
            api_key: true,
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
