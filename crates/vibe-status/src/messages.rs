//! The conversation in a transcript, as `vibe_read_messages` gives it (PRD:
//! MCP interface): what was asked, what the agent answered, and its tool
//! calls in short form. Read from the transcript on the host, so it works
//! while the agent sleeps, and never wakes it.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// How much of the end of a transcript is read first, and the most read: a
/// long conversation's transcript runs to many megabytes, and only its
/// tail is wanted.
const FIRST_READ: u64 = 256 * 1024;
const MAX_READ: u64 = 16 * 1024 * 1024;

/// The longest a tool call's input is shown.
const TOOL_INPUT_CHARS: usize = 200;

/// Who said a message, and one message of the conversation: the socket
/// contract's types, so `agents.messages` carries them as they are.
pub use vibe_core::model::MessageRole as Role;
pub use vibe_core::rpc::Message;

/// The last `last` messages of the transcript at `path`, oldest first.
pub fn last_messages(path: &Path, last: usize) -> io::Result<Vec<Message>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut want = FIRST_READ;
    loop {
        let from = len.saturating_sub(want);
        file.seek(SeekFrom::Start(from))?;
        let mut bytes = Vec::new();
        (&mut file).take(len - from).read_to_end(&mut bytes)?;
        // A read from inside the file starts mid-line: that line is dropped.
        let text = String::from_utf8_lossy(&bytes);
        let text = if from > 0 {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &text
        };
        let messages = parse(text);
        if messages.len() >= last || from == 0 || want >= MAX_READ {
            let skip = messages.len().saturating_sub(last);
            return Ok(messages.into_iter().skip(skip).collect());
        }
        want = (want * 4).min(MAX_READ);
    }
}

/// One of Pi's `message` entries: user text, assistant text and each tool
/// call, as Claude's are. An assistant entry is the whole response, so nothing
/// needs joining. System prompts, tool results, thinking and a failed turn's
/// empty message are left out; a failed turn says why, as Pi's screen does.
fn parse_pi_entry(entry: &Value, out: &mut Vec<Message>) {
    let Some(message) = entry.get("message") else {
        return;
    };
    let at = entry
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let blocks: Vec<Value> = match message.get("content") {
        Some(Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut push = |role: Role, text: String| {
        out.push(Message {
            role,
            text,
            at: at.clone(),
        });
    };
    match message.get("role").and_then(Value::as_str) {
        Some("user") => {
            let said = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let said = said.trim();
            if !said.is_empty() {
                push(Role::User, said.to_owned());
            }
        }
        Some("assistant") => {
            for block in &blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.trim().is_empty() {
                            push(Role::Assistant, text.trim().to_owned());
                        }
                    }
                    Some("toolCall") => push(Role::Tool, pi_tool_line(block)),
                    _ => {}
                }
            }
            if let Some(why) = message.get("errorMessage").and_then(Value::as_str) {
                push(Role::Assistant, format!("Error: {why}"));
            }
        }
        _ => {}
    }
}

/// A tool call of Pi's as one short line: its name and a short form of its
/// arguments, as Claude's are shown.
fn pi_tool_line(block: &Value) -> String {
    let as_claude = serde_json::json!({
        "name": block.get("name"),
        "input": block.get("arguments"),
    });
    tool_line(&as_claude)
}

/// The messages in a stretch of transcript, oldest first.
///
/// User text and assistant text become one message each, and each tool
/// call one `tool` message. Tool results, thinking, sidechains (subagents)
/// and the entries Claude marks as meta are left out. Claude writes one
/// reply as several lines sharing a message id, one per block, so those are
/// joined.
///
/// Pi's entries are a `message` holding the role, and are told apart here so
/// a caller needn't know whose transcript it has ([`parse_pi_entry`]).
pub fn parse(text: &str) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut last_assistant: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with('{') {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("isMeta").and_then(Value::as_bool) == Some(true)
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let kind = entry.get("type").and_then(Value::as_str);
        if kind == Some("message") {
            parse_pi_entry(&entry, &mut out);
            continue;
        }
        if kind != Some("user") && kind != Some("assistant") {
            continue;
        }
        let at = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let message = entry.get("message");
        let blocks: Vec<Value> = match message.and_then(|m| m.get("content")) {
            Some(Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let text_of = |b: &Value| {
            (b.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| b.get("text").and_then(Value::as_str))
                .flatten()
                .map(str::to_owned)
        };
        if kind == Some("user") {
            last_assistant = None;
            let said = blocks
                .iter()
                .filter_map(text_of)
                .collect::<Vec<_>>()
                .join("\n");
            let said = said.trim();
            if !said.is_empty() {
                out.push(Message {
                    role: Role::User,
                    text: said.to_owned(),
                    at,
                });
            }
            continue;
        }
        let id = message
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        for block in &blocks {
            if let Some(text) = text_of(block) {
                let same_reply = id.is_some() && id == last_assistant;
                match out.last_mut() {
                    Some(prev) if same_reply && prev.role == Role::Assistant => {
                        prev.text = format!("{}\n{text}", prev.text).trim().to_owned();
                    }
                    _ if !text.trim().is_empty() => out.push(Message {
                        role: Role::Assistant,
                        text: text.trim().to_owned(),
                        at: at.clone(),
                    }),
                    _ => {}
                }
            } else if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                out.push(Message {
                    role: Role::Tool,
                    text: tool_line(block),
                    at: at.clone(),
                });
            }
        }
        last_assistant = id;
    }
    out
}

