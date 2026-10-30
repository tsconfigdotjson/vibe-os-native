//! Replays every example in `protocol/examples/` through the daemon, in
//! order, as one session, and checks it answers exactly as recorded,
//! including the events that follow a response.

mod common;

use common::{Conn, Golden, HELLO, examples, serve};
use serde_json::Value;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibed::dispatch::{Session, handle_message};

#[tokio::test]
async fn dispatcher_answers_every_example_exactly() {
    let golden = Golden::new().with_sessions();
    let core = golden.core();
    let mut session = Session::default();
    let mut events = None;
    for ex in examples(&golden.home) {
        let Some(line) = &ex.line else { continue };
        let got = handle_message(&core, &mut session, line)
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
    let golden = Golden::new().with_sessions();
    let server = serve(
        golden.core(),
        tempfile::tempdir().unwrap(),
        MAX_MESSAGE_BYTES,
    );
    // One connection for all of them: the socket serves many messages.
    let mut conn = Conn::open(&server.path).await;
    for ex in examples(&golden.home) {
        let Some(line) = &ex.line else { continue };
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
