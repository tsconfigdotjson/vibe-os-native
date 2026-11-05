//! Talking to agents, for MCP (PRD: MCP interface): typing into an agent's
//! terminal (`vibe_send`), waiting for its answer (`vibe_wait`), and
//! reading its screen and its conversation (`vibe_read_output`,
//! `vibe_read_messages`).
//!
//! A send types into the shim as a control client, which doesn't count as
//! attached and doesn't resize the terminal. Text is pasted the way a
//! terminal pastes: wrapped in bracketed-paste markers when the program has
//! asked for them, as Claude Code does, with line endings as carriage
//! returns; then Enter, a moment later, so the two are separate reads.
//!
//! Each send first marks where the agent's transcript is, and a wait asks
//! whether the agent has answered since that mark, so a quiet moment
//! before it starts on the send isn't taken for its answer. A start's
//! prompt counts as a send: an agent with no mark yet is waited on from
//! the start of its transcript.
//!
//! Reads never wake an agent: a sleeping or exited agent gives the screen
//! its shim saved, and its conversation comes from the transcript on the
//! host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use vibe_core::model::{Activity, AgentState, validate_ref};
use vibe_core::rpc::{
    AgentCheck, AgentCheckParams, AgentMessagesParams, AgentOutput, AgentOutputParams,
    AgentSendParams, AgentSent, ErrorCode, ErrorObject, MessageList,
};
use vibe_core::shim::{self, FrameKind, Output};
use vibe_core::store::AgentRecord;
use vibe_mcp::{Check, Input, Screen, Sent};

use crate::core::{Core, error, invalid, lock, store_error};
use crate::sessions;

/// Between a paste and the Enter that submits it, so they are two reads.
const SUBMIT_DELAY: Duration = Duration::from_millis(300);

/// Between keys pressed in one send, so each arrives as its own read.
const KEY_GAP: Duration = Duration::from_millis(100);

/// How long a shim may take to take a send, or answer a read.
const SHIM_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a request id is remembered: time for a caller who never heard to
/// find out why and ask again.
const SEND_KEPT: Duration = Duration::from_secs(10 * 60);

/// The most request ids remembered; past it the oldest goes first.
const MAX_SENDS_KEPT: usize = 4096;

/// A send made with a request id, for its agent: what it was, and how it went
/// once it's done.
/// Its text isn't kept, only a hash of it.
pub(crate) struct KeptSend {
    print: String,
    at: Instant,
    answer: watch::Receiver<Option<Result<Sent, ErrorObject>>>,
}

impl KeptSend {
    /// Whether it's still being made: its answer's yet to come, and whatever was
    /// making it hasn't gone.
    fn under_way(&self) -> bool {
        self.answer.borrow().is_none() && self.answer.has_changed().is_ok()
    }
}

/// Forgets sends past [`SEND_KEPT`]. Not one under way: a retry would type it again.
fn forget_old_sends(kept: &mut HashMap<String, KeptSend>) {
    kept.retain(|_, s| s.under_way() || s.at.elapsed() < SEND_KEPT);
}

/// Forgets the oldest finished send if there's no room for another. Sends under way
/// all stay for the same reason, even past [`MAX_SENDS_KEPT`]: there are only so many
/// of those, as each is a call being answered.
fn make_room_for_a_send(kept: &mut HashMap<String, KeptSend>) {
    if kept.len() < MAX_SENDS_KEPT {
        return;
    }
    let oldest = kept
        .iter()
        .filter(|(_, s)| !s.under_way())
        .min_by_key(|(_, s)| s.at)
        .map(|(slot, _)| slot.clone());
    if let Some(oldest) = oldest {
        kept.remove(&oldest);
    }
}

/// After a wake that started Claude again, how long its screen must stay
/// still before it's taken to be ready for input, unless it turns on
/// bracketed paste first; and the longest that's waited for.
const SETTLE: Duration = Duration::from_secs(1);
const READY_WITHIN: Duration = Duration::from_secs(30);

/// How long a Pi that's just started may hand a send back. Fetching its tools is
/// the slowest it does: two downloads, each given up on after 10 s.
const PI_STARTUP_WITHIN: Duration = Duration::from_secs(60);

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// The bytes a paste of `text` puts on the wire: line endings become
/// carriage returns, the way a terminal sends Enter, and with `bracketed`
/// the text is wrapped in the markers, so a multi-line prompt arrives as
/// one block instead of submitting at its first newline. Markers inside the
/// text are dropped, as one would end the paste early.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text
        .replace(PASTE_START, "")
        .replace(PASTE_END, "")
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    if bracketed {
        format!("{PASTE_START}{body}{PASTE_END}").into_bytes()
    } else {
        body.into_bytes()
    }
}