/// A tool call as one short line: its name, then what it's about.
fn tool_line(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let input = block.get("input");
    // The field that says what the call is about, for the tools that have
    // one.
    let main = input.and_then(|i| {
        [
            "command",
            "file_path",
            "pattern",
            "url",
            "query",
            "description",
            "prompt",
        ]
        .iter()
        .find_map(|k| i.get(*k).and_then(Value::as_str))
    });
    let detail = match (main, input) {
        (Some(m), _) => m.to_owned(),
        (None, Some(i)) => i.to_string(),
        (None, None) => String::new(),
    };
    let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = detail.chars().take(TOOL_INPUT_CHARS).collect();
    let more = if short.len() < detail.len() {
        "…"
    } else {
        ""
    };
    format!("{name}({short}{more})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CONVERSATION: &str = r#"{"type":"user","sessionId":"s","cwd":"/w"}
{"type":"user","timestamp":"2026-10-30T12:00:00.001Z","message":{"role":"user","content":"fix the build"}}
{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>"}}
{"type":"assistant","timestamp":"2026-10-30T12:00:01.000Z","message":{"id":"m1","role":"assistant","content":[{"type":"thinking","thinking":"hmm"}]}}
{"type":"assistant","timestamp":"2026-10-30T12:00:01.500Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Looking."}]}}
{"type":"assistant","timestamp":"2026-10-30T12:00:02.000Z","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo   build\n--locked"}}]}}
{"type":"user","timestamp":"2026-10-30T12:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
{"type":"assistant","isSidechain":true,"message":{"id":"x","role":"assistant","content":[{"type":"text","text":"a subagent"}]}}
{"type":"assistant","timestamp":"2026-10-30T12:00:04.000Z","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"It builds."}]}}
{"type":"assistant","timestamp":"2026-10-30T12:00:04.100Z","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"Done."}]}}
{"type":"assistant","message":{"id":"m3","content":[{"type":"tool_use","name":"Write","input":{"content":"x"}}]}}
not json
{"type":"summary","summary":"Build fix"}
"#;

    #[test]
    fn a_conversation_reads_as_messages() {
        let m = parse(CONVERSATION);
        let got: Vec<(Role, &str)> = m.iter().map(|m| (m.role, m.text.as_str())).collect();
        assert_eq!(
            got,
            [
                (Role::User, "fix the build"),
                (Role::Assistant, "Looking."),
                (Role::Tool, "Bash(cargo build --locked)"),
                (Role::Assistant, "It builds.\nDone."),
                (Role::Tool, r#"Write({"content":"x"})"#),
            ]
        );
        assert_eq!(m[0].at.as_deref(), Some("2026-10-30T12:00:00.001Z"));
        assert_eq!(m[4].at, None);
    }

    #[test]
    fn long_tool_inputs_are_cut() {
        let long = "y".repeat(500);
        let line = format!(
            r#"{{"type":"assistant","message":{{"id":"m","content":[{{"type":"tool_use","name":"Grep","input":{{"pattern":"{long}"}}}}]}}}}"#
        );
        let m = parse(&line);
        assert_eq!(m[0].text, format!("Grep({}…)", "y".repeat(200)));
    }

    #[test]
    fn the_last_messages_come_from_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut f = File::create(&path).unwrap();
        // Far more than the first read, so the reads grow.
        let pad = "z".repeat(1000);
        for i in 0..2000 {
            writeln!(
                f,
                r#"{{"type":"user","message":{{"content":"prompt {i} {pad}"}}}}"#
            )
            .unwrap();
        }
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"id":"a","content":[{{"type":"text","text":"last"}}]}}}}"#
        )
        .unwrap();
        drop(f);
        let m = last_messages(&path, 2).unwrap();
        assert_eq!(m.len(), 2);
        assert!(m[0].text.starts_with("prompt 1999 "));
        assert_eq!(m[1].text, "last");
        // More than the first read holds.
        let m = last_messages(&path, 1000).unwrap();
        assert_eq!(m.len(), 1000);
        assert!(m[0].text.starts_with("prompt 1001 "));
        // More than there are.
        let m = last_messages(&path, 5000).unwrap();
        assert_eq!(m.len(), 2001);
        assert!(last_messages(&dir.path().join("none"), 1).is_err());
    }
}
