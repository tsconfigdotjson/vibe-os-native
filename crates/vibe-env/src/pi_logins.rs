//! The owner's one Pi login for `linux` agents, and how each VM gets it (PRD:
//! Who gets which credentials; Exceptions).
//!
//! **Nothing a guest can write is shared between two VMs.** Pi runs a `!command`
//! it finds in `auth.json`, and has to write that file, so a directory two VMs
//! both write to lets one run code in the other. So each VM, and the toolbox,
//! has a private agent dir with its own `auth.json`, and `vibed` on the Mac is
//! the only one who moves anything between them:
//!
//! - **the store**, `~/VibeOS/linux-pi-logins/auth.json`, which no guest mounts
//!   and only this module writes;
//! - **a copy**, each private `auth.json`: [`Broker::sync`] compares it with what
//!   it last left there, takes what changed to the store if it may, and gives it
//!   what changed in the store.
//!
//! **What goes out is rebuilt, never passed along.** A copy is read as data from
//! a guest: at most [`pi::MAX_AUTH_FILE`], not through a link. A file that can't
//! be read as an object of logins quarantines the copy, as Node's `JSON.parse`
//! and `serde_json` needn't agree on what a strange file says. Within one, each
//! provider stands alone: an entry that isn't plain ([`plain_entry`]) goes
//! nowhere, and the rest are looked at as usual. The store only ever holds what
//! was parsed, written out afresh, so no `!command` or `$VARIABLE` can reach
//! another VM through it.
//!
//! **Who wins, per provider.** The toolbox is the owner's: a change there goes
//! through whatever it replaces, unless it's an OAuth login the store has already
//! moved on from, which is a stale read and not a sign-in. From an agent's VM only
//! what a sign-in or a token refresh would do goes through ([`Broker::judge`]), up
//! to limits an agent can't use to fill the store. What doesn't stays in that VM
//! alone. A login's content can't be checked from here, so this keeps a hostile VM
//! from running code in another, and not from offering it an account of its own
//! (PRD: threat model says so).
//!
//! **The owner's settings reach a VM read-only.** `~/VibeOS/linux-pi/` is a share
//! the VM can't write ([`link_config`]), with a link in its private agent dir for
//! each thing in it. A VM can replace those links, in its own directory only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use vibe_core::paths::Layout;

use crate::pi;

/// How long a copy's lock may stand before that's an alert. Pi itself takes one
/// over after 30 seconds, and holds it only for a token refresh.
const STUCK_AFTER_MS: u64 = 60_000;

/// How far ahead an agent's VM may say a login expires: past any provider's real
/// one, and short of one that would outlast every refresh for good.
const MAX_EXPIRES_AHEAD_MS: u64 = 400 * 24 * 60 * 60 * 1000;

/// The most one login may hold, from an agent's VM. Far past a real one, a
/// Copilot login's list of models included.
const MAX_AGENT_ENTRY: usize = 32 * 1024;

/// How many providers the store may have before an agent's VM can't add another.
/// Pi 1.5 knows about 50.
const MAX_AGENT_PROVIDERS: usize = 64;

/// How big the store may grow by a new provider from an agent's VM. A refresh of
/// a login already there isn't held to this, or a store filled by a hostile VM
/// would stop rotated tokens reaching the rest. The owner's sign-ins go past it too,
/// up to [`pi::MAX_AUTH_FILE`], which is what keeps a copy of it readable, and which
/// these limits together can't reach from agents alone: [`MAX_AGENT_ENTRY`] by
/// [`MAX_AGENT_PROVIDERS`] is 2 MiB.
const MAX_AGENT_STORE: usize = 256 * 1024;

/// How many old logins are remembered per provider, to know a stale one by.
const MAX_HISTORY: usize = 32;

/// How long the owner is shown what an agent did with a login.
const NOTICE_FOR_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// How many of those are kept, whatever an agent tries: first sign-ins are few,
/// being held to [`MAX_AGENT_PROVIDERS`], and what's refused makes way for them.
const MAX_NOTICES: usize = 2 * MAX_AGENT_PROVIDERS;

/// A share's names on its way to a VM aren't these, whatever the case on a Mac's
/// disk: the first two are the copy's own, and sessions go elsewhere.
const NEVER_LINKED: &[&str] = &["auth.json", "auth.json.lock", "sessions"];

/// Where it's kept who offered a login that's now in the store, so the owner can
/// find it and forget it, however long ago that was.
const OFFERED_FILE: &str = "offered.json";

/// Where the store remembers logins it has moved on from, by digest.
const HISTORY_FILE: &str = "history.json";

/// What doesn't say who a login is or where it goes: a provider's list of models
/// or scopes, which a refresh may change. Any other field of a login has to stay
/// as it was for a refresh from an agent's VM to go through.
const HARMLESS_FIELDS: &[&str] = &["availableModelIds", "scopes", "scope"];

/// One thing at a time touches the store: this tick's sweep, or a VM's launch.
static BROKER: Mutex<()> = Mutex::new(());

