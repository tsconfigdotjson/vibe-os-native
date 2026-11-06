//! MCP on a Linux host's tailnet name, by the toggle in Settings → Hosts: `mcp_tailscale`
//! in `host.toml`, and `/mcp` on Tailscale Serve, at `https://<machine>.<tailnet>.ts.net/mcp`
//! (`mcp.serve.get`, `mcp.serve.set`).
//!
//! Serve points at the Tailscale listener, never at loopback: the loopback listener only
//! takes loopback `Host` headers (`vibe_mcp::http`), and Serve passes the `ts.net` name.
//!
//! What's run here is one `tailscale` command with one path. Tailscale's install and login
//! stay the owner's, Funnel is never turned on, and nothing else in Serve is touched: a `/mcp`
//! that points elsewhere is left where it is, with a word to say so. `tailscale serve` can
//! say nothing's wrong and still not have done it, or wait for someone to act on a link, so
//! what it did is always read back afterwards, and it's only given so long.

use std::ffi::{OsStr, OsString};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use vibe_core::limits;
use vibe_core::model::EventKind;
use vibe_core::rpc::{ErrorCode, ErrorObject, HostParams, McpServe, McpServeSetParams};
use vibe_mcp::PATH as SERVE_PATH;

use crate::core::{Core, error, invalid, limits_error, lock};

/// The HTTPS port `/mcp` is served on: the one a URL needs no port for.
const SERVE_PORT: u16 = 443;
/// `--https=443`, in the commands.
const HTTPS_FLAG: &str = "--https=443";
/// `--set-path=/mcp`, in the commands. `SERVE_PATH` is `/mcp`: a test holds them together.
const PATH_FLAG: &str = "--set-path=/mcp";
/// `/mcp/`, which is also `/mcp` to Tailscale: it's what a directory served at `/mcp` is kept
/// as, and setting `/mcp` replaces it without a word.
const SLASHED_PATH: &str = "/mcp/";
/// How long one `tailscale` command may take, as it can take forever: `serve` waits for HTTPS
/// to be turned on. A change is at most 7 commands (a look, the change, a look, and the look
/// that answers), which together have to fit in what a forwarded call gets.
pub const TAILSCALE_WAIT: Duration = Duration::from_secs(15);
/// How much of what `tailscale` says is kept: more than any status, on any tailnet, as only
/// this node is asked for. What's cut off doesn't parse, and is treated as unreadable.
const MAX_OUTPUT: usize = 4 << 20;
/// How much of what it says goes into a message.
const MAX_SAID: usize = 2000;

/// The line that takes `path` off Serve by hand, leaving the rest alone.
fn off_line(path: &str) -> String {
    format!("tailscale serve {HTTPS_FLAG} --set-path={path} off")
}

/// What `tailscale` did when run.
#[derive(Debug, Default)]
struct Ran {
    /// It exited with success.
    ok: bool,
    /// It was still going when its time ran out, so it was killed.
    timed_out: bool,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// All it said, for a person: what it prints, then its errors, cut to a sensible length.
    fn said(&self) -> String {
        let all = format!("{}\n{}", self.stdout.trim(), self.stderr.trim());
        let all = all.trim();
        match all.char_indices().nth(MAX_SAID) {
            Some((end, _)) => format!("{}…", &all[..end]),
            None => all.to_owned(),
        }
    }
}

/// Keeps what's read from a pipe as it comes, so what was said before a command is killed is
/// still there. Past [`MAX_OUTPUT`] it's read and dropped, so the command doesn't block.
fn collect<R>(pipe: Option<R>, into: Arc<Mutex<Vec<u8>>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk).await {
            if n == 0 {
                return;
            }
            let mut kept = lock(&into);
            let room = MAX_OUTPUT.saturating_sub(kept.len());
            kept.extend_from_slice(&chunk[..n.min(room)]);
        }
    })
}