/// The screen only, with no history, and whether the program wants
/// bracketed paste: a read from past the end of the history.
pub(crate) fn screen_now(socket: &Path) -> std::io::Result<Output> {
    let mut c = shim::connect(socket, &shim::control_hello("vibed"), SHIM_TIMEOUT)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    c.read_output(Some(u64::MAX))
}

/// Types `chunks` into the shim at `socket`, each followed by its pause,
/// then asks for its status, so everything typed has reached it before
/// this returns.
fn type_into(socket: &Path, chunks: &[(Vec<u8>, Duration)]) -> std::io::Result<()> {
    let mut c = shim::connect(socket, &shim::control_hello("vibed"), SHIM_TIMEOUT)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    for (bytes, pause) in chunks {
        for frame in shim::output_frames(FrameKind::Input, bytes) {
            shim::write_frame(&mut c.stream, &frame)?;
        }
        if !pause.is_zero() {
            std::thread::sleep(*pause);
        }
    }
    c.status().map(|_| ())
}

/// The refusal of text sent to an agent on a question.
fn on_a_question(agent_ref: &str, woken: &str) -> ErrorObject {
    error(
        ErrorCode::Conflict,
        format!(
            "{agent_ref} is waiting on a question, so nothing was pasted.{woken} Read it with \
             vibe_read_output, then answer with vibe_send keys: the option's number, like \
             [\"2\"], or [\"Up\"] and [\"Down\"] to move the ❯ marker, then [\"Enter\"]. Or \
             pass force: true to paste anyway"
        ),
    )
}

/// The bytes of a send, in the reads they go as.
fn chunks_of(input: &Input, bracketed: bool) -> Vec<(Vec<u8>, Duration)> {
    match input {
        Input::Text { text, submit, .. } => {
            let mut out = vec![(
                paste_bytes(text, bracketed),
                if *submit {
                    SUBMIT_DELAY
                } else {
                    Duration::ZERO
                },
            )];
            if *submit {
                out.push((b"\r".to_vec(), Duration::ZERO));
            }
            out
        }
        Input::Keys(keys) => {
            let last = keys.len().saturating_sub(1);
            // Each name was checked before the send began.
            keys.iter()
                .enumerate()
                .map(|(i, k)| {
                    (
                        vibe_mcp::keys::bytes(k).unwrap_or_default(),
                        if i < last { KEY_GAP } else { Duration::ZERO },
                    )
                })
                .collect()
        }
    }
}

/// The part of a saved screen's history after `after`, as a shim would
/// give it.
fn saved_since(output: Output, after: Option<u64>) -> Screen {
    let total = output.cursor;
    let first_kept = total.saturating_sub(output.history.len() as u64);
    let after = after.unwrap_or(first_kept).min(total);
    let start = after.max(first_kept);
    Screen {
        screen: output.screen,
        history: output
            .history
            .into_iter()
            .skip((start - first_kept) as usize)
            .collect(),
        cursor: total,
        dropped: start - after,
        more: false,
        live: false,
    }
}

impl Core {
    /// The record of `agent_ref` on `host`, for a call that names it.
    fn named(&self, host: Option<&str>, agent_ref: &str) -> Result<AgentRecord, ErrorObject> {
        let host = self.host(host)?;
        validate_ref(agent_ref).map_err(|_| {
            error(
                ErrorCode::NotFound,
                format!("no agent called {agent_ref} on {host}"),
            )
        })?;
        self.store().agent(host, agent_ref).map_err(|e| {
            let e = store_error(e);
            if e.code == ErrorCode::NotFound.code() {
                error(
                    ErrorCode::NotFound,
                    format!("no agent called {agent_ref} on {host}"),
                )
            } else {
                e
            }
        })
    }

    /// One send at a time to each agent, so two can't interleave.
    pub(crate) fn typing(&self, agent_ref: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(lock(&self.typing).entry(agent_ref.to_owned()).or_default())
    }

    /// Types `input` into `agent_ref`'s terminal (PRD: MCP interface,
    /// `vibe_send`). One asleep, paused or exited is woken or resumed
    /// first. Text is refused while it's on a question, unless forced:
    /// pasted into a menu, it would pick whatever its keys spell.
    pub async fn send_input(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        input: Input,
    ) -> Result<Sent, ErrorObject> {
        self.send_noting(host, agent_ref, input, &AtomicBool::new(false))
            .await
    }

