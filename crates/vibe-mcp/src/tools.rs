//! The eight `vibe_*` tools (PRD: MCP interface), as an `rmcp` server
//! handler on a [`Daemon`].
//!
//! Each tool is a thin wrapper over the daemon's core handlers. What a
//! wrapper adds is what a model needs and a script doesn't: refs that say
//! their host, a short text next to the structured result, and errors that
//! say what to call next, as VibeOS's do.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use vibe_core::model::{AgentState, LOCAL_HOST};
use vibe_core::rpc::{Agent, AgentDeleteParams, AgentStartParams, ErrorCode, MemoryUse};

use crate::{
    AgentView, Caller, Check, Daemon, Error, Input, Message, Role, Target, display_ref, keys,
    split_ref,
};

/// The most text one result carries, about 8k tokens: Claude Code warns
/// above 10k tokens of tool output (PRD: MCP interface, Rules carried over).
pub const MAX_RESULT_CHARS: usize = 32_000;

/// The longest `vibe_wait`, and its default.
pub const MAX_WAIT_SECS: u64 = 240;
const DEFAULT_WAIT_SECS: u64 = 120;

/// How often a wait reports progress when nothing has changed, so a
/// client's idle timeout never fires on a long turn.
const PROGRESS_EVERY: Duration = Duration::from_secs(20);

/// How often a wait looks at its agents.
const WAIT_POLL: Duration = Duration::from_millis(250);

const DEFAULT_WAIT_LINES: usize = 30;
const DEFAULT_READ_LINES: usize = 50;
const DEFAULT_MESSAGES: usize = 10;
const MAX_LINES: u64 = 2000;
const MAX_MESSAGES: u64 = 500;
const MAX_WAIT_AGENTS: usize = 32;

/// What a read's text may hold, leaving room for the line about it within
/// [`MAX_RESULT_CHARS`].
const BODY_CHARS: usize = MAX_RESULT_CHARS - 1000;

/// What a wait waits for, as VibeOS's `until` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Until {
    /// Not at work: with `turn`, the answer to the last send.
    Settled,
    Idle,
    Question,
    /// Any change of state or activity.
    Change,
}

const INSTRUCTIONS: &str = "VibeOS Native runs coding agents (Claude Code), each in its own git worktree and its own compartment, on this host and the hosts connected to it.

To hand an agent a task: vibe_list_projects to find a project and its profiles, vibe_start_agent with a prompt, then vibe_wait until it has answered, then vibe_read_messages for its answer (or vibe_read_output for its screen). Answer its questions with vibe_send. When it's done, vibe_delete_agent; its branch is kept.

An agent is named by its ref, like quiet-amber-otter, or vps:quiet-amber-otter on another host. Its state is busy, idle, question (stopped on a menu, a yes/no or a permission prompt), asleep, paused (a limit hit) or exited. An agent on a question needs an answer: read it with vibe_read_output, then vibe_send keys, like [\"2\"] or [\"y\"] for a numbered menu or a yes/no, or [\"Down\", \"Enter\"] to move the ❯ marker. A send wakes an agent that is asleep or exited; reads never do.";

/// Cuts text to [`MAX_RESULT_CHARS`] from the start, keeping the end, with
/// a note saying how much was cut.
pub fn clip(text: &str) -> String {
    clip_to(text, MAX_RESULT_CHARS)
}

fn clip_to(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let cut = count - max;
    let start = text.char_indices().nth(cut).map_or(text.len(), |(i, _)| i);
    format!(
        "[{cut} earlier characters cut; ask for fewer lines, or read on from a cursor with after]\n{}",
        &text[start..]
    )
}

/// The MCP server for one request: `rmcp` makes one per request, as it
/// serves statelessly.
#[derive(Debug)]
pub struct Server<D> {
    daemon: Arc<D>,
}

impl<D> Clone for Server<D> {
    fn clone(&self) -> Self {
        Server {
            daemon: Arc::clone(&self.daemon),
        }
    }
}

/// A tool's outcome: a result, or the text of a tool error for the model.
type Outcome = Result<CallToolResult, String>;

fn ok(text: &str, structured: Value) -> Outcome {
    let mut r = CallToolResult::success(vec![ContentBlock::text(clip(text))]);
    r.structured_content = Some(structured);
    Ok(r)
}

fn fail(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(clip(&text))])
}

/// A core error, as a model reads it, with what to do next where that
/// helps.
fn explain(e: &Error, what: Option<&str>) -> String {
    let not_found = e.code == ErrorCode::NotFound.code();
    match what {
        Some(name) if not_found && e.message.contains(name) => format!(
            "{}. vibe_list_agents lists the agents and their refs.",
            e.message.trim_end_matches('.')
        ),
        _ => e.message.clone(),
    }
}

/// A size, as `4G` or `340M`.
fn human(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes >= GIB {
        let g = bytes as f64 / GIB as f64;
        if bytes.is_multiple_of(GIB) {
            format!("{}G", bytes / GIB)
        } else {
            format!("{g:.1}G")
        }
    } else {
        format!("{}M", bytes.div_ceil(MIB))
    }
}

// Arguments.

fn object(schema: Value) -> Arc<JsonObject> {
    match schema {
        Value::Object(m) => Arc::new(m),
        _ => unreachable!("tool schemas are objects"),
    }
}

/// The argument `key`, or its VibeOS name, so an orchestrator written for
/// VibeOS needs only its tool names changed.
fn arg<'a>(args: &'a JsonObject, key: &str, alias: Option<&str>) -> Option<&'a Value> {
    args.get(key)
        .or_else(|| alias.and_then(|a| args.get(a)))
        .filter(|v| !v.is_null())
}

