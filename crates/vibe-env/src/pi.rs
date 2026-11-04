//! Pi's state for an agent, and how it's started (PRD: Profiles; Who gets
//! which credentials; Matching an agent to its transcript).
//!
//! Pi keeps its settings, models and logins in one directory, its agent dir:
//! `~/.pi/agent`, or `PI_CODING_AGENT_DIR`. Its sessions go in
//! `sessions/--<cwd>--/` there, or wherever `PI_CODING_AGENT_SESSION_DIR`
//! says. VibeOS always says, so it knows where an agent's transcript is
//! whatever the owner's `sessionDir` setting is, and gives the path Pi would
//! have used on its own, so the owner's own `pi --resume` still finds it.
//!
//! **One login per user.** Pi's logins are the owner's, from Pi's own `/login`:
//! VibeOS doesn't pick a provider or supply a key. They're in `auth.json` in the
//! agent dir, and their OAuth refresh tokens rotate.
//!
//! - `macos` and a Linux host: the owner's own agent dir, which every agent there
//!   uses, with only its sessions its own;
//! - `linux` on the Mac: each VM has a private agent dir, as Pi runs code from
//!   that file and a guest can write it, and `vibed` keeps each in step with one
//!   store no guest mounts ([`crate::pi_logins`]). What the owner keeps in
//!   `~/VibeOS/linux-pi/` reaches them through a share they can't write.
//!
//! **The environment is an allowlist.** Pi uses any provider key it finds in
//! its environment, so a stray `OPENROUTER_API_KEY` in `vibed`'s own would
//! silently bill that account. A Pi process starts from an empty environment
//! plus [`ALLOWED_ENV`] and what's named on purpose in `pi.env`
//! ([`read_env_file`]). `vibed` never gives it `CLAUDE_CODE_OAUTH_TOKEN`, and
//! the toolbox's `pi` removes it from the shell's environment ([`WRAPPER`]).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vibe_core::claude_token::TOKEN_ENV;

/// The environment variable that moves Pi's agent dir.
pub const AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// The environment variable that moves Pi's sessions.
pub const SESSION_DIR_ENV: &str = "PI_CODING_AGENT_SESSION_DIR";

/// What says which shared services an agent gets, as `mcpServers` JSON, the same
/// as Claude's `--mcp-config`: `vibe-services.ts` reads it.
pub const SERVICES_ENV: &str = "VIBE_SERVICES";

/// The extension that registers the shared MCP services, for
/// `pi -e`, which is added exactly when an agent's environment holds
/// [`SERVICES_ENV`]. It's written beside each native agent, and in the Pi layer
/// for `linux` VMs.
pub const EXTENSION: &str = include_str!("vibe-services.ts");

/// The extension's file name.
pub const EXTENSION_NAME: &str = "vibe-services.ts";

/// Where Pi's agent dir is mounted in a VM.
pub const GUEST_AGENT_DIR: &str = "/home/vibe/.pi/agent";

/// Where the owner's settings, extensions and so on are mounted in a VM,
/// read-only at the share itself. Links in its agent dir point here.
pub const GUEST_CONFIG_DIR: &str = "/home/vibe/.pi/config";

/// That share's tag.
pub const CONFIG_TAG: &str = "pi-config";

/// The largest `auth.json` looked at, far past any real one, and all a guest can
/// make `vibed` read every few seconds. A copy has to hold the whole store, so this
/// is past what the limits on agents' offers can add up to
/// (`pi_logins::MAX_AGENT_ENTRY` by `MAX_AGENT_PROVIDERS`).
pub const MAX_AUTH_FILE: u64 = 4 << 20;

/// Where a VM's sessions are mounted, beside its agent dir.
pub const GUEST_SESSION_DIR: &str = "/home/vibe/.pi/sessions";

/// The agent's own sessions on the host, in its directory, for a VM.
pub const VM_SESSIONS: &str = "pi-sessions";

/// The agent's private agent dir on the host, in its directory, for a VM.
pub const VM_AGENT_DIR: &str = "pi-agent";

/// What `vibed` knows of that directory's `auth.json`, beside it, where the
/// guest can't see it.
pub const VM_LOGIN_STATE: &str = "pi-login-state.json";

/// The file in which the owner names, on purpose, what Pi agents get in their
/// environment, in `~/.config/vibeos/`: `NAME=value`, one to a line.
pub const ENV_FILE: &str = "pi.env";

/// What a Pi process may keep of `vibed`'s environment. Everything else goes,
/// provider keys above all. Names ending in `*` are prefixes.
pub const ALLOWED_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_*",
    "TERM",
    "COLORTERM",
    "TZ",
    "TMPDIR",
    // systemd's, which a Linux host's scopes are asked for through.
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "SSH_AUTH_SOCK",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "EDITOR",
    "VISUAL",
    // Where toolchains are, which builds an agent runs need.
    "DEVELOPER_DIR",
    "SDKROOT",
    "JAVA_HOME",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "GOPATH",
    "GOROOT",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "NVM_DIR",
    "PNPM_HOME",
    "BUN_INSTALL",
    "HOMEBREW_PREFIX",
    "__CF_USER_TEXT_ENCODING",
];

/// Anthropic's own name for an OAuth token, which Pi would use as one.
pub const ANTHROPIC_OAUTH_ENV: &str = "ANTHROPIC_OAUTH_TOKEN";