    /// [`Core::send_input`] at most once for `id`, so a caller who never heard
    /// how a send went can make it again. The same id gets the first one's
    /// answer without typing anything, and waits for it if it's still under
    /// way. One refused before any of it was typed isn't remembered, so it can
    /// be made again with the same id. It's kept for its agent on the host that
    /// types, and forgotten after [`SEND_KEPT`] or when that `vibed` restarts.
    pub async fn send_once(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        input: Input,
        id: &str,
    ) -> Result<Sent, ErrorObject> {
        vibe_mcp::check_request_id(id).map_err(invalid)?;
        let print = vibe_core::keys::hash(&format!("{input:?}"));
        let slot = format!("{agent_ref}\n{id}");
        let (first, mut answer) = {
            let mut kept = lock(&self.sends_kept);
            forget_old_sends(&mut kept);
            match kept.get(&slot) {
                Some(s) if s.print != print => {
                    return Err(error(
                        ErrorCode::Conflict,
                        format!("request_id {id} was used for a different send; use a new one"),
                    ));
                }
                Some(s) => (None, s.answer.clone()),
                None => {
                    make_room_for_a_send(&mut kept);
                    let (done, answer) = watch::channel(None);
                    kept.insert(
                        slot.clone(),
                        KeptSend {
                            print,
                            at: Instant::now(),
                            answer: answer.clone(),
                        },
                    );
                    (Some(done), answer)
                }
            }
        };
        if let Some(done) = first {
            let once = SendOnce {
                slot,
                host: host.map(str::to_owned),
                agent: agent_ref.to_owned(),
                input,
                done,
            };
            // On a task of its own, where it can: a caller that goes away
            // mustn't leave it half typed for its retry to type again.
            match self.me.get().and_then(std::sync::Weak::upgrade) {
                Some(core) => {
                    tokio::spawn(async move { once.run(&core).await });
                }
                None => once.run(self).await,
            }
        }
        let answered = answer.wait_for(Option::is_some).await;
        match answered {
            Ok(a) => a.clone().expect("waited for it"),
            Err(_) => Err(error(
                ErrorCode::InternalError,
                format!("Internal error: the send to {agent_ref} ended without an answer"),
            )),
        }
    }

    /// [`Core::send_input`], with `typing_began` set just before any of it is
    /// typed: until then, nothing has reached the agent.
    async fn send_noting(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        input: Input,
        typing_began: &AtomicBool,
    ) -> Result<Sent, ErrorObject> {
        let record = self.named(host, agent_ref)?;
        let host = record.host.clone();
        // A sleep or a wake under way is waited for: then it's woken, or
        // typed to, as that left it.
        if record.starting || !self.wait_not_busy(agent_ref).await {
            return Err(error(
                ErrorCode::Conflict,
                format!("{agent_ref} is being started, woken or deleted; try again in a moment"),
            ));
        }
        let mut record = self.named(Some(&host), agent_ref)?;
        if let Input::Keys(keys) = &input {
            if keys.is_empty() {
                return Err(invalid("give keys to press"));
            }
            for k in keys {
                vibe_mcp::keys::bytes(k).map_err(invalid)?;
            }
        }
        let typing = self.typing(agent_ref);
        let _one_at_a_time = typing.lock().await;
        let debounce = self.idle_debounce();
        let status = self.agent_status(&mut record, debounce).await;
        let state = self.observed_state(&record, &status).await;
        if let Input::Text { force: false, .. } = &input
            && state == AgentState::Question
        {
            return Err(on_a_question(agent_ref, ""));
        }
        let socket = self.layout.session_socket(agent_ref);
        let woke = matches!(
            state,
            AgentState::Asleep | AgentState::Paused | AgentState::Exited
        );
        let mut just_started = false;
        if woke {
            self.resume_agent(Some(&host), agent_ref).await?;
            crate::log!("vibed: {agent_ref} was {state}, and a send woke it");
            just_started = self.wait_started(agent_ref, &socket).await?;
            // Asleep or paused, it read as that; woken where it was, it may
            // be on a question. One resumed with `--resume` has lost any
            // question it had (PRD: Going to sleep), whatever its
            // transcript last said.
            let mut record = self.named(Some(&host), agent_ref)?;
            let status = self.agent_status(&mut record, debounce).await;
            if state != AgentState::Exited
                && let Input::Text { force: false, .. } = &input
                && self.observed_state(&record, &status).await == AgentState::Question
            {
                return Err(on_a_question(agent_ref, " It was woken first."));
            }
        }
        // Started moments ago, by a resume or a start that came before this. The
        // look for a question came before the wait, and one may have come up since.
        if self.wait_started(agent_ref, &socket).await? {
            just_started = true;
            let mut record = self.named(Some(&host), agent_ref)?;
            let status = self.agent_status(&mut record, debounce).await;
            if let Input::Text { force: false, .. } = &input
                && self.observed_state(&record, &status).await == AgentState::Question
            {
                return Err(on_a_question(agent_ref, ""));
            }
        }
        // Where its transcript is now: the answer to this send is what
        // comes after. It's kept only once the send is in, so a send that
        // failed leaves the wait on the one before.
        let mark = self.mark_send(Some(&host), agent_ref).await?;
        let s = socket.clone();
        let screen = tokio::task::spawn_blocking(move || screen_now(&s))
            .await
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?;
        let screen = match screen {
            Ok(screen) => screen,
            Err(e) => {
                // A Pi that gave up again as it was woken has said why.
                let said = match self.named(Some(&host), agent_ref) {
                    Ok(now) if now.agent == vibe_core::model::AgentKind::Pi => {
                        self.pi_gave_up_with(&now).await
                    }
                    _ => None,
                };
                let why = said.unwrap_or_else(|| e.to_string());
                return Err(error(
                    ErrorCode::Conflict,
                    format!("{agent_ref} isn't running, so nothing was sent: {why}"),
                ));
            }
        };
        let chunks = chunks_of(&input, screen.bracketed_paste);
        typing_began.store(true, Ordering::SeqCst);
        let s = socket.clone();
        tokio::task::spawn_blocking(move || type_into(&s, &chunks))
            .await
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?
            .map_err(|e| {
                error(
                    ErrorCode::CommandFailed,
                    format!("{agent_ref} didn't take the send: {e}"),
                )
            })?;
        if just_started
            && record.agent == vibe_core::model::AgentKind::Pi
            && let Input::Text {
                text, submit: true, ..
            } = &input
        {
            see_pi_take(agent_ref, &socket, text).await?;
        }
        // A delete may have run meanwhile: then there's nothing to wait on,
        // and a new agent by the same name mustn't inherit this mark.
        // Its debounce starts again from the send: Claude may not have
        // written what it read yet, and it mustn't sleep with it unread.
        lock(&self.sleep_watch).woke(agent_ref);
        let state = match self.store().agent(&host, agent_ref) {
            Ok(r) => {
                lock(&self.sends).insert(agent_ref.to_owned(), mark);
                // Pi takes `/model` or `!ls` itself, and writes nothing for them: no
                // answer is owed, and nothing on its screen is about them. Any other
                // prompt is owed one, whatever it starts with.
                let answer_owed = r.agent == vibe_core::model::AgentKind::Pi
                    && matches!(&input, Input::Text { submit: true, text, .. }
                        if !pi_takes_itself(text));
                if answer_owed {
                    lock(&self.pi_sent_at)
                        .insert(agent_ref.to_owned(), crate::status::wall_clock());
                } else {
                    // The mark just taken is this send's, so one before it says
                    // nothing now.
                    lock(&self.pi_sent_at).remove(agent_ref);
                }
                r.state
            }
            Err(_) => AgentState::Exited,
        };
        Ok(Sent { woke, state })
    }

