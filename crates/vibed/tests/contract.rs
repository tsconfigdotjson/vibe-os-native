//! Replays every example in `protocol/examples/` through the daemon and
//! checks it answers exactly as recorded.

mod common;

use common::{Conn, HELLO, examples, golden_info, start};
use serde_json::Value;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibed::dispatch::handle_message;

#[test]
fn dispatcher_answers_every_example_exactly() {
    for ex in examples() {
        let got = handle_message(&golden_info(), &ex.line)
            .map(|s| serde_json::from_str::<Value>(&s).unwrap())
            .unwrap_or(Value::Null);
        assert_eq!(got, ex.response, "{}", ex.name);
    }
}

#[tokio::test]
async fn socket_answers_every_example_exactly() {
    let server = start(golden_info(), MAX_MESSAGE_BYTES);
    // One connection for all of them: the socket serves many messages.
    let mut conn = Conn::open(&server.path).await;
    for ex in examples() {
        conn.send(&ex.line).await;
        conn.send(b"\n").await;
        if ex.response.is_null() {
            // Nothing comes back, so the next line is the probe's answer.
            conn.send(HELLO.as_bytes()).await;
            conn.send(b"\n").await;
            assert_eq!(conn.reply().await["id"], "probe", "{}", ex.name);
        } else {
            assert_eq!(conn.reply().await, ex.response, "{}", ex.name);
        }
    }
}
