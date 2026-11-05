//! Pi's own model list and sign-in, for the app, with no terminal (PRD: Profiles;
//! Who gets which credentials). `vibed` runs the Pi this VibeOS pins with
//! [`vibe_env::pi_setup`]'s extension, carrying its questions out and the owner's
//! answers back. Pi's own code signs in and writes the login, so all that `/login`
//! would do is what's done, and only a provider's name is ever read here.
//!
//! **Where a login lands.** For a host's native agents, in the owner's own agent dir,
//! under Pi's own lock, as `/login` there would. For `linux` agents on a Mac there's
//! no Pi to run but in the toolbox, which has no browser, so a Pi on the Mac signs in
//! with an agent dir made for it, and what that leaves goes through the broker as the
//! owner's sign-in from the toolbox does. It never uses the toolbox's agent dir: a
//! guest can write that, and Pi on the Mac would run what it found there.
//!
//! **A browser on another machine.** A login for a Linux host opens its page on the
//! Mac, and the page comes back to `localhost` there, not to Pi. The Mac's `vibed`
//! listens at that address on loopback for as long as the sign-in lasts, and passes
//! on that page's own answer, by its `state`, as the address the owner would otherwise
//! paste. It does so only for pages Pi is known to use
//! ([`vibe_env::pi_setup::redirect_of`]), as all it knows of a page is the host's word.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use vibe_core::model::{LOCAL_HOST, PiLoginState, PiPromptKind, PiTarget};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    ErrorCode, ErrorObject, Method, PiLogin, PiLoginAnswer, PiLoginAnswerParams, PiLoginCancel,
    PiLoginGet, PiLoginGetParams, PiLoginParams, PiLoginStartParams, PiModels, PiModelsParams,
};
use vibe_env::pi;
use vibe_env::pi_logins::{self, Broker, Synced};
use vibe_env::pi_release;
use vibe_env::pi_setup::{self, Redirect, Said};

use crate::core::{Core, error, lock};
use crate::link::Link;

/// How long a list may take: it reads files and starts Pi, and that's a second.
const LIST_LIMIT: Duration = Duration::from_secs(30);

/// How long a sign-in may go on. A device code lasts up to half an hour.
const LOGIN_LIMIT: Duration = Duration::from_secs(30 * 60);

/// How long a call waits for a sign-in to have something new to say.
const SETTLE: Duration = Duration::from_secs(30);

/// How long after a page to open a question about it may follow.
const SETTLE_AFTER_PAGE: Duration = Duration::from_secs(1);

/// How often it's looked at whether anyone's still watching a sign-in.
const IDLE_LOOK: Duration = Duration::from_secs(5);

/// How long `pi.login.get` waits on a sign-in that hasn't changed.
const LONG_POLL: Duration = Duration::from_secs(20);

/// How long Pi gets to leave once it's told it's over.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The longest answer, far past any key.
const MAX_ANSWER: usize = 64 << 10;

/// The most of a browser's request that's read, and how long it may take.
const MAX_REQUEST: usize = 16 << 10;
const REQUEST_PATIENCE: Duration = Duration::from_secs(5);

/// Where a run's own directories go, under the data directory.
const SCRATCH: &str = ".pi-setup";

/// What this holds for `pi.models` and `pi.login.*`.
#[derive(Default)]
pub struct PiSetup {
    /// What each target's list was, and what it was made from.
    lists: tokio::sync::Mutex<BTreeMap<&'static str, (String, PiModels)>>,
    /// One start at a time, and the last sign-in, which stays once it's over so
    /// whoever's watching it can hear how it ended.
    starting: tokio::sync::Mutex<()>,
    login: Mutex<Option<Login>>,
    /// On a Mac, what's listening for the browser of a sign-in on another host.
    catcher: Mutex<Option<Catcher>>,
}

struct Login {
    state: watch::Sender<PiLogin>,
    commands: mpsc::Sender<Told>,
    heard: Heard,
}

/// When a client was last heard of. With nobody watching, a sign-in's given up on: an
/// app that quit part way shouldn't leave Pi holding its port for half an hour.
#[derive(Clone)]
struct Heard(Arc<Mutex<Instant>>);

impl Heard {
    fn now() -> Heard {
        Heard(Arc::new(Mutex::new(Instant::now())))
    }