fn string(args: &JsonObject, key: &str) -> Result<Option<String>, String> {
    match args.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn required(args: &JsonObject, key: &str) -> Result<String, String> {
    string(args, key)?
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{key} is required"))
}

fn boolean(args: &JsonObject, key: &str) -> Result<Option<bool>, String> {
    match args.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("{key} must be true or false")),
    }
}

fn integer(args: &JsonObject, key: &str, min: u64, max: u64) -> Result<Option<u64>, String> {
    match args.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => match v.as_u64().or_else(|| {
            v.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                .map(|f| f as u64)
        }) {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(format!("{key} must be a whole number from {min} to {max}")),
        },
    }
}

/// The agent a call names, by `agent` (or VibeOS's `window`).
fn agent_arg(args: &JsonObject) -> Result<String, String> {
    match arg(args, "agent", Some("window")) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        Some(_) => Err("agent must be a ref, like quiet-amber-otter".into()),
        None => Err("agent is required: a ref, like quiet-amber-otter".into()),
    }
}

// The tools' definitions.

fn tool(name: &'static str, title: &str, description: &str, schema: Value) -> Tool {
    let mut t = Tool::default();
    t.name = name.into();
    t.title = Some(title.to_owned());
    t.description = Some(description.to_owned().into());
    t.input_schema = object(schema);
    t
}

fn hints(read_only: bool, destructive: bool, idempotent: bool) -> ToolAnnotations {
    let mut a = ToolAnnotations::new();
    a.read_only_hint = Some(read_only);
    if !read_only {
        a.destructive_hint = Some(destructive);
        a.idempotent_hint = Some(idempotent);
    }
    a.open_world_hint = Some(false);
    a
}

const STATES: [&str; 6] = ["busy", "idle", "question", "asleep", "paused", "exited"];

/// Every tool, in the order `tools/list` gives them.
pub(crate) fn tools() -> Vec<Tool> {
    let agent = json!({"type": "string", "description": "The agent's ref, like quiet-amber-otter, or vps:quiet-amber-otter on another host"});
    let mut list = vec![
        tool(
            "vibe_list_projects",
            "List projects",
            "Lists the projects (git repositories) on every connected host, each with its host and its profiles: the presets an agent starts from, with their environment (linux or macos), model, reasoning, mode and memory.",
            json!({"type": "object", "properties": {"host": {"type": "string", "description": "Only this host's"}}}),
        ),
        tool(
            "vibe_start_agent",
            "Start an agent",
            "Starts an agent: cuts a new git worktree and branch of a project, creates its compartment, and starts Claude Code on it with the prompt, submitted. Returns its ref. Then call vibe_wait, and vibe_read_messages for its answer.",
            json!({"type": "object", "properties": {
                "project": {"type": "string", "description": "The project, by name"},
                "profile": {"type": "string", "description": "The project's profile to start it from, by name, like software-eng-linux"},
                "prompt": {"type": "string", "description": "What to work on"},
                "host": {"type": "string", "description": "The host to start it on: needed only when the project is on more than one"},
                "from": {"type": "string", "description": "The branch or commit to start from. Defaults to origin's default branch"},
                "name": {"type": "string", "description": "Its ref, which also names its worktree and branch: lowercase letters, digits and dashes. Three random words if absent"}
            }, "required": ["project", "profile", "prompt"]}),
        ),
        tool(
            "vibe_list_agents",
            "List agents",
            "Lists the agents on every connected host, with their ref, host, state (busy, idle, question, asleep, paused or exited), activity, profile, worktree, branch, memory (what it uses now, against its profile's limit, the most it may use) and the command that attaches a terminal. Also each host's memory: what's in use of its total, its swap, and why it's short of memory, while it is. Memory never stops an agent from starting: a host swaps if its agents use more than it has.",
            json!({"type": "object", "properties": {
                "host": {"type": "string", "description": "Only this host's"},
                "project": {"type": "string", "description": "Only this project's"},
                "state": {"type": "string", "enum": STATES, "description": "Only agents in this state"}
            }}),
        ),
        tool(
            "vibe_send",
            "Send to an agent",
            "Sends text to an agent and presses Enter, or presses keys. Text is refused while the agent is on a question: read the question with vibe_read_output, then answer with keys: the option's number, like [\"2\"], [\"y\"] for yes/no, or [\"Down\", \"Enter\"] to pick the next option in a menu without numbers. [\"Escape\"] stops a busy agent. An agent that is asleep, paused or exited is woken or resumed first. Keys are one character or Enter, Escape, Tab, BackTab, Backspace, Delete, Space, Up, Down, Left, Right, Home, End, PageUp, PageDown, Ctrl+<letter>. Then call vibe_wait.",
            json!({"type": "object", "properties": {
                "agent": agent.clone(),
                "text": {"type": "string", "description": "Text to paste"},
                "submit": {"type": "boolean", "description": "Press Enter after the text. Default true"},
                "keys": {"type": "array", "items": {"type": "string"}, "description": "Instead of text: keys to press, like [\"2\"] or [\"Enter\"]"},
                "force": {"type": "boolean", "description": "Paste the text even when the agent is on a question"}
            }, "required": ["agent"]}),
        ),
        tool(
            "vibe_wait",
            "Wait for an agent",
            &format!(
                "Blocks until an agent has answered the last send to it (or its start's prompt), not just until it's quiet, or until the timeout. With several agents, returns when the first one does. Returns its state and its last lines. Sends progress every 20 seconds and on every change. Does not wake an agent that is asleep. Up to {MAX_WAIT_SECS} seconds, default {DEFAULT_WAIT_SECS}."
            ),
            json!({"type": "object", "properties": {
                "agents": {"description": "One agent's ref, or several", "anyOf": [
                    {"type": "string"},
                    {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_WAIT_AGENTS}
                ]},
                "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_SECS, "description": format!("Seconds, up to {MAX_WAIT_SECS}. Default {DEFAULT_WAIT_SECS}")},
                "background": {"type": "boolean", "description": "Also wait for commands the agent left running in the background, and subagents, to finish. Off by default: a server left running never finishes"},
                "until": {"type": "string", "enum": ["settled", "idle", "question", "change"], "description": "settled (default): it has stopped; idle or question: that state; change: any change of state or activity"},
                "turn": {"type": "boolean", "description": "Wait for the answer to the last send. Default true; false waits only for the state until asks for"},
                "lines": {"type": "integer", "minimum": 0, "maximum": MAX_LINES, "description": format!("Lines of its screen to return. Default {DEFAULT_WAIT_LINES}")}
            }, "required": ["agents"]}),
        ),
        tool(
            "vibe_read_messages",
            "Read an agent's conversation",
            "Reads the last messages of an agent's conversation, from its transcript: what was asked, what it answered, and its tool calls in short form. The way to read an agent's answer. Works while it's asleep, and doesn't wake it.",
            json!({"type": "object", "properties": {
                "agent": agent.clone(),
                "last": {"type": "integer", "minimum": 1, "maximum": MAX_MESSAGES, "description": format!("How many messages, from the end. Default {DEFAULT_MESSAGES}")}
            }, "required": ["agent"]}),
        ),
        tool(
            "vibe_read_output",
            "Read an agent's screen",
            "Reads what an agent's terminal shows, as plain text. With after (a cursor from an earlier read), returns only the lines that scrolled off since, plus the screen as it is now. Doesn't wake an agent: one that is asleep or exited gives the last screen saved.",
            json!({"type": "object", "properties": {
                "agent": agent.clone(),
                "lines": {"type": "integer", "minimum": 1, "maximum": MAX_LINES, "description": format!("How many lines, from the bottom. Default {DEFAULT_READ_LINES}; with after, every new line unless given")},
                "after": {"type": "integer", "minimum": 0, "description": "A cursor from an earlier read: only what came since"}
            }, "required": ["agent"]}),
        ),
        tool(
            "vibe_delete_agent",
            "Delete an agent",
            "Ends an agent and removes everything VibeOS made for it: its worktree, temp directory, transcripts, scrollback, compartment and snapshot. Its branch is kept. Refused while it has uncommitted changes or unpushed commits, unless force.",
            json!({"type": "object", "properties": {
                "agent": agent,
                "force": {"type": "boolean", "description": "Delete even with uncommitted changes or unpushed commits"}
            }, "required": ["agent"]}),
        ),
    ];
    let annotations = [
        hints(true, false, true),
        hints(false, false, false),
        hints(true, false, true),
        hints(false, false, false),
        hints(true, false, true),
        hints(true, false, true),
        hints(true, false, true),
        hints(false, true, true),
    ];
    for (t, a) in list.iter_mut().zip(annotations) {
        t.annotations = Some(a);
    }
    list
}

