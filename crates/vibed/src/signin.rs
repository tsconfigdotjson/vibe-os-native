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
}

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
    if into.url.is_none() {
        into.url = words
            .iter()
            .find(|w| url_can_be_opened(w))
            .map(|w| (*w).to_owned());
    }
    match what {
        SignInWhat::Github | SignInWhat::ToolboxGithub => {
            // "! First copy your one-time code: XXXX-XXXX"
            if into.code.is_none()
                && let Some(at) = words.windows(2).position(|w| w == ["one-time", "code:"])
            {
                into.code = words
                    .get(at + 2)
                    .filter(|c| {
                        c.len() <= 32 && c.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    })
                    .map(|c| (*c).to_owned());
            }
        }
        SignInWhat::Claude | SignInWhat::ToolboxClaude => {
            // "Paste code here if prompted > "
            if said.contains("Paste code here") {
                into.asks_for_code = true;
            }
        }
    }
}

/// `text` without a terminal's colours and moves, which split what a flow says.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else if c == '\r' {
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// The last thing a failed flow said, that's safe to show: what looks like a token or
/// a page's address goes.
fn last_words(said: &str) -> Option<String> {
    // Its prompt has no line end, so what's said next is on the same line.
    let line = said
        .lines()
        .map(|l| l.rsplit("prompted > ").next().unwrap_or(l).trim())
        .rfind(|l| !l.is_empty() && !l.contains("Paste code") && !l.contains("https://"))?;
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
    scratch: Option<PathBuf>,
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
                // On this Mac, where the browser is, into a place of its own: the
                // owner's own `gh` login here isn't touched, and the login goes on
                // into the toolbox after.
                let scratch = self
                    .layout
                    .data_dir()
                    .join(format!(".toolbox-gh-{}", self.new_token_id()?));
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
                    scratch: Some(scratch),
                })
            }
            SignInWhat::ToolboxClaude => {
                if self.toolbox.is_none() {
                    return Err(refused("this host has no Linux toolbox"));
                }
                Ok(Flow {
                    program: self.vibe_cli.clone(),
                    args: vec!["setup-token"],
                    env: vec![("NO_COLOR", "1".into())],
                    scratch: None,
                })
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
                error: None,
            }),
            news: watch::channel(0).0,
            stdin: tokio::sync::Mutex::new(None),
            task: Mutex::new(None),
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
        // Whatever else is under way for this goes: there's one at a time.
        {
            let mut all = lock(&self.sign_ins);
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
            let said = Arc::new(Mutex::new(String::new()));
            let read = |mut from: Box<dyn tokio::io::AsyncRead + Send + Unpin>| {
                let (said, session) = (Arc::clone(&said), Arc::clone(&watching));
                async move {
                    let mut chunk = [0u8; 4096];
                    while let Ok(n) = from.read(&mut chunk).await {
                        if n == 0 {
                            return;
                        }
                        let mut all = lock(&said);
                        if all.len() < MOST_TO_READ {
                            all.push_str(&plain(&String::from_utf8_lossy(&chunk[..n])));
                        }
                        session.change(|now| read_said(what, &all, now));
                    }
                }
            };
            let finished = tokio::time::timeout(SIGN_IN_FOR, async {
                let (_, _, status) =
                    tokio::join!(read(Box::new(stdout)), read(Box::new(stderr)), child.wait());
                status
            })
            .await;
            let said = lock(&said).clone();
            let ok = matches!(&finished, Ok(Ok(status)) if status.success());
            let mut why = match &finished {
                Err(_) => Some("nobody finished signing in, so that was given up on".to_owned()),
                Ok(Err(e)) => Some(e.to_string()),
                Ok(Ok(_)) if ok => None,
                Ok(Ok(status)) => {
                    Some(last_words(&said).unwrap_or_else(|| format!("it ended with {status}")))
                }
            };
            if ok
                && let (Some(core), SignInWhat::ToolboxGithub) = (&me, what)
                && let Some(scratch) = &flow.scratch
                && let Err(e) = core.toolbox_take_github_login(&flow.program, scratch).await
            {
                why = Some(e);
            }
            if let Some(scratch) = &flow.scratch {
                let _ = std::fs::remove_dir_all(scratch);
            }
            watching.change(|now| {
                now.state = if why.is_none() {
                    SignInState::Done
                } else {
                    SignInState::Failed
                };
                now.error = why;
                now.asks_for_code = false;
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
        if what == SignInWhat::Github {
            self.check_gh().await;
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
        // A code is one word. Anything else couldn't be, and mustn't reach a prompt
        // as more than one line.
        if p.value.is_empty() || p.value.len() > 4096 || p.value.chars().any(char::is_control) {
            return Err(error(ErrorCode::InvalidParams, "that isn't a code"));
        }
        let mut stdin = session.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(error(ErrorCode::Conflict, "nothing's asking for that now"));
        };
        write_line(pipe, &p.value).await.map_err(|e| {
            error(
                ErrorCode::CommandFailed,
                format!("it can't be told that: {e}"),
            )
        })?;
        drop(stdin);
        session.change(|now| now.asks_for_code = false);
        let now = lock(&session.now).clone();
        Ok(now)
    }

    /// `signin.cancel`.
    pub async fn sign_in_cancel(&self, p: SignInParams) -> Result<SignIn, ErrorObject> {
        let session = self.sign_in_session(&p.id)?;
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

async fn write_line(to: &mut (impl AsyncWrite + Unpin), value: &str) -> std::io::Result<()> {
    to.write_all(value.as_bytes()).await?;
    to.write_all(b"\n").await?;
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
        assert_eq!(
            last_words("x\nerror: access_denied\n\n").as_deref(),
            Some("error: access_denied")
        );
        assert_eq!(last_words("token sk-ant-oat01-secret\n"), None);
        assert_eq!(last_words("oops gho_abc\n"), None);
        assert_eq!(last_words("\n\n"), None);
        assert_eq!(
            last_words("Paste code here if prompted > That code was refused.\n").as_deref(),
            Some("That code was refused.")
        );
    }

    #[test]
    fn colours_and_moves_dont_split_a_word() {
        assert_eq!(plain("\u{1b}[1mcode:\u{1b}[0m A\r\n"), "code: A\n\n");
    }
}