/// Runs `program args` with no stdin, for up to `wait`. A command that outlasts it is killed,
/// with whatever it started: `tailscale serve` waits for HTTPS to be turned on, and that's
/// for someone to do. What it said up to then is kept, as that says how.
async fn run(program: &OsStr, args: &[&str], wait: Duration) -> std::io::Result<Ran> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()?;
    let (out, err) = (Arc::default(), Arc::default());
    let mut readers = [
        collect(child.stdout.take(), Arc::clone(&out)),
        collect(child.stderr.take(), Arc::clone(&err)),
    ];
    let status = tokio::time::timeout(wait, child.wait()).await;
    let timed_out = status.is_err();
    if timed_out {
        if let Some(group) = child.id().and_then(|id| i32::try_from(id).ok()) {
            // SAFETY: only signals the process group this command was made the leader of,
            // which can't have been reused: it hasn't been waited on yet.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        let _ = child.wait().await;
    }
    // Something it left behind may hold a pipe open: what's been said by now will do.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        for reader in &mut readers {
            let _ = reader.await;
        }
    })
    .await;
    readers.iter().for_each(|r| r.abort());
    let text = |bytes: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&lock(bytes)).into_owned();
    Ok(Ran {
        ok: matches!(status, Ok(Ok(s)) if s.success()),
        timed_out,
        stdout: text(&out),
        stderr: text(&err),
    })
}

/// What's at `/mcp` on this node's `:443`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServePath {
    Absent,
    /// This host's MCP, by its Tailscale address and port.
    Ours,
    /// Anything else: as it should be described to the owner, and the path it's at, or none
    /// if it goes with a command still running.
    Foreign(String, Option<&'static str>),
}

/// How things stand with Tailscale here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Look {
    /// This node's name, with no trailing dot.
    name: String,
    path: ServePath,
    /// Whether Funnel is on for this node's `:443`. `/mcp` there would be public.
    funnel: bool,
    /// The address to point Serve at: one MCP can listen on that Tailscale says is its own.
    /// An address in its range on another interface, as a cloud network can have, isn't.
    listen_on: Option<IpAddr>,
}

impl Look {
    fn url(&self) -> String {
        format!("https://{}{SERVE_PATH}", self.name)
    }
}

/// This node's name and addresses, from `tailscale status --json`. Only with it up and
/// logged in: the rest isn't this code's to do.
fn parse_status(json: &str) -> Result<(String, Vec<IpAddr>), String> {
    let status: Value =
        serde_json::from_str(json).map_err(|e| format!("its status doesn't read: {e}"))?;
    let state = status["BackendState"].as_str().unwrap_or("unknown");
    if state != "Running" {
        return Err(format!(
            "Tailscale is {state} on this host. It needs to be running and logged in, which \
             VibeOS leaves to you"
        ));
    }
    let name = status["Self"]["DNSName"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_owned();
    if name.is_empty() {
        return Err(
            "Tailscale has no name for this host. Turn on MagicDNS for your tailnet".to_owned(),
        );
    }
    let addresses = [&status["TailscaleIPs"], &status["Self"]["TailscaleIPs"]]
        .into_iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|ip| ip.as_str()?.parse().ok())
        .collect();
    Ok((name, addresses))
}

/// A handler, as the owner should see it.
fn describe_handler(handler: &Value) -> String {
    for (field, what) in [("Proxy", ""), ("Path", "the path "), ("Text", "some text")] {
        match handler[field].as_str() {
            Some(_) if field == "Text" => return what.to_owned(),
            Some(target) => return format!("{what}{target}"),
            None => {}
        }
    }
    "something else".to_owned()
}

/// What `serve` (`tailscale serve status --json`) has at `/mcp` on `name`, set in the
/// background or by a command still running in the foreground, and whether Funnel's on there.
/// Whatever isn't `targets`, this host's own, isn't this code's to change.
fn parse_serve(json: &str, name: &str, targets: &[String]) -> Result<(ServePath, bool), String> {
    let serve: Value =
        serde_json::from_str(json).map_err(|e| format!("what it serves doesn't read: {e}"))?;
    if !serve.is_object() && !serve.is_null() {
        return Err("what it serves doesn't read: it isn't an object".to_owned());
    }
    let site = format!("{name}:{SERVE_PORT}");
    let handler_in = |config: &Value, path: &str| -> Option<Value> {
        config["Web"][&site]["Handlers"]
            .get(path)
            .filter(|h| !h.is_null())
            .cloned()
    };
    let funnel_in =
        |config: &Value| -> bool { config["AllowFunnel"][&site].as_bool().unwrap_or(false) };
    let foreground: Vec<&Value> = serve["Foreground"]
        .as_object()
        .map(|sessions| sessions.values().collect())
        .unwrap_or_default();
    let funnel = funnel_in(&serve) || foreground.iter().any(|c| funnel_in(c));
    let in_foreground = foreground
        .iter()
        .find_map(|c| handler_in(c, SERVE_PATH).or_else(|| handler_in(c, SLASHED_PATH)));
    if let Some(handler) = in_foreground {
        let held = format!(
            "{}, from a `tailscale serve` still running in the foreground",
            describe_handler(&handler)
        );
        return Ok((ServePath::Foreign(held, None), funnel));
    }
    // Never this host's, whatever its target: setting `/mcp` would replace it.
    if let Some(handler) = handler_in(&serve, SLASHED_PATH) {
        let what = describe_handler(&handler);
        return Ok((ServePath::Foreign(what, Some(SLASHED_PATH)), funnel));
    }
    let path = match handler_in(&serve, SERVE_PATH) {
        None => ServePath::Absent,
        Some(handler) => match handler["Proxy"].as_str() {
            Some(target) if targets.iter().any(|t| t == target) => ServePath::Ours,
            _ => ServePath::Foreign(describe_handler(&handler), Some(SERVE_PATH)),
        },
    };
    Ok((path, funnel))
}

