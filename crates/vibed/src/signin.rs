//! Signing in with one click (PRD: Who gets which credentials): the provider's own
//! flow, run where the login's to live, with no terminal. `gh`'s device code and
//! Claude Code's page both say what they need of a person and then wait, so `vibed`
//! runs them, reads what they say, and hands the app the page to open on the Mac, the
//! code to enter there, or a place for the code a page shows. Nothing here signs
//! anyone in, or keeps what they print.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::watch;
use vibe_core::model::{SignInState, SignInWhat, ToolboxState};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, SignIn, SignInAnswerParams, SignInGetParams, SignInParams,
    SignInStartParams,
};
use vibe_env::pi_setup::url_can_be_opened;

use crate::core::{Core, error, lock};

/// How long a sign-in may wait for a person.
const SIGN_IN_FOR: Duration = Duration::from_secs(15 * 60);

/// How long `signin.get` waits for news.
const WAIT_FOR_NEWS: Duration = Duration::from_secs(25);

/// As much of what a flow says as is kept to read: what's wanted comes first.
const MOST_TO_READ: usize = 64 * 1024;

/// One flow under way, or finished.
#[derive(Debug)]
pub(crate) struct Session {
    now: Mutex<SignIn>,
    news: watch::Sender<u64>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// What the flow's said, as it said it, since it was last answered. Read whole each
    /// time, as a read can end anywhere, in an escape or a character. Never kept once
    /// it's done.
    said: Mutex<Vec<u8>>,
}

/// The pages each flow's own tool sends a person to: what's opened with no click, and
/// taken over any other page it mentions. A host's word is all there is for a page, so
/// anything else takes a click, showing where it goes.
pub fn is_the_providers_page(what: SignInWhat, url: &str) -> bool {
    let Some(rest) = url.get(8..) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hosts: &[&str] = match what {
        SignInWhat::Github | SignInWhat::ToolboxGithub => &["github.com"],
        SignInWhat::Claude | SignInWhat::ToolboxClaude => &[
            "claude.com",
            "claude.ai",
            "platform.claude.com",
            "console.anthropic.com",
        ],
    };
    url.get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        && hosts.iter().any(|h| host.eq_ignore_ascii_case(h))
}

/// The private directory a sign-in borrows, gone however it ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How a sign-in's scratch directory is named.
const SCRATCH_PREFIX: &str = ".toolbox-gh-";

impl Session {
    fn change(&self, change: impl FnOnce(&mut SignIn)) {
        let mut now = lock(&self.now);
        let before = now.clone();
        change(&mut now);
        if *now != before {
            now.seq += 1;
            self.news.send_replace(now.seq);
        }
    }
}

/// What a flow's said so far, from the start, is taken to mean. Only a whole word, with
/// space after it, is read: more of it may be on its way.
pub fn read_said(what: SignInWhat, said: &str, into: &mut SignIn) {
    let words: Vec<&str> = said.split_whitespace().collect();
    let whole = if said.ends_with(char::is_whitespace) {
        words.len()
    } else {
        words.len().saturating_sub(1)
    };
    let words = &words[..whole];
    // The provider's own page, over anything else it mentions first.
    if into
        .url
        .as_deref()
        .is_none_or(|u| !is_the_providers_page(what, u))
    {
        let page = words
            .iter()
            .find(|w| url_can_be_opened(w) && is_the_providers_page(what, w))
            .or_else(|| words.iter().find(|w| url_can_be_opened(w)));
        if let Some(page) = page {
            into.url = Some((*page).to_owned());
        }
    }
    match what {
        SignInWhat::Github | SignInWhat::ToolboxGithub => {
            // "! First copy your one-time code: XXXX-XXXX", or with a clipboard to put
            // it on, as `gh` 2.103 has on a Mac: "! One-time code (XXXX-XXXX) copied to
            // clipboard". That's where it'd stay if it weren't read here.
            if into.code.is_none() {
                let plain_code = |c: &&str| {
                    !c.is_empty()
                        && c.len() <= 32
                        && c.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                };
                let first = words
                    .windows(2)
                    .position(|w| w == ["one-time", "code:"])
                    .and_then(|at| words.get(at + 2))
                    .copied();
                let copied = words
                    .windows(2)
                    .position(|w| w[0].eq_ignore_ascii_case("one-time") && w[1] == "code")
                    .and_then(|at| words.get(at + 2))
                    .and_then(|c| c.strip_prefix('(')?.strip_suffix(')'));
                into.code = first.or(copied).filter(plain_code).map(str::to_owned);
            }
        }
        SignInWhat::Claude | SignInWhat::ToolboxClaude => {
            // "Paste code here if prompted > "
            if said.contains("Paste code here") {
                into.asks_for_code = true;
            }
            // It says this, and waits for another.
            if let Some(line) = said.lines().find(|l| l.contains("Invalid code")) {
                into.error = Some(line.trim().chars().take(200).collect());
                into.asks_for_code = true;
            }
        }
    }
}