    /// If `agent_ref`'s program was started and hasn't been waited for yet, waits
    /// until it's up: typed to before that, what's typed waits in a terminal that
    /// isn't raw yet, so its Enter arrives as a newline in the text and nothing is
    /// submitted. A frozen program thawed is where it was, and isn't waited for.
    /// Says whether it waited, and fails if the program still isn't up: only the
    /// first send is held to that, so another types, for a caller who's looked.
    async fn wait_started(&self, agent_ref: &str, socket: &Path) -> Result<bool, ErrorObject> {
        let started = lock(&self.started_at).remove(agent_ref);
        let Some(left) = started.and_then(|at| READY_WITHIN.checked_sub(at.elapsed())) else {
            return Ok(false);
        };
        if !wait_ready(socket, left).await {
            return Err(error(
                ErrorCode::Conflict,
                format!(
                    "{agent_ref} hasn't come up {}s after it started, so nothing was sent. See \
                     what it shows with vibe_read_output; a send from now on is typed without \
                     waiting",
                    READY_WITHIN.as_secs()
                ),
            ));
        }
        Ok(true)
    }

    /// Where `agent_ref` is, and whether it has answered its last send
    /// (PRD: MCP interface, `vibe_wait`). Never wakes it.
    pub async fn check_agent(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        background: bool,
    ) -> Result<Check, ErrorObject> {
        let mut record = self.named(host, agent_ref)?;
        let mark = lock(&self.sends)
            .get(agent_ref)
            .cloned()
            .unwrap_or_default();
        let debounce = self.idle_debounce();
        let (status, answered) = self
            .with_tail(&mut record, debounce, move |t, now| {
                (t.status(now), t.answered_after(&mark, now, background))
            })
            .await
            .unwrap_or_else(|| (vibe_status::Status::unknown(), false));
        // A Pi that failed where only its screen shows has answered all it will.
        let (status, answered) = if record.agent == vibe_core::model::AgentKind::Pi {
            let (status, failed) = self.pi_read_status(&mut record, debounce, status).await;
            (status, answered || failed)
        } else {
            (status, answered)
        };
        let state = if record.starting || self.is_busy(agent_ref) {
            record.state
        } else {
            self.observed_state(&record, &status).await
        };
        // A permission prompt agent-tail only guesses at is no answer while
        // the agent is busy, as a long silent tool run is: the wait goes on
        // until the screen shows the dialog, or the tool ends.
        let answered = answered
            && !(status.activity == Activity::AwaitingApproval && state == AgentState::Busy);
        Ok(Check {
            state,
            activity: status.activity,
            line: status.line.clone(),
            answered,
        })
    }

