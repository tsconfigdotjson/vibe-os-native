//! The relay's framing: one connection the Mac opened, carrying every shim's
//! conversation on that machine. The same over a VM's vsock port and over
//! `ssh <host> vibe services`.
//!
//! A frame is a kind byte, a big-endian `u32` stream, a big-endian `u32`
//! length and that many bytes. Both ends say [`Frame::Hello`] first. The far
//! end from the Mac opens streams, one per shim; the Mac answers a
//! [`Frame::Ping`] with a [`Frame::Pong`], which is how a machine notices the
//! Mac has gone. Bodies are JSON-RPC messages as they are.

use std::fmt;
use std::io::{self, Read, Write};

use serde_json::{Value, json};

use crate::MAX_MESSAGE;

/// Bumped on any incompatible change. Each end says its own in its hello.
pub const PROTOCOL: u32 = 1;

/// The vsock port the Mac dials in a VM, beside the guest's control and
/// terminal ports.
pub const VSOCK_PORT: u32 = 1102;

/// The bytes before a frame's payload.
pub const HEADER_LEN: usize = 9;

const HELLO: u8 = 0;
const OPEN: u8 = 1;
const MESSAGE: u8 = 2;
const CLOSE: u8 = 3;
const PING: u8 = 4;
const PONG: u8 = 5;

/// One frame on a relay.
#[derive(Clone, PartialEq, Eq)]
pub enum Frame {
    Hello {
        protocol: u32,
    },
    /// A shim wants `service`. `agent` is its agent's ref, if it said: for
    /// the audit log, as a host's word is only a claim.
    Open {
        stream: u32,
        service: String,
        agent: Option<String>,
    },
    /// A JSON-RPC message, either way.
    Message {
        stream: u32,
        body: Vec<u8>,
    },
    /// This stream is over, either way, and why if there's a reason worth
    /// saying.
    Close {
        stream: u32,
        reason: Option<String>,
    },
    Ping,
    Pong,
}

// A body is a JSON-RPC message, whose arguments are not for logs.
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Frame::Hello { protocol } => write!(f, "Hello({protocol})"),
            Frame::Open {
                stream,
                service,
                agent,
            } => write!(f, "Open({stream}, {service}, {agent:?})"),
            Frame::Message { stream, body } => {
                write!(f, "Message({stream}, {} bytes)", body.len())
            }
            Frame::Close { stream, reason } => write!(f, "Close({stream}, {reason:?})"),
            Frame::Ping => write!(f, "Ping"),
            Frame::Pong => write!(f, "Pong"),
        }
    }
}

fn bad(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

impl Frame {
    /// The frame's bytes, whole, to write in one go.
    pub fn encode(&self) -> Vec<u8> {
        let (kind, stream, payload): (u8, u32, Vec<u8>) = match self {
            Frame::Hello { protocol } => (
                HELLO,
                0,
                json!({"protocol": protocol}).to_string().into_bytes(),
            ),
            Frame::Open {
                stream,
                service,
                agent,
            } => {
                let mut open = json!({"service": service});
                if let Some(agent) = agent {
                    open["agent"] = json!(agent);
                }
                (OPEN, *stream, open.to_string().into_bytes())
            }
            Frame::Message { stream, body } => (MESSAGE, *stream, body.clone()),
            Frame::Close { stream, reason } => (
                CLOSE,
                *stream,
                reason.clone().unwrap_or_default().into_bytes(),
            ),
            Frame::Ping => (PING, 0, Vec::new()),
            Frame::Pong => (PONG, 0, Vec::new()),
        };
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(kind);
        out.extend_from_slice(&stream.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// The payload's length a header announces, once it's checked. For a
    /// reader that fills its own buffers, as the broker's async one does.
    pub fn payload_len(header: &[u8; HEADER_LEN]) -> io::Result<usize> {
        let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
        if len > MAX_MESSAGE {
            return Err(bad(format!("a frame of {len} bytes, more than 16 MB")));
        }
        Ok(len)
    }

    /// The frame a header and its payload make.
    pub fn decode(header: &[u8; HEADER_LEN], payload: Vec<u8>) -> io::Result<Frame> {
        let stream = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let object = |payload: &[u8]| -> io::Result<Value> {
            let v: Value = serde_json::from_slice(payload).map_err(|e| bad(e.to_string()))?;
            if v.is_object() {
                Ok(v)
            } else {
                Err(bad("a frame's JSON isn't an object"))
            }
        };
        Ok(match header[0] {
            HELLO => Frame::Hello {
                protocol: object(&payload)?["protocol"]
                    .as_u64()
                    .ok_or_else(|| bad("a hello says no protocol"))?
                    as u32,
            },
            OPEN => {
                let v = object(&payload)?;
                Frame::Open {
                    stream,
                    service: v["service"]
                        .as_str()
                        .ok_or_else(|| bad("an open says no service"))?
                        .to_owned(),
                    agent: v["agent"].as_str().map(str::to_owned),
                }
            }
            MESSAGE => Frame::Message {
                stream,
                body: payload,
            },
            CLOSE => Frame::Close {
                stream,
                reason: if payload.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&payload).into_owned())
                },
            },
            PING => Frame::Ping,
            PONG => Frame::Pong,
            kind => return Err(bad(format!("a frame of unknown kind {kind}"))),
        })
    }
}