/// A private `auth.json` the store keeps in step with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// What to call it to the owner: an agent's ref, or "the toolbox".
    pub name: String,
    /// The private agent dir that holds it.
    pub dir: PathBuf,
    /// Where [`Broker::sync`] keeps what it knows of it, which no guest mounts.
    pub state: PathBuf,
    /// The toolbox, whose changes are the owner's own.
    pub owner: bool,
}

/// The copy in an agent's VM, whose directory is `agent_dir`.
pub fn vm_copy(agent: &str, agent_dir: &Path) -> Copy {
    Copy {
        name: agent.to_owned(),
        dir: agent_dir.join(pi::VM_AGENT_DIR),
        state: agent_dir.join(pi::VM_LOGIN_STATE),
        owner: false,
    }
}

/// The toolbox's copy, in `dir`, the owner's own. It has only this for a Pi: it
/// has `sudo` and the Claude token, so nothing of the owner's read-only share
/// runs there.
pub fn toolbox_copy(dir: &Path, store_dir: &Path) -> Copy {
    Copy {
        name: "the toolbox".to_owned(),
        dir: dir.to_owned(),
        state: store_dir.join("toolbox-state.json"),
        owner: true,
    }
}

/// What a sign-in from the app left, in a directory made for it that nothing else
/// has seen: the owner's own, as the toolbox's is, and fresh, so all that's in it is
/// new. It's looked at once, by [`Broker::sync`], and then it's gone.
pub fn sign_in_copy(dir: &Path, state: &Path) -> Copy {
    Copy {
        name: "the app's sign-in".to_owned(),
        dir: dir.to_owned(),
        state: state.to_owned(),
        owner: true,
    }
}

/// What [`Broker::sync`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Synced {
    Done(Report),
    /// Something holds its lock; the next tick tries.
    Busy,
    /// There's no such directory: the agent is gone, and only
    /// [`Broker::prepare_vm`] makes one. Nothing was written.
    Gone,
    /// It can't be trusted, and why: nothing was read from it or written to it.
    Quarantined(String),
}

/// What moved. Names of providers, never what they hold.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Taken from this copy to the store.
    pub taken: Vec<String>,
    /// Of those, the ones the store had nothing for.
    pub first: Vec<String>,
    /// Changed in this copy and left there, as a refresh that lost out would be.
    pub kept: Vec<String>,
    /// Signed out of in the toolbox, and so everywhere.
    pub removed: Vec<String>,
    /// Old logins this copy wrote back from a stale read, replaced by the store's.
    pub stale: Vec<String>,
    /// What this copy holds that isn't plain, which goes nowhere, and why. It
    /// stands for as long as the copy holds it.
    pub bad: BTreeMap<String, String>,
    /// Whether it holds a login under a name that's only odd. It stands for as long
    /// as the copy holds it, and is nothing to be alarmed by.
    pub odd_name: bool,
    /// What this copy offered and was refused, and why: said once.
    pub refused: BTreeMap<String, String>,
    /// The store couldn't be read, and the owner's sign-in replaced it. What was
    /// there is beside it, under this name.
    pub set_aside: Option<String>,
}

/// What [`Broker::sync`] knows of a copy, by provider. Digests, so it holds no
/// login.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// What was in the copy when last looked at.
    #[serde(default)]
    seen: BTreeMap<String, String>,
    /// What was in the store then, to give it only what's new since.
    #[serde(default)]
    given: BTreeMap<String, String>,
    /// When its lock was first found held, by this machine's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    busy_since: Option<u64>,
}

/// What the owner should hear about what an agent did with a login, kept so it
/// outlasts this tick and this `vibed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Notice {
    provider: String,
    text: String,
    at_ms: u64,
    /// A login every agent now uses, which nothing refused may push out of sight.
    #[serde(default)]
    first: bool,
}

/// A provider's login in `text`, and what's wrong with it, if anything: all of
/// `text` has to be an object of objects first. A number too big for this parser,
/// a BOM, or depth it won't follow are all things Pi's parser takes differently,
/// so none of that file can be trusted.
pub fn parse_copy(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| "its auth.json can't be read as JSON".to_owned())?;
    match value {
        Value::Object(entries) => Ok(entries),
        _ => Err("its auth.json isn't an object of logins".to_owned()),
    }
}

/// What's said of a name that's merely odd, as an owner's custom provider's might
/// be, with a space, a `:` or a `/`, or long. It's still not repeated, and doesn't
/// go further: only what's said of it differs.
pub const ODD_NAME: &str = "it has a login under a name VibeOS can't pass on, as a provider's name \
    here has to be letters, digits, '-', '_' or '.', up to 64 of them";

/// The entries of `text`, if every one is plain, as the store has to be.
pub fn parse_plain(text: &str) -> Result<Map<String, Value>, String> {
    let entries = parse_copy(text)?;
    for (name, entry) in &entries {
        plain_login(name, entry)?;
    }
    Ok(entries)
}

