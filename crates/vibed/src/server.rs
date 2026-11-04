//! The Unix socket: binding it safely and serving newline-delimited
//! JSON-RPC on it, with event notifications for connections that subscribe.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use vibe_core::rpc::{ErrorCode, ErrorObject, Event, EventNotice, Id, Request, Response};

use crate::core::Core;
use crate::dispatch::{self, Session};

pub use vibe_core::socket::{BindError, SocketGuard, bind, lock_path};

/// How long a closing connection may take to send what's queued to a client
/// that has stopped reading, before it's dropped.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a response waits for the events published before its call to
/// go out first, to a client that may have stopped reading.
const ORDER_TIMEOUT: Duration = Duration::from_secs(2);

/// A subscribed connection's event forwarder, and what keeps its events in
/// order with the responses: a call's response goes out after the events
/// published before the call, and before those published during it, as the
/// examples in `protocol/` show. An idle subscription isn't held back.
struct Forwarder {
    closing: oneshot::Sender<()>,
    task: JoinHandle<()>,
    /// The last `seq` the forwarder may send now: everything, between
    /// calls; while a call runs, what was published before it.
    gate: watch::Sender<u64>,
    /// The last `seq` it has queued.
    sent: watch::Receiver<u64>,
}

/// Lines queued for a connection's writer. When a client stops reading, its
/// queue fills, and then its events lag rather than pile up.
const QUEUE: usize = 64;