/// `text` as a person would read it off a terminal: no colours, a move to another
/// column counting as the space it shows as, and a hyperlink's address as a word of its
/// own, as a TUI may give a long one whole only there.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            if matches!(c, 'G' | 'C') {
                                out.push(' ');
                            }
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut body = String::new();
                    let mut ended = false;
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            ended = true;
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            ended = true;
                            break;
                        }
                        body.push(c);
                    }
                    // "8;<params>;<address>". One a read ended partway through is no
                    // address yet: the next read has the whole of it.
                    if ended
                        && let Some(rest) = body.strip_prefix("8;")
                        && let Some((_, address)) = rest.split_once(';')
                        && !address.is_empty()
                    {
                        out.push_str(address);
                        out.push(' ');
                    }
                }
                _ => {}
            },
            '\r' => out.push('\n'),
            c => out.push(c),
        }
    }
    out
}

/// The last thing a failed flow said, that's safe to show. For the flow that prints a
/// long-lived token, only what `vibe` itself says: a terminal wraps a token over lines,
/// and no look at a line can tell. For the others, what looks like a token or a page's
/// address goes.
fn last_words(what: SignInWhat, said: &str) -> Option<String> {
    let line = said
        .lines()
        .map(|l| l.rsplit("prompted > ").next().unwrap_or(l).trim())
        .rfind(|l| {
            !l.is_empty()
                && !l.contains("Paste code")
                && !l.contains("https://")
                && (what != SignInWhat::ToolboxClaude || l.starts_with("vibe:"))
        })?;
    if line.contains("sk-") || line.contains("gho_") || line.contains("ghp_") {
        return None;
    }
    Some(line.chars().take(300).collect())
}

/// What to run for `what`.
struct Flow {
    program: OsString,
    args: Vec<&'static str>,
    env: Vec<(&'static str, OsString)>,
    /// A private directory to throw away after, and from which a toolbox's login is
    /// read first.
    scratch: Option<Scratch>,
}

impl Core {
    fn sign_in_session(&self, id: &str) -> Result<Arc<Session>, ErrorObject> {
        lock(&self.sign_ins)
            .iter()
            .find(|s| lock(&s.now).id == id)
            .cloned()
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no sign-in {id}")))
    }