/// Whether `entry` is a login for a provider of this name, and if not, what to
/// tell the owner. A name that isn't one is never repeated: it's a guest's text,
/// on its way to the owner's terminal and Settings.
pub fn plain_login(name: &str, entry: &Value) -> Result<(), String> {
    if !pi::provider_name_ok(name) {
        return Err(if pi::name_means_only_itself(name) {
            ODD_NAME.to_owned()
        } else {
            "its auth.json has a login under a name no provider has".to_owned()
        });
    }
    plain_entry(entry).map_err(|why| format!("its login for {name} {why}"))
}

/// Whether `entry` is only what Pi's logins are (`auth/types.ts`), each value
/// taken for what it says. An API key may have a flat map beside it, as
/// Cloudflare's has its account's id (`cloudflare-auth.ts`), and may have no key
/// at all, as Bedrock's and Vertex's don't, which sign in from a profile, a
/// credential chain or a file (`amazon-bedrock.ts`, `google-vertex.ts`). An OAuth
/// login may have a provider's own fields: plain values, a list of strings, as
/// Copilot's models and ChatGPT's scopes are, or a flat map.
fn plain_entry(entry: &Value) -> Result<(), &'static str> {
    let Value::Object(fields) = entry else {
        return Err("isn't an object");
    };
    match fields.get("type").and_then(Value::as_str) {
        Some("api_key") => {
            for (name, value) in fields {
                match name.as_str() {
                    "type" => {}
                    "key" => plain_text(value.as_str().ok_or("has a key that isn't text")?, true)?,
                    "env" => plain_map(value)?,
                    _ => return Err("has more than a key"),
                }
            }
            Ok(())
        }
        Some("oauth") => {
            let is_text = |name| fields.get(name).is_some_and(Value::is_string);
            if !is_text("access") || !is_text("refresh") {
                return Err("has no tokens");
            }
            if !fields
                .get("expires")
                .and_then(Value::as_f64)
                .is_some_and(|at| at > 0.0)
            {
                return Err("has no expiry");
            }
            for (name, value) in fields {
                if !matches!(name.as_str(), "type" | "access" | "refresh" | "expires") {
                    plain_field(value)?;
                }
            }
            Ok(())
        }
        _ => Err("is of no kind Pi signs in with"),
    }
}

/// A provider's own field of an OAuth login.
fn plain_field(value: &Value) -> Result<(), &'static str> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        Value::String(text) => plain_text(text, false),
        Value::Array(items) => {
            for item in items {
                plain_text(item.as_str().ok_or("holds more than plain values")?, false)?;
            }
            Ok(())
        }
        Value::Object(_) => plain_map(value),
    }
}

fn plain_map(value: &Value) -> Result<(), &'static str> {
    let Value::Object(map) = value else {
        return Err("holds more than plain values");
    };
    for (name, item) in map {
        let name_ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
        if !name_ok {
            return Err("holds more than plain values");
        }
        plain_text(item.as_str().ok_or("holds more than plain values")?, false)?;
    }
    Ok(())
}

/// Why `key` couldn't be a login a VM is given, if it couldn't: what to say before Pi
/// is ever given it, as Pi on the Mac runs a `!command` as soon as it's saved.
pub fn plain_key(key: &str) -> Result<(), &'static str> {
    plain_text(key, true)
}

/// Whether `text` means only itself. Pi runs one starting with `!`, and fills in
/// `$NAME` and `${NAME}` from its environment (`resolve-config-value.ts`). A key
/// has no spaces in it either, though a list of scopes may.
fn plain_text(text: &str, key: bool) -> Result<(), &'static str> {
    if key && text.is_empty() {
        return Err("has an empty key");
    }
    if text.trim_start().starts_with('!') {
        return Err("would run a command");
    }
    if text.contains('$') {
        return Err("would read Pi's environment");
    }
    if text.chars().any(char::is_control) || key && text.chars().any(char::is_whitespace) {
        return Err("has a key with spaces in it");
    }
    Ok(())
}

fn digest(entry: &Value) -> String {
    let text = serde_json::to_string(entry).unwrap_or_default();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn digests(entries: &Map<String, Value>) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, entry)| (name.clone(), digest(entry)))
        .collect()
}

/// What says who a login is and where it goes: all but its tokens, its expiry
/// and what a refresh may change.
fn identity(entry: &Value) -> Map<String, Value> {
    let mut fields = entry.as_object().cloned().unwrap_or_default();
    for name in ["access", "refresh", "expires"] {
        fields.remove(name);
    }
    for name in HARMLESS_FIELDS {
        fields.remove(*name);
    }
    fields
}

fn expires(entry: &Value) -> f64 {
    entry["expires"].as_f64().unwrap_or(0.0)
}

/// Every string a login holds, however it holds it.
fn strings_of(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings_of(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings_of(v, out)),
        _ => {}
    }
}