/// Accepts connections until `shutdown` resolves. Each connection is served
/// on its own task, one message at a time.
pub async fn serve(
    listener: UnixListener,
    core: Arc<Core>,
    max_message_bytes: usize,
    shutdown: impl Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let core = core.clone();
                    tokio::spawn(async move {
                        if let Err(e) = connection(stream, core, max_message_bytes).await {
                            crate::log!("vibed: connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    // Usually out of file descriptors; back off rather than spin.
                    crate::log!("vibed: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
}

/// Serves one connection. Responses and events go through one writer task,
/// so lines never interleave, and a subscription's events start after the
/// response to `events.subscribe` ([`Forwarder`] keeps the rest in order).
async fn connection(stream: UnixStream, core: Arc<Core>, max_bytes: usize) -> io::Result<()> {
    let (read, write) = stream.into_split();
    let (tx, rx) = mpsc::channel(QUEUE);
    let writer = tokio::spawn(write_lines(write, rx));
    let mut forwarder = None;
    let read_result = read_messages(read, &core, max_bytes, &tx, &mut forwarder).await;
    if let Some(Forwarder {
        closing, mut task, ..
    }) = forwarder
    {
        // Events already waiting, such as those the last message caused,
        // still go out; then the forwarder stops.
        let _ = closing.send(());
        if tokio::time::timeout(FLUSH_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
    // The writer sends what's queued, then ends once every sender is gone;
    // unless the client has stopped reading, which mustn't keep it forever.
    drop(tx);
    let mut writer = writer;
    let write_result = match tokio::time::timeout(FLUSH_TIMEOUT, &mut writer).await {
        Ok(joined) => joined.unwrap_or_else(|e| Err(io::Error::other(e))),
        Err(_) => {
            writer.abort();
            Ok(())
        }
    };
    read_result.and(write_result).or_else(|e| {
        // A client that goes away mid-reply is a normal end, not an error.
        match e.kind() {
            io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected => Ok(()),
            _ => Err(e),
        }
    })
}

async fn read_messages(
    read: tokio::net::unix::OwnedReadHalf,
    core: &Arc<Core>,
    max_bytes: usize,
    tx: &mpsc::Sender<String>,
    forwarder: &mut Option<Forwarder>,
) -> io::Result<()> {
    let mut reader = BufReader::new(read);
    let mut session = Session::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        let limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut limited = (&mut reader).take(limit);
        let read = limited.read_until(b'\n', &mut line);
        let n = match session.quiet_limit() {
            None => read.await?,
            Some(quiet) => match tokio::time::timeout(quiet, read).await {
                Ok(n) => n?,
                Err(_) => {
                    crate::log!(
                        "vibed: a link went quiet for {} s; closing it",
                        quiet.as_secs()
                    );
                    return Ok(());
                }
            },
        };
        if n == 0 {
            return Ok(());
        }
        let complete = line.last() == Some(&b'\n');
        if complete {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        } else if line.len() > max_bytes {
            let error = ErrorObject::new(
                ErrorCode::InvalidRequest,
                format!("Message too large: the limit is {max_bytes} bytes"),
            );
            let _ = tx
                .send(dispatch::encode(&Response::error(Id::Null, error)))
                .await;
            return Ok(());
        }
        // Without a newline the peer has closed its end: answer what it
        // sent, then stop.
        if !line.iter().all(u8::is_ascii_whitespace) {
            // Events published from here on wait for this call's response.
            let start = core.last_event_seq();
            if let Some(f) = forwarder.as_ref() {
                let _ = f.gate.send(start);
            }
            if let Some(frame) = session.accepted_frame(&line)
                && tx.send(frame).await.is_err()
            {
                return Ok(());
            }
            let reply = dispatch::handle_message(core, &mut session, &line).await;
            if let Some(f) = forwarder.as_mut() {
                // Those published before it go first.
                let _ = tokio::time::timeout(ORDER_TIMEOUT, f.sent.wait_for(|s| *s >= start)).await;
            }
            if let Some(reply) = reply
                && tx.send(reply).await.is_err()
            {
                // The writer is gone: the client stopped listening.
                return Ok(());
            }
            if let Some(f) = forwarder.as_ref() {
                let _ = f.gate.send(u64::MAX);
            }
        }
        if let Some((seq, events)) = session.take_subscription() {
            let (closing, closed) = oneshot::channel();
            let (gate, gated) = watch::channel(u64::MAX);
            let (sent_tx, sent) = watch::channel(seq);
            let task = tokio::spawn(forward_events(
                seq,
                events,
                closed,
                gated,
                sent_tx,
                core.clone(),
                tx.clone(),
            ));
            *forwarder = Some(Forwarder {
                closing,
                task,
                gate,
                sent,
            });
        }
        if !complete {
            return Ok(());
        }
    }
}

/// Sends each event as an `events.event` notification, starting after
/// `seq`, none past what `gate` allows, and says in `sent` how far it got.
/// Once `closed` fires, it sends the events already waiting and stops.
async fn forward_events(
    mut seq: u64,
    mut events: broadcast::Receiver<Event>,
    mut closed: oneshot::Receiver<()>,
    mut gate: watch::Receiver<u64>,
    sent: watch::Sender<u64>,
    core: Arc<Core>,
    tx: mpsc::Sender<String>,
) {
    use broadcast::error::{RecvError, TryRecvError};
    let mut closing = false;
    loop {
        let event = if closing {
            match events.try_recv() {
                Ok(e) => e,
                Err(TryRecvError::Lagged(missed)) => core.lagged(missed, seq + missed),
                Err(TryRecvError::Empty | TryRecvError::Closed) => return,
            }
        } else {
            tokio::select! {
                biased;
                received = events.recv() => match received {
                    Ok(e) => e,
                    Err(RecvError::Lagged(missed)) => core.lagged(missed, seq + missed),
                    Err(RecvError::Closed) => return,
                },
                _ = &mut closed => {
                    closing = true;
                    continue;
                }
            }
        };
        // Published during a call: after its response. Closing, the
        // connection's last response is out already.
        if !closing && event.seq > *gate.borrow() {
            tokio::select! {
                biased;
                _ = gate.wait_for(|allowed| *allowed >= event.seq) => {}
                _ = &mut closed => closing = true,
            }
        }
        seq = event.seq;
        let line = dispatch::encode(&Request::notification::<EventNotice>(&event));
        if tx.send(line).await.is_err() {
            return;
        }
        let _ = sent.send(seq);
    }
}

async fn write_lines(
    mut write: OwnedWriteHalf,
    mut lines: mpsc::Receiver<String>,
) -> io::Result<()> {
    while let Some(line) = lines.recv().await {
        write_line(&mut write, &line).await?;
    }
    // macOS refuses a shutdown once the peer has closed; that's fine.
    let _ = write.shutdown().await;
    Ok(())
}

async fn write_line(write: &mut (impl AsyncWriteExt + Unpin), line: &str) -> io::Result<()> {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    write.write_all(&buf).await
}