/// Why a change to Serve didn't take.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// This user may not change Serve: `sudo tailscale set --operator=<user>` allows it.
    Denied,
    /// The tailnet has HTTPS, or Serve, turned off, and maybe where to turn it on.
    NoHttps(Option<String>),
    Other,
}

/// Which it was, from what `tailscale` said.
fn classify(said: &str) -> Refusal {
    let low = said.to_lowercase();
    if [
        "access denied",
        "permission denied",
        "--operator",
        "no permission",
    ]
    .iter()
    .any(|w| low.contains(w))
    {
        return Refusal::Denied;
    }
    // Only with these words: a link can be to anything, and it prints this node's own.
    let https_off = [
        "serve is not enabled",
        "https is not enabled",
        "https cert support is not enabled",
        "tls certificate support is not enabled",
        "enable https",
    ];
    if https_off.iter().any(|w| low.contains(w)) {
        let link = said
            .split_whitespace()
            .find(|w| w.starts_with("https://") && !w.contains(".ts.net"))
            .map(|w| w.trim_end_matches(['.', ',', ')', '"', '\'']).to_owned());
        return Refusal::NoHttps(link);
    }
    Refusal::Other
}

/// Where `tailscale` is.
enum Program {
    /// A Mac: its toggle is its Tailscale address (`mcp.set`), and nothing else.
    NotHere,
    /// Not installed, which isn't VibeOS's to do.
    NotInstalled,
    At(OsString),
}

impl Core {
    fn tailscale_program(&self) -> Program {
        let Some(program) = &self.tailscale_cli else {
            return Program::NotHere;
        };
        if Path::new(program).is_absolute() {
            return Program::At(program.clone());
        }
        let name = program.to_string_lossy();
        let path = std::env::var_os("PATH");
        let found = crate::core::find_tool(&name, path.as_deref(), Some(self.layout.home()))
            .or_else(|| {
                // A service's PATH can be short, and this is where its packages put it.
                ["/usr/bin", "/usr/sbin"]
                    .iter()
                    .map(|d| Path::new(d).join(&*name))
                    .find(|p| p.is_file())
            });
        found.map_or(Program::NotInstalled, |p| Program::At(p.into_os_string()))
    }

    /// This host's Tailscale addresses, as MCP's listeners go by them.
    fn listener_addresses(&self) -> Vec<IpAddr> {
        (self.tailscale)()
            .into_iter()
            .filter(|ip| vibe_mcp::net::is_tailscale(*ip))
            .collect()
    }