/// Whether a login sends Pi somewhere of its own choosing. Pi takes a GitHub
/// Copilot token's `proxy-ep` for where to send every request, finding it with
/// the unanchored `/proxy-ep=([^;]+)/` (`github-copilot.ts`). So everything that
/// expression could find, in any string of the login, has to be a host of
/// Copilot's, whole, or under `enterprise`, the domain a login the owner signed in
/// to from the toolbox says it's with ([`owners_enterprise`]). No agent has a say in
/// that: its first sign-in can't name one, and a refresh can't change one.
fn leads_elsewhere(entry: &Value, enterprise: Option<&str>) -> bool {
    let mut texts = Vec::new();
    strings_of(entry, &mut texts);
    texts.iter().any(|text| {
        text.match_indices("proxy-ep=").any(|(at, needle)| {
            let rest = &text[at + needle.len()..];
            let host = rest.split(';').next().unwrap_or_default();
            !host.is_empty() && !copilot_host(host) && !enterprise_host(host, enterprise)
        })
    })
}

/// `^[a-z0-9.-]+\.githubcopilot\.com$`.
fn copilot_host(host: &str) -> bool {
    host.strip_suffix(".githubcopilot.com").is_some_and(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
    })
}

/// The enterprise to trust for a refresh of `name`: what its login in the store says,
/// only if it's the owner's own login and not one an agent offered, and only if it's
/// a domain with a dot in it, so it can't be `com` and take in all of `evil.com`.
fn owners_enterprise<'a>(
    name: &str,
    store: &'a Map<String, Value>,
    offered: &BTreeMap<String, String>,
) -> Option<&'a str> {
    if offered.contains_key(name) {
        return None;
    }
    store
        .get(name)?
        .get("enterpriseUrl")?
        .as_str()
        .filter(|domain| {
            domain.split('.').filter(|label| !label.is_empty()).count() >= 2
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        })
}

/// Whether a login's field says where it goes by its name alone, whatever its
/// value looks like: an enterprise named `corp` is as good as a URL.
fn field_names_a_place(field: &str) -> bool {
    let field = field.to_ascii_lowercase();
    [
        "url",
        "uri",
        "domain",
        "host",
        "endpoint",
        "enterprise",
        "gateway",
    ]
    .iter()
    .any(|word| field.contains(word))
}

/// Whether `host` is the enterprise's, as a GitHub Enterprise login's proxy is, in
/// the same plain characters.
fn enterprise_host(host: &str, enterprise: Option<&str>) -> bool {
    enterprise
        .filter(|domain| !domain.is_empty())
        .is_some_and(|domain| {
            host.strip_suffix(domain)
                .and_then(|name| name.strip_suffix('.'))
                .is_some_and(|name| {
                    !name.is_empty()
                        && name.chars().all(|c| {
                            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-'
                        })
                })
        })
}

/// How a new login from an agent's VM goes.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Take,
    /// It stays where it is, as a refresh that lost out does.
    Keep,
    /// It stays where it is, and the owner is told why.
    Refuse(String),
}

/// Pi's lock on `auth.json` in a directory: the directory `auth.json.lock`
/// beside it, as `proper-lockfile` makes it. Gone when dropped.
struct AuthLock(PathBuf);

impl Drop for AuthLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

enum Lock {
    Got(AuthLock),
    Held,
    Bad(String),
}

fn take_lock(dir: &Path) -> Lock {
    let path = dir.join("auth.json.lock");
    match fs::create_dir(&path) {
        Ok(()) => Lock::Got(AuthLock(path)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => Lock::Held,
            Ok(_) => Lock::Bad("its auth.json.lock isn't a lock".to_owned()),
            // Let go just now.
            Err(_) => Lock::Held,
        },
        Err(e) => Lock::Bad(format!("its auth.json can't be locked: {e}")),
    }
}

fn pretty(entries: &Map<String, Value>) -> String {
    format!("{:#}\n", Value::Object(entries.clone()))
}

/// Writes `auth.json` in `dir` in one step, so nothing reads half of it. A
/// rename replaces a link rather than following it, and `create_new` won't
/// follow one a guest left at the temporary name.
fn write_auth(dir: &Path, entries: &Map<String, Value>) -> io::Result<()> {
    write_file(dir, "auth.json", pretty(entries).as_bytes())
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    static TEMP: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{name}.vibe-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_file(&tmp);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, dir.join(name)));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn read_state(path: &Path) -> State {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_state(path: &Path, state: &State) -> io::Result<()> {
    let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("a state file with no name"))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    write_file(dir, name, &bytes)
}

/// The store: where it is, and what it needs to judge a login by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broker {
    store: PathBuf,
    config: PathBuf,
    env_file: PathBuf,
}

impl Broker {
    pub fn new(store: PathBuf, config: PathBuf, env_file: PathBuf) -> Broker {
        Broker {
            store,
            config,
            env_file,
        }
    }

    /// The one for this machine's `vibed`.
    pub fn of(layout: &Layout) -> Broker {
        Broker::new(
            layout.linux_pi_logins_dir(),
            layout.linux_pi_dir(),
            layout.pi_env_file(),
        )
    }