/// What the owner can't set in `pi.env`: the Claude tokens, which Anthropic's
/// terms keep to Claude Code, the paths VibeOS finds its transcripts and logins
/// by, and what says which shared services an agent gets, which is its profile's
/// to say.
const REFUSED_IN_ENV_FILE: &[&str] = &[
    TOKEN_ENV,
    ANTHROPIC_OAUTH_ENV,
    AGENT_DIR_ENV,
    SESSION_DIR_ENV,
    SERVICES_ENV,
    vibe_relay::local::SOCKET_ENV,
    vibe_relay::local::AGENT_ENV,
    vibe_relay::local::CACHE_ENV,
];

/// What a VM's own environment says, which a line of `pi.env` written for the
/// Mac mustn't replace: a Mac's `PATH` is no use in a guest.
const GUEST_OWNED_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
];

/// `/opt/vibeos/bin/pi` in the Pi layer, what's found on `PATH`: the toolbox's
/// shell holds the Claude token, and Pi would use it. An agent's environment
/// never had it, and it goes through this all the same.
pub const WRAPPER: &str = "#!/bin/sh\n\
# VibeOS: Pi never gets a Claude token, even from a shell that has one.\n\
exec env -u CLAUDE_CODE_OAUTH_TOKEN -u ANTHROPIC_OAUTH_TOKEN /opt/vibeos/pi/pi \"$@\"\n";

/// `env`, without what a VM's own environment says.
pub fn for_guest(env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.into_iter()
        .filter(|(name, _)| !GUEST_OWNED_ENV.contains(&name.as_str()))
        .collect()
}

/// What of `inherited`, `vibed`'s own environment, a Pi process may keep.
pub fn allowed_env(inherited: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    inherited
        .into_iter()
        .filter(|(name, _)| is_allowed(name))
        .collect()
}

fn is_allowed(name: &str) -> bool {
    name != TOKEN_ENV
        && name != ANTHROPIC_OAUTH_ENV
        && ALLOWED_ENV.iter().any(|a| match a.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => *a == name,
        })
}

/// What `path` (a `pi.env`) sets on purpose. Blank lines and `#` comments are
/// skipped, and a leading `export ` is allowed, so a file made for a shell
/// reads as it looks. Nothing if there's no such file. It holds keys, so one
/// others can read is refused rather than used.
pub fn read_env_file(path: &Path) -> Result<Vec<(String, String)>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let mode = fs::metadata(path)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} can be read by others, and it holds your keys: run `chmod 600` on it",
            path.display()
        ));
    }
    let mut out = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let bad = || format!("{} line {}: write it as NAME=value", path.display(), i + 1);
        let (name, value) = line.split_once('=').ok_or_else(bad)?;
        let name = name.trim();
        let name_ok = !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return Err(bad());
        }
        if REFUSED_IN_ENV_FILE.contains(&name) {
            return Err(format!(
                "{} line {}: {name} can't be set for Pi agents",
                path.display(),
                i + 1
            ));
        }
        out.insert(name.to_owned(), unquoted(value.trim()).to_owned());
    }
    Ok(out.into_iter().collect())
}

/// `value` without one pair of matching quotes around it.
fn unquoted(value: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(q).and_then(|rest| rest.strip_suffix(q)) {
            return inner;
        }
    }
    value
}

/// What every Pi process gets, wherever it runs: no phoning home. The managed
/// layer, or the owner, keeps Pi current.
pub fn quiet_env() -> Vec<(String, String)> {
    vec![
        ("PI_SKIP_VERSION_CHECK".into(), "1".into()),
        ("PI_TELEMETRY".into(), "0".into()),
    ]
}

/// The name Pi gives the directory of sessions run in `cwd`.
pub fn session_dir_name(cwd: &Path) -> String {
    let path = cwd.to_string_lossy();
    let path = path.strip_prefix('/').unwrap_or(&path);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

/// The session a transcript's file name says it holds:
/// `<ISO time>_<session id>.jsonl`, the time having no `_` in it.
pub fn session_of_file(name: &str) -> Option<&str> {
    let (_, id) = name.strip_suffix(".jsonl")?.split_once('_')?;
    (!id.is_empty()).then_some(id)
}

/// Where Pi keeps its state for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiHome {
    /// The agent dir: the owner's own, for every agent on the host, or a VM's
    /// private one.
    pub dir: PathBuf,
    /// This agent's sessions, which only it writes to: a worktree has one
    /// agent, so even under a shared agent dir they're its own.
    pub sessions: PathBuf,
    /// Where the owner's `models.json` is on this machine, for a VM's agent
    /// dir, which only links to it by a path in the guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
}

impl PiHome {
    /// The owner's own agent dir, `own_dir`, for an agent working in
    /// `worktree`.
    pub fn user(own_dir: PathBuf, worktree: &Path) -> PiHome {
        PiHome {
            sessions: own_dir.join("sessions").join(session_dir_name(worktree)),
            dir: own_dir,
            config: None,
        }
    }

    /// `dir` as the agent dir, with sessions of their own in `sessions`.
    pub fn at(dir: PathBuf, sessions: PathBuf) -> PiHome {
        PiHome {
            dir,
            sessions,
            config: None,
        }
    }

    /// With the owner's settings in `config`, for a VM's agent dir.
    pub fn with_config(mut self, config: PathBuf) -> PiHome {
        self.config = Some(config);
        self
    }

