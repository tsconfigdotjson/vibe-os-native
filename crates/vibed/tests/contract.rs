//! Replays every example in `protocol/examples/` through the daemon, in
//! order, as one session, and checks it answers exactly as recorded,
//! including the events that follow a response.

mod common;

use common::{Conn, GOLDEN_HOME, Golden, HELLO, examples, serve};
use serde_json::Value;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibed::dispatch::{Session, handle_message};

/// What another host's link opens with.
fn link_hello() -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": "link", "method": "daemon.hello", "params": {
        "client": "vibed link calls from tests", "protocol": vibe_core::rpc::PROTOCOL_VERSION,
    }})
    .to_string()
}

/// Whether an example is of a call only another host's link can make.
fn link_only(example: &str) -> bool {
    example.contains("services-token") || example.contains("services-follow")
}

#[tokio::test]
async fn dispatcher_answers_every_example_exactly() {
    let golden = Golden::new().for_examples();
    let core = golden.core();
    let mut session = Session::default();
    let mut link = Session::default();
    handle_message(&core, &mut link, link_hello().as_bytes()).await;
    let mut events = None;
    for ex in examples(&golden) {
        let Some(line) = &ex.line else { continue };
        let session = if link_only(&ex.name) {
            &mut link
        } else {
            &mut session
        };
        let got = handle_message(&core, session, line)
            .await
            .map(|s| serde_json::from_str::<Value>(&s).unwrap())
            .unwrap_or(Value::Null);
        assert_eq!(got, ex.response, "{}", ex.name);
        if let Some((_, rx)) = session.take_subscription() {
            events = Some(rx);
        }
        let mut sent = Vec::new();
        if let Some(rx) = &mut events {
            while let Ok(e) = rx.try_recv() {
                let note = vibe_core::rpc::Request::notification::<vibe_core::rpc::EventNotice>(&e);
                sent.push(serde_json::to_value(note).unwrap());
            }
        }
        assert_eq!(sent, ex.events, "{}: events", ex.name);
    }
}

#[tokio::test]
async fn socket_answers_every_example_exactly() {
    let golden = Golden::new().for_examples();
    let server = serve(
        golden.core(),
        tempfile::tempdir().unwrap(),
        MAX_MESSAGE_BYTES,
    );
    // One connection for all of them: the socket serves many messages.
    let mut conn = Conn::open(&server.path).await;
    let mut link = Conn::open(&server.path).await;
    link.send(format!("{}\n", link_hello()).as_bytes()).await;
    link.reply().await;
    for ex in examples(&golden) {
        let Some(line) = &ex.line else { continue };
        if link_only(&ex.name) {
            link.send(line).await;
            link.send(b"\n").await;
            assert_eq!(link.reply().await, ex.response, "{}", ex.name);
            continue;
        }
        conn.send(line).await;
        conn.send(b"\n").await;
        if ex.response.is_null() {
            // Nothing comes back, so the next line is the probe's answer.
            conn.send(HELLO.as_bytes()).await;
            conn.send(b"\n").await;
            assert_eq!(conn.reply().await["id"], "probe", "{}", ex.name);
        } else {
            assert_eq!(conn.reply().await, ex.response, "{}", ex.name);
        }
        for (i, event) in ex.events.iter().enumerate() {
            assert_eq!(&conn.reply().await, event, "{}: event {i}", ex.name);
        }
    }
}

/// Writes what the dispatcher answers each example with, and the events that
/// follow, to `$VIBE_RECORD_EXAMPLES/<example>`, with the golden home put
/// back. For a change to the contract: review each one before copying its
/// `response` and `events` into `protocol/examples/`.
#[tokio::test]
#[ignore = "writes files; run by hand with VIBE_RECORD_EXAMPLES set"]
async fn record_examples() {
    let out = std::path::PathBuf::from(
        std::env::var_os("VIBE_RECORD_EXAMPLES").expect("set VIBE_RECORD_EXAMPLES to a directory"),
    );
    std::fs::create_dir_all(&out).unwrap();
    let golden = Golden::new().for_examples();
    let home = golden.home.to_string_lossy().into_owned();
    let core = golden.core();
    let mut session = Session::default();
    let mut events = None;
    for ex in examples(&golden) {
        let Some(line) = &ex.line else { continue };
        // As the daemon writes them, so their fields stay in its order.
        let got = handle_message(&core, &mut session, line)
            .await
            .unwrap_or_else(|| "null".into());
        if let Some((_, rx)) = session.take_subscription() {
            events = Some(rx);
        }
        let mut sent = Vec::new();
        if let Some(rx) = &mut events {
            while let Ok(e) = rx.try_recv() {
                let note = vibe_core::rpc::Request::notification::<vibe_core::rpc::EventNotice>(&e);
                sent.push(serde_json::to_string(&note).unwrap());
            }
        }
        let mut text = format!(r#"{{"response": {got}, "events": [{}]}}"#, sent.join(", "))
            .replace(&home, GOLDEN_HOME);
        if let Some(fake) = &golden.service {
            text = common::swap_service(&text, &fake.base, common::GOLDEN_SERVICE);
        }
        std::fs::write(out.join(&ex.name), text).unwrap();
    }
}
