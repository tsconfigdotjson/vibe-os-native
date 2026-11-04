//! How shims reach the broker. On the Mac they connect to a local socket; from
//! a VM or a Linux host they come over a relay connection the Mac opened, many
//! on one (`vibe_relay::frame`).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use vibe_relay::MAX_MESSAGE;
use vibe_relay::frame::{Frame, HEADER_LEN, PROTOCOL};
use vibe_relay::local::{self, Hello};

use crate::broker::Services;

/// Who's at the far end of a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// One agent's VM: that's who's calling, whatever its frames claim.
    Agent(String),
    /// A Linux host, by the name it was added under. It says which of its
    /// agents is calling, for the audit log.
    Host(String),
}

impl Origin {
    fn agent(&self, claimed: Option<&str>) -> String {
        match self {
            Origin::Agent(agent) => agent.clone(),
            Origin::Host(host) => format!("{host}:{}", claimed.unwrap_or("?")),
        }
    }
}

/// How many shims a relay may have at once, so a host can't make the Mac hold
/// without limit.
const MAX_STREAMS: usize = 256;

/// Messages queued for a relay's writer.
const QUEUE: usize = 256;

async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    match r.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut header[1..]).await?;
    let mut payload = vec![0; Frame::payload_len(&header)?];
    r.read_exact(&mut payload).await?;
    Frame::decode(&header, payload).map(Some)
}

/// Serves the broker's end of one relay until it ends. A relay opened as a new
/// one replaces the old at the far end, which closes it.
pub async fn serve_relay(
    services: Arc<Services>,
    origin: Origin,
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
) -> io::Result<()> {
    writer
        .write_all(&Frame::Hello { protocol: PROTOCOL }.encode())
        .await?;
    writer.flush().await?;
    match read_frame(&mut reader).await? {
        Some(Frame::Hello { protocol }) if protocol == PROTOCOL => {}
        Some(Frame::Hello { protocol }) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the far end speaks relay protocol {protocol}, not {PROTOCOL}"),
            ));
        }
        None => return Ok(()),
        Some(other) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the far end opened with {other:?}, not a hello"),
            ));
        }
    }
    let (frames, mut queued) = mpsc::channel::<Vec<u8>>(QUEUE);
    let write_task = tokio::spawn(async move {
        while let Some(bytes) = queued.recv().await {
            if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                return;
            }
        }
    });
    let mut streams: HashMap<u32, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let ended = loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        match frame {
            Frame::Open {
                stream,
                service,
                agent,
            } => {
                streams.retain(|_, tx| !tx.is_closed());
                let opened = if streams.len() >= MAX_STREAMS {
                    Err("too many shims on this relay".to_owned())
                } else {
                    let (out, from_broker) = mpsc::channel::<Vec<u8>>(64);
                    services
                        .open(&service, origin.agent(agent.as_deref()), out)
                        .map(|tx| (tx, from_broker))
                };
                match opened {
                    Ok((tx, mut from_broker)) => {
                        streams.insert(stream, tx);
                        let frames = frames.clone();
                        tokio::spawn(async move {
                            while let Some(body) = from_broker.recv().await {
                                let frame = Frame::Message { stream, body };
                                if frames.send(frame.encode()).await.is_err() {
                                    return;
                                }
                            }
                            let close = Frame::Close {
                                stream,
                                reason: None,
                            };
                            let _ = frames.send(close.encode()).await;
                        });
                    }
                    Err(reason) => {
                        let close = Frame::Close {
                            stream,
                            reason: Some(reason),
                        };
                        let _ = frames.send(close.encode()).await;
                    }
                }
            }
            Frame::Message { stream, body } => {
                if let Some(tx) = streams.get(&stream) {
                    // A shim that's far behind loses its stream: it reconnects.
                    if tx.try_send(body).is_err() {
                        streams.remove(&stream);
                    }
                }
            }
            Frame::Close { stream, .. } => {
                streams.remove(&stream);
            }
            Frame::Ping => {
                if frames.send(Frame::Pong.encode()).await.is_err() {
                    break Ok(());
                }
            }
            Frame::Pong | Frame::Hello { .. } => {}
        }
    };
    // Dropping each stream's channel ends its calls.
    drop(streams);
    drop(frames);
    write_task.abort();
    ended
}

/// Says whether a shim may have what it asks for.
pub type Admit = Arc<dyn Fn(&str, Option<&str>) -> Result<(), String> + Send + Sync>;

/// Serves one client of the Mac's local socket: a shim, as there's no hub here.
/// It takes no relay: the Mac is where relays end, not where they begin.
pub async fn serve_local(services: Arc<Services>, conn: UnixStream, admit: Admit) {
    let (read, mut write) = conn.into_split();
    let mut reader = BufReader::new(read);
    let Some(hello) = read_line(&mut reader).await else {
        return;
    };
    let (service, agent) = match Hello::parse(&hello) {
        Ok(Hello::Open { service, agent }) => (service, agent),
        Ok(Hello::Relay) => {
            let refused = local::answer(&Err("the Mac doesn't take a relay".into()));
            let _ = write.write_all(refused.as_bytes()).await;
            return;
        }
        Err(why) => {
            let _ = write.write_all(local::answer(&Err(why)).as_bytes()).await;
            return;
        }
    };
    let (out, mut from_broker) = mpsc::channel::<Vec<u8>>(64);
    let opened = admit(&service, agent.as_deref()).and_then(|()| {
        services.open(
            &service,
            agent.unwrap_or_else(|| "(no agent)".to_owned()),
            out,
        )
    });
    let tx = match opened {
        Ok(tx) => tx,
        Err(why) => {
            let _ = write.write_all(local::answer(&Err(why)).as_bytes()).await;
            return;
        }
    };
    if write
        .write_all(local::answer(&Ok(())).as_bytes())
        .await
        .is_err()
    {
        return;
    }
    let writing = tokio::spawn(async move {
        while let Some(mut body) = from_broker.recv().await {
            body.push(b'\n');
            if write.write_all(&body).await.is_err() {
                return;
            }
        }
    });
    while let Some(line) = read_line(&mut reader).await {
        if tx.send(line).await.is_err() {
            break;
        }
    }
    drop(tx);
    writing.abort();
}

async fn read_line(r: &mut (impl AsyncBufReadExt + Unpin)) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let n = r
        .take(MAX_MESSAGE as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .ok()?;
    if n == 0 || line.last() != Some(&b'\n') {
        return None;
    }
    line.pop();
    Some(line)
}