    /// The transcript of session `session_id`, if it has one yet. Pi writes
    /// the file at the first user message, not at its start.
    pub fn transcript(&self, session_id: &str) -> Option<PathBuf> {
        fs::read_dir(&self.sessions)
            .ok()?
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_str()
                    .and_then(session_of_file)
                    .is_some_and(|id| id == session_id)
            })
            .map(|e| e.path())
    }

    /// Makes the agent dir and the sessions directory.
    pub fn prefill(&self) -> io::Result<()> {
        make_private(&self.dir)?;
        make_private(&self.sessions)
    }

    /// The providers the agent dir holds a login or key for, by name, from
    /// `auth.json`. Never what they hold. None if it can't be read.
    pub fn logins(&self) -> Vec<String> {
        logins_in(&self.dir)
    }

    /// What Pi lacks to answer with `model` here, as far as can be told without
    /// running it; `env_file` is what `pi.env` sets. None if nothing's missing,
    /// which only says Pi will try: a screen showing otherwise says the rest
    /// (`vibe_status::pi_error_on_screen`). A key in `pi.env` counts for the
    /// provider it's for, and any other line for nothing. Only a provider Pi
    /// names a key for can be said to lack one: an extension's, or one that can
    /// sign in from what's around it, is left to try.
    pub fn login_missing(
        &self,
        model: Option<&str>,
        env_file: &[(String, String)],
    ) -> Option<String> {
        let logins = self.logins();
        let own = self.key_providers();
        let set = |name: &str| env_file.iter().any(|(k, v)| k == name && !v.is_empty());
        let has_key = |names: &[&str]| names.iter().any(|name| set(name));
        let provider = model.and_then(|m| m.split_once('/')).map(|(p, _)| p);
        let enough = match provider {
            Some(p) if AMBIENT_PROVIDERS.contains(&p) => true,
            Some(p) => match PROVIDER_ENV.iter().find(|(known, _)| *known == p) {
                None => true,
                Some((_, names)) => logins.iter().chain(&own).any(|l| l == p) || has_key(names),
            },
            None => {
                !logins.is_empty()
                    || !own.is_empty()
                    || PROVIDER_ENV.iter().any(|(_, names)| has_key(names))
            }
        };
        (!enough).then(|| match provider {
            Some(p) => format!("Pi isn't signed in to {p} here, which this profile's model needs"),
            None => "Pi isn't signed in to any provider here".to_owned(),
        })
    }

    /// The providers `models.json` gives a key of their own.
    pub(crate) fn key_providers(&self) -> Vec<String> {
        let path = self
            .config
            .as_deref()
            .unwrap_or(&self.dir)
            .join("models.json");
        let Ok(file) = fs::File::open(path) else {
            return Vec::new();
        };
        let Ok(text) = io::read_to_string(file.take(MAX_AUTH_FILE)) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        value["providers"].as_object().map_or(Vec::new(), |map| {
            map.iter()
                .filter(|(_, p)| p["apiKey"].as_str().is_some_and(|k| !k.is_empty()))
                .map(|(name, _)| name.clone())
                .collect()
        })
    }

    /// Forgets the agent: its sessions, and its private agent dir if it has
    /// one, with the logins in it. Nothing of a shared one.
    pub fn forget(&self) -> io::Result<()> {
        let gone = |dir: &Path| match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
        gone(&self.sessions)?;
        if self.config.is_some() {
            gone(&self.dir)?;
        }
        Ok(())
    }
}

/// The environment variables that give each provider a key, from Pi 1.5's
/// `env-api-keys.ts`: what a line of `pi.env` is for. Re-check it when the pin
/// moves.
const PROVIDER_ENV: &[(&str, &[&str])] = &[
    (
        "anthropic",
        &[
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_FEDERATION_RULE_ID",
            "ANTHROPIC_IDENTITY_TOKEN_FILE",
        ],
    ),
    ("github-copilot", &["COPILOT_GITHUB_TOKEN"]),
    ("ant-ling", &["ANT_LING_API_KEY"]),
    ("qwen-token-plan", &["QWEN_TOKEN_PLAN_API_KEY"]),
    ("qwen-token-plan-cn", &["QWEN_TOKEN_PLAN_CN_API_KEY"]),
    ("qwen-token-plan-individual", &["QWEN_TOKEN_PLAN_API_KEY"]),
    ("openai", &["OPENAI_API_KEY"]),
    ("azure", &["AZURE_OPENAI_API_KEY"]),
    ("nvidia", &["NVIDIA_API_KEY"]),
    ("deepseek", &["DEEPSEEK_API_KEY"]),
    ("google", &["GEMINI_API_KEY"]),
    ("groq", &["GROQ_API_KEY"]),
    ("cerebras", &["CEREBRAS_API_KEY"]),
    ("xai", &["XAI_API_KEY"]),
    ("typesafe", &["TYPESAFE_API_KEY"]),
    ("radius", &["RADIUS_API_KEY"]),
    ("openrouter", &["OPENROUTER_API_KEY"]),
    ("vercel-ai-gateway", &["AI_GATEWAY_API_KEY"]),
    ("zai", &["ZAI_API_KEY"]),
    ("zai-coding-cn", &["ZAI_CODING_CN_API_KEY"]),
    ("mistral", &["MISTRAL_API_KEY"]),
    ("minimax", &["MINIMAX_API_KEY"]),
    ("minimax-cn", &["MINIMAX_CN_API_KEY"]),
    ("moonshotai", &["MOONSHOT_API_KEY"]),
    ("moonshotai-cn", &["MOONSHOT_API_KEY"]),
    ("huggingface", &["HF_TOKEN"]),
    ("fireworks", &["FIREWORKS_API_KEY"]),
    ("together", &["TOGETHER_API_KEY"]),
    ("baseten", &["BASETEN_API_KEY"]),
    ("opencode", &["OPENCODE_API_KEY"]),
    ("opencode-go", &["OPENCODE_API_KEY"]),
    ("kimi-coding", &["KIMI_API_KEY"]),
    ("meta", &["META_API_KEY"]),
    ("cloudflare-workers-ai", &["CLOUDFLARE_API_KEY"]),
    ("cloudflare-ai-gateway", &["CLOUDFLARE_API_KEY"]),
    ("xiaomi", &["XIAOMI_API_KEY"]),
    ("xiaomi-token-plan-cn", &["XIAOMI_TOKEN_PLAN_CN_API_KEY"]),
    ("xiaomi-token-plan-ams", &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]),
    ("xiaomi-token-plan-sgp", &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]),
    // These two can also sign in from what's around them: `AMBIENT_PROVIDERS`.
    (
        "google-vertex",
        &[
            "GOOGLE_CLOUD_API_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
        ],
    ),
    (
        "amazon-bedrock",
        &[
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_ACCESS_KEY_ID",
            "AWS_PROFILE",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
        ],
    ),
];

