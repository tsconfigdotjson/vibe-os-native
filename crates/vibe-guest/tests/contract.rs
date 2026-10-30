//! Checks `vibe_guest::proto` and `vibe_guest::boot` against the contract in
//! `protocol/vibe-guest/`: every example decodes into its type and encodes
//! back to the same JSON, and every request, error code and JSON frame has
//! an example.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use vibe_guest::boot::Boot;
use vibe_guest::proto::{
    ClockSet, Done, ErrorCode, Exit, Frozen, Launched, Processes, Refused, Request, Status,
    TermHello, TermSize, TermWelcome, Thawed, decode_reply, decode_request, encode_reply,
};

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/vibe-guest")
}

/// Every `.json` file in `dir`, by name.
fn load_all(dir: &str) -> Vec<(String, Value)> {
    let mut paths: Vec<PathBuf> = fs::read_dir(contract_dir().join(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    paths
        .into_iter()
        .map(|p| {
            let text = fs::read_to_string(&p).unwrap();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, value)
        })
        .collect()
}

/// Decodes a reply into `T` (or a failure) and encodes it again, which must
/// give the same JSON. The failure's code, if it was one.
fn round_trip<T: DeserializeOwned + Serialize>(name: &str, reply: &Value) -> Option<ErrorCode> {
    let decoded =
        decode_reply::<T>(&reply.to_string()).unwrap_or_else(|e| panic!("{name}: {e}: {reply}"));
    let again: Value = serde_json::from_str(&encode_reply(&decoded)).unwrap();
    assert_eq!(&again, reply, "{name}");
    decoded.err().map(|f| f.code)
}

#[test]
fn every_example_round_trips() {
    let mut ops = BTreeSet::new();
    let mut codes = BTreeSet::new();
    for (name, example) in load_all("examples") {
        let reply = &example["reply"];
        let request = match (example.get("request"), example.get("request_raw")) {
            (Some(request), None) => {
                let decoded =
                    decode_request(&request.to_string()).unwrap_or_else(|f| panic!("{name}: {f}"));
                assert_eq!(&serde_json::to_value(&decoded).unwrap(), request, "{name}");
                decoded
            }
            (None, Some(raw)) => {
                let failure = decode_request(raw.as_str().unwrap()).unwrap_err();
                assert_eq!(
                    serde_json::to_value(&failure).unwrap()["error"],
                    reply["error"]
                );
                codes.insert(format!("{:?}", failure.code));
                assert_eq!(round_trip::<Done>(&name, reply), Some(failure.code));
                continue;
            }
            _ => panic!("{name}: needs `request` or `request_raw`"),
        };
        let op = serde_json::to_value(&request).unwrap()["op"]
            .as_str()
            .unwrap()
            .to_owned();
        ops.insert(op);
        let code = match request {
            Request::Status => round_trip::<Status>(&name, reply),
            Request::Launch(_) => round_trip::<Launched>(&name, reply),
            Request::Processes => round_trip::<Processes>(&name, reply),
            Request::Freeze => round_trip::<Frozen>(&name, reply),
            Request::Thaw { .. } => round_trip::<Thawed>(&name, reply),
            Request::Clock { .. } => round_trip::<ClockSet>(&name, reply),
            Request::Poweroff => round_trip::<Done>(&name, reply),
        };
        if let Some(code) = code {
            codes.insert(format!("{code:?}"));
        }
    }
    let all_ops: BTreeSet<String> = [
        "status",
        "launch",
        "processes",
        "freeze",
        "thaw",
        "clock",
        "poweroff",
    ]
    .map(String::from)
    .into();
    assert_eq!(ops, all_ops, "an example of every request");
    let all_codes: BTreeSet<String> = ErrorCode::ALL.iter().map(|c| format!("{c:?}")).collect();
    assert_eq!(codes, all_codes, "an example of every error code");
}

fn frame<T: DeserializeOwned + Serialize>(name: &str, payload: &Value) {
    let decoded: T =
        serde_json::from_value(payload.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(&serde_json::to_value(&decoded).unwrap(), payload, "{name}");
}

#[test]
fn every_terminal_frame_round_trips() {
    let mut kinds = BTreeSet::new();
    for (name, example) in load_all("terminal") {
        let kind = example["kind"].as_str().unwrap().to_owned();
        let payload = &example["payload"];
        match kind.as_str() {
            "hello" => frame::<TermHello>(&name, payload),
            "welcome" => frame::<TermWelcome>(&name, payload),
            "resize" => frame::<TermSize>(&name, payload),
            "exit" => frame::<Exit>(&name, payload),
            "refused" => frame::<Refused>(&name, payload),
            other => panic!("{name}: no JSON frame `{other}`"),
        }
        kinds.insert(kind);
    }
    let all: BTreeSet<String> = ["hello", "welcome", "resize", "exit", "refused"]
        .map(String::from)
        .into();
    assert_eq!(kinds, all, "an example of every JSON frame");
}

#[test]
fn the_command_line_reads_and_writes_back() {
    let text = fs::read_to_string(contract_dir().join("cmdline.txt")).unwrap();
    let line = text.trim_end();
    let (boot, problems) = Boot::parse(line);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(boot.layers, ["managed", "home", "template", "base"]);
    assert_eq!(boot.upper.as_deref(), Some("agent"));
    assert_eq!((boot.uid(), boot.gid()), (501, 20));
    assert!(boot.shares.iter().any(|s| s.path == "/home/vibe/.claude"));
    assert_eq!(boot.append_to("console=hvc0 quiet").unwrap(), line);
}
