//! A blocking client for the `vibed` socket.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use vibe_core::rpc::{Id, MAX_MESSAGE_BYTES, Method, Outcome, Request, Response};

#[derive(Debug)]
pub(crate) enum ClientError {
    Connect {
        path: PathBuf,
        source: io::Error,
    },
    Io(io::Error),
    /// The daemon answered with something that isn't the expected response.
    Protocol(String),
    /// The daemon answered with an error.
    Rpc(vibe_core::rpc::ErrorObject),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Connect { path, source } => write!(
                f,
                "can't reach vibed at {}: {source} (is vibed running?)",
                path.display()
            ),
            ClientError::Io(e) => write!(f, "talking to vibed: {e}"),
            ClientError::Protocol(m) => write!(f, "unexpected answer from vibed: {m}"),
            ClientError::Rpc(e) => write!(f, "vibed: {e}"),
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub(crate) fn connect(path: &Path) -> Result<UnixStream, ClientError> {
    UnixStream::connect(path).map_err(|source| ClientError::Connect {
        path: path.to_owned(),
        source,
    })
}

/// Calls `M` on the daemon at `socket` and waits up to `timeout` for each
/// read and write.
pub(crate) fn call<M: Method>(
    socket: &Path,
    params: &M::Params,
    timeout: Duration,
) -> Result<M::Result, ClientError> {
    let stream = connect(socket)?;
    // Never zero, which a socket refuses.
    let timeout = timeout.max(Duration::from_millis(1));
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let id = Id::Number(1);
    let mut line = serde_json::to_vec(&Request::call::<M>(id.clone(), params))
        .map_err(|e| ClientError::Protocol(e.to_string()))?;
    line.push(b'\n');
    (&stream).write_all(&line)?;

    let mut reply = Vec::new();
    let limit = MAX_MESSAGE_BYTES as u64 + 1;
    BufReader::new((&stream).take(limit)).read_until(b'\n', &mut reply)?;
    if reply.last() != Some(&b'\n') {
        return Err(ClientError::Protocol(if reply.len() as u64 >= limit {
            "the response is too large".into()
        } else {
            "the connection closed before a response".into()
        }));
    }
    let response: Response =
        serde_json::from_slice(&reply).map_err(|e| ClientError::Protocol(e.to_string()))?;
    // vibed answers a message whose id it couldn't read, such as one that's
    // too large, with an error and a null id. That error is ours too.
    let ours = response.id == id
        || (response.id == Id::Null && matches!(response.outcome, Outcome::Error(_)));
    if !ours {
        return Err(ClientError::Protocol(format!(
            "a response for id {:?}, not {id:?}",
            response.id
        )));
    }
    match response.outcome {
        Outcome::Result(v) => {
            serde_json::from_value(v).map_err(|e| ClientError::Protocol(e.to_string()))
        }
        Outcome::Error(e) => Err(ClientError::Rpc(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use vibe_core::rpc::{ErrorCode, Hello, HelloParams};

    /// A fake daemon that reads one line and answers with `reply` verbatim.
    fn fake(reply: &'static str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            (&stream).write_all(reply.as_bytes()).unwrap();
        });
        (dir, path)
    }

    fn hello(path: &Path) -> Result<vibe_core::rpc::HelloResult, ClientError> {
        let params = HelloParams {
            client: "t".into(),
            protocol: 1,
        };
        call::<Hello>(path, &params, Duration::from_secs(10))
    }

    #[test]
    fn decodes_a_result() {
        let (_d, p) = fake(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"daemon\":\"vibed\",\"version\":\"1\",\"protocol\":1,\"os\":\"linux\",\"arch\":\"x86_64\",\"pid\":5}}\n",
        );
        assert_eq!(hello(&p).unwrap().pid, 5);
    }

    #[test]
    fn surfaces_an_rpc_error() {
        let (_d, p) = fake(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"Protocol version mismatch\",\"data\":{\"protocol\":2}}}\n",
        );
        match hello(&p) {
            Err(ClientError::Rpc(e)) => {
                assert_eq!(e.code, ErrorCode::ProtocolMismatch.code());
                assert_eq!(
                    ClientError::Rpc(e).to_string(),
                    "vibed: Protocol version mismatch (-32000)"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejects_a_mismatched_id() {
        let (_d, p) = fake("{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n");
        assert!(matches!(hello(&p), Err(ClientError::Protocol(m)) if m.contains("id")));
    }

    #[test]
    fn surfaces_an_error_with_a_null_id() {
        let (_d, p) = fake(
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"Message too large\"}}\n",
        );
        assert!(matches!(hello(&p), Err(ClientError::Rpc(e)) if e.message == "Message too large"));
        // A result with a null id is still someone else's.
        let (_d, p) = fake("{\"jsonrpc\":\"2.0\",\"id\":null,\"result\":{}}\n");
        assert!(matches!(hello(&p), Err(ClientError::Protocol(_))));
    }

    #[test]
    fn rejects_a_wrong_result_shape() {
        let (_d, p) = fake("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"daemon\":\"vibed\"}}\n");
        assert!(matches!(hello(&p), Err(ClientError::Protocol(_))));
    }

    #[test]
    fn rejects_garbage_and_early_close() {
        let (_d, p) = fake("not json\n");
        assert!(matches!(hello(&p), Err(ClientError::Protocol(_))));
        let (_d, p) = fake("");
        assert!(
            matches!(hello(&p), Err(ClientError::Protocol(m)) if m.contains("closed")),
            "a daemon that hangs up without answering"
        );
    }

    #[test]
    fn says_when_vibed_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let err = hello(&dir.path().join("none.sock")).unwrap_err();
        assert!(matches!(err, ClientError::Connect { .. }));
        assert!(err.to_string().contains("is vibed running?"), "{err}");
    }
}