    async fn sign_in_flow(&self, what: SignInWhat) -> Result<Flow, ErrorObject> {
        let refused = |why: &str| error(ErrorCode::Conflict, why.to_owned());
        match what {
            SignInWhat::Github => Ok(Flow {
                program: self.gh_program().await,
                args: vec!["auth", "login", "--web", "--hostname", "github.com"],
                env: vec![("GH_PROMPT_DISABLED", "1".into()), ("NO_COLOR", "1".into())],
                scratch: None,
            }),
            SignInWhat::Claude => {
                let checks = Arc::clone(&self.checks);
                let found = tokio::task::spawn_blocking(move || checks.claude())
                    .await
                    .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?;
                let (path, _) = found.map_err(|why| {
                    refused(&format!(
                        "Claude Code isn't here yet: {why}. VibeOS is on it"
                    ))
                })?;
                Ok(Flow {
                    program: path.into_os_string(),
                    args: vec!["auth", "login"],
                    env: vec![("NO_COLOR", "1".into())],
                    scratch: None,
                })
            }
            SignInWhat::ToolboxGithub => {
                if self.toolbox.is_none() {
                    return Err(refused("this host has no Linux toolbox"));
                }
                if *lock(&self.toolbox_state) != ToolboxState::Closed {
                    return Err(refused(
                        "the toolbox is in use: close it, and you can sign in from here",
                    ));
                }
                if self.toolbox.as_ref().is_some_and(|t| t.has_unsaved()) {
                    return Err(refused(
                        "the toolbox has changes waiting to be saved: open its shell and exit \
                         it, and you can sign in from here",
                    ));
                }
                // On this Mac, where the browser is, into a place of its own: the
                // owner's own `gh` login here isn't touched, and the login goes on
                // into the toolbox after.
                let scratch = self
                    .layout
                    .data_dir()
                    .join(format!("{SCRATCH_PREFIX}{}", self.new_token_id()?));
                vibe_core::paths::private_dir(&scratch).map_err(|e| {
                    error(
                        ErrorCode::CommandFailed,
                        format!("can't make a place for it: {e}"),
                    )
                })?;
                Ok(Flow {
                    program: self.gh_program().await,
                    args: vec![
                        "auth",
                        "login",
                        "--web",
                        "--hostname",
                        "github.com",
                        "--insecure-storage",
                    ],
                    env: vec![
                        ("GH_PROMPT_DISABLED", "1".into()),
                        ("NO_COLOR", "1".into()),
                        ("GH_CONFIG_DIR", scratch.clone().into_os_string()),
                    ],
                    scratch: Some(Scratch(scratch)),
                })
            }
            SignInWhat::ToolboxClaude => {
                if self.toolbox.is_none() {
                    return Err(refused("this host has no Linux toolbox"));
                }
                // launchd gives `vibed` no PATH to find it with, so it's told.
                let checks = Arc::clone(&self.checks);
                let found = tokio::task::spawn_blocking(move || checks.claude())
                    .await
                    .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?;
                let (claude, _) = found.map_err(|why| {
                    refused(&format!(
                        "Claude Code isn't here yet: {why}. VibeOS is on it"
                    ))
                })?;
                Ok(Flow {
                    program: self.vibe_cli.clone(),
                    args: vec!["setup-token"],
                    env: vec![
                        ("NO_COLOR", "1".into()),
                        ("VIBE_CLAUDE", claude.into_os_string()),
                    ],
                    scratch: None,
                })
            }
        }
    }