    /// How things stand with Tailscale, as it says now. Anything that doesn't read is an
    /// error: nothing is changed on a guess.
    async fn serve_look(&self, program: &OsStr, port: u16) -> Result<Look, ErrorObject> {
        let failed = |why: String| error(ErrorCode::CommandFailed, sentence_end(&why));
        let read = |args: &'static [&'static str]| async move {
            let ran = run(program, args, self.tailscale_wait)
                .await
                .map_err(|e| failed(format!("Can't run tailscale: {e}")))?;
            if ran.ok {
                return Ok(ran.stdout);
            }
            Err(failed(if ran.timed_out {
                format!(
                    "Tailscale didn't answer in {} s",
                    self.tailscale_wait.as_secs()
                )
            } else {
                format!("Tailscale can't say how it stands here: {}", ran.said())
            }))
        };
        let status = read(&["status", "--json", "--peers=false"]).await?;
        let (name, addresses) = parse_status(&status).map_err(failed)?;
        let targets: Vec<String> = addresses
            .iter()
            .map(|ip| format_target(*ip, port))
            .collect();
        let listen_on = self
            .listener_addresses()
            .into_iter()
            .find(|ip| addresses.contains(ip));
        let serve = read(&["serve", "status", "--json"]).await?;
        let (path, funnel) = parse_serve(&serve, &name, &targets).map_err(failed)?;
        Ok(Look {
            name,
            path,
            funnel,
            listen_on,
        })
    }

    /// `mcp.serve.get`: how this host's MCP stands on its tailnet name, as Tailscale says
    /// now, for the toggle. Only reads. Tailscale being unavailable is an answer, not an
    /// error: the toggle says so.
    pub async fn mcp_serve(&self, p: HostParams) -> Result<McpServe, ErrorObject> {
        self.host(p.host.as_deref())?;
        let l = limits::load(&self.layout.host_config()).map_err(limits_error)?;
        let unavailable = |why: String| McpServe {
            on: false,
            available: false,
            url: None,
            note: Some(why),
        };
        let program = match self.tailscale_program() {
            Program::NotHere => return Ok(unavailable(NOT_HERE.to_owned())),
            Program::NotInstalled => return Ok(unavailable(NOT_INSTALLED.to_owned())),
            Program::At(program) => program,
        };
        let look = match self.serve_look(&program, l.mcp_port).await {
            Ok(look) => look,
            Err(e) => return Ok(unavailable(e.message)),
        };
        let on = look.path == ServePath::Ours && l.mcp_tailscale;
        let listening = look.listen_on.is_some();
        Ok(McpServe {
            on,
            available: listening || on,
            url: on.then(|| look.url()),
            note: stands_note(&look, l.mcp_tailscale, listening),
        })
    }

    /// `mcp.serve.set`: turns MCP on this host's tailnet name on or off, `mcp_tailscale` and
    /// `/mcp` on Serve together. One change at a time. Whatever stops it leaves things as they
    /// were: a `/mcp` that isn't this host's stays, and so does `mcp_tailscale`. Serve is
    /// changed first, so a refused turn-on never opens the Tailscale listener, and if `vibed`
    /// goes down between the two, what's left is one the toggle says how to finish.
    pub async fn set_mcp_serve(&self, p: McpServeSetParams) -> Result<McpServe, ErrorObject> {
        self.host(p.host.as_deref())?;
        let _one_at_a_time = self.serve_lock.lock().await;
        // A broken host.toml stops it here, before Serve is touched.
        let l = limits::load(&self.layout.host_config()).map_err(limits_error)?;
        let program = self.tailscale_program();
        if p.on {
            self.serve_on(&program, l.mcp_port, l.mcp_tailscale).await?;
        } else {
            self.serve_off(&program, l.mcp_port, l.mcp_tailscale)
                .await?;
        }
        // It's as asked, so whatever was refused before isn't waiting for `sudo` any more.
        self.operator_allowed();
        self.publish(self.event(EventKind::McpChanged));
        self.mcp_serve(HostParams::default()).await
    }

    async fn serve_on(
        &self,
        program: &Program,
        port: u16,
        was_on: bool,
    ) -> Result<(), ErrorObject> {
        let program = match program {
            Program::NotHere => return Err(invalid(NOT_HERE)),
            Program::NotInstalled => {
                return Err(error(ErrorCode::CommandFailed, NOT_INSTALLED));
            }
            Program::At(program) => program,
        };
        let look = self.serve_look(program, port).await?;
        if let ServePath::Foreign(what, path) = &look.path {
            return Err(error(ErrorCode::Conflict, foreign_note(what, *path)));
        }
        if look.funnel {
            return Err(error(ErrorCode::Conflict, funnel_note(&look)));
        }
        let Some(ip) = look.listen_on else {
            return Err(error(ErrorCode::CommandFailed, NO_ADDRESS));
        };
        // As it should be already, Serve isn't written to.
        let adding = look.path != ServePath::Ours;
        if adding {
            let target = format_target(ip, port);
            let set = ["serve", "--bg", HTTPS_FLAG, PATH_FLAG, target.as_str()];
            self.change_serve(program, &set, port, ServePath::Ours)
                .await?;
        }
        if !was_on && let Err(e) = self.write_mcp_tailscale(true) {
            if adding {
                // Back as it was. The write's error is the one to report.
                let off = ["serve", HTTPS_FLAG, PATH_FLAG, "off"];
                if let Err(undone) = self
                    .change_serve(program, &off, port, ServePath::Absent)
                    .await
                {
                    crate::log!("vibed: can't take /mcp back off Serve: {}", undone.message);
                }
            }
            return Err(e);
        }
        Ok(())
    }

    async fn serve_off(
        &self,
        program: &Program,
        port: u16,
        was_on: bool,
    ) -> Result<(), ErrorObject> {
        match program {
            Program::NotHere => return Err(invalid(NOT_HERE)),
            // With no Tailscale, there's no Serve to take it off.
            Program::NotInstalled => {}
            Program::At(program) => {
                let look = self.serve_look(program, port).await?;
                match &look.path {
                    ServePath::Foreign(what, path) => {
                        return Err(error(ErrorCode::Conflict, foreign_note(what, *path)));
                    }
                    ServePath::Absent => {}
                    ServePath::Ours => {
                        let off = ["serve", HTTPS_FLAG, PATH_FLAG, "off"];
                        self.change_serve(program, &off, port, ServePath::Absent)
                            .await?;
                    }
                }
            }
        }
        if was_on {
            self.write_mcp_tailscale(false)?;
        }
        Ok(())
    }

    /// Runs one `tailscale serve` that's to leave `/mcp` as `want`, and reads back whether it
    /// did, whatever it exited with: it can exit 0 having only printed a link.
    async fn change_serve(
        &self,
        program: &OsStr,
        args: &[&str],
        port: u16,
        want: ServePath,
    ) -> Result<(), ErrorObject> {
        let ran = run(program, args, self.tailscale_wait).await.map_err(|e| {
            error(
                ErrorCode::CommandFailed,
                format!("Can't run tailscale: {e}"),
            )
        })?;
        let after = self.serve_look(program, port).await;
        match &after {
            Ok(look) if look.path == want => {
                crate::log!("vibed: tailscale serve {}", args[args.len() - 1]);
                Ok(())
            }
            Ok(Look {
                path: ServePath::Foreign(what, path),
                ..
            }) => Err(error(ErrorCode::Conflict, foreign_note(what, *path))),
            _ => Err(self.serve_refused(&ran, after.err())),
        }
    }

    /// A change to Serve that didn't take, in the words that help.
    fn serve_refused(&self, ran: &Ran, unread: Option<ErrorObject>) -> ErrorObject {
        let said = ran.said();
        match classify(&said) {
            Refusal::Denied => {
                if !self.tailscale_needs_operator.swap(true, Ordering::SeqCst) {
                    self.publish(self.event(EventKind::HostLink));
                }
                let user = std::env::var("USER").unwrap_or_else(|_| "$USER".to_owned());
                error(
                    ErrorCode::CommandFailed,
                    format!(
                        "Tailscale doesn't let {user} change what it serves here. Choose Allow… \
                         in Settings → Hosts, or run sudo tailscale set --operator={user} on \
                         this host, then try again. Tailscale said: {said}"
                    ),
                )
            }
            Refusal::NoHttps(link) => {
                let mut e = error(
                    ErrorCode::CommandFailed,
                    format!(
                        "Your tailnet needs HTTPS turned on for this. Turn it on, then try \
                         again. Tailscale said: {said}"
                    ),
                );
                e.data = link.map(|url| json!({ "tailscale_enable_url": url }));
                e
            }
            Refusal::Other if ran.timed_out => error(
                ErrorCode::CommandFailed,
                format!(
                    "tailscale serve didn't finish in {} s. {said}",
                    self.tailscale_wait.as_secs()
                ),
            ),
            Refusal::Other => match unread {
                Some(e) if ran.ok => error(
                    ErrorCode::CommandFailed,
                    format!(
                        "tailscale serve ran, but what it did can't be read back. {}",
                        e.message
                    ),
                ),
                Some(e) if said.is_empty() => e,
                _ if said.is_empty() => error(
                    ErrorCode::CommandFailed,
                    "tailscale serve said nothing, and didn't change /mcp",
                ),
                _ => error(
                    ErrorCode::CommandFailed,
                    format!("tailscale serve didn't change /mcp. It said: {said}"),
                ),
            },
        }
    }

    /// Having changed Serve, this user may: nothing's waiting for `sudo`.
    fn operator_allowed(&self) {
        if self.tailscale_needs_operator.swap(false, Ordering::SeqCst) {
            self.publish(self.event(EventKind::HostLink));
        }
    }

    /// Whether `sudo tailscale set --operator=<user>` is what's standing in the way, as a
    /// change to Serve last found.
    pub(crate) fn tailscale_waits_for_admin(&self) -> bool {
        self.tailscale_needs_operator.load(Ordering::SeqCst)
    }
}