    fn again(&self) {
        *lock(&self.0) = Instant::now();
    }

    fn silent_for(&self) -> Duration {
        lock(&self.0).elapsed()
    }
}

enum Told {
    Answer { id: String, value: String },
    Cancel,
}

struct Catcher {
    login: String,
    port: u16,
    heard: Heard,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for PiSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiSetup").finish_non_exhaustive()
    }
}

impl Drop for Catcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A directory for one run of Pi, gone with this, whichever way it ends: a sign-in
/// for `linux` agents has a login in it. What a crash leaves goes at the next start
/// ([`clear_scratch`]).
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(layout: &Layout, name: &str) -> std::io::Result<Scratch> {
        let root = layout.data_dir().join(SCRATCH);
        vibe_core::paths::private_dir(&layout.data_dir())?;
        pi::make_private(&root)?;
        let scratch = Scratch {
            dir: root.join(name),
        };
        let _ = std::fs::remove_dir_all(&scratch.dir);
        pi::make_private(&scratch.cwd())?;
        pi::make_private(&scratch.agent_dir())?;
        std::fs::write(scratch.extension(), pi_setup::EXTENSION)?;
        Ok(scratch)
    }

    fn cwd(&self) -> PathBuf {
        self.dir.join("cwd")
    }

    fn agent_dir(&self) -> PathBuf {
        self.dir.join("agent")
    }

    fn extension(&self) -> PathBuf {
        self.dir.join(pi_setup::EXTENSION_NAME)
    }

    fn state(&self) -> PathBuf {
        self.dir.join("sign-in-state.json")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Takes away what a run left if `vibed` went down during it, a login perhaps.
pub fn clear_scratch(layout: &Layout) {
    let _ = std::fs::remove_dir_all(layout.data_dir().join(SCRATCH));
}

/// Pi's own stream of what it says, a line at a time, of bounded size.
type Lines = tokio::io::Lines<BufReader<tokio::io::Take<ChildStdout>>>;

/// Pi started in `scratch`, on `env`, having been given `command`.
async fn start_pi(
    program: &Path,
    scratch: &Scratch,
    env: Vec<(String, String)>,
    command: &str,
) -> std::io::Result<(Child, ChildStdin, Lines)> {
    let mut child = Command::new(program)
        .args(pi_setup::args(&scratch.extension()))
        .env_clear()
        .envs(env)
        .current_dir(scratch.cwd())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // What it prints there isn't for anyone, and an error's said in its way.
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
        return Err(std::io::Error::other("Pi started with no pipes"));
    };
    stdin.write_all(format!("{command}\n").as_bytes()).await?;
    stdin.flush().await?;
    Ok((
        child,
        stdin,
        BufReader::new(stdout.take(pi_setup::MAX_OUTPUT)).lines(),
    ))
}

/// What a list depends on, to know when it's out of date: Pi's own files, by when
/// they changed, and which Pi.
fn fingerprint(program: &Path, files: &[PathBuf]) -> String {
    let mut print = program.to_string_lossy().into_owned();
    for file in files {
        let seen = std::fs::metadata(file).ok().map(|m| {
            let at = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            (at, m.len())
        });
        print.push_str(&format!(";{seen:?}"));
    }
    print
}

impl Core {
    /// Which Pi a target comes to here: a host with no VMs has only its own.
    fn pi_target(&self, asked: PiTarget) -> PiTarget {
        if self.info().os == "macos" {
            asked
        } else {
            PiTarget::Native
        }
    }