/// Writes a frame whole. Frames from several threads must share a lock.
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    w.write_all(&frame.encode())?;
    w.flush()
}

/// Reads the next frame; `None` if the stream ended between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    match r.read_exact(&mut header[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut header[1..])?;
    let mut payload = vec![0; Frame::payload_len(&header)?];
    r.read_exact(&mut payload)?;
    Frame::decode(&header, payload).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> Vec<Frame> {
        vec![
            Frame::Hello { protocol: PROTOCOL },
            Frame::Open {
                stream: 7,
                service: "linear".into(),
                agent: Some("quiet-amber-otter".into()),
            },
            Frame::Open {
                stream: 8,
                service: "linear".into(),
                agent: None,
            },
            Frame::Message {
                stream: 7,
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#.to_vec(),
            },
            Frame::Message {
                stream: 7,
                body: Vec::new(),
            },
            Frame::Close {
                stream: 7,
                reason: Some("gone".into()),
            },
            Frame::Close {
                stream: 8,
                reason: None,
            },
            Frame::Ping,
            Frame::Pong,
        ]
    }

    #[test]
    fn every_kind_of_frame_reads_back_as_it_was_written() {
        let mut wire = Vec::new();
        for f in every_kind() {
            write_frame(&mut wire, &f).unwrap();
        }
        let mut r = wire.as_slice();
        for f in every_kind() {
            assert_eq!(read_frame(&mut r).unwrap(), Some(f));
        }
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn the_layout_is_kind_stream_length_payload() {
        let bytes = Frame::Message {
            stream: 0x0102_0304,
            body: b"hi".to_vec(),
        }
        .encode();
        assert_eq!(bytes, [2, 1, 2, 3, 4, 0, 0, 0, 2, b'h', b'i']);
    }

    #[test]
    fn a_frame_cut_off_is_an_error_not_an_end() {
        let wire = Frame::Ping.encode();
        assert!(read_frame(&mut &wire[..3]).is_err());
        let wire = Frame::Message {
            stream: 1,
            body: b"body".to_vec(),
        }
        .encode();
        assert!(read_frame(&mut &wire[..wire.len() - 1]).is_err());
    }

    #[test]
    fn nothing_too_big_or_unknown_gets_through() {
        let mut huge = vec![MESSAGE, 0, 0, 0, 1];
        huge.extend_from_slice(&(MAX_MESSAGE as u32 + 1).to_be_bytes());
        let err = read_frame(&mut huge.as_slice()).unwrap_err();
        assert!(err.to_string().contains("more than 16 MB"), "{err}");
        let unknown = [99, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(read_frame(&mut unknown.as_slice()).is_err());
        // A hello or an open that isn't what it says.
        for kind in [HELLO, OPEN] {
            let mut wire = vec![kind, 0, 0, 0, 0];
            wire.extend_from_slice(&2u32.to_be_bytes());
            wire.extend_from_slice(b"[]");
            assert!(read_frame(&mut wire.as_slice()).is_err());
        }
    }

    #[test]
    fn a_frames_debug_never_shows_its_body() {
        let shown = format!(
            "{:?}",
            Frame::Message {
                stream: 1,
                body: b"the secret argument".to_vec()
            }
        );
        assert_eq!(shown, "Message(1, 19 bytes)");
    }
}