    /// The store's own directory.
    pub fn store_dir(&self) -> &Path {
        &self.store
    }

    /// The store, which only this module writes. It's held to the same rules all
    /// the same, in case of a hand edit: it reaches every VM.
    pub fn read_store(&self) -> io::Result<Map<String, Value>> {
        let bad = |why: String| {
            io::Error::other(format!("{}: {why}", self.store.join("auth.json").display()))
        };
        match pi::read_plain_file(&self.store.join("auth.json")).map_err(bad)? {
            None => Ok(Map::new()),
            Some(text) => parse_plain(&text).map_err(bad),
        }
    }

    /// The providers the owner gives a key of their own, which an agent's first
    /// sign-in can't be let take over: in Pi a stored login owns its provider, and
    /// a key is only looked at when there's none (`auth/resolve.ts`). None if
    /// that can't be told, when no first sign-in can be let through.
    fn own_keys(&self) -> Option<BTreeSet<String>> {
        let env = pi::read_env_file(&self.env_file).ok()?;
        let home =
            pi::PiHome::at(self.store.clone(), self.store.clone()).with_config(self.config.clone());
        Some(
            pi::providers_with_keys(&env)
                .into_iter()
                .chain(home.key_providers())
                .collect(),
        )
    }

    /// How a new login for `name` from an agent's VM goes, against the store.
    fn judge(
        &self,
        name: &str,
        new: &Value,
        store: &Map<String, Value>,
        own_keys: Option<&BTreeSet<String>>,
        offered: &BTreeMap<String, String>,
        now_ms: u64,
    ) -> Verdict {
        let refuse = |why: &str| Verdict::Refuse(why.to_owned());
        if leads_elsewhere(new, owners_enterprise(name, store, offered)) {
            return refuse("it would send requests somewhere of its own");
        }
        if serde_json::to_string(new).map_or(usize::MAX, |t| t.len()) > MAX_AGENT_ENTRY {
            return refuse("it's far bigger than a login is");
        }
        let oauth = new["type"] == "oauth";
        let ahead = (now_ms + MAX_EXPIRES_AHEAD_MS) as f64;
        if oauth && expires(new) > ahead {
            return refuse("its expiry is implausibly far off");
        }
        let mut after = store.clone();
        after.insert(name.to_owned(), new.clone());
        let Some(have) = store.get(name) else {
            // Only a new provider can fill the store.
            if serde_json::to_string(&after).map_or(usize::MAX, |t| t.len()) > MAX_AGENT_STORE {
                return refuse("the store has no more room for what agents offer");
            }
            if store.len() >= MAX_AGENT_PROVIDERS {
                return refuse("the store has no more room for what agents offer");
            }
            match own_keys {
                Some(keys) if keys.contains(name) => {
                    return refuse("you've given it a key of your own, which this would override");
                }
                None => return refuse("pi.env can't be read, to tell if you've given it a key"),
                Some(_) => {}
            }
            // With no key, it signs in from whatever's around it. That's the
            // owner's to choose for every agent, and an agent's offer isn't.
            if !oauth && !new.as_object().is_some_and(|f| f.contains_key("key")) {
                return refuse("it has no key, and would use what's around each agent");
            }
            // What a first sign-in says of where it goes could send others
            // anywhere: Cloudflare's account and gateway, Copilot's enterprise.
            let says_where = identity(new).iter().any(|(field, value)| {
                field != "type"
                    && (field == "env" || field_names_a_place(field) || names_a_place(value))
            });
            return if says_where {
                refuse("it says where its requests go")
            } else {
                Verdict::Take
            };
        };
        // What keeps every copy readable, whoever's growing it. Agents alone can't
        // get here.
        if pretty(&after).len() as u64 > pi::MAX_AUTH_FILE {
            return refuse("the store is as big as a copy of it can be");
        }
        if oauth
            && have["type"] == "oauth"
            && identity(have) == identity(new)
            && expires(new) > expires(have)
        {
            Verdict::Take
        } else {
            Verdict::Keep
        }
    }