/// The providers `env`, what `pi.env` sets, gives a key for.
pub fn providers_with_keys(env: &[(String, String)]) -> Vec<String> {
    PROVIDER_ENV
        .iter()
        .filter(|(_, names)| {
            names
                .iter()
                .any(|name| env.iter().any(|(k, v)| k == name && !v.is_empty()))
        })
        .map(|(provider, _)| (*provider).to_owned())
        .collect()
}

/// Providers that can sign in from what's around them, which can't be told from
/// here: Vertex from a file of `gcloud`'s, Bedrock from a role or a profile.
const AMBIENT_PROVIDERS: &[&str] = &["google-vertex", "amazon-bedrock"];

/// The owner's own agent dir: `~/.pi/agent`, unless `PI_CODING_AGENT_DIR` moves
/// it in `vibed`'s environment, as it moves it for the owner's own `pi`.
pub fn own_dir(home: &Path) -> PathBuf {
    own_dir_in(std::env::var(AGENT_DIR_ENV).ok(), home)
}

fn own_dir_in(moved: Option<String>, home: &Path) -> PathBuf {
    match moved.filter(|d| !d.is_empty()) {
        None => home.join(".pi/agent"),
        Some(d) if d == "~" => home.to_owned(),
        Some(d) => match d.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(d),
        },
    }
}

/// What's in `path`, read as a guest's or an owner's file may need: not through
/// a link, which a guest could point anywhere, nor a pipe or a device, which
/// would never end, nor more than [`MAX_AUTH_FILE`]. None if there's no such
/// file; otherwise an error says what's wrong with it, to follow its name.
pub fn read_plain_file(path: &Path) -> Result<Option<String>, String> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err("is a link".to_owned()),
        Err(e) => return Err(format!("can't be read: {e}")),
    };
    let meta = file.metadata().map_err(|e| format!("can't be read: {e}"))?;
    if !meta.is_file() {
        return Err("isn't a plain file".to_owned());
    }
    let too_big = || format!("is over {} MiB", MAX_AUTH_FILE >> 20);
    if meta.len() > MAX_AUTH_FILE {
        return Err(too_big());
    }
    let text = io::read_to_string(file.take(MAX_AUTH_FILE + 1))
        .map_err(|e| format!("can't be read: {e}"))?;
    if text.len() as u64 > MAX_AUTH_FILE {
        return Err(too_big());
    }
    Ok(Some(text))
}