    /// What `agent_ref`'s terminal shows, and what scrolled off it since
    /// `after` (PRD: MCP interface, `vibe_read_output`): live from its
    /// shim, or the last screen its shim saved. Never wakes it.
    pub async fn agent_output(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        after: Option<u64>,
    ) -> Result<Screen, ErrorObject> {
        self.named(host, agent_ref)?;
        let socket = self.layout.session_socket(agent_ref);
        if let sessions::Probe::Running(_) = sessions::probe(&socket).await {
            let read = tokio::task::spawn_blocking(move || {
                let mut c = shim::connect(&socket, &shim::control_hello("vibed"), SHIM_TIMEOUT)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                c.read_output(after)
            })
            .await
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?;
            // It may have ended since; then its record says the rest.
            if let Ok(o) = read {
                return Ok(Screen {
                    screen: o.screen,
                    history: o.history,
                    cursor: o.cursor,
                    dropped: o.dropped,
                    more: o.more,
                    live: true,
                });
            }
        }
        Ok(match sessions::read_record(&self.layout, agent_ref) {
            Some(record) => saved_since(record.output, after),
            None => Screen::default(),
        })
    }

    /// The last `last` messages of `agent_ref`'s conversation, from its
    /// transcript (PRD: MCP interface, `vibe_read_messages`). Never wakes
    /// it. None before Claude has written any.
    pub async fn agent_messages(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        last: usize,
    ) -> Result<Vec<vibe_status::Message>, ErrorObject> {
        let mut record = self.named(host, agent_ref)?;
        let debounce = self.idle_debounce();
        let path: Option<PathBuf> = self
            .with_tail(&mut record, debounce, |t, _| {
                t.transcript().map(Path::to_owned)
            })
            .await
            .flatten();
        let Some(path) = path else {
            return Ok(Vec::new());
        };
        tokio::task::spawn_blocking(move || vibe_status::last_messages(&path, last))
            .await
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?
            .map_err(|e| {
                error(
                    ErrorCode::InternalError,
                    format!("Internal error: can't read {agent_ref}'s transcript: {e}"),
                )
            })
    }
}

/// How many messages `agents.messages` gives when it isn't told, and the
/// most it gives, as `vibe_read_messages` does.
const DEFAULT_MESSAGES: u32 = 10;
const MAX_MESSAGES: u32 = 500;

/// What `agents.send` types: text or keys, as `vibe_send` takes them.
pub fn input_of(p: &AgentSendParams) -> Result<Input, ErrorObject> {
    match (&p.text, &p.keys) {
        (Some(_), Some(_)) => Err(invalid("give text or keys, not both")),
        (None, None) => Err(invalid("give text to send, or keys to press")),
        (Some(text), None) => Ok(Input::Text {
            text: text.clone(),
            submit: p.submit.unwrap_or(true),
            force: p.force.unwrap_or(false),
        }),
        (None, Some(keys)) => {
            if p.submit.is_some() {
                return Err(invalid(
                    "submit goes with text; press [\"Enter\"] with keys",
                ));
            }
            Ok(Input::Keys(keys.clone()))
        }
    }
}

/// One send under way for [`Core::send_once`].
struct SendOnce {
    slot: String,
    host: Option<String>,
    agent: String,
    input: Input,
    done: watch::Sender<Option<Result<Sent, ErrorObject>>>,
}

impl SendOnce {
    async fn run(self, core: &Core) {
        let typing_began = AtomicBool::new(false);
        let answer = core
            .send_noting(self.host.as_deref(), &self.agent, self.input, &typing_began)
            .await;
        if answer.is_err() && !typing_began.load(Ordering::SeqCst) {
            lock(&core.sends_kept).remove(&self.slot);
        }
        self.done.send_replace(Some(answer));
    }
}

/// `input` as `agents.send` carries it to another host, to be typed there once
/// for `request_id`.
pub fn send_params(host: &str, agent: &str, input: &Input, request_id: &str) -> AgentSendParams {
    let mut p = AgentSendParams {
        host: Some(host.to_owned()),
        agent: agent.to_owned(),
        request_id: Some(request_id.to_owned()),
        ..Default::default()
    };
    match input {
        Input::Text {
            text,
            submit,
            force,
        } => {
            p.text = Some(text.clone());
            p.submit = Some(*submit);
            p.force = force.then_some(true);
        }
        Input::Keys(keys) => p.keys = Some(keys.clone()),
    }
    p
}