    /// Keeps `copy` and the store in step, under the copy's lock. An error is
    /// about the store, and so about every VM; anything about the copy is in what
    /// it returns, and about it alone.
    pub fn sync(&self, copy: &Copy, now_ms: u64) -> io::Result<Synced> {
        let _one_at_a_time = BROKER.lock().unwrap_or_else(PoisonError::into_inner);
        if !copy.dir.is_dir() {
            return Ok(Synced::Gone);
        }
        pi::make_private(&self.store)?;
        let mut state = read_state(&copy.state);
        let _lock = match take_lock(&copy.dir) {
            Lock::Got(lock) => lock,
            Lock::Bad(why) => return Ok(Synced::Quarantined(why)),
            Lock::Held => {
                let since = *state.busy_since.get_or_insert(now_ms);
                self.write_copy_state(copy, &state)?;
                return Ok(if now_ms.saturating_sub(since) > STUCK_AFTER_MS {
                    Synced::Quarantined(
                        "its auth.json has been locked for over a minute".to_owned(),
                    )
                } else {
                    Synced::Busy
                });
            }
        };
        state.busy_since = None;
        let text = match pi::read_plain_file(&copy.dir.join("auth.json")) {
            Ok(text) => text,
            Err(why) => return Ok(Synced::Quarantined(format!("its auth.json {why}"))),
        };
        let mut all = match text.as_deref().map(parse_copy).transpose() {
            Ok(all) => all.unwrap_or_default(),
            Err(why) => return Ok(Synced::Quarantined(why)),
        };
        let mut report = Report::default();
        // Each provider stands alone: what isn't plain goes nowhere, and stays
        // as it was unless the store has something new to give in its place.
        let mut entries = Map::new();
        for (name, entry) in &all {
            match plain_login(name, entry) {
                Ok(()) => {
                    entries.insert(name.clone(), entry.clone());
                }
                Err(_) if pi::name_means_only_itself(name) && !pi::provider_name_ok(name) => {
                    report.odd_name = true;
                }
                Err(why) => {
                    report.bad.insert(name.clone(), why);
                }
            }
        }
        let changed = |name: &String, entry: &Value| state.seen.get(name) != Some(&digest(entry));

        let mut store = match self.read_store() {
            Ok(store) => store,
            // A store nothing can read mustn't stop the owner signing in again.
            Err(_) if copy.owner && entries.iter().any(|(n, e)| changed(n, e)) => {
                report.set_aside = Some(self.set_store_aside(now_ms)?);
                Map::new()
            }
            Err(e) => return Err(e),
        };
        let mut history = self.read_history();
        let own_keys = self.own_keys();
        let mut notices = self.read_notices(now_ms);
        let notices_before = notices.clone();
        let mut offered = self.read_offered();
        let offered_before = offered.clone();
        let mut store_changed = false;
        // A new entry that's one the store once held is a stale read being written
        // back, or something the owner forgot being offered again. Tokens never
        // come round again, so that's no sign-in; the owner's key may.
        let owner = copy.owner;
        let stale_read = |name: &str, entry: &Value, history: &BTreeMap<String, Vec<String>>| {
            (entry["type"] == "oauth" || !owner)
                && history
                    .get(name)
                    .is_some_and(|old| old.contains(&digest(entry)))
        };
        let mut restore = BTreeSet::new();

        // What the copy changed since it was last looked at.
        for (name, new) in &entries {
            if !changed(name, new) || store.get(name) == Some(new) {
                continue;
            }
            if stale_read(name, new, &history) {
                report.stale.push(name.clone());
                restore.insert(name.clone());
                continue;
            }
            let verdict = if copy.owner {
                Verdict::Take
            } else {
                self.judge(name, new, &store, own_keys.as_ref(), &offered, now_ms)
            };
            match verdict {
                Verdict::Take => {
                    let first = !store.contains_key(name);
                    if first {
                        report.first.push(name.clone());
                    }
                    if let Some(old) = store.insert(name.clone(), new.clone()) {
                        remember(&mut history, name, &old);
                    }
                    report.taken.push(name.clone());
                    store_changed = true;
                    if copy.owner {
                        notices.retain(|n| n.provider != *name);
                        offered.remove(name);
                    } else if first {
                        offered.insert(name.clone(), copy.name.clone());
                        notices.push(Notice {
                            provider: name.clone(),
                            text: format!(
                                "{} signed in to {name}, and every Pi agent here now uses that \
                                 login. If that wasn't you, sign in to it in Settings → Agents to \
                                 replace it.",
                                copy.name
                            ),
                            at_ms: now_ms,
                            first: true,
                        });
                    }
                }
                Verdict::Keep => report.kept.push(name.clone()),
                Verdict::Refuse(why) => {
                    let text = format!(
                        "{} offered a login for {name} that wasn't passed on, as {why}. It stays \
                         in that agent. If that wasn't you, something in it may be hostile.",
                        copy.name
                    );
                    // Offered again and again, it's said once.
                    notices.retain(|n| n.text != text);
                    notices.push(Notice {
                        provider: name.clone(),
                        text,
                        at_ms: now_ms,
                        first: false,
                    });
                    report.refused.insert(name.clone(), why);
                }
            }
        }
        // What went from the copy. One sign-out is a sign-out, the owner's being
        // everyone's, and an agent's its own until there's a new login to give it.
        // Several at once is a file that was read as missing and written as `{}`.
        let gone: Vec<String> = state
            .seen
            .keys()
            .filter(|n| !entries.contains_key(*n) && !report.bad.contains_key(*n))
            .cloned()
            .collect();
        if gone.len() > 1 {
            report.stale.extend(gone.iter().cloned());
            restore.extend(gone);
        } else if copy.owner {
            for name in gone {
                if let Some(old) = store.remove(&name) {
                    remember(&mut history, &name, &old);
                    notices.retain(|n| n.provider != name);
                    offered.remove(&name);
                    report.removed.push(name);
                    store_changed = true;
                }
            }
        }
        if copy.owner && report.taken.is_empty() && report.removed.is_empty() {
            // Nothing of the owner's to carry past the limit.
        } else if copy.owner {
            // It always gets through: what agents offered makes way for it.
            while pretty(&store).len() as u64 > pi::MAX_AUTH_FILE {
                let largest = store
                    .iter()
                    .filter(|(name, _)| !entries.contains_key(*name))
                    .max_by_key(|(_, v)| serde_json::to_string(v).map_or(0, |t| t.len()))
                    .map(|(name, _)| name.clone());
                let Some(name) = largest else {
                    return Ok(Synced::Quarantined(format!(
                        "its logins come to over {} MiB, too much to pass on",
                        pi::MAX_AUTH_FILE >> 20
                    )));
                };
                store.remove(&name);
            }
        }
        if store_changed {
            write_auth(&self.store, &store)?;
            self.write_history(&history)?;
        }
        trim_notices(&mut notices);
        if notices != notices_before {
            self.write_json(NOTICES_FILE, &notices)?;
        }
        if offered != offered_before {
            self.write_json(OFFERED_FILE, &offered)?;
        }

        // What the store has that this copy hasn't been given.
        let given = digests(&store);
        let mut names: BTreeSet<&String> = state.given.keys().chain(given.keys()).collect();
        names.extend(restore.iter());
        for name in names {
            if state.given.get(name) == given.get(name) && !restore.contains(name) {
                continue;
            }
            match store.get(name) {
                Some(entry) => {
                    all.insert(name.clone(), entry.clone());
                    entries.insert(name.clone(), entry.clone());
                    report.bad.remove(name);
                }
                None => {
                    all.remove(name);
                    entries.remove(name);
                }
            }
        }
        let on_disk = text.as_deref().map(parse_copy).transpose().ok().flatten();
        if on_disk.as_ref() != Some(&all)
            && let Err(e) = write_auth(&copy.dir, &all)
        {
            return Ok(Synced::Quarantined(format!(
                "its auth.json can't be written: {e}"
            )));
        }
        state.seen = digests(&entries);
        state.given = given;
        self.write_copy_state(copy, &state)?;
        Ok(Synced::Done(report))
    }