/// The entries of `auth.json` in `dir`. Nothing if it can't be read.
fn auth_entries(dir: &Path) -> serde_json::Map<String, Value> {
    let text = read_plain_file(&dir.join("auth.json")).ok().flatten();
    match text.and_then(|text| serde_json::from_str::<Value>(&text).ok()) {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

/// The providers `auth.json` in `dir` has entries for. A name that can't be a
/// provider's isn't one, and isn't repeated: it's on its way to a terminal.
pub fn logins_in(dir: &Path) -> Vec<String> {
    auth_entries(dir)
        .keys()
        .filter(|name| provider_name_ok(name))
        .cloned()
        .collect()
}

/// Whether `name` could be a provider's: what Pi's are, in characters that mean
/// nothing to a terminal and few enough to read.
pub fn provider_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Makes `dir`, and what's above it, for this user alone.
pub fn make_private(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Whether an agent's environment offers it shared services.
pub fn offers_services(env: &[(String, String)]) -> bool {
    env.iter().any(|(name, _)| name == SERVICES_ENV)
}

/// The `-e` that loads the shared services' extension at `extension`, for an
/// agent with `env`. Nothing without any.
pub fn service_args(env: &[(String, String)], extension: &str) -> Vec<String> {
    if offers_services(env) {
        vec!["-e".into(), extension.to_owned()]
    } else {
        Vec::new()
    }
}

/// Writes the extension beside a native agent, for its launch to load, and
/// says where. It's rewritten each time, so a new `vibed` brings a new one.
pub fn write_extension(agent_dir: &Path) -> io::Result<PathBuf> {
    let path = agent_dir.join(EXTENSION_NAME);
    fs::write(&path, EXTENSION)?;
    Ok(path)
}

/// Pi's arguments for an agent, on a session id, which both starts a session
/// and resumes one: `prompt` goes after `--`, so one starting with `-` is
/// never taken for an option. Pi reads `@` as a file to attach even there, and
/// exits if it can't, so a prompt starting with one gets a space before it.
/// `--approve` trusts the worktree for this process, as Claude's pre-fill does,
/// so Pi doesn't stop to ask.
pub fn args(
    session_id: &str,
    model: Option<&str>,
    thinking: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["--session-id".to_owned(), session_id.to_owned()];
    if let Some(model) = model {
        args.extend(["--model".into(), model.to_owned()]);
    }
    args.extend(["--thinking".into(), thinking.to_owned(), "--approve".into()]);
    if let Some(prompt) = prompt {
        let prompt = if prompt.starts_with('@') {
            format!(" {prompt}")
        } else {
            prompt.to_owned()
        };
        args.extend(["--".into(), prompt]);
    }
    args
}

/// What `pi auth check` says of a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthCheck {
    Ready,
    /// It can't answer with this model, and why.
    NotReady(String),
}

/// Asks Pi itself whether it can answer with `model`: read-only, so a check
/// never spends a rotating refresh token.
pub fn auth_check_args(model: &str) -> Vec<String> {
    ["auth", "check", "--model", model, "--json", "--no-refresh"]
        .map(str::to_owned)
        .to_vec()
}

/// What `pi auth check --json` printed, `output` being all of it. None if it
/// doesn't say: then Pi is left to try, and the screen shows how that went. Only
/// `not_ready` says it can't: Pi says `invalid` of a model it doesn't know, and
/// this command loads no extensions, so that's every extension's provider too.
pub fn read_auth_check(model: &str, output: &str) -> Option<AuthCheck> {
    let said = output.lines().rev().find_map(|l| {
        serde_json::from_str::<Value>(l.trim())
            .ok()
            .filter(Value::is_object)
    })?;
    let provider = said["provider"].as_str().unwrap_or(model);
    match said["status"].as_str()? {
        "ready" => Some(AuthCheck::Ready),
        "not_ready" => Some(AuthCheck::NotReady(format!(
            "Pi isn't signed in to {provider} here, which {model} needs"
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b5c3f6e-1111-4000-8000-000000000001";

    #[test]
    fn sessions_go_where_pi_would_put_them() {
        assert_eq!(
            session_dir_name(Path::new(
                "/Users/me/VibeOS/worktrees/web.app/quiet-amber-otter"
            )),
            "--Users-me-VibeOS-worktrees-web.app-quiet-amber-otter--"
        );
        let home = PiHome::user(
            own_dir_in(None, Path::new("/Users/me")),
            Path::new("/Users/me/VibeOS/worktrees/web/q"),
        );
        assert_eq!(home.dir, Path::new("/Users/me/.pi/agent"));
        assert_eq!(
            home.sessions,
            Path::new("/Users/me/.pi/agent/sessions/--Users-me-VibeOS-worktrees-web-q--")
        );
    }

    #[test]
    fn transcripts_are_found_by_their_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let home = PiHome::at(tmp.path().join("agent"), tmp.path().join("sessions"));
        assert_eq!(home.transcript(ID), None, "no directory yet");
        home.prefill().unwrap();
        assert_eq!(home.transcript(ID), None, "nothing's been asked yet");
        let file = home
            .sessions
            .join(format!("2026-11-02T11-05-31-000Z_{ID}.jsonl"));
        fs::write(&file, "{}\n").unwrap();
        fs::write(
            home.sessions.join("2026-11-02T11-05-31-000Z_other.jsonl"),
            "",
        )
        .unwrap();
        fs::write(home.sessions.join(format!("{ID}.txt")), "").unwrap();
        assert_eq!(home.transcript(ID), Some(file));
        assert_eq!(session_of_file("t_a_b.jsonl"), Some("a_b"));
        assert_eq!(session_of_file("t.jsonl"), None);
        assert_eq!(session_of_file("t_.jsonl"), None);
        assert_eq!(session_of_file("t_a.txt"), None);
    }

    #[test]
    fn prefill_only_makes_the_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let home = PiHome::at(tmp.path().join("a/agent"), tmp.path().join("s/x"));
        // Again and at once from many threads: it has no temp file to race on.
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| home.prefill().unwrap());
            }
        });
        assert!(home.sessions.is_dir());
        let mode = fs::metadata(&home.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(fs::read_dir(&home.dir).unwrap().count(), 0);
    }

    #[test]
    fn an_owners_agent_dir_is_where_they_moved_it() {
        let home = Path::new("/Users/me");
        assert_eq!(own_dir_in(None, home), home.join(".pi/agent"));
        assert_eq!(
            own_dir_in(Some(String::new()), home),
            home.join(".pi/agent")
        );
        assert_eq!(own_dir_in(Some("/x/pi".into()), home), Path::new("/x/pi"));
        assert_eq!(own_dir_in(Some("~/x".into()), home), home.join("x"));
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn logins_name_providers_and_never_what_they_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let home = PiHome::at(tmp.path().to_owned(), tmp.path().join("s"));
        assert!(home.logins().is_empty());
        fs::write(
            tmp.path().join("auth.json"),
            r#"{"anthropic":{"type":"oauth","access":"sk-secret","refresh":"r","expires":1},
                "openai":{"type":"api_key","key":"sk-other"}}"#,
        )
        .unwrap();
        assert_eq!(home.logins(), ["anthropic", "openai"]);
        assert!(!format!("{:?}", home.logins()).contains("sk-"));
        // A name that can't be a provider's isn't repeated.
        fs::write(
            tmp.path().join("auth.json"),
            "{\"\\u001b[2Jx\":{\"type\":\"oauth\"},\"openai\":{\"type\":\"oauth\"}}",
        )
        .unwrap();
        assert_eq!(home.logins(), ["openai"]);
        fs::write(tmp.path().join("auth.json"), "not json").unwrap();
        assert!(home.logins().is_empty());
    }

    #[test]
    fn a_big_auth_json_is_never_read_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let file = fs::File::create(tmp.path().join("auth.json")).unwrap();
        file.set_len(1 << 30).unwrap();
        assert!(logins_in(tmp.path()).is_empty());
        assert!(
            read_plain_file(&tmp.path().join("auth.json"))
                .unwrap_err()
                .contains("MiB")
        );
        assert_eq!(read_plain_file(&tmp.path().join("nothing")), Ok(None));
    }

    #[test]
    fn a_link_in_the_agent_dir_never_has_the_mac_read_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("secret.json");
        fs::write(&elsewhere, r#"{"from-the-mac":{"type":"oauth"}}"#).unwrap();
        let shared = tmp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        std::os::unix::fs::symlink(&elsewhere, shared.join("auth.json")).unwrap();
        assert!(logins_in(&shared).is_empty());
    }

    #[test]
    fn only_a_login_for_the_right_provider_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let home = PiHome::at(tmp.path().to_owned(), tmp.path().join("s"));
        let model = Some("anthropic/claude-sonnet-4");
        // Nothing at all.
        assert!(
            home.login_missing(None, &[])
                .unwrap()
                .contains("any provider")
        );
        assert!(
            home.login_missing(model, &[])
                .unwrap()
                .contains("anthropic")
        );
        // A line of pi.env that isn't a provider's key is no login.
        let unrelated = pairs(&[("EDITOR_THEME", "dark")]);
        assert!(home.login_missing(None, &unrelated).is_some());
        assert!(home.login_missing(model, &unrelated).is_some());
        // Nor is a key for another provider, for a model named, nor an empty one.
        let other = pairs(&[("OPENROUTER_API_KEY", "k")]);
        assert_eq!(home.login_missing(None, &other), None);
        assert!(home.login_missing(model, &other).is_some());
        let empty = pairs(&[("ANTHROPIC_API_KEY", "")]);
        assert!(home.login_missing(model, &empty).is_some());
        assert_eq!(
            home.login_missing(model, &pairs(&[("ANTHROPIC_API_KEY", "k")])),
            None
        );
        // A models.json with no key in it is nothing to answer with.
        for useless in [
            "{}",
            r#"{"providers":{}}"#,
            r#"{"providers":{"anthropic":{}}}"#,
        ] {
            fs::write(tmp.path().join("models.json"), useless).unwrap();
            assert!(home.login_missing(None, &[]).is_some(), "{useless}");
            assert!(home.login_missing(model, &[]).is_some(), "{useless}");
        }
        fs::write(
            tmp.path().join("models.json"),
            r#"{"providers":{"anthropic":{"apiKey":"k"}}}"#,
        )
        .unwrap();
        assert_eq!(home.login_missing(model, &[]), None);
        fs::remove_file(tmp.path().join("models.json")).unwrap();
        // A login, for the wrong provider and the right one.
        fs::write(
            tmp.path().join("auth.json"),
            r#"{"openai":{"type":"oauth"}}"#,
        )
        .unwrap();
        assert_eq!(home.login_missing(None, &[]), None);
        assert!(
            home.login_missing(model, &[])
                .unwrap()
                .contains("anthropic")
        );
        fs::write(
            tmp.path().join("auth.json"),
            r#"{"anthropic":{"type":"oauth"}}"#,
        )
        .unwrap();
        assert_eq!(home.login_missing(model, &[]), None);
    }

    #[test]
    fn only_providers_pi_names_a_key_for_can_lack_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = PiHome::at(tmp.path().to_owned(), tmp.path().join("s"));
        // Every one of Pi 1.5's, by the key it reads.
        for (model, key) in [
            ("baseten/m", "BASETEN_API_KEY"),
            ("minimax-cn/m", "MINIMAX_CN_API_KEY"),
            ("moonshotai-cn/m", "MOONSHOT_API_KEY"),
            ("opencode-go/m", "OPENCODE_API_KEY"),
            ("zai-coding-cn/m", "ZAI_CODING_CN_API_KEY"),
            ("meta/m", "META_API_KEY"),
            ("openrouter/m", "OPENROUTER_API_KEY"),
        ] {
            assert!(home.login_missing(Some(model), &[]).is_some(), "{model}");
            assert_eq!(
                home.login_missing(Some(model), &pairs(&[(key, "k")])),
                None,
                "{model}"
            );
        }
        // One Pi doesn't name is an extension's or the owner's: it may try.
        assert_eq!(home.login_missing(Some("mine/m"), &[]), None);
        // As may those that sign in from what's around them.
        assert_eq!(home.login_missing(Some("google-vertex/m"), &[]), None);
        assert_eq!(home.login_missing(Some("amazon-bedrock/m"), &[]), None);
        // R4: and their own keys are the owner's, as anyone else's are.
        for (provider, name) in [
            ("amazon-bedrock", "AWS_PROFILE"),
            ("amazon-bedrock", "AWS_BEARER_TOKEN_BEDROCK"),
            ("amazon-bedrock", "AWS_WEB_IDENTITY_TOKEN_FILE"),
            ("google-vertex", "GOOGLE_CLOUD_API_KEY"),
            ("google-vertex", "GOOGLE_APPLICATION_CREDENTIALS"),
        ] {
            assert_eq!(
                providers_with_keys(&pairs(&[(name, "x")])),
                [provider],
                "{name}"
            );
        }
        // With no model named, what says one of them is meant counts too.
        assert!(home.login_missing(None, &[]).is_some());
        let role = pairs(&[("AWS_WEB_IDENTITY_TOKEN_FILE", "/t")]);
        assert_eq!(home.login_missing(None, &role), None);
    }

    #[test]
    fn a_vms_key_of_its_own_is_found_where_the_owner_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("models.json"),
            r#"{"providers":{"anthropic":{"apiKey":"k"}}}"#,
        )
        .unwrap();
        let home = PiHome::at(tmp.path().join("a"), tmp.path().join("s"));
        let model = Some("anthropic/m");
        assert!(home.login_missing(model, &[]).is_some());
        // In the agent dir it's only a link to a path in the guest.
        let home = home.with_config(config);
        assert_eq!(home.login_missing(model, &[]), None);
    }

    #[test]
    fn pis_own_check_is_read_for_what_it_says() {
        assert_eq!(
            auth_check_args("a/b"),
            ["auth", "check", "--model", "a/b", "--json", "--no-refresh"]
        );
        assert_eq!(
            read_auth_check(
                "a/b",
                r#"{"status":"ready","provider":"a","authType":"oauth"}"#
            ),
            Some(AuthCheck::Ready)
        );
        let AuthCheck::NotReady(why) = read_auth_check(
            "a/b",
            r#"{"status":"not_ready","provider":"a","reason":"credentials_not_configured"}"#,
        )
        .unwrap() else {
            panic!("not ready");
        };
        assert!(why.contains("signed in to a"), "{why}");
        // What it says of an extension's provider, as it loads none: let it try.
        let invalid = r#"{"status":"invalid","provider":"a/b","reason":"invalid_state"}"#;
        assert_eq!(read_auth_check("a/b", invalid), None);
        // A failure to run the check, a Pi too old to know, or anything else
        // says nothing.
        assert_eq!(read_auth_check("a/b", "Error: Failed\n"), None);
        assert_eq!(read_auth_check("a/b", "usage: pi ...\n"), None);
        assert_eq!(read_auth_check("a/b", ""), None);
    }

    #[test]
    fn forgetting_a_vm_takes_its_private_copy_of_the_login() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, config) = (tmp.path().join("pi-agent"), tmp.path().join("config"));
        let home =
            PiHome::at(dir.clone(), tmp.path().join("pi-sessions")).with_config(config.clone());
        home.prefill().unwrap();
        fs::create_dir_all(&config).unwrap();
        fs::write(dir.join("auth.json"), "{}").unwrap();
        home.forget().unwrap();
        assert!(!dir.exists());
        assert!(config.is_dir());
        home.forget().unwrap();
    }

    #[test]
    fn forgetting_keeps_the_shared_login() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agent");
        let home = PiHome::at(dir.clone(), dir.join("sessions/--w-q--"));
        home.prefill().unwrap();
        fs::write(dir.join("auth.json"), "{}").unwrap();
        fs::write(home.sessions.join(format!("t_{ID}.jsonl")), "").unwrap();
        let other = dir.join("sessions/--w-other--");
        fs::create_dir_all(&other).unwrap();
        home.forget().unwrap();
        assert!(!home.sessions.exists());
        assert!(dir.join("auth.json").is_file());
        assert!(other.is_dir());
        home.forget().unwrap();
    }

    #[test]
    fn stray_keys_never_get_through() {
        let inherited = [
            ("HOME", "/h"),
            ("PATH", "/bin"),
            ("LC_ALL", "C"),
            ("OPENROUTER_API_KEY", "stray"),
            ("ANTHROPIC_API_KEY", "stray"),
            ("OPENAI_API_KEY", "stray"),
            ("GEMINI_API_KEY", "stray"),
            ("XAI_API_KEY", "stray"),
            ("ANTHROPIC_OAUTH_TOKEN", "stray"),
            ("ANTHROPIC_AUTH_TOKEN", "stray"),
            ("AWS_SECRET_ACCESS_KEY", "stray"),
            ("GH_TOKEN", "stray"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "stray"),
            ("PI_CODING_AGENT_DIR", "/elsewhere"),
            ("VIBE_API_KEY", "stray"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let kept = allowed_env(inherited);
        let names: Vec<&str> = kept.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["HOME", "PATH", "LC_ALL"]);
        // Whatever the list comes to say, the token isn't on it.
        assert!(!is_allowed(TOKEN_ENV));
        for name in ALLOWED_ENV {
            let upper = name.to_ascii_uppercase();
            assert!(
                !upper.contains("KEY") && !upper.contains("TOKEN") && !upper.contains("SECRET"),
                "{name}"
            );
        }
    }

    #[test]
    fn the_env_file_is_what_the_owner_set_on_purpose() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(ENV_FILE);
        assert_eq!(read_env_file(&path), Ok(Vec::new()), "no file, nothing set");
        fs::write(
            &path,
            "# for Pi\n\nOPENROUTER_API_KEY=sk-or-1\nexport XAI_API_KEY=\"x y\"\nA_B='c=d'\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_env_file(&path).unwrap(),
            [
                ("A_B".to_owned(), "c=d".to_owned()),
                ("OPENROUTER_API_KEY".to_owned(), "sk-or-1".to_owned()),
                ("XAI_API_KEY".to_owned(), "x y".to_owned()),
            ]
        );
        for bad in ["no equals", "1A=x", "a b=x", "=x"] {
            fs::write(&path, bad).unwrap();
            assert!(
                read_env_file(&path).unwrap_err().contains("NAME=value"),
                "{bad}"
            );
        }
        // Set on purpose or not, the Claude token doesn't reach Pi.
        for refused in [
            TOKEN_ENV,
            ANTHROPIC_OAUTH_ENV,
            AGENT_DIR_ENV,
            SESSION_DIR_ENV,
            SERVICES_ENV,
            vibe_relay::local::SOCKET_ENV,
            vibe_relay::local::AGENT_ENV,
            vibe_relay::local::CACHE_ENV,
        ] {
            fs::write(&path, format!("{refused}=x")).unwrap();
            let err = read_env_file(&path).unwrap_err();
            assert!(err.contains("can't be set"), "{err}");
        }
        // Keys others can read aren't used.
        fs::write(&path, "A=b").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_env_file(&path).unwrap_err().contains("chmod 600"));
    }

    #[test]
    fn one_flag_starts_and_resumes() {
        assert_eq!(
            args(ID, Some("openai/gpt-5"), "high", Some("-fix the build")),
            [
                "--session-id",
                ID,
                "--model",
                "openai/gpt-5",
                "--thinking",
                "high",
                "--approve",
                "--",
                "-fix the build"
            ]
        );
        // On a resume there's no prompt, and with no model Pi uses its own.
        assert_eq!(
            args(ID, None, "low", None),
            ["--session-id", ID, "--thinking", "low", "--approve"]
        );
    }

    #[test]
    fn a_prompt_starting_with_at_is_a_prompt_and_not_a_file() {
        let args = args(ID, None, "low", Some("@src/main.rs fix this"));
        assert_eq!(args[args.len() - 2..], ["--", " @src/main.rs fix this"]);
        // Anywhere else it's only a character.
        let args = super::args(ID, None, "low", Some("look at @x"));
        assert_eq!(args[args.len() - 1], "look at @x");
    }

    #[test]
    fn a_vm_keeps_its_own_path_whatever_pi_env_says() {
        let env = pairs(&[
            ("PATH", "/opt/homebrew/bin"),
            ("HOME", "/Users/me"),
            ("A", "b"),
        ]);
        assert_eq!(for_guest(env), pairs(&[("A", "b")]));
    }

    /// The real wrapper, in a real shell, around a program that shows its
    /// environment.
    #[test]
    fn the_wrapper_keeps_a_claude_token_from_pi() {
        let tmp = tempfile::tempdir().unwrap();
        let shows = tmp.path().join("shows");
        fs::write(&shows, "#!/bin/sh\nprintf 'args:%s\\n' \"$@\"\nenv\n").unwrap();
        fs::set_permissions(&shows, fs::Permissions::from_mode(0o755)).unwrap();
        let wrapper = tmp.path().join("pi");
        assert!(WRAPPER.contains("/opt/vibeos/pi/pi"));
        fs::write(
            &wrapper,
            WRAPPER.replace("/opt/vibeos/pi/pi", &shows.to_string_lossy()),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let out = std::process::Command::new(&wrapper)
            .args(["--a b", "c"])
            .env(TOKEN_ENV, "sk-ant-secret")
            .env(ANTHROPIC_OAUTH_ENV, "sk-ant-other")
            .env("KEPT", "yes")
            .output()
            .unwrap();
        let out = String::from_utf8(out.stdout).unwrap();
        assert!(!out.contains("sk-ant"), "{out}");
        assert!(out.contains("KEPT=yes"), "{out}");
        assert!(out.contains("args:--a b\nargs:c\n"), "{out}");
    }

    #[test]
    fn services_only_when_there_are_any() {
        assert!(service_args(&pairs(&[("A", "b")]), "/x").is_empty());
        let fake = r#"{"mcpServers":{"vibe-fake":{"type":"stdio","command":"vibe","args":["mcp-proxy","fake"]}}}"#;
        let env = pairs(&[("A", "b"), (SERVICES_ENV, fake)]);
        assert_eq!(service_args(&env, "/m/v.ts"), ["-e", "/m/v.ts"]);
    }

    #[test]
    fn the_extension_registers_each_server_it_is_given() {
        for needle in [
            "registerMcpServer",
            "mcpServers",
            "VIBE_SERVICES",
            "\"direct\"",
        ] {
            assert!(EXTENSION.contains(needle), "{needle}");
        }
        // It holds no login, and writes no file.
        for needle in ["writeFile", "Bearer", "token"] {
            assert!(!EXTENSION.contains(needle), "{needle}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = write_extension(tmp.path()).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), EXTENSION);
    }
}