    /// What a flow left behind that nothing's looking after, as after a crash: each is
    /// private, and may hold a login. Nothing runs longer than this.
    pub(crate) fn sweep_sign_in_scratch(&self) {
        let Ok(entries) = std::fs::read_dir(self.layout.data_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > SIGN_IN_FOR + Duration::from_secs(5 * 60));
            if old
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(SCRATCH_PREFIX)
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn new_token_id(&self) -> Result<String, ErrorObject> {
        let random = self.random_16()?;
        Ok(random.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// `signin.start`: starts `what`'s flow here, in place of one of it under way.
    pub async fn sign_in_start(&self, p: SignInStartParams) -> Result<SignIn, ErrorObject> {
        let host = self.host(p.host.as_deref())?.to_owned();
        let flow = self.sign_in_flow(p.what).await?;
        let session = Arc::new(Session {
            now: Mutex::new(SignIn {
                id: self.new_token_id()?,
                host,
                what: p.what,
                seq: 0,
                state: SignInState::Running,
                url: None,
                code: None,
                asks_for_code: false,
                opens_itself: self.info().os == "macos"
                    && matches!(p.what, SignInWhat::Claude | SignInWhat::ToolboxClaude),
                finishing: false,
                error: None,
            }),
            news: watch::channel(0).0,
            stdin: tokio::sync::Mutex::new(None),
            task: Mutex::new(None),
            said: Mutex::default(),
        });
        let mut command = Command::new(&flow.program);
        command
            .args(&flow.args)
            .envs(flow.env.iter().map(|(k, v)| (*k, v)))
            .env("HOME", self.layout.home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            error(
                ErrorCode::CommandFailed,
                format!("can't run {}: {e}", flow.program.to_string_lossy()),
            )
        })?;
        *session.stdin.lock().await = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        // Whatever else is under way for this goes: there's one at a time. But not one
        // that's been approved, and is putting the login where it goes.
        {
            let mut all = lock(&self.sign_ins);
            if all.iter().any(|s| {
                let now = lock(&s.now);
                now.what == p.what && now.finishing && now.state == SignInState::Running
            }) {
                return Err(error(
                    ErrorCode::Conflict,
                    "that sign-in is finishing: try again when it's done",
                ));
            }
            for old in all.iter().filter(|s| lock(&s.now).what == p.what) {
                if let Some(task) = lock(&old.task).take() {
                    task.abort();
                }
                old.change(|now| {
                    if now.state == SignInState::Running {
                        now.state = SignInState::Cancelled;
                    }
                });
            }
            all.retain(|s| lock(&s.now).what != p.what);
            all.push(Arc::clone(&session));
        }
        let (me, watching) = (
            self.me.get().and_then(std::sync::Weak::upgrade),
            Arc::clone(&session),
        );
        let what = p.what;
        let task = tokio::spawn(async move {
            let read = |mut from: Box<dyn tokio::io::AsyncRead + Send + Unpin>| {
                let session = Arc::clone(&watching);
                tokio::spawn(async move {
                    let mut chunk = [0u8; 4096];
                    while let Ok(n) = from.read(&mut chunk).await {
                        if n == 0 {
                            return;
                        }
                        let said = {
                            let mut all = lock(&session.said);
                            if all.len() < MOST_TO_READ {
                                all.extend_from_slice(&chunk[..n]);
                            }
                            plain(&String::from_utf8_lossy(&all))
                        };
                        session.change(|now| read_said(what, &said, now));
                    }
                })
            };
            let readers = [read(Box::new(stdout)), read(Box::new(stderr))];
            let finished = tokio::time::timeout(SIGN_IN_FOR, child.wait()).await;
            let ok = matches!(&finished, Ok(Ok(status)) if status.success());
            let takes_it_on = ok && what == SignInWhat::ToolboxGithub;
            if takes_it_on {
                // Approved: it can't be cancelled from here, or the login's lost.
                watching.change(|now| {
                    now.finishing = true;
                    now.asks_for_code = false;
                });
            }
            // A grandchild may hold its output open: its exit's what counts.
            for reader in readers {
                let abort = reader.abort_handle();
                if tokio::time::timeout(Duration::from_secs(2), reader)
                    .await
                    .is_err()
                {
                    abort.abort();
                }
            }
            let said = plain(&String::from_utf8_lossy(&std::mem::take(&mut *lock(
                &watching.said,
            ))));
            let refused = lock(&watching.now).error.clone();
            let mut why = match &finished {
                Err(_) => Some("nobody finished signing in, so that was given up on".to_owned()),
                Ok(Err(e)) => Some(e.to_string()),
                Ok(Ok(_)) if ok => None,
                Ok(Ok(status)) => Some(
                    last_words(what, &said)
                        .or(refused)
                        .unwrap_or_else(|| format!("it ended with {status}")),
                ),
            };
            drop(said);
            if takes_it_on {
                let token = match (&me, &flow.scratch) {
                    (Some(core), Some(scratch)) => {
                        core.toolbox_github_token(&flow.program, &scratch.0).await
                    }
                    _ => Err("this vibed isn't running as it should".to_owned()),
                };
                // Read: it needn't be on disk a moment longer.
                drop(flow.scratch);
                why = match (token, &me) {
                    (Ok(token), Some(core)) => core.toolbox_give_github(token).await.err(),
                    (Err(e), _) => Some(e),
                    (Ok(_), None) => Some("this vibed isn't running as it should".to_owned()),
                };
            }
            watching.change(|now| {
                now.state = if why.is_none() {
                    SignInState::Done
                } else {
                    SignInState::Failed
                };
                now.error = why;
                now.asks_for_code = false;
                now.finishing = false;
            });
            if let Some(core) = me {
                core.signed_in(what).await;
            }
        });
        *lock(&session.task) = Some(task);
        let now = lock(&session.now).clone();
        Ok(now)
    }

    /// What follows a flow, however it went: what shows how things stand is looked
    /// at again now, and what git needs is set up.
    async fn signed_in(&self, what: SignInWhat) {
        *lock(&self.doctor_found) = None;
        match what {
            SignInWhat::Github => {
                self.check_gh().await;
            }
            SignInWhat::Claude => self.look_at_claude_login().await,
            SignInWhat::ToolboxGithub | SignInWhat::ToolboxClaude => {
                self.publish(self.event(vibe_core::model::EventKind::ToolboxChanged));
            }
        }
    }

    /// `signin.get`: how it stands, once it's moved on from `seen`, or as it is after a
    /// while: so the app can ask again without asking often.
    pub async fn sign_in_get(&self, p: SignInGetParams) -> Result<SignIn, ErrorObject> {
        let session = self.sign_in_session(&p.id)?;
        let mut news = session.news.subscribe();
        if let Some(seen) = p.seen {
            let _ = tokio::time::timeout(WAIT_FOR_NEWS, news.wait_for(|seq| *seq != seen)).await;
        }
        let now = lock(&session.now).clone();
        Ok(now)
    }

    /// `signin.answer`: the code a page showed. Never logged, nor kept.
    pub async fn sign_in_answer(&self, p: SignInAnswerParams) -> Result<SignIn, ErrorObject> {
        let session = self.sign_in_session(&p.id)?;
        let what = lock(&session.now).what;
        // A code is one word. Anything else couldn't be, and mustn't reach a prompt
        // as more than one line.
        if p.value.is_empty() || p.value.len() > 4096 || p.value.chars().any(char::is_control) {
            return Err(error(ErrorCode::InvalidParams, "that isn't a code"));
        }
        let mut stdin = session.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(error(ErrorCode::Conflict, "nothing's asking for that now"));
        };
        // A TUI on a terminal takes Return as a terminal sends it; a plain prompt takes
        // a line.
        let end = if what == SignInWhat::ToolboxClaude {
            "\r"
        } else {
            "\n"
        };
        write_line(pipe, &p.value, end).await.map_err(|e| {
            error(
                ErrorCode::CommandFailed,
                format!("it can't be told that: {e}"),
            )
        })?;
        drop(stdin);
        // From here it's what it says about this that counts.
        lock(&session.said).clear();
        session.change(|now| {
            now.asks_for_code = false;
            now.error = None;
        });
        let now = lock(&session.now).clone();
        Ok(now)
    }

    /// `signin.cancel`.
    pub async fn sign_in_cancel(&self, p: SignInParams) -> Result<SignIn, ErrorObject> {
        let session = self.sign_in_session(&p.id)?;
        // Approved, and on its way: stopping now would lose it.
        if lock(&session.now).finishing {
            let now = lock(&session.now).clone();
            return Ok(now);
        }
        if let Some(task) = lock(&session.task).take() {
            task.abort();
        }
        session.change(|now| {
            if now.state == SignInState::Running {
                now.state = SignInState::Cancelled;
                now.asks_for_code = false;
            }
        });
        let now = lock(&session.now).clone();
        Ok(now)
    }
}

async fn write_line(
    to: &mut (impl AsyncWrite + Unpin),
    value: &str,
    end: &str,
) -> std::io::Result<()> {
    to.write_all(value.as_bytes()).await?;
    to.write_all(end.as_bytes()).await?;
    to.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(what: SignInWhat) -> SignIn {
        SignIn {
            id: "x".into(),
            host: "local".into(),
            what,
            seq: 0,
            state: SignInState::Running,
            url: None,
            code: None,
            asks_for_code: false,
            opens_itself: false,
            finishing: false,
            error: None,
        }
    }

    /// As the real ones printed them: gh 2.46 with no terminal, and Claude Code 2.1.318.
    #[test]
    fn what_the_flows_really_say_is_read() {
        let mut s = fresh(SignInWhat::Github);
        read_said(
            SignInWhat::Github,
            "\n! First copy your one-time code: B177-C539\nOpen this URL to continue in your web \
             browser: https://github.com/login/device\n",
            &mut s,
        );
        assert_eq!(s.code.as_deref(), Some("B177-C539"));
        assert_eq!(s.url.as_deref(), Some("https://github.com/login/device"));
        assert!(!s.asks_for_code);

        // gh 2.103.3, the one VibeOS installs, where it has a clipboard to use: every
        // Mac. And where it hasn't.
        let mut s = fresh(SignInWhat::Github);
        read_said(
            SignInWhat::Github,
            "! One-time code (B177-C539) copied to clipboard\nOpen this URL to continue in your \
             web browser: https://github.com/login/device\n",
            &mut s,
        );
        assert_eq!(s.code.as_deref(), Some("B177-C539"));
        let mut s = fresh(SignInWhat::Github);
        read_said(
            SignInWhat::Github,
            "! Failed to copy one-time code to clipboard\n! First copy your one-time code: \
             B177-C539\nOpen this URL: https://github.com/login/device\n",
            &mut s,
        );
        assert_eq!(s.code.as_deref(), Some("B177-C539"));

        let mut s = fresh(SignInWhat::Claude);
        read_said(
            SignInWhat::Claude,
            "Opening browser to sign in…\nIf the browser didn't open, visit: \
             https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c\nPaste code here if \
             prompted > ",
            &mut s,
        );
        assert_eq!(
            s.url.as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c")
        );
        assert!(s.asks_for_code);
    }

    /// Bytes as `vibe setup-token` really passed them on from Claude Code 2.1.318: the
    /// address whole only in a hyperlink, and columns where the spaces would be.
    #[test]
    fn the_setup_tokens_terminal_is_read_as_a_person_would() {
        let bytes = "\u{1b}]8;id=1vyx0kg;https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c&state=2mYk\u{7}\
                     \u{1b}[38;5;246mhttps://claude.com/cai/oauth/authori\u{1b}[39m\u{1b}]8;;\u{7}\r\r\n\
                     \u{1b}]8;id=1vyx0kg;https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c&state=2mYk\u{7}\
                     \u{1b}[38;5;246mze?code=true\u{1b}[39m\u{1b}]8;;\u{7}\r\r\n\r\r\n\r\r\n\
                     \u{1b}[2GPaste\u{1b}[8Gcode\u{1b}[13Ghere\u{1b}[18Gif\u{1b}[21Gprompted\u{1b}[30G>\r\r\n\
                     \u{1b}[>0q\u{1b}[?u\u{1b}]7501;?\u{7}\u{1b}[c";
        let mut s = fresh(SignInWhat::ToolboxClaude);
        read_said(SignInWhat::ToolboxClaude, &plain(bytes), &mut s);
        assert_eq!(
            s.url.as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c&state=2mYk")
        );
        assert!(s.asks_for_code);
    }

    #[test]
    fn the_providers_own_page_is_told_from_any_other() {
        use SignInWhat::{Claude, Github, ToolboxClaude, ToolboxGithub};
        assert!(is_the_providers_page(
            Github,
            "https://github.com/login/device"
        ));
        assert!(is_the_providers_page(
            ToolboxGithub,
            "https://GITHUB.com/login"
        ));
        assert!(!is_the_providers_page(
            Github,
            "https://github.com.evil.example/login"
        ));
        assert!(!is_the_providers_page(
            Github,
            "https://evil.example/github.com/"
        ));
        assert!(!is_the_providers_page(Github, "https://claude.com/"));
        assert!(!is_the_providers_page(Github, "http://github.com/login"));
        assert!(is_the_providers_page(
            Claude,
            "https://claude.com/cai/oauth/authorize?x=1"
        ));
        assert!(is_the_providers_page(ToolboxClaude, "https://claude.ai/x"));
        assert!(!is_the_providers_page(
            Claude,
            "https://claude.com.evil.example/"
        ));
        // Its own page, over one it happens to mention first.
        let mut s = fresh(Github);
        read_said(
            Github,
            "see https://docs.example.com/help then https://github.com/login/device ",
            &mut s,
        );
        assert_eq!(s.url.as_deref(), Some("https://github.com/login/device"));
    }

    #[test]
    fn a_wrong_code_is_said_and_another_is_asked_for() {
        let mut s = fresh(SignInWhat::Claude);
        read_said(
            SignInWhat::Claude,
            "Invalid code. Please make sure the full code was copied.\n",
            &mut s,
        );
        assert!(s.asks_for_code);
        assert!(s.error.as_deref().unwrap().starts_with("Invalid code"));
    }

    #[test]
    fn a_word_cut_short_or_what_a_browser_cant_open_isnt_taken() {
        let mut s = fresh(SignInWhat::Github);
        read_said(
            SignInWhat::Github,
            "visit https://github.com/login/de",
            &mut s,
        );
        assert_eq!(s.url, None, "more of it may be coming");
        read_said(SignInWhat::Github, "visit http://evil.example/ ", &mut s);
        assert_eq!(s.url, None, "only https");
        read_said(SignInWhat::Github, "your one-time code: $(rm) ", &mut s);
        assert_eq!(s.code, None);
    }

    #[test]
    fn what_a_failed_flow_said_is_kept_only_if_its_safe() {
        let g = SignInWhat::Github;
        assert_eq!(
            last_words(g, "x\nerror: access_denied\n\n").as_deref(),
            Some("error: access_denied")
        );
        assert_eq!(last_words(g, "token sk-ant-oat01-secret\n"), None);
        assert_eq!(last_words(g, "oops gho_abc\n"), None);
        assert_eq!(last_words(g, "\n\n"), None);
        assert_eq!(
            last_words(
                SignInWhat::Claude,
                "Paste code here if prompted > That code was refused.\n"
            )
            .as_deref(),
            Some("That code was refused.")
        );
        // Where a token's printed, on a terminal that may wrap it: only `vibe`'s words.
        let t = SignInWhat::ToolboxClaude;
        assert_eq!(last_words(t, "sk-ant-oat01-abc\ndefghij\n"), None);
        assert_eq!(
            last_words(t, "sk-ant-oat01-abc\nvibe: can't keep it\n").as_deref(),
            Some("vibe: can't keep it")
        );
    }

    #[test]
    fn colours_and_moves_dont_split_a_word() {
        assert_eq!(plain("\u{1b}[1mcode:\u{1b}[0m A\r\n"), "code: A\n\n");
        assert_eq!(plain("a\u{1b}[8Gb"), "a b");
        assert_eq!(
            plain("\u{1b}]8;;https://x.example/\u{1b}\\t"),
            "https://x.example/ t"
        );
        assert_eq!(plain("\u{1b}]8;;\u{7}t"), "t");
        assert_eq!(plain("\u{1b}]0;title\u{7}t"), "t");
        assert_eq!(plain("a \u{1b}]8;;https://x.example/lo"), "a ");
    }
}