// Results.

fn agent_json(v: &AgentView) -> Value {
    let mut j = agent_value(&v.agent);
    if let Value::Object(m) = &mut j {
        m.insert("attach_remote".into(), json!(v.attach_remote));
        m.insert("available".into(), json!(v.available));
    }
    j
}

/// An agent as every tool gives one: its ref as MCP writes it, and its
/// memory in MCP's own names, `memory` for what it uses now and
/// `memory_limit` for its profile's.
fn agent_value(a: &Agent) -> Value {
    let mut j = serde_json::to_value(a).expect("agents serialize");
    if let Value::Object(m) = &mut j {
        m.insert("ref".into(), display_ref(&a.host, &a.agent).into());
        m.remove("memory_used");
        m.insert("memory".into(), json!(a.memory_used));
        m.insert("memory_limit".into(), json!(a.memory));
    }
    j
}

/// One agent as a line: ref, state and activity, profile, project, branch,
/// memory, and how to attach.
fn agent_line(v: &AgentView) -> String {
    let a = &v.agent;
    let mut parts = vec![
        display_ref(&a.host, &a.agent),
        format!("{} ({})", a.state, a.status.activity),
        a.profile.clone(),
        format!("{} · branch {}", a.project, a.branch),
    ];
    if let Some(m) = a.memory_used {
        parts.push(format!("{} of {}", human(m), human(a.memory)));
    }
    if let Some(until) = limited_until(a) {
        parts.push(until);
    }
    if let Some(line) = &a.status.line {
        parts.push(line.clone());
    }
    parts.push(format!("attach: {}", v.attach_remote));
    if !v.available {
        parts.push(format!(
            "unavailable: {} can't be reached now, so this is how it was last seen",
            a.host
        ));
    }
    parts.join("  ")
}

/// A host's memory as a line: in use of its total, its swap, and why it's
/// short, while it is.
fn memory_line(host: &str, m: &MemoryUse) -> String {
    let mut line = format!(
        "{host}: {} of {} memory in use",
        human(m.used),
        human(m.total)
    );
    if m.swap_total > 0 {
        line.push_str(&format!(
            ", {} of {} swap",
            human(m.swap_used),
            human(m.swap_total)
        ));
    }
    if let Some(why) = &m.pressure {
        line.push_str(&format!(
            ". Short of memory: {why}. Agents keep running, and it swaps"
        ));
    }
    line
}

/// A host's memory in MCP's names, as an agent's are: `memory` for what's
/// in use, beside its total and swap.
fn memory_json(host: &str, m: &MemoryUse) -> Value {
    json!({
        "host": host,
        "memory": m.used,
        "memory_total": m.total,
        "swap": m.swap_used,
        "swap_total": m.swap_total,
        "pressure": m.pressure,
    })
}