// The socket's doors to the same handlers MCP calls, which a Mac uses to
// reach a Linux host's agents over its link.
impl Core {
    pub async fn send_call(&self, p: AgentSendParams) -> Result<AgentSent, ErrorObject> {
        let input = input_of(&p)?;
        let host = p.host.as_deref();
        let sent = match p.request_id.as_deref() {
            Some(id) => self.send_once(host, &p.agent, input, id).await?,
            None => self.send_input(host, &p.agent, input).await?,
        };
        Ok(AgentSent {
            woke: sent.woke,
            state: sent.state,
        })
    }

    pub async fn check_call(&self, p: AgentCheckParams) -> Result<AgentCheck, ErrorObject> {
        let c = self
            .check_agent(p.host.as_deref(), &p.agent, p.background.unwrap_or(false))
            .await?;
        Ok(AgentCheck {
            state: c.state,
            activity: c.activity,
            line: c.line,
            answered: c.answered,
        })
    }

    pub async fn output_call(&self, p: AgentOutputParams) -> Result<AgentOutput, ErrorObject> {
        let s = self
            .agent_output(p.host.as_deref(), &p.agent, p.after)
            .await?;
        Ok(AgentOutput {
            screen: s.screen,
            history: s.history,
            cursor: s.cursor,
            dropped: s.dropped,
            more: s.more,
            live: s.live,
        })
    }

    pub async fn messages_call(&self, p: AgentMessagesParams) -> Result<MessageList, ErrorObject> {
        let last = p.last.unwrap_or(DEFAULT_MESSAGES);
        if !(1..=MAX_MESSAGES).contains(&last) {
            return Err(invalid(format!("last must be from 1 to {MAX_MESSAGES}")));
        }
        let messages = self
            .agent_messages(p.host.as_deref(), &p.agent, last as usize)
            .await?;
        Ok(MessageList { messages })
    }
}

