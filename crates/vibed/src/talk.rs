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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibe_core::model::{AgentState, validate_ref};
use vibe_core::rpc::{ErrorCode, ErrorObject};
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

/// After a wake that started Claude again, how long its screen must stay
/// still before it's taken to be ready for input, unless it turns on
/// bracketed paste first; and the longest that's waited for.
const SETTLE: Duration = Duration::from_secs(1);
const READY_WITHIN: Duration = Duration::from_secs(30);

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
fn screen_now(socket: &Path) -> std::io::Result<Output> {
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
            keys.iter()
                .enumerate()
                .map(|(i, k)| (k.clone(), if i < last { KEY_GAP } else { Duration::ZERO }))
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
    fn typing(&self, agent_ref: &str) -> Arc<tokio::sync::Mutex<()>> {
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
        let mut record = self.named(host, agent_ref)?;
        let host = record.host.clone();
        if record.starting || self.is_busy(agent_ref) {
            return Err(error(
                ErrorCode::Conflict,
                format!("{agent_ref} is being started, resumed or deleted; try again in a moment"),
            ));
        }
        if let Input::Keys(keys) = &input
            && keys.is_empty()
        {
            return Err(invalid("give keys to press"));
        }
        let typing = self.typing(agent_ref);
        let _one_at_a_time = typing.lock().await;
        let debounce = self.idle_debounce();
        let status = self.agent_status(&mut record, debounce).await;
        let state = self.observed_state(&record, &status).await;
        if let Input::Text { force: false, .. } = &input
            && state == AgentState::Question
        {
            return Err(error(
                ErrorCode::Conflict,
                format!(
                    "{agent_ref} is waiting on a question, so nothing was pasted. Read it with \
                     vibe_read_output, then answer with vibe_send keys: the option's number, like \
                     [\"2\"], or [\"Up\"] and [\"Down\"] to move the ❯ marker, then [\"Enter\"]. Or \
                     pass force: true to paste anyway"
                ),
            ));
        }
        let socket = self.layout.session_socket(agent_ref);
        let woke = matches!(
            state,
            AgentState::Asleep | AgentState::Paused | AgentState::Exited
        );
        if woke {
            let ran = self.session_runs(agent_ref).await;
            self.resume_agent(Some(&host), agent_ref).await?;
            // A new Claude must be up before it's typed to; a frozen one
            // thawed is where it was.
            if !ran {
                wait_ready(&socket).await;
            }
            crate::log!("vibed: {agent_ref} was {state}, and a send woke it");
        }
        // Where its transcript is now: the answer to this send is what
        // comes after.
        let mark = self.mark_send(Some(&host), agent_ref).await?;
        lock(&self.sends).insert(agent_ref.to_owned(), mark);
        let s = socket.clone();
        let screen = tokio::task::spawn_blocking(move || screen_now(&s))
            .await
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?
            .map_err(|e| {
                error(
                    ErrorCode::Conflict,
                    format!("{agent_ref} isn't running, so nothing was sent: {e}"),
                )
            })?;
        let chunks = chunks_of(&input, screen.bracketed_paste);
        tokio::task::spawn_blocking(move || type_into(&socket, &chunks))
            .await
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?
            .map_err(|e| {
                error(
                    ErrorCode::CommandFailed,
                    format!("{agent_ref} didn't take the send: {e}"),
                )
            })?;
        let state = match self.store().agent(&host, agent_ref) {
            Ok(r) => r.state,
            Err(_) => state,
        };
        Ok(Sent { woke, state })
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
        let state = if record.starting || self.is_busy(agent_ref) {
            record.state
        } else {
            self.observed_state(&record, &status).await
        };
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

/// Waits for a Claude just started to be ready for input: it has turned
/// on bracketed paste, or its screen has drawn something and stayed still
/// for [`SETTLE`]. Gives up after [`READY_WITHIN`], and at once if its
/// session ends.
async fn wait_ready(socket: &Path) {
    let begun = Instant::now();
    let mut last: Option<(String, Instant)> = None;
    while begun.elapsed() < READY_WITHIN {
        let s = socket.to_owned();
        let Ok(Ok(out)) = tokio::task::spawn_blocking(move || screen_now(&s)).await else {
            return;
        };
        if out.bracketed_paste {
            return;
        }
        match &last {
            Some((screen, since)) if *screen == out.screen => {
                if !screen.trim().is_empty() && since.elapsed() >= SETTLE {
                    return;
                }
            }
            _ => last = Some((out.screen, Instant::now())),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let keys = Input::Keys(vec![b"\x1b[B".to_vec(), b"\r".to_vec()]);
        assert_eq!(
            chunks_of(&keys, true),
            [
                (b"\x1b[B".to_vec(), KEY_GAP),
                (b"\r".to_vec(), Duration::ZERO)
            ]
        );
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