fn limited_until(a: &vibe_core::rpc::Agent) -> Option<String> {
    a.status.stop_reason?;
    Some(match &a.status.resets_at {
        Some(t) => format!("limited until {t}"),
        None => "limited until its usage limit resets".into(),
    })
}

fn message_line(m: &Message) -> String {
    match m.role {
        Role::Tool => format!("→ {}", m.text),
        Role::User => format!("user: {}", m.text),
        Role::Assistant => format!("assistant: {}", m.text),
    }
}

/// The last `n` lines of `history` and then `screen`.
fn last_lines(history: &[String], screen: &str, n: usize) -> String {
    let mut lines: Vec<&str> = history.iter().map(String::as_str).collect();
    lines.extend(screen.lines());
    let skip = lines.len().saturating_sub(n);
    lines[skip..].join("\n")
}

impl<D: Daemon> Server<D> {
    pub fn new(daemon: Arc<D>) -> Server<D> {
        Server { daemon }
    }

    /// The host and ref `name` names. A bare ref is this host's, unless
    /// another host has it and this one doesn't.
    async fn resolve(&self, c: &Caller, name: &str) -> Result<Target, String> {
        let (host, agent) = split_ref(name);
        if agent.is_empty() {
            return Err("agent must be a ref, like quiet-amber-otter".into());
        }
        let target = |host: &str| Target {
            host: host.to_owned(),
            agent: agent.to_owned(),
        };
        if let Some(h) = host {
            return Ok(target(h));
        }
        let hosts = self.daemon.hosts().await.map_err(|e| e.message)?;
        if hosts.len() <= 1 {
            return Ok(target(hosts.first().map_or(LOCAL_HOST, String::as_str)));
        }
        let mut found = Vec::new();
        for h in &hosts {
            if let Ok(list) = self.daemon.agents(c, h).await
                && list.iter().any(|v| v.agent.agent == agent)
            {
                found.push(h.clone());
            }
        }
        match found.as_slice() {
            [] => Ok(target(LOCAL_HOST)),
            [one] => Ok(target(one)),
            several => Err(format!(
                "{agent} is on {}: name it with its host, as {}",
                several.join(" and "),
                several
                    .iter()
                    .map(|h| format!("{h}:{agent}"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            )),
        }
    }

    async fn list_projects(&self, c: &Caller, args: &JsonObject) -> Outcome {
        let only = string(args, "host")?;
        let hosts = match &only {
            Some(h) => vec![h.clone()],
            None => self.daemon.hosts().await.map_err(|e| e.message)?,
        };
        let mut projects = Vec::new();
        let mut unreachable = Vec::new();
        for h in &hosts {
            match self.daemon.projects(c, h).await {
                Ok(p) => projects.extend(p),
                Err(e) if only.is_some() => return Err(e.message),
                Err(e) => unreachable.push(format!("{h} is unavailable: {}", e.message)),
            }
        }
        let mut text: Vec<String> = projects
            .iter()
            .map(|p| {
                let mut lines = vec![format!("{} (on {}) {}", p.name, p.host, p.path)];
                lines.extend(p.profiles.iter().map(|r| {
                    format!(
                        "  {}: {}, {}, {} reasoning, {} mode, {}",
                        r.name,
                        r.environment,
                        r.model,
                        r.reasoning,
                        r.mode,
                        human(r.memory)
                    )
                }));
                lines.join("\n")
            })
            .collect();
        if text.is_empty() {
            text.push("no projects".into());
        }
        text.extend(unreachable);
        ok(&text.join("\n"), json!({ "projects": projects }))
    }

    async fn start_agent(&self, c: &Caller, args: &JsonObject) -> Outcome {
        let project = required(args, "project")?;
        let profile = required(args, "profile")?;
        let prompt = required(args, "prompt")?;
        let from = string(args, "from")?.filter(|s| !s.is_empty());
        let name = string(args, "name")?.filter(|s| !s.is_empty());
        let host = match string(args, "host")?.filter(|s| !s.is_empty()) {
            Some(h) => h,
            None => self.project_host(c, &project).await?,
        };
        let a = self
            .daemon
            .start_agent(
                c,
                AgentStartParams {
                    host: Some(host.clone()),
                    project,
                    profile,
                    prompt,
                    from,
                    name,
                },
            )
            .await
            .map_err(|e| e.message)?;
        let r = display_ref(&a.host, &a.agent);
        let text = format!(
            "started {r} as {} in {} on {}, on branch {}. The prompt is submitted; call vibe_wait to know when it has answered",
            a.profile, a.project, a.host, a.branch
        );
        ok(&text, agent_value(&a))
    }

    /// The one host `project` is on.
    async fn project_host(&self, c: &Caller, project: &str) -> Result<String, String> {
        let hosts = self.daemon.hosts().await.map_err(|e| e.message)?;
        let mut on = Vec::new();
        for h in &hosts {
            if let Ok(list) = self.daemon.projects(c, h).await
                && list.iter().any(|p| p.name == project)
            {
                on.push(h.clone());
            }
        }
        match on.as_slice() {
            [one] => Ok(one.clone()),
            [] if hosts.len() <= 1 => Ok(hosts.first().cloned().unwrap_or(LOCAL_HOST.into())),
            [] => Err(format!(
                "no project called {project} on {}. vibe_list_projects lists them",
                hosts.join(", ")
            )),
            several => Err(format!(
                "{project} is a project on {}: pass host to say which",
                several.join(" and ")
            )),
        }
    }

    async fn list_agents(&self, c: &Caller, args: &JsonObject) -> Outcome {
        let only = string(args, "host")?;
        let project = string(args, "project")?;
        let state = match string(args, "state")? {
            Some(s) if !STATES.contains(&s.as_str()) => {
                return Err(format!("state must be one of {}", STATES.join(", ")));
            }
            s => s,
        };
        let hosts = match &only {
            Some(h) => vec![h.clone()],
            None => self.daemon.hosts().await.map_err(|e| e.message)?,
        };
        let mut agents = Vec::new();
        let mut unreachable = Vec::new();
        for h in &hosts {
            match self.daemon.agents(c, h).await {
                Ok(list) => agents.extend(list),
                Err(e) if only.is_some() => return Err(e.message),
                Err(e) => unreachable.push(format!("{h} is unavailable: {}", e.message)),
            }
        }
        agents.retain(|v| {
            project.as_ref().is_none_or(|p| &v.agent.project == p)
                && state.as_ref().is_none_or(|s| v.agent.state.as_str() == s)
        });
        let mut text: Vec<String> = agents.iter().map(agent_line).collect();
        if text.is_empty() {
            text.push("no agents".into());
        }
        text.extend(unreachable);
        let list: Vec<Value> = agents.iter().map(agent_json).collect();
        let memory: Vec<(String, MemoryUse)> = self
            .daemon
            .memory(c)
            .await
            .into_iter()
            .filter(|(h, _)| only.as_ref().is_none_or(|o| o == h))
            .collect();
        text.extend(memory.iter().map(|(h, m)| memory_line(h, m)));
        let hosts: Vec<Value> = memory.iter().map(|(h, m)| memory_json(h, m)).collect();
        ok(&text.join("\n"), json!({ "agents": list, "hosts": hosts }))
    }

    async fn send(&self, c: &Caller, args: &JsonObject) -> Outcome {
        let name = agent_arg(args)?;
        let text = string(args, "text")?;
        let submit = boolean(args, "submit")?;
        let force = boolean(args, "force")?.unwrap_or(false);
        let keys = match args.get("keys").filter(|v| !v.is_null()) {
            None => None,
            Some(Value::Array(items)) if !items.is_empty() => Some(
                items
                    .iter()
                    .map(|k| k.as_str().ok_or("keys must be strings"))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Some(_) => return Err("keys must be a list of keys, like [\"2\"]".into()),
        };
        let input = match (text, keys) {
            (Some(_), Some(_)) => return Err("give text or keys, not both".into()),
            (None, None) => return Err("give text to send, or keys to press".into()),
            (Some(text), None) => Input::Text {
                text,
                submit: submit.unwrap_or(true),
                force,
            },
            (None, Some(keys)) => {
                if submit.is_some() {
                    return Err("submit goes with text; press [\"Enter\"] with keys".into());
                }
                for k in &keys {
                    keys::bytes(k)?;
                }
                Input::Keys(keys.iter().map(|k| (*k).to_owned()).collect())
            }
        };
        let t = self.resolve(c, &name).await?;
        let r = display_ref(&t.host, &t.agent);
        let what = match &input {
            Input::Keys(_) => {
                let names: Vec<&str> = args["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!("pressed {} in {r}", names.join(" "))
            }
            Input::Text { submit: true, .. } => format!("sent to {r} and submitted"),
            Input::Text { submit: false, .. } => format!("pasted into {r}, not submitted"),
        };
        let sent = self
            .daemon
            .send(c, &t, input)
            .await
            .map_err(|e| explain(&e, Some(&t.agent)))?;
        ok(
            &format!(
                "{what}{}. Call vibe_wait to know when it has answered",
                if sent.woke { " (woke it first)" } else { "" }
            ),
            json!({"ref": r, "host": t.host, "delivered": true, "woke": sent.woke, "started": sent.woke, "state": sent.state}),
        )
    }

    async fn read_messages(&self, c: &Caller, args: &JsonObject) -> Outcome {
        let name = agent_arg(args)?;
        let last = integer(args, "last", 1, MAX_MESSAGES)?.map_or(DEFAULT_MESSAGES, |n| n as usize);
        let t = self.resolve(c, &name).await?;
        let messages = self
            .daemon
            .read_messages(c, &t, last)
            .await
            .map_err(|e| explain(&e, Some(&t.agent)))?;
        let text = messages
            .iter()
            .map(message_line)
            .collect::<Vec<_>>()
            .join("\n\n");
        let text = if text.is_empty() {
            "no messages yet".to_owned()
        } else {
            text
        };
        // The structured copy is cut too, as a client may show it instead.
        let mut budget = MAX_RESULT_CHARS;
        let mut kept: Vec<Value> = Vec::new();
        for m in messages.iter().rev() {
            let cost = m.text.chars().count();
            if cost > budget {
                if kept.is_empty() {
                    kept.push(json!({"role": m.role, "text": clip(&m.text), "at": m.at}));
                }
                break;
            }
            budget -= cost;
            kept.push(json!({"role": m.role, "text": m.text, "at": m.at}));
        }
        kept.reverse();
        ok(
            &text,
            json!({"ref": display_ref(&t.host, &t.agent), "messages": kept}),
        )
    }

    async fn read_output(&self, c: &Caller, args: &JsonObject) -> Outcome {
        let name = agent_arg(args)?;
        let lines = integer(args, "lines", 1, MAX_LINES)?;
        let after = integer(args, "after", 0, u64::MAX)?;
        let t = self.resolve(c, &name).await?;
        let r = display_ref(&t.host, &t.agent);
        let out = self
            .daemon
            .read_output(c, &t, after)
            .await
            .map_err(|e| explain(&e, Some(&t.agent)))?;
        let state = self
            .daemon
            .check(c, &t, false)
            .await
            .map(|c| c.state.to_string())
            .unwrap_or_else(|_| "unknown".into());
        let mut cursor = out.cursor;
        let mut more = out.more;
        let body = match after {
            None => {
                let n = lines.map_or(DEFAULT_READ_LINES, |n| n as usize);
                clip_to(&last_lines(&out.history, &out.screen, n), BODY_CHARS)
            }
            Some(after) => {
                let mut history = out.history.clone();
                if let Some(n) = lines {
                    let skip = history.len().saturating_sub(n as usize);
                    history.drain(..skip);
                }
                // Read on from a cursor, the oldest lines come first, and
                // what doesn't fit is left for the next read: the cursor
                // says where to go on from.
                let screen = clip_to(&out.screen, BODY_CHARS / 4);
                let mut budget = BODY_CHARS.saturating_sub(screen.chars().count());
                let mut kept = 0;
                for line in &history {
                    let cost = line.chars().count() + 1;
                    if cost > budget {
                        break;
                    }
                    budget -= cost;
                    kept += 1;
                }
                if kept < history.len() {
                    cursor = cursor.saturating_sub((history.len() - kept) as u64);
                    more = true;
                    history.truncate(kept);
                }
                let mut parts = Vec::new();
                if !history.is_empty() {
                    parts.push(format!("new since {after}:\n{}", history.join("\n")));
                }
                parts.push(format!("screen:\n{screen}"));
                parts.join("\n\n")
            }
        };
        let mut head = format!("{r} is {state}. cursor {cursor}");
        if out.dropped > 0 {
            head.push_str(&format!(" ({} lines were no longer held)", out.dropped));
        }
        if more {
            head.push_str("; more history waits: read again with after set to the cursor");
        }
        if !out.live {
            head.push_str(". This is the last screen saved: it isn't running");
        }
        ok(
            &format!("{head}\n\n{body}"),
            json!({
                "ref": r, "state": state, "text": body, "cursor": cursor,
                "dropped": out.dropped, "more": more, "live": out.live,
            }),
        )
    }

    async fn delete_agent(&self, c: &Caller, args: &JsonObject) -> Outcome {
        let name = agent_arg(args)?;
        let force = boolean(args, "force")?;
        let t = self.resolve(c, &name).await?;
        let a = self
            .daemon
            .delete_agent(
                c,
                AgentDeleteParams {
                    host: Some(t.host.clone()),
                    agent: t.agent.clone(),
                    force,
                },
            )
            .await
            .map_err(|e| explain(&e, Some(&t.agent)))?;
        let r = display_ref(&a.host, &a.agent);
        ok(
            &format!("deleted {r}; its branch {} is kept", a.branch),
            json!({"deleted": r, "host": a.host, "branch": a.branch}),
        )
    }

    async fn wait(
        &self,
        c: &Caller,
        args: &JsonObject,
        ctx: &RequestContext<RoleServer>,
    ) -> Outcome {
        let names: Vec<String> = match arg(args, "agents", Some("windows"))
            .or_else(|| arg(args, "agent", Some("window")))
        {
            Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
            Some(Value::Array(a)) if !a.is_empty() && a.len() <= MAX_WAIT_AGENTS => a
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("agents must be refs".to_owned())
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(format!(
                    "agents is required: one ref, or a list of up to {MAX_WAIT_AGENTS}"
                ));
            }
        };
        let timeout = integer(args, "timeout", 1, MAX_WAIT_SECS)?.unwrap_or(DEFAULT_WAIT_SECS);
        let background = boolean(args, "background")?.unwrap_or(false);
        let turn = boolean(args, "turn")?.unwrap_or(true);
        let until = match string(args, "until")?.as_deref() {
            None | Some("settled") => Until::Settled,
            Some("idle") => Until::Idle,
            Some("question") => Until::Question,
            Some("change") => Until::Change,
            Some(_) => return Err("until must be settled, idle, question or change".into()),
        };
        let lines =
            integer(args, "lines", 0, MAX_LINES)?.map_or(DEFAULT_WAIT_LINES, |n| n as usize);
        let mut targets = Vec::new();
        for n in &names {
            let t = self.resolve(c, n).await?;
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
        let token = ctx.meta.get_progress_token();
        let begun = Instant::now();
        let deadline = begun + Duration::from_secs(timeout);
        let mut progress = 0u32;
        let mut last_report = begun;
        let mut seen: HashMap<Target, (AgentState, String)> = HashMap::new();
        let mut first: HashMap<Target, (AgentState, String)> = HashMap::new();
        let describe = |checks: &[(Target, Check)]| {
            checks
                .iter()
                .map(|(t, c)| {
                    format!(
                        "{}: {} ({}){}",
                        display_ref(&t.host, &t.agent),
                        c.state,
                        c.activity,
                        c.line
                            .as_ref()
                            .map(|l| format!(", {l}"))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        // What each agent last said, for while its host's link is down: a
        // blip of the network doesn't end the wait, as nothing stopped.
        let mut last: HashMap<Target, Check> = HashMap::new();
        let (target, check, reached, checks) = loop {
            let mut checks = Vec::new();
            for t in &targets {
                match self.daemon.check(c, t, background).await {
                    Ok(c) => {
                        last.insert(t.clone(), c.clone());
                        checks.push((t.clone(), c));
                    }
                    Err(e)
                        if e.code == vibe_core::rpc::ErrorCode::HostUnavailable.code()
                            && Instant::now() < deadline =>
                    {
                        if let Some(c) = last.get(t) {
                            checks.push((t.clone(), c.clone()));
                        }
                    }
                    Err(e) => return Err(explain(&e, Some(&t.agent))),
                }
            }
            if checks.is_empty() {
                // Every host asked is out of reach for now; ask again.
                let pause = WAIT_POLL.min(deadline.saturating_duration_since(Instant::now()));
                tokio::select! {
                    _ = ctx.ct.cancelled() => return Err("the wait was cancelled".into()),
                    _ = tokio::time::sleep(pause) => {}
                }
                continue;
            }
            let mut changed = false;
            for (t, c) in &checks {
                let now = (c.state, c.activity.to_string());
                first.entry(t.clone()).or_insert_with(|| now.clone());
                if seen.get(t) != Some(&now) {
                    changed = true;
                    seen.insert(t.clone(), now);
                }
            }
            let done = checks.iter().find(|(t, c)| {
                let answered = !turn || c.answered;
                match until {
                    Until::Settled if turn => c.answered,
                    Until::Settled => c.state != AgentState::Busy,
                    Until::Idle => answered && c.state == AgentState::Idle,
                    Until::Question => answered && c.state == AgentState::Question,
                    Until::Change => first.get(t) != seen.get(t),
                }
            });
            if let Some((t, c)) = done {
                break (t.clone(), c.clone(), true, checks);
            }
            // One that can't answer without a send or a resume ends the
            // wait too: it won't move on its own.
            let stuck = checks.iter().find(|(_, c)| {
                matches!(
                    c.state,
                    AgentState::Asleep | AgentState::Exited | AgentState::Paused
                )
            });
            if let Some((t, c)) = stuck {
                break (t.clone(), c.clone(), false, checks);
            }
            if Instant::now() >= deadline {
                let (t, c) = checks[0].clone();
                break (t, c, false, checks);
            }
            if let Some(token) = &token
                && (changed && progress > 0
                    || last_report.elapsed() >= PROGRESS_EVERY
                    || progress == 0)
            {
                progress += 1;
                last_report = Instant::now();
                let mut p = ProgressNotificationParam::new(token.clone(), f64::from(progress));
                p.message = Some(format!(
                    "{} ({}s)",
                    describe(&checks),
                    begun.elapsed().as_secs()
                ));
                let _ = ctx.peer.notify_progress(p).await;
            }
            let pause = WAIT_POLL.min(deadline.saturating_duration_since(Instant::now()));
            tokio::select! {
                _ = ctx.ct.cancelled() => return Err("the wait was cancelled".into()),
                _ = tokio::time::sleep(pause) => {}
            }
        };
        let waited = begun.elapsed().as_secs_f64();
        let r = display_ref(&target.host, &target.agent);
        let head = if reached {
            format!(
                "{r} has answered: it is {} after {}s",
                check.state,
                waited.round()
            )
        } else {
            match check.state {
                AgentState::Asleep => format!("{r} is asleep: vibe_send wakes it"),
                AgentState::Exited => {
                    format!(
                        "{r} has exited: vibe_send resumes it, or read why with vibe_read_output"
                    )
                }
                AgentState::Paused => format!(
                    "{r} is paused: it hit a limit. Raise its profile's memory and send to it to resume it, or delete it"
                ),
                state => format!(
                    "still {state} after {}s; call vibe_wait again to keep waiting",
                    waited.round()
                ),
            }
        };
        let hint = if reached && check.activity == vibe_core::model::Activity::AwaitingApproval {
            "\nIt may be waiting on a permission prompt: read it with vibe_read_output, and answer with vibe_send keys. If it's a command that's still running instead, call vibe_wait again."
        } else if reached && check.state == AgentState::Question {
            "\nIt is asking something: read it with vibe_read_output, and answer with vibe_send keys."
        } else if reached && check.state == AgentState::Exited {
            "\nIt has exited since: vibe_send resumes it."
        } else if reached && check.state == AgentState::Paused {
            "\nIt has been paused by a limit since. Raise its profile's memory and send to it to resume it, or delete it."
        } else if reached && check.activity == vibe_core::model::Activity::BackgroundWork {
            "\nIt still has work running in the background, and will answer again when that finishes. To wait for it, call vibe_wait with background: true."
        } else {
            ""
        };
        let others = if checks.len() > 1 {
            format!("\n{}", describe(&checks).replace("; ", "\n"))
        } else {
            String::new()
        };
        let screen = if lines > 0 {
            match self.daemon.read_output(c, &target, None).await {
                Ok(out) => last_lines(&out.history, &out.screen, lines),
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };
        let all: Vec<Value> = checks
            .iter()
            .map(|(t, c)| json!({"ref": display_ref(&t.host, &t.agent), "state": c.state, "activity": c.activity, "answered": c.answered}))
            .collect();
        ok(
            &format!(
                "{head}{hint}{others}{}",
                if screen.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{screen}")
                }
            ),
            json!({
                "ref": r, "state": check.state, "activity": check.activity,
                "reached": reached, "waited": waited, "waitedMs": (waited * 1000.0).round(),
                "agents": all, "text": clip(&screen),
            }),
        )
    }
}

impl<D: Daemon> Server<D> {
    /// Runs the tool `name`, which is one of [`tools`].
    async fn run(
        &self,
        c: &Caller,
        name: &str,
        args: &JsonObject,
        ctx: &RequestContext<RoleServer>,
    ) -> Outcome {
        match name {
            "vibe_list_projects" => self.list_projects(c, args).await,
            "vibe_start_agent" => self.start_agent(c, args).await,
            "vibe_list_agents" => self.list_agents(c, args).await,
            "vibe_send" => self.send(c, args).await,
            "vibe_wait" => self.wait(c, args, ctx).await,
            "vibe_read_messages" => self.read_messages(c, args).await,
            "vibe_read_output" => self.read_output(c, args).await,
            "vibe_delete_agent" => self.delete_agent(c, args).await,
            other => Err(format!("no tool called {other}")),
        }
    }
}

impl<D: Daemon> ServerHandler for Server<D> {
    fn get_info(&self) -> ServerConfig {
        let mut me = Implementation::new("vibe-os-native", env!("CARGO_PKG_VERSION"));
        me.title = Some("VibeOS Native".into());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(me)
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args: JsonObject = request.arguments.clone().unwrap_or_default();
        // The gate in front put it there: no request reaches a tool without
        // a key.
        let Some(caller) = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.extensions.get::<Caller>())
            .cloned()
        else {
            return Err(ErrorData::invalid_request("an API key is needed", None));
        };
        let name = request.name.as_ref();
        let about = arg(&args, "agent", Some("window"))
            .or_else(|| arg(&args, "agents", Some("windows")))
            .or_else(|| args.get("project"))
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        // A tool's name as it is, and anything else quoted, so nothing a
        // caller sends can start a log line of its own.
        let known = tools().iter().any(|t| t.name == name);
        let shown = if known {
            name.to_owned()
        } else {
            format!("{name:?}")
        };
        self.daemon.log(&format!(
            "mcp: key {:?} called {shown}{about}",
            caller.name()
        ));
        if !known {
            return Err(ErrorData::invalid_params(
                format!("no tool called {name}"),
                None,
            ));
        }
        // On a task of its own, so a handler that panics fails its call
        // instead of leaving the client waiting on a response.
        let (server, tool) = (self.clone(), name.to_owned());
        let outcome =
            tokio::spawn(async move { server.run(&caller, &tool, &args, &context).await })
                .await
                .unwrap_or_else(|e| Err(format!("Internal error: the call failed: {e}")));
        Ok(outcome
            .unwrap_or_else(|e| {
                self.daemon.log(&format!("mcp: {name} failed: {e:?}"));
                fail(e)
            })
            .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_keeps_its_end() {
        assert_eq!(clip_to("short", 10), "short");
        let text = "a".repeat(5) + "end";
        let c = clip_to(&text, 4);
        assert!(c.starts_with("[4 earlier characters cut;"), "{c}");
        assert!(c.ends_with("\naend"), "{c}");
        // Characters, not bytes, so a cut never splits one.
        let c = clip_to("ééééé", 2);
        assert!(c.ends_with("\néé"), "{c}");
        let big = "x".repeat(MAX_RESULT_CHARS + 100);
        let c = clip(&big);
        assert!(c.starts_with("[100 earlier characters cut"));
        assert_eq!(c.chars().filter(|c| *c == 'x').count(), MAX_RESULT_CHARS);
    }

    #[test]
    fn sizes_read_short() {
        assert_eq!(human(4 << 30), "4G");
        assert_eq!(human(340 << 20), "340M");
        assert_eq!(human((3 << 30) / 2), "1.5G");
        assert_eq!(human(1), "1M");
    }

    #[test]
    fn the_last_lines_run_into_the_screen() {
        let h = vec!["1".to_owned(), "2".to_owned()];
        assert_eq!(last_lines(&h, "3\n4", 3), "2\n3\n4");
        assert_eq!(last_lines(&h, "3", 10), "1\n2\n3");
        assert_eq!(last_lines(&h, "3", 0), "");
    }

    #[test]
    fn eight_tools_with_schemas() {
        let t = tools();
        let names: Vec<&str> = t.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "vibe_list_projects",
                "vibe_start_agent",
                "vibe_list_agents",
                "vibe_send",
                "vibe_wait",
                "vibe_read_messages",
                "vibe_read_output",
                "vibe_delete_agent"
            ]
        );
        for tool in &t {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert!(tool.description.is_some());
            assert!(tool.annotations.is_some());
        }
        let start = &t[1].input_schema;
        assert_eq!(start["required"], json!(["project", "profile", "prompt"]));
        // Reads say they're reads: a client may run them without asking.
        for read in [
            "vibe_list_projects",
            "vibe_list_agents",
            "vibe_wait",
            "vibe_read_messages",
            "vibe_read_output",
        ] {
            let tool = t.iter().find(|t| t.name == read).unwrap();
            assert_eq!(
                tool.annotations.as_ref().unwrap().read_only_hint,
                Some(true),
                "{read}"
            );
        }
        let delete = t.iter().find(|t| t.name == "vibe_delete_agent").unwrap();
        assert_eq!(
            delete.annotations.as_ref().unwrap().destructive_hint,
            Some(true)
        );
    }

    #[test]
    fn arguments_are_checked() {
        let args = |v: Value| v.as_object().unwrap().clone();
        let a = args(json!({"agent": " x ", "n": 3, "f": 2.0, "b": true, "s": "t"}));
        assert_eq!(agent_arg(&a).unwrap(), "x");
        assert_eq!(agent_arg(&args(json!({"window": "w"}))).unwrap(), "w");
        assert!(agent_arg(&args(json!({}))).is_err());
        assert!(agent_arg(&args(json!({"agent": 3}))).is_err());
        assert_eq!(integer(&a, "n", 1, 5).unwrap(), Some(3));
        assert_eq!(integer(&a, "f", 1, 5).unwrap(), Some(2));
        assert!(integer(&a, "n", 4, 5).is_err());
        assert!(integer(&a, "s", 0, 5).is_err());
        assert_eq!(integer(&a, "none", 0, 5).unwrap(), None);
        assert_eq!(boolean(&a, "b").unwrap(), Some(true));
        assert!(boolean(&a, "s").is_err());
        assert!(required(&args(json!({"p": " "})), "p").is_err());
        assert!(string(&a, "n").is_err());
    }
}