    /// The Pi this can talk to, if there is one yet: the pinned one, as what
    /// [`pi_setup`] does isn't a documented API. An owner's own `pi` of that version
    /// will do, and it's left as it is otherwise.
    async fn pi_program_now(&self) -> Option<PathBuf> {
        if let Some(program) = &self.pi_driver {
            return Some(program.clone());
        }
        let kept = pi_release::kept_binary(&self.layout.data_dir(), pi_release::PINNED);
        let checks = Arc::clone(&self.checks);
        tokio::task::spawn_blocking(move || {
            if kept.is_file() {
                return Some(kept);
            }
            match checks.pi() {
                Ok((path, version)) if version.trim() == pi_release::PINNED => Some(path),
                _ => None,
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// That Pi, getting it first if there isn't one, which takes as long as the
    /// download. It's checked as any install is, and linked nowhere.
    async fn pi_program(&self) -> Result<PathBuf, String> {
        if let Some(program) = self.pi_program_now().await {
            return Ok(program);
        }
        let _one_at_a_time = self.pi_installing.lock().await;
        if let Some(program) = self.pi_program_now().await {
            return Ok(program);
        }
        let platform =
            pi_release::platform_here().ok_or("Pi publishes no build for this machine")?;
        let (releases, data) = (self.pi_releases.clone(), self.layout.data_dir());
        let kept = tokio::task::spawn_blocking(move || {
            pi_release::keep(&releases, pi_release::PINNED, platform, &data)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("can't get Pi: {e}"))?;
        crate::log!("vibed: Pi {} is at {}", pi_release::PINNED, kept.display());
        Ok(kept)
    }

    /// A new name for a run, or a sign-in.
    fn pi_run_name(&self) -> Result<String, ErrorObject> {
        let random = self.random_16()?;
        Ok(random.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// `pi.models`.
    pub async fn pi_models(&self, p: PiModelsParams) -> Result<PiModels, ErrorObject> {
        self.host(p.host.as_deref())?;
        let target = self.pi_target(p.target);
        let no_list = |why: String| PiModels {
            host: LOCAL_HOST.to_owned(),
            target,
            providers: Vec::new(),
            error: Some(vibe_core::model::clean_alert(&why)),
        };
        // One at a time: it's the same answer for whoever asked meanwhile.
        let mut lists = self.pi_setup.lists.lock().await;
        let program = match self.pi_program().await {
            Ok(program) => program,
            Err(why) => return Ok(no_list(why)),
        };
        let files = self.pi_list_files(target);
        let print = {
            let program = program.clone();
            tokio::task::spawn_blocking(move || fingerprint(&program, &files))
                .await
                .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?
        };
        if let Some((seen, list)) = lists.get(target.as_str())
            && *seen == print
        {
            return Ok(list.clone());
        }
        match self.pi_list(target, &program).await {
            Ok(list) => {
                lists.insert(target.as_str(), (print, list.clone()));
                Ok(list)
            }
            Err(why) => Ok(no_list(why)),
        }
    }

    /// What a target's list comes from.
    fn pi_list_files(&self, target: PiTarget) -> Vec<PathBuf> {
        let mut files = vec![self.layout.pi_env_file()];
        match target {
            PiTarget::Native => {
                let own = pi::own_dir(self.layout.home());
                for name in ["auth.json", "models.json", "models-store.json"] {
                    files.push(own.join(name));
                }
            }
            PiTarget::Vm => {
                files.push(self.layout.linux_pi_logins_dir().join("auth.json"));
                files.push(self.layout.linux_pi_dir().join("models.json"));
            }
        }
        files
    }

    /// Asks Pi for a target's list. It's offline and touches no login. Native agents'
    /// is in the owner's own agent dir, where Pi sees what they have. A VM's Pi
    /// can't be run from here, so it's the Mac's, of the same version, on the
    /// owner's models and with the store's logins looked at beside it.
    async fn pi_list(&self, target: PiTarget, program: &Path) -> Result<PiModels, String> {
        let name = self.pi_run_name().map_err(|e| e.message)?;
        let scratch = Scratch::new(&self.layout, &name).map_err(|e| e.to_string())?;
        let keys = pi::read_env_file(&self.layout.pi_env_file()).unwrap_or_default();
        let (agent_dir, store) = match target {
            PiTarget::Native => (pi::own_dir(self.layout.home()), Vec::new()),
            PiTarget::Vm => {
                let models = self.layout.linux_pi_dir().join("models.json");
                if let Ok(Some(text)) = pi::read_plain_file(&models) {
                    let _ = std::fs::write(scratch.agent_dir().join("models.json"), text);
                }
                (
                    scratch.agent_dir(),
                    pi::logins_in(&self.layout.linux_pi_logins_dir()),
                )
            }
        };
        pi::make_private(&agent_dir).map_err(|e| e.to_string())?;
        let env = pi_setup::env(std::env::vars(), &agent_dir, keys, true);
        let (_child, _stdin, mut lines) =
            start_pi(program, &scratch, env, &pi_setup::catalog_command())
                .await
                .map_err(|e| format!("can't run Pi: {e}"))?;
        let catalog = tokio::time::timeout(LIST_LIMIT, async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match pi_setup::read_line(&line) {
                        Some(Said::Catalog(catalog)) => return Ok(catalog),
                        Some(Said::Failed(why)) => return Err(why),
                        _ => {}
                    },
                    _ => return Err("Pi stopped before it gave its list".to_owned()),
                }
            }
        })
        .await
        .map_err(|_| "Pi took too long to give its list".to_owned())??;
        Ok(PiModels {
            host: LOCAL_HOST.to_owned(),
            target,
            providers: pi_setup::providers_of(catalog, |id| store.iter().any(|s| s == id)),
            error: None,
        })
    }

    /// `pi.login.start`.
    pub async fn pi_login_start(&self, p: PiLoginStartParams) -> Result<PiLogin, ErrorObject> {
        self.host(p.host.as_deref())?;
        if !pi::provider_name_ok(&p.provider) {
            return Err(error(
                ErrorCode::InvalidParams,
                "that's not the name of one of Pi's providers",
            ));
        }
        let target = self.pi_target(p.target);
        let _one_at_a_time = self.pi_setup.starting.lock().await;
        // Whatever's listening for another host's browser wants a port Pi may want.
        *lock(&self.pi_setup.catcher) = None;
        // There's one at a time, and a port for its browser to come back to.
        let previous = lock(&self.pi_setup.login).take();
        if let Some(previous) = previous {
            let mut state = previous.state.subscribe();
            let _ = previous.commands.send(Told::Cancel).await;
            let _ = tokio::time::timeout(
                EXIT_GRACE + SETTLE_AFTER_PAGE,
                state.wait_for(|s| s.state != PiLoginState::Running),
            )
            .await;
        }
        let program = self
            .pi_program()
            .await
            .map_err(|why| error(ErrorCode::CommandFailed, why))?;
        let id = self.pi_run_name()?;
        let scratch = Scratch::new(&self.layout, &id)
            .map_err(|e| error(ErrorCode::CommandFailed, e.to_string()))?;
        let agent_dir = match target {
            PiTarget::Native => pi::own_dir(self.layout.home()),
            PiTarget::Vm => scratch.agent_dir(),
        };
        pi::make_private(&agent_dir).map_err(|e| error(ErrorCode::CommandFailed, e.to_string()))?;
        // No key of `pi.env`'s: it would say nothing here, and a login is what's
        // being made.
        let env = pi_setup::env(std::env::vars(), &agent_dir, Vec::new(), false);
        let (child, stdin, lines) = start_pi(
            &program,
            &scratch,
            env,
            &pi_setup::login_command(&p.provider, p.method),
        )
        .await
        .map_err(|e| error(ErrorCode::CommandFailed, format!("can't run Pi: {e}")))?;
        let (state, watching) = watch::channel(PiLogin {
            id: id.clone(),
            host: LOCAL_HOST.to_owned(),
            target,
            provider: p.provider.clone(),
            method: p.method,
            seq: 0,
            state: PiLoginState::Running,
            url: None,
            instructions: None,
            code: None,
            progress: None,
            prompt: None,
            error: None,
        });
        let (commands, commanded) = mpsc::channel(4);
        let heard = Heard::now();
        *lock(&self.pi_setup.login) = Some(Login {
            state: state.clone(),
            commands,
            heard: heard.clone(),
        });
        let finish = match target {
            PiTarget::Native => None,
            PiTarget::Vm => Some(Broker::of(&self.layout)),
        };
        let publish = self.pi_changed_publisher();
        tokio::spawn(drive(Run {
            child,
            stdin,
            lines,
            commanded,
            state,
            scratch,
            finish,
            publish,
            heard: heard.clone(),
            idle: self.pi_login_idle,
        }));
        let now = settled(watching, 0).await;
        heard.again();
        Ok(now)
    }

    /// The sign-in `id` names, whoever's asking having just been heard from.
    fn pi_login_of(
        &self,
        id: &str,
    ) -> Result<(watch::Sender<PiLogin>, mpsc::Sender<Told>, Heard), ErrorObject> {
        lock(&self.pi_setup.login)
            .as_ref()
            .filter(|l| l.state.borrow().id == id)
            .map(|l| {
                l.heard.again();
                (l.state.clone(), l.commands.clone(), l.heard.clone())
            })
            .ok_or_else(|| error(ErrorCode::NotFound, "that sign-in is over"))
    }

    /// `pi.login.get`.
    pub async fn pi_login_get(&self, p: PiLoginGetParams) -> Result<PiLogin, ErrorObject> {
        self.host(p.host.as_deref())?;
        let (state, _, heard) = self.pi_login_of(&p.id)?;
        let mut state = state.subscribe();
        if let Some(seen) = p.seen {
            let _ = tokio::time::timeout(LONG_POLL, state.wait_for(|s| s.seq != seen)).await;
        }
        heard.again();
        let now = state.borrow().clone();
        Ok(now)
    }

    /// `pi.login.answer`.
    pub async fn pi_login_answer(&self, p: PiLoginAnswerParams) -> Result<PiLogin, ErrorObject> {
        self.host(p.host.as_deref())?;
        if p.value.len() > MAX_ANSWER {
            return Err(error(ErrorCode::InvalidParams, "that answer is too long"));
        }
        let (state, commands, heard) = self.pi_login_of(&p.id)?;
        let moved_on = || {
            error(
                ErrorCode::Conflict,
                "that question has been answered already",
            )
        };
        // Taken as it's read, so only one answer gets through.
        let mut taken = Ok(());
        state.send_if_modified(|now| {
            let Some(prompt) = now.prompt.as_ref().filter(|q| q.id == p.prompt) else {
                taken = Err(moved_on());
                return false;
            };
            if prompt.kind == PiPromptKind::Select
                && !prompt.options.iter().any(|o| o.id == p.value)
            {
                taken = Err(error(
                    ErrorCode::InvalidParams,
                    "that isn't one of the choices",
                ));
                return false;
            }
            // For VMs it could only be refused once Pi's saved it, and Pi on this Mac
            // runs a `!command` as soon as it has. It stays asked.
            if prompt.kind == PiPromptKind::Secret
                && now.target == PiTarget::Vm
                && let Err(why) = pi_logins::plain_key(&p.value)
            {
                taken = Err(error(
                    ErrorCode::InvalidParams,
                    format!(
                        "that can't be given to Linux agents, as it {why}: give the key itself"
                    ),
                ));
                return false;
            }
            now.prompt = None;
            now.seq += 1;
            true
        });
        taken?;
        let watching = state.subscribe();
        let seq = watching.borrow().seq;
        let answer = Told::Answer {
            id: p.prompt,
            value: p.value,
        };
        let _ = commands.send(answer).await;
        let now = settled(watching, seq).await;
        heard.again();
        Ok(now)
    }

    /// `pi.login.cancel`.
    pub async fn pi_login_cancel(&self, p: PiLoginParams) -> Result<PiLogin, ErrorObject> {
        self.host(p.host.as_deref())?;
        let (state, commands, _) = self.pi_login_of(&p.id)?;
        let mut state = state.subscribe();
        let _ = commands.send(Told::Cancel).await;
        let _ = tokio::time::timeout(
            EXIT_GRACE + SETTLE_AFTER_PAGE,
            state.wait_for(|s| s.state != PiLoginState::Running),
        )
        .await;
        let now = state.borrow().clone();
        Ok(now)
    }

    /// Hears what a host said of a sign-in in a call this one forwarded. A page that
    /// comes back to loopback, with a question waiting for where it ended, is the
    /// cue to listen here for the browser (see the top of this file). If that port's
    /// taken the question's still there to be answered by hand. A cancel that didn't
    /// get through is the owner's word all the same, and any other that didn't leaves
    /// it to run out of being heard from.
    pub(crate) fn pi_login_forwarded(&self, link: &Link, method: &str, value: Option<&Value>) {
        let mut catcher = lock(&self.pi_setup.catcher);
        let Some(value) = value else {
            if method == PiLoginCancel::NAME {
                *catcher = None;
            }
            return;
        };
        let Ok(login) = serde_json::from_value::<PiLogin>(value.clone()) else {
            return;
        };
        if let Some(c) = catcher.as_ref().filter(|c| c.login == login.id) {
            c.heard.again();
        }
        if login.state != PiLoginState::Running {
            if catcher.as_ref().is_some_and(|c| c.login == login.id) {
                *catcher = None;
            }
            return;
        }
        let asks_for_it = login
            .prompt
            .as_ref()
            .is_some_and(|q| q.kind == PiPromptKind::ManualCode);
        let redirect = login.url.as_deref().and_then(pi_setup::redirect_of);
        let (true, Some(redirect), Some(me), Some(runtime)) =
            (asks_for_it, redirect, self.me.get(), self.runtime.get())
        else {
            return;
        };
        if catcher.as_ref().is_some_and(|c| {
            c.login == login.id && c.port == redirect.port && !c.task.is_finished()
        }) {
            return;
        }
        // Before the new one takes its port, the old one lets go of it.
        *catcher = None;
        let listener = match bind_loopback(redirect.port) {
            Ok(listener) => listener,
            Err(e) => {
                crate::log!(
                    "vibed: can't listen for a browser on port {}: {e}",
                    redirect.port
                );
                return;
            }
        };
        let port = redirect.port;
        let heard = Heard::now();
        let task = runtime.spawn(catch_redirect(
            listener,
            redirect,
            login.id.clone(),
            link.name.clone(),
            me.clone(),
            (heard.clone(), self.pi_login_idle),
        ));
        *catcher = Some(Catcher {
            login: login.id,
            port,
            heard,
            task,
        });
    }

    /// Hands where a browser came back to on to the sign-in on `host`, and says how
    /// that went.
    async fn pi_login_relay(&self, host: &str, id: &str, back: String) -> Result<PiLogin, String> {
        let link = self
            .link(host)
            .ok_or_else(|| format!("{host} is no longer linked"))?;
        let now = self
            .forward_typed::<PiLoginGet>(
                &link,
                &PiLoginGetParams {
                    host: None,
                    id: id.to_owned(),
                    seen: None,
                },
            )
            .await
            .map_err(|e| e.message)?;
        let prompt = now.prompt.ok_or("it isn't waiting for that")?;
        self.forward_typed::<PiLoginAnswer>(
            &link,
            &PiLoginAnswerParams {
                host: None,
                id: id.to_owned(),
                prompt: prompt.id,
                value: back,
            },
        )
        .await
        .map_err(|e| e.message)
    }
}

fn bind_loopback(port: u16) -> std::io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

/// The sign-in once there's something to show or ask, or it's over, or after a while,
/// from after `since`.
async fn settled(mut state: watch::Receiver<PiLogin>, since: u64) -> PiLogin {
    let mut until = Instant::now() + SETTLE;
    loop {
        {
            let now = state.borrow_and_update();
            if now.seq > since {
                if now.state != PiLoginState::Running || now.prompt.is_some() || now.code.is_some()
                {
                    return now.clone();
                }
                if now.url.is_some() {
                    until = until.min(Instant::now() + SETTLE_AFTER_PAGE);
                }
            }
        }
        let left = until.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, state.changed()).await {
            Ok(Ok(())) => {}
            _ => return state.borrow().clone(),
        }
    }
}

/// One sign-in, on its way.
struct Run {
    child: Child,
    stdin: ChildStdin,
    lines: Lines,
    commanded: mpsc::Receiver<Told>,
    state: watch::Sender<PiLogin>,
    scratch: Scratch,
    /// Where it goes once it's made, for one made away from where it's used.
    finish: Option<Broker>,
    publish: Box<dyn Fn() + Send + Sync>,
    /// When a client was last heard of, and how long it can be silent.
    heard: Heard,
    idle: Duration,
}

/// How a sign-in ended.
enum Ended {
    Done,
    Failed(String),
    Cancelled,
}

/// Runs a sign-in until it's over, and says how that went.
async fn drive(mut run: Run) {
    let ended = talk(&mut run).await;
    // It's said what it had to, and anything it wrote is written: closing its input
    // is what ends an RPC run.
    let Run {
        mut child,
        stdin,
        lines,
        state,
        scratch,
        finish,
        publish,
        ..
    } = run;
    drop(stdin);
    drop(lines);
    if tokio::time::timeout(EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let provider = state.borrow().provider.clone();
    let ended = match (ended, finish) {
        (Ended::Done, Some(broker)) => {
            let (dir, at) = (scratch.agent_dir(), scratch.state());
            let landed =
                tokio::task::spawn_blocking(move || sign_in_lands(&broker, &dir, &at, &provider))
                    .await;
            match landed {
                Ok(Ok(())) => Ended::Done,
                Ok(Err(why)) => Ended::Failed(why),
                Err(e) => Ended::Failed(e.to_string()),
            }
        }
        (ended, _) => ended,
    };
    drop(scratch);
    // Said before the state changes, so whoever hears it's over hears this first.
    if matches!(ended, Ended::Done) {
        publish();
    }
    state.send_modify(|now| {
        now.seq += 1;
        now.url = None;
        now.instructions = None;
        now.code = None;
        now.progress = None;
        now.prompt = None;
        match ended {
            Ended::Done => now.state = PiLoginState::Done,
            Ended::Cancelled => now.state = PiLoginState::Cancelled,
            Ended::Failed(why) => {
                now.state = PiLoginState::Failed;
                now.error = Some(vibe_core::model::clean_alert(&why));
            }
        }
    });
}

/// Carries what Pi says and what it's told, until it's over.
async fn talk(run: &mut Run) -> Ended {
    let limit = tokio::time::sleep(LOGIN_LIMIT);
    tokio::pin!(limit);
    let mut looks = tokio::time::interval(IDLE_LOOK.min(run.idle));
    loop {
        tokio::select! {
            _ = looks.tick() => {
                if run.heard.silent_for() > run.idle {
                    return Ended::Cancelled;
                }
            }
            line = run.lines.next_line() => {
                let Ok(Some(line)) = line else {
                    return Ended::Failed("Pi stopped before it finished signing in".to_owned());
                };
                match pi_setup::read_line(&line) {
                    Some(Said::Done) => return Ended::Done,
                    Some(Said::Failed(why)) => return Ended::Failed(why),
                    Some(said) => hear(&run.state, said),
                    None => {}
                }
            }
            command = run.commanded.recv() => match command {
                Some(Told::Answer { id, value }) => {
                    let line = format!("{}\n", pi_setup::answer_line(&id, &value));
                    let sent = run.stdin.write_all(line.as_bytes()).await;
                    if sent.is_err() || run.stdin.flush().await.is_err() {
                        return Ended::Failed("Pi stopped before it finished signing in".to_owned());
                    }
                }
                Some(Told::Cancel) | None => return Ended::Cancelled,
            },
            () = &mut limit => {
                return Ended::Failed("it was still waiting after half an hour, so it was given up on".to_owned());
            }
        }
    }
}

/// What Pi said, kept for whoever's watching.
fn hear(state: &watch::Sender<PiLogin>, said: Said) {
    state.send_modify(|now| {
        now.seq += 1;
        match said {
            Said::Url { url, instructions } => {
                now.url = Some(url);
                now.instructions = instructions;
            }
            Said::DeviceCode { code, url } => {
                now.code = Some(code);
                now.url = url;
            }
            Said::Progress(what) => now.progress = Some(what),
            Said::Prompt(prompt) => now.prompt = Some(prompt),
            Said::PromptOver => now.prompt = None,
            Said::Catalog(_) | Said::Done | Said::Failed(_) => {}
        }
    });
}

/// Puts what a sign-in for `linux` agents left, in a directory made for it, in the
/// store they get theirs from: it's the owner's, as one in the toolbox is, and held
/// to the same rule of being plain. It's only done once the store holds what this
/// sign-in made: a login that's already there says nothing about this one.
fn sign_in_lands(broker: &Broker, dir: &Path, state: &Path, provider: &str) -> Result<(), String> {
    let failed = |why: String| Err(format!("that login can't be given to your agents: {why}"));
    let made = pi::read_plain_file(&dir.join("auth.json"))
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|mut all| all.get_mut(provider).map(Value::take));
    let Some(made) = made else {
        return failed("Pi left no login for it".to_owned());
    };
    if let Err(why) = pi_logins::plain_login(provider, &made) {
        return failed(why);
    }
    let copy = pi_logins::sign_in_copy(dir, state);
    match broker.sync(&copy, pi_logins::now_ms()) {
        Err(e) => failed(e.to_string()),
        Ok(Synced::Busy) => failed("its file was locked".to_owned()),
        Ok(Synced::Gone) => failed("its directory went".to_owned()),
        Ok(Synced::Quarantined(why)) => failed(why),
        Ok(Synced::Done(_)) => match broker.read_store() {
            Ok(store) if store.get(provider) == Some(&made) => Ok(()),
            Ok(_) => failed("it didn't reach the store".to_owned()),
            Err(e) => failed(e.to_string()),
        },
    }
}

/// Listens at where a browser means to come back to, until it does, and passes that
/// on. Only that page's own answer counts ([`Redirect::is_answered_by`]), so nothing
/// else on this Mac can end the sign-in. It lasts no longer than a sign-in can, and
/// lets go of its port once nobody's been heard from.
async fn catch_redirect(
    listener: TcpListener,
    redirect: Redirect,
    login: String,
    host: String,
    core: Weak<Core>,
    (heard, idle): (Heard, Duration),
) {
    let until = tokio::time::sleep(LOGIN_LIMIT);
    tokio::pin!(until);
    let mut looks = tokio::time::interval(IDLE_LOOK.min(idle));
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = &mut until => return,
            _ = looks.tick() => {
                if heard.silent_for() > idle {
                    return;
                }
                continue;
            }
        };
        let Ok((mut stream, _)) = accepted else {
            continue;
        };
        let target = tokio::time::timeout(REQUEST_PATIENCE, read_target(&mut stream))
            .await
            .ok()
            .flatten();
        let Some(target) = target.filter(|t| redirect.is_answered_by(t)) else {
            let _ = respond(&mut stream, "404 Not Found", "Nothing here.").await;
            continue;
        };
        let back = format!("http://{}:{}{target}", redirect.host, redirect.port);
        let Some(core) = core.upgrade() else { return };
        let page = match core.pi_login_relay(&host, &login, back).await {
            Ok(now) if now.state == PiLoginState::Failed => format!(
                "That didn't work: {}. Go back to VibeOS to try again.",
                now.error.unwrap_or_default()
            ),
            Ok(_) => "That's it. You can close this tab and go back to VibeOS.".to_owned(),
            Err(why) => format!("That didn't work: {why}. Go back to VibeOS to try again."),
        };
        let _ = respond(&mut stream, "200 OK", &page).await;
        return;
    }
}

/// What a browser's request asks for, from a `GET` it finished sending.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut seen = Vec::new();
    let mut buffer = [0_u8; 2048];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buffer).await.ok()?;
        if n == 0 || seen.len() + n > MAX_REQUEST {
            return None;
        }
        seen.extend_from_slice(&buffer[..n]);
    }
    let text = std::str::from_utf8(&seen).ok()?;
    let mut words = text.lines().next()?.split(' ');
    (words.next()? == "GET").then(|| words.next().map(str::to_owned))?
}