    /// Notes what's known of a copy. If its directory went meanwhile, so did the
    /// agent, and there's nothing to say.
    fn write_copy_state(&self, copy: &Copy, state: &State) -> io::Result<()> {
        match write_state(&copy.state, state) {
            Err(_) if !copy.dir.is_dir() => Ok(()),
            done => done,
        }
    }

    /// Moves an unreadable store out of the way and says where, for the owner to
    /// look at. Nothing's lost, and nothing of it is used.
    fn set_store_aside(&self, now_ms: u64) -> io::Result<String> {
        let name = format!("auth.json.unreadable-{now_ms}");
        fs::rename(self.store.join("auth.json"), self.store.join(&name))?;
        Ok(name)
    }

    fn read_json<T: serde::de::DeserializeOwned + Default>(&self, name: &str) -> T {
        fs::read(self.store.join(name))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        write_file(&self.store, name, &bytes)
    }

    fn read_history(&self) -> BTreeMap<String, Vec<String>> {
        self.read_json(HISTORY_FILE)
    }

    fn write_history(&self, history: &BTreeMap<String, Vec<String>>) -> io::Result<()> {
        self.write_json(HISTORY_FILE, history)
    }

    fn read_offered(&self) -> BTreeMap<String, String> {
        self.read_json(OFFERED_FILE)
    }

    fn read_notices(&self, now_ms: u64) -> Vec<Notice> {
        let mut notices: Vec<Notice> = self.read_json(NOTICES_FILE);
        notices.retain(|n| now_ms.saturating_sub(n.at_ms) < NOTICE_FOR_MS);
        notices
    }

    /// The logins in the store that came from an agent and not the owner, by
    /// provider, with the agent. They stay listed until the owner signs in to
    /// that provider from the toolbox, or forgets it.
    pub fn offered(&self) -> BTreeMap<String, String> {
        let _one_at_a_time = BROKER.lock().unwrap_or_else(PoisonError::into_inner);
        self.read_offered()
    }

    /// Takes these providers' logins out of the store, and so out of every copy at
    /// its next look, whoever they came from, and says which there were. Offered
    /// again as they were, they're refused; anything new is a new offer, so an
    /// agent that's offering junk has to be stopped too.
    pub fn forget(&self, providers: &[String], now_ms: u64) -> io::Result<Vec<String>> {
        let _one_at_a_time = BROKER.lock().unwrap_or_else(PoisonError::into_inner);
        let mut store = self.read_store()?;
        let mut history = self.read_history();
        let mut notices = self.read_notices(now_ms);
        let mut offered = self.read_offered();
        let mut gone = Vec::new();
        for name in providers {
            if let Some(old) = store.remove(name) {
                remember(&mut history, name, &old);
                gone.push(name.clone());
            }
            notices.retain(|n| n.provider != *name);
            offered.remove(name);
        }
        if !gone.is_empty() {
            write_auth(&self.store, &store)?;
            self.write_history(&history)?;
        }
        self.write_json(NOTICES_FILE, &notices)?;
        self.write_json(OFFERED_FILE, &offered)?;
        Ok(gone)
    }