/// `http://<ip>:<port>/mcp`, as Serve is pointed at and keeps it.
fn format_target(ip: IpAddr, port: u16) -> String {
    format!("http://{}{SERVE_PATH}", SocketAddr::new(ip, port))
}

/// With a full stop, if it wants one.
fn sentence_end(why: &str) -> String {
    let why = why.trim();
    if why.ends_with(['.', '!', '?']) {
        why.to_owned()
    } else {
        format!("{why}.")
    }
}

const NOT_HERE: &str =
    "This is for a Linux host. A Mac's own toggle serves MCP on its Tailscale address.";
const NOT_INSTALLED: &str =
    "Tailscale isn't installed on this host. Install it and log in; VibeOS leaves that to you.";
const NO_ADDRESS: &str = "This host has no Tailscale address on its network interfaces, so MCP \
                          has nowhere to listen for Tailscale to reach it.";

fn foreign_note(what: &str, path: Option<&str>) -> String {
    let at = path.unwrap_or(SERVE_PATH);
    let remedy = match path {
        Some(path) => format!(
            "If nothing needs it, remove it with `{}`, then try again.",
            off_line(path)
        ),
        None => "If nothing needs it, stop that command, then try again.".to_owned(),
    };
    format!(
        "Tailscale already serves {at} here, from {what}, which isn't this host's MCP. VibeOS \
         leaves it alone. {remedy}"
    )
}