async fn respond(stream: &mut TcpStream, status: &str, text: &str) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>VibeOS</title><p>{}</p>",
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn request(port: u16, text: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn a_browsers_request_is_read_and_nothing_else_is_taken_for_one() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut got = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                got.push(read_target(&mut stream).await);
                respond(&mut stream, "200 OK", "<&>").await.unwrap();
            }
            got
        });
        let answer = request(port, "GET /callback?code=1 HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.contains("&lt;&amp;&gt;"), "{answer}");
        request(port, "POST /callback?code=1 HTTP/1.1\r\n\r\n").await;
        request(port, "GET\r\n\r\n").await;
        let too_much = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_REQUEST));
        request(port, &too_much).await;
        assert_eq!(
            server.await.unwrap(),
            [Some("/callback?code=1".to_owned()), None, None, None]
        );
    }

    #[test]
    fn what_a_crash_leaves_of_a_run_goes() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path(), "linux");
        let scratch = Scratch::new(&layout, "abc").unwrap();
        assert!(scratch.extension().is_file());
        assert!(scratch.cwd().is_dir());
        let dir = scratch.dir.clone();
        drop(scratch);
        assert!(!dir.exists());
        let scratch = Scratch::new(&layout, "abc").unwrap();
        std::mem::forget(scratch);
        clear_scratch(&layout);
        assert!(!layout.data_dir().join(SCRATCH).exists());
    }
}