    /// What's wrong with what the broker keeps beside the store, which it goes on
    /// without rather than stopping: each of these only makes it know less.
    pub fn problems(&self) -> Vec<String> {
        let _one_at_a_time = BROKER.lock().unwrap_or_else(PoisonError::into_inner);
        let mut problems = Vec::new();
        let unreadable = |name: &str| {
            fs::read(self.store.join(name))
                .is_ok_and(|bytes| serde_json::from_slice::<Value>(&bytes).is_err())
        };
        for (name, what) in [
            (NOTICES_FILE, "what agents did with a login may be missing"),
            (
                HISTORY_FILE,
                "an old login written back may be taken for a new one",
            ),
            (OFFERED_FILE, "which logins came from agents may be missing"),
        ] {
            if unreadable(name) {
                problems.push(format!(
                    "{} can't be read, so {what}. Remove it if this stays.",
                    self.store.join(name).display()
                ));
            }
        }
        problems
    }

    /// What the owner should hear of what agents did with a login: a first
    /// sign-in, or what was refused. It stands until they sign in to that provider
    /// from the toolbox, or for [`NOTICE_FOR_MS`], and outlasts a restart.
    pub fn notices(&self, now_ms: u64) -> Vec<String> {
        let _one_at_a_time = BROKER.lock().unwrap_or_else(PoisonError::into_inner);
        self.read_notices(now_ms)
            .into_iter()
            .map(|n| n.text)
            .collect()
    }

    /// Readies the private agent dir of a VM about to run, with the owner's
    /// settings linked. This alone makes the directory. `running` if its VM already
    /// is, when a lock there may be Pi's own.
    pub fn prepare_dir(&self, copy: &Copy, running: bool) -> io::Result<()> {
        pi::make_private(&self.config)?;
        pi::make_private(&copy.dir)?;
        link_config(&self.config, &copy.dir)?;
        if !running {
            clear_lock(&copy.dir);
        }
        Ok(())
    }

    /// [`Broker::prepare_dir`], and the store's logins given to it.
    pub fn prepare_vm(&self, copy: &Copy, running: bool, now_ms: u64) -> io::Result<Synced> {
        self.prepare_dir(copy, running)?;
        self.sync(copy, now_ms)
    }
}

/// Where what the owner should hear is kept, in the store's directory.
pub const NOTICES_FILE: &str = "notices.json";

/// Where a golden's offered login is noted, in the store's directory.
pub const OFFERED_FILE_NAME: &str = OFFERED_FILE;

/// Oldest first, what's refused making way for what's in use.
fn trim_notices(notices: &mut Vec<Notice>) {
    while notices.len() > MAX_NOTICES {
        let oldest = notices.iter().position(|n| !n.first).unwrap_or(0);
        notices.remove(oldest);
    }
}

fn remember(history: &mut BTreeMap<String, Vec<String>>, name: &str, old: &Value) {
    let list = history.entry(name.to_owned()).or_default();
    list.push(digest(old));
    let at = list.len().saturating_sub(MAX_HISTORY);
    list.drain(..at);
}

fn names_a_place(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains('.') || s.contains("://"),
        Value::Array(_) | Value::Object(_) => true,
        _ => false,
    }
}

/// A lock with nothing left to hold it, before a VM boots: its Pi can't be
/// running. A guest may have left anything there.
pub fn clear_lock(dir: &Path) {
    let path = dir.join("auth.json.lock");
    if fs::remove_dir(&path).is_err() {
        let _ = fs::remove_file(&path);
    }
}

/// Gives the private agent dir `dir` a link to each thing the owner keeps in
/// `config`, through the read-only share, and takes back links to what's gone.
/// Whatever the VM put there itself stays, it being the VM's own. Nothing here
/// follows a link, and a name Pi comes to use later is that VM's alone unless
/// the owner has one.
pub fn link_config(config: &Path, dir: &Path) -> io::Result<()> {
    let mut names = Vec::new();
    for entry in fs::read_dir(config)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str().map(str::to_owned) else {
            continue;
        };
        if !NEVER_LINKED.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    let guest = Path::new(pi::GUEST_CONFIG_DIR);
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let gone = !entry
            .file_name()
            .to_str()
            .is_some_and(|n| names.iter().any(|name| name == n));
        if target.starts_with(guest) && gone {
            fs::remove_file(entry.path())?;
        }
    }
    for name in &names {
        match symlink(guest.join(name), dir.join(name)) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// This machine's clock, as [`Broker::sync`] wants it.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
#[path = "pi_logins/tests.rs"]
mod tests;