/// Waits for a program just started to be ready for input: it has turned
/// on bracketed paste, or its screen has drawn something and stayed still
/// for [`SETTLE`]. Looks at least once. False if `within` passes first; if its
/// session ends the wait's over at once, and what comes next finds that out.
async fn wait_ready(socket: &Path, within: Duration) -> bool {
    let begun = Instant::now();
    let mut last: Option<(String, Instant)> = None;
    loop {
        let s = socket.to_owned();
        let Ok(Ok(out)) = tokio::task::spawn_blocking(move || screen_now(&s)).await else {
            return true;
        };
        if out.bracketed_paste {
            return true;
        }
        match &last {
            Some((screen, since)) if *screen == out.screen => {
                if !screen.trim().is_empty() && since.elapsed() >= SETTLE {
                    return true;
                }
            }
            _ => last = Some((out.screen, Instant::now())),
        }
        if begun.elapsed() >= within {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Pi 1.5's own commands that are only ever the whole of what's submitted
/// (`interactive-mode.ts`). With anything after them, they go to the model.
const PI_COMMANDS: &[&str] = &[
    "/settings",
    "/scoped-models",
    "/share",
    "/copy",
    "/session",
    "/changelog",
    "/hotkeys",
    "/fork",
    "/clone",
    "/tree",
    "/trust",
    "/logout",
    "/new",
    "/reload",
    "/debug",
    "/arminsayshi",
    "/dementedelves",
    "/resume",
    "/quit",
];

/// Those that may have words after them, past a space.
const PI_COMMANDS_WITH_WORDS: &[&str] = &[
    "/model",
    "/thinking",
    "/export",
    "/import",
    "/bug",
    "/name",
    "/login",
    "/compact",
];

/// Whether Pi deals with `text` itself when it's submitted, as its editor sorts it
/// (`interactive-mode.ts`), so no turn comes of it: nothing at all, one of its
/// commands, or a `!` with a command to run. Everything else goes on as a prompt,
/// a path or a `/skill:` as much as a question, and can fail as any prompt can.
///
/// Only Pi knows an extension's commands, so one reads as a prompt here. If it writes
/// and draws nothing, under what reads just like one of Pi's errors, that reads as
/// the turn having failed, until the next send.
///
/// A paste gets to Pi's editor with its tabs as four spaces and its other control
/// characters gone (`editor.ts`), so that's what's looked at.
fn pi_takes_itself(text: &str) -> bool {
    let text: String = text
        .replace('\t', "    ")
        .chars()
        .filter(|c| *c >= ' ' || matches!(c, '\n' | '\r'))
        .collect();
    let text = text.trim();
    let command = text.strip_prefix("!!").or_else(|| text.strip_prefix('!'));
    text.is_empty()
        || PI_COMMANDS.contains(&text)
        || PI_COMMANDS_WITH_WORDS.iter().any(|name| {
            text.strip_prefix(name)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
        })
        || command.is_some_and(|command| !command.trim().is_empty())
}

/// Sees a Pi that's just started take what was sent. It can look ready, with its
/// editor up, while its startup goes on, which can take a while if it's fetching
/// its tools: until then it hands back whatever's submitted. So while the text's
/// still in its editor, Enter is pressed again. Nothing's typed twice. If it never
/// takes it, the send says so.
async fn see_pi_take(agent_ref: &str, socket: &Path, text: &str) -> Result<(), ErrorObject> {
    let begun = Instant::now();
    loop {
        tokio::time::sleep(SUBMIT_DELAY).await;
        let s = socket.to_owned();
        let Ok(Ok(out)) = tokio::task::spawn_blocking(move || screen_now(&s)).await else {
            return Ok(());
        };
        if !vibe_status::pi_editor_holds(&out.screen, text) {
            return Ok(());
        }
        if begun.elapsed() >= PI_STARTUP_WITHIN {
            return Err(error(
                ErrorCode::Conflict,
                format!(
                    "{agent_ref} is still starting after {}s, and hasn't taken that: it's in \
                     its editor, to be submitted with [\"Enter\"]",
                    PI_STARTUP_WITHIN.as_secs()
                ),
            ));
        }
        let s = socket.to_owned();
        let enter = vec![(b"\r".to_vec(), Duration::ZERO)];
        tokio::task::spawn_blocking(move || type_into(&s, &enter))
            .await
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?
            .map_err(|e| {
                error(
                    ErrorCode::CommandFailed,
                    format!("{agent_ref} didn't take the send: {e}"),
                )
            })?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kept send, `age` old: finished, or still under way for as long as the sender
    /// it comes with is held.
    fn a_send(
        age: Duration,
        finished: bool,
    ) -> (KeptSend, watch::Sender<Option<Result<Sent, ErrorObject>>>) {
        let (done, answer) = watch::channel(None);
        if finished {
            done.send_replace(Some(Err(invalid("how it went"))));
        }
        let kept = KeptSend {
            print: String::new(),
            at: Instant::now().checked_sub(age).unwrap(),
            answer,
        };
        (kept, done)
    }

    #[test]
    fn a_send_under_way_is_never_forgotten() {
        let mut kept = HashMap::new();
        let mut held = Vec::new();
        let long_ago = SEND_KEPT + Duration::from_secs(60);

        // Past its time, it goes only if it's finished.
        for (slot, finished) in [("under way", false), ("finished", true)] {
            let (send, done) = a_send(long_ago, finished);
            kept.insert(slot.to_owned(), send);
            held.push(done);
        }
        forget_old_sends(&mut kept);
        assert!(kept.contains_key("under way"));
        assert!(!kept.contains_key("finished"));

        // Whatever was making it having gone, it goes the same way.
        held.clear();
        forget_old_sends(&mut kept);
        assert!(kept.is_empty());

        // With no room, the oldest finished one goes, not an older one under way.
        let (send, done) = a_send(long_ago, false);
        kept.insert("under way".to_owned(), send);
        held.push(done);
        let (send, done) = a_send(Duration::from_secs(120), true);
        kept.insert("oldest finished".to_owned(), send);
        held.push(done);
        for n in kept.len()..MAX_SENDS_KEPT {
            let (send, done) = a_send(Duration::from_secs(1), true);
            kept.insert(format!("newer {n}"), send);
            held.push(done);
        }
        make_room_for_a_send(&mut kept);
        assert_eq!(kept.len(), MAX_SENDS_KEPT - 1);
        assert!(kept.contains_key("under way"));
        assert!(!kept.contains_key("oldest finished"));

        // With room, nothing goes.
        make_room_for_a_send(&mut kept);
        assert_eq!(kept.len(), MAX_SENDS_KEPT - 1);

        // With all of them under way, none goes.
        kept.clear();
        held.clear();
        for n in 0..MAX_SENDS_KEPT {
            let (send, done) = a_send(Duration::from_secs(1), false);
            kept.insert(format!("under way {n}"), send);
            held.push(done);
        }
        make_room_for_a_send(&mut kept);
        assert_eq!(kept.len(), MAX_SENDS_KEPT);
    }

    #[test]
    fn pastes_are_bracketed_when_asked_for() {
        assert_eq!(paste_bytes("hi", false), b"hi");
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(
            paste_bytes("x\x1b[201~y\x1b[200~", true),
            b"\x1b[200~xy\x1b[201~"
        );
    }

    #[test]
    fn sends_go_as_separate_reads() {
        let text = Input::Text {
            text: "go".into(),
            submit: true,
            force: false,
        };
        assert_eq!(
            chunks_of(&text, false),
            [
                (b"go".to_vec(), SUBMIT_DELAY),
                (b"\r".to_vec(), Duration::ZERO)
            ]
        );
        let pasted = Input::Text {
            text: "go".into(),
            submit: false,
            force: false,
        };
        assert_eq!(
            chunks_of(&pasted, true),
            [(b"\x1b[200~go\x1b[201~".to_vec(), Duration::ZERO)]
        );
        let keys = Input::Keys(vec!["Down".into(), "Enter".into()]);
        assert_eq!(
            chunks_of(&keys, true),
            [
                (b"\x1b[B".to_vec(), KEY_GAP),
                (b"\r".to_vec(), Duration::ZERO)
            ]
        );
    }

    #[test]
    fn sends_over_the_socket_are_text_or_keys() {
        let p = |text: Option<&str>, keys: Option<&[&str]>, submit: Option<bool>| AgentSendParams {
            host: None,
            agent: "a".into(),
            text: text.map(str::to_owned),
            submit,
            force: None,
            keys: keys.map(|k| k.iter().map(|s| s.to_string()).collect()),
            request_id: None,
        };
        assert_eq!(
            input_of(&p(Some("hi"), None, None)).unwrap(),
            Input::Text {
                text: "hi".into(),
                submit: true,
                force: false
            }
        );
        assert_eq!(
            input_of(&p(None, Some(&["Down"]), None)).unwrap(),
            Input::Keys(vec!["Down".into()])
        );
        for bad in [
            p(Some("a"), Some(&["b"]), None),
            p(None, None, None),
            p(None, Some(&["Enter"]), Some(true)),
        ] {
            assert_eq!(
                input_of(&bad).unwrap_err().code,
                ErrorCode::InvalidParams.code()
            );
        }
        // And back, as a Mac forwards a send to a Linux host.
        for input in [
            Input::Text {
                text: "go".into(),
                submit: false,
                force: true,
            },
            Input::Keys(vec!["2".into(), "Enter".into()]),
        ] {
            let params = send_params("vps", "a", &input, "id-1");
            assert_eq!(params.host.as_deref(), Some("vps"));
            assert_eq!(params.request_id.as_deref(), Some("id-1"));
            assert_eq!(input_of(&params).unwrap(), input);
        }
    }

    /// As Pi 1.5's editor sorts what's submitted (`interactive-mode.ts`).
    #[test]
    fn what_pi_takes_itself_owes_no_answer() {
        for text in [
            "",
            "  \n",
            "/model",
            "  /model\n",
            "/model openai/gpt-5",
            "/compact keep the plan",
            "/login",
            "/name a name",
            "/model\twhat's this?",
            "/model\x07",
            "/new",
            "/quit",
            "/settings",
            "/session",
            "!ls",
            "! ls",
            " !ls",
            "!!ls",
            "!! ls",
            "!!!",
        ] {
            assert!(pi_takes_itself(text), "{text:?}");
        }
        for text in [
            // A path, a skill or template, or nothing Pi knows: all go to the model.
            "/tmp/notes.txt is empty, why?",
            "/skill:review the diff",
            "/fix the tests",
            "/models",
            // Only some of Pi's commands take words, and only after a space.
            "/quit now",
            "/new branch please",
            "/session what's this?",
            "/model\nwhat's this?",
            // With no command to run, Pi sends these on too.
            "!",
            "!!",
            " ! ",
            "hello",
            "what does /model do?",
        ] {
            assert!(!pi_takes_itself(text), "{text:?}");
        }
    }

    #[test]
    fn a_saved_screen_reads_from_a_cursor() {
        let saved = Output {
            screen: "$".into(),
            history: vec!["c".into(), "d".into(), "e".into()],
            // Five lines scrolled off; the first two are no longer kept.
            cursor: 5,
            dropped: 0,
            more: false,
            bracketed_paste: false,
        };
        let all = saved_since(saved.clone(), None);
        assert_eq!(all.history, ["c", "d", "e"]);
        assert_eq!((all.cursor, all.dropped, all.live), (5, 0, false));
        let some = saved_since(saved.clone(), Some(3));
        assert_eq!(some.history, ["d", "e"]);
        let lost = saved_since(saved.clone(), Some(0));
        assert_eq!(lost.history, ["c", "d", "e"]);
        assert_eq!(lost.dropped, 2);
        let none = saved_since(saved, Some(99));
        assert!(none.history.is_empty());
        assert_eq!(none.cursor, 5);
    }
}