fn funnel_note(look: &Look) -> String {
    format!(
        "Tailscale Funnel is on for {}:{SERVE_PORT}, so anything served there is on the public \
         internet. VibeOS won't serve MCP there, or change your Funnel. Turn Funnel off for that \
         name yourself, then try again.",
        look.name
    )
}

/// What the toggle should say of how things stand, if anything.
fn stands_note(look: &Look, flag: bool, listening: bool) -> Option<String> {
    Some(match &look.path {
        ServePath::Foreign(what, path) => foreign_note(what, *path),
        ServePath::Ours if look.funnel && flag => format!(
            "Tailscale Funnel is on for {}:{SERVE_PORT}, so this MCP is on the public internet, \
             behind its API keys. Turn Funnel off, or turn this off.",
            look.name
        ),
        _ if look.funnel => funnel_note(look),
        ServePath::Ours if !flag => format!(
            "Tailscale serves /mcp for this host, but mcp_tailscale is off, so it can't answer. \
             Turn this on to finish, or remove it with `{}`.",
            off_line(SERVE_PATH)
        ),
        ServePath::Ours if !listening => NO_ADDRESS.to_owned(),
        ServePath::Absent if !listening => NO_ADDRESS.to_owned(),
        ServePath::Absent if flag => "MCP answers on this host's Tailscale address, as \
             mcp_tailscale is on, but not on Tailscale Serve. Turn this on for its https URL. \
             Turning it off after that turns mcp_tailscale off too."
            .to_owned(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "vps.tailnet-example.ts.net";

    fn targets() -> Vec<String> {
        vec!["http://100.101.102.103:7878/mcp".to_owned()]
    }

    #[test]
    fn the_flags_are_for_the_mcp_path() {
        assert_eq!(PATH_FLAG, format!("--set-path={SERVE_PATH}"));
        assert_eq!(HTTPS_FLAG, format!("--https={SERVE_PORT}"));
    }

    #[test]
    fn a_target_is_how_tailscale_keeps_it() {
        assert_eq!(
            format_target("100.101.102.103".parse().unwrap(), 7878),
            "http://100.101.102.103:7878/mcp"
        );
        assert_eq!(
            format_target("fd7a:115c:a1e0::1".parse().unwrap(), 7878),
            "http://[fd7a:115c:a1e0::1]:7878/mcp"
        );
    }

    #[test]
    fn status_says_the_name_and_only_when_up() {
        let (name, ips) = parse_status(
            r#"{"BackendState":"Running","TailscaleIPs":["100.101.102.103","fd7a:115c:a1e0::1"],
                "Self":{"DNSName":"vps.tailnet-example.ts.net."}}"#,
        )
        .unwrap();
        assert_eq!(name, NAME);
        assert_eq!(ips.len(), 2);
        let e = parse_status(r#"{"BackendState":"NeedsLogin","Self":{"DNSName":""}}"#).unwrap_err();
        assert!(e.contains("NeedsLogin"), "{e}");
        let e = parse_status(r#"{"BackendState":"Running","Self":{"DNSName":""}}"#).unwrap_err();
        assert!(e.contains("MagicDNS"), "{e}");
        assert!(parse_status("not json").is_err());
    }

    #[test]
    fn serve_tells_ours_from_anything_else() {
        let site =
            |handlers: &str| format!(r#"{{"Web":{{"{NAME}:443":{{"Handlers":{{{handlers}}}}}}}}}"#);
        let parse = |json: &str| parse_serve(json, NAME, &targets()).unwrap();
        assert_eq!(parse("{}"), (ServePath::Absent, false));
        assert_eq!(parse("null"), (ServePath::Absent, false));
        // Other paths, and /mcp on another name or port, are no business of this.
        assert_eq!(
            parse(&site(r#""/":{"Proxy":"http://127.0.0.1:3000"}"#)),
            (ServePath::Absent, false)
        );
        assert_eq!(
            parse(&format!(
                r#"{{"Web":{{"{NAME}:8443":{{"Handlers":{{"/mcp":{{"Proxy":"http://127.0.0.1:1"}}}}}}}}}}"#
            )),
            (ServePath::Absent, false)
        );
        assert_eq!(
            parse(&site(
                r#""/mcp":{"Proxy":"http://100.101.102.103:7878/mcp"}"#
            )),
            (ServePath::Ours, false)
        );
        // Each near miss is someone else's, not ours: loopback, another port, no path.
        for (target, _) in [
            ("http://127.0.0.1:7878/mcp", ()),
            ("http://100.101.102.103:9000/mcp", ()),
            ("http://100.101.102.103:7878", ()),
            ("http://100.101.102.103:7878/mcp/", ()),
            ("https+insecure://100.101.102.103:7878/mcp", ()),
        ] {
            assert_eq!(
                parse(&site(&format!(r#""/mcp":{{"Proxy":"{target}"}}"#))),
                (ServePath::Foreign(target.to_owned(), Some("/mcp")), false),
                "{target}"
            );
        }
        assert_eq!(
            parse(&site(r#""/mcp":{"Path":"/srv/www"}"#)),
            (
                ServePath::Foreign("the path /srv/www".to_owned(), Some("/mcp")),
                false
            )
        );
        assert_eq!(
            parse(&site(r#""/mcp":{"Text":"hello"}"#)),
            (
                ServePath::Foreign("some text".to_owned(), Some("/mcp")),
                false
            )
        );
        assert_eq!(
            parse(&site(r#""/mcp":{}"#)),
            (
                ServePath::Foreign("something else".to_owned(), Some("/mcp")),
                false
            )
        );
    }

    /// What the real CLI keeps a directory at `/mcp` as, and replaces with no complaint.
    #[test]
    fn a_slashed_mcp_is_never_this_hosts_and_a_path_under_it_is_no_business_of_this() {
        let site =
            |handlers: &str| format!(r#"{{"Web":{{"{NAME}:443":{{"Handlers":{{{handlers}}}}}}}}}"#);
        let parse = |json: &str| parse_serve(json, NAME, &targets()).unwrap();
        assert_eq!(
            parse(&site(r#""/mcp/":{"Path":"/srv/www"}"#)),
            (
                ServePath::Foreign("the path /srv/www".to_owned(), Some("/mcp/")),
                false
            )
        );
        assert_eq!(
            parse(&site(
                r#""/mcp/":{"Proxy":"http://100.101.102.103:7878/mcp"}"#
            )),
            (
                ServePath::Foreign("http://100.101.102.103:7878/mcp".to_owned(), Some("/mcp/")),
                false
            ),
            "even at this host's target"
        );
        assert_eq!(
            parse(&site(
                r#""/mcp":{"Proxy":"http://100.101.102.103:7878/mcp"},"/mcp/":{"Path":"/srv/www"}"#
            ))
            .0,
            ServePath::Foreign("the path /srv/www".to_owned(), Some("/mcp/"))
        );
        assert_eq!(
            parse(&site(r#""/mcp/sub":{"Proxy":"http://127.0.0.1:3000"}"#)),
            (ServePath::Absent, false),
            "the real CLI leaves that where it is"
        );
        // In a foreground command too.
        let foreground = format!(
            r#"{{"Foreground":{{"abc":{{"Web":{{"{NAME}:443":{{"Handlers":{{"/mcp/":{{"Path":"/srv/www"}}}}}}}}}}}}}}"#
        );
        assert!(matches!(
            parse(&foreground).0,
            ServePath::Foreign(what, None) if what.contains("foreground")
        ));
        // The line to remove it names the path it's at: `--set-path=/mcp off` doesn't reach it.
        let note = foreign_note("the path /srv/www", Some("/mcp/"));
        assert!(note.contains("--set-path=/mcp/ off"), "{note}");
    }

    #[test]
    fn a_change_fits_in_what_a_forwarded_call_gets() {
        assert!(TAILSCALE_WAIT * 7 < crate::hosts::call_timeout("mcp.serve.set"));
    }

    #[test]
    fn serve_sees_funnel_and_a_foreground_command() {
        let parse = |json: &str| parse_serve(json, NAME, &targets()).unwrap();
        assert_eq!(
            parse(&format!(r#"{{"AllowFunnel":{{"{NAME}:443":true}}}}"#)),
            (ServePath::Absent, true)
        );
        assert_eq!(
            parse(&format!(r#"{{"AllowFunnel":{{"{NAME}:8443":true}}}}"#)),
            (ServePath::Absent, false),
            "another port's Funnel doesn't reach /mcp on 443"
        );
        let foreground = format!(
            r#"{{"Foreground":{{"abc":{{"AllowFunnel":{{"{NAME}:443":true}},"Web":{{"{NAME}:443":{{"Handlers":{{"/mcp":{{"Proxy":"http://100.101.102.103:7878/mcp"}}}}}}}}}}}}}}"#
        );
        let (path, funnel) = parse(&foreground);
        assert!(funnel);
        assert!(
            matches!(path, ServePath::Foreign(what, None) if what.contains("foreground")),
            "a session's, even at this target, ends with it"
        );
    }

    #[test]
    fn what_doesnt_read_is_never_taken_for_empty() {
        for json in ["", "not json", "[]", "3"] {
            assert!(parse_serve(json, NAME, &targets()).is_err(), "{json:?}");
        }
    }

    #[test]
    fn refusals_are_told_apart_by_what_tailscale_says() {
        assert_eq!(
            classify("sending serve config: Access denied: serve config denied"),
            Refusal::Denied
        );
        assert_eq!(
            classify("To not require root, use 'sudo tailscale set --operator=$USER' once."),
            Refusal::Denied
        );
        assert_eq!(
            classify(
                "\nServe is not enabled on your tailnet.\nTo enable, visit:\n\n         \
                 https://login.tailscale.com/f/serve?node=abc\n"
            ),
            Refusal::NoHttps(Some(
                "https://login.tailscale.com/f/serve?node=abc".to_owned()
            ))
        );
        assert_eq!(
            classify("HTTPS cert support is not enabled/configured for your tailnet."),
            Refusal::NoHttps(None)
        );
        assert_eq!(classify("port 443 is in use"), Refusal::Other);
        assert_eq!(
            classify("MagicDNS is not enabled for this tailnet"),
            Refusal::Other
        );
        assert_eq!(
            classify("bad target; see https://tailscale.com/kb/1247"),
            Refusal::Other
        );
        // This node's own URL, as it prints when it works, isn't a link to act on.
        assert_eq!(
            classify(&format!(
                "Available within your tailnet: https://{NAME}/mcp"
            )),
            Refusal::Other
        );
    }

    #[test]
    fn what_is_said_is_kept_to_a_sensible_length() {
        let ran = Ran {
            stderr: "é".repeat(MAX_SAID + 5),
            ..Ran::default()
        };
        assert_eq!(ran.said().chars().count(), MAX_SAID + 1);
    }
}
