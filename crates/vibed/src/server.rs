//! The Unix socket: binding it safely and serving newline-delimited
//! JSON-RPC on it, with event notifications for connections that subscribe.

use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use vibe_core::rpc::{ErrorCode, ErrorObject, Event, EventNotice, Id, Request, Response};

use crate::core::Core;
use crate::dispatch::{self, Session};

/// Lines queued for a connection's writer. When a client stops reading, its
/// queue fills, and then its events lag rather than pile up.
const QUEUE: usize = 64;

#[derive(Debug)]
pub enum BindError {
    /// The socket's directory is open to other users.
    InsecureDir { dir: PathBuf, mode: u32 },
    /// The socket's directory belongs to another user, who could swap the
    /// socket.
    NotOwned { dir: PathBuf, uid: u32 },
    /// Another `vibed` holds the lock for this socket.
    AlreadyRunning { path: PathBuf },
    /// Something other than a socket is at the socket's path.
    NotASocket { path: PathBuf },
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindError::InsecureDir { dir, mode } => write!(
                f,
                "{} is open to other users (mode {mode:o}); run `chmod 700` on it",
                dir.display()
            ),
            BindError::NotOwned { dir, uid } => {
                write!(f, "{} belongs to uid {uid}, not this user", dir.display())
            }
            BindError::AlreadyRunning { path } => {
                write!(f, "another vibed is already serving {}", path.display())
            }
            BindError::NotASocket { path } => {
                write!(
                    f,
                    "{} exists and isn't a socket; not touching it",
                    path.display()
                )
            }
            BindError::Io { what, path, source } => {
                write!(f, "can't {what} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for BindError {}

/// Holds the socket's lock for as long as the daemon runs, and removes the
/// socket when dropped.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    _lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The lock file that makes one daemon per socket: `<socket>.lock`.
pub fn lock_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Binds the socket at `path`, readable and writable only by this user.
///
/// The directory is created `0700` if missing, and refused if it's open to
/// other users or owned by one, since the socket is as good as a shell. A lock next to the
/// socket keeps a second daemon from stealing it; with the lock held, any
/// socket left at `path` is stale and is replaced.
pub fn bind(path: &Path) -> Result<(std::os::unix::net::UnixListener, SocketGuard), BindError> {
    let io_err = |what, path: &Path| {
        let path = path.to_owned();
        move |source| BindError::Io { what, path, source }
    };
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    match fs::metadata(dir) {
        Ok(meta) => {
            // SAFETY: geteuid has no preconditions and can't fail.
            let me = unsafe { libc::geteuid() };
            if meta.uid() != me {
                return Err(BindError::NotOwned {
                    dir: dir.to_owned(),
                    uid: meta.uid(),
                });
            }
            let mode = meta.permissions().mode() & 0o7777;
            if mode & 0o077 != 0 {
                return Err(BindError::InsecureDir {
                    dir: dir.to_owned(),
                    mode,
                });
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_err("create", dir))?,
        Err(e) => return Err(io_err("read", dir)(e)),
    }

    let lock_path = lock_path(path);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(io_err("open", &lock_path))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(BindError::AlreadyRunning {
                path: path.to_owned(),
            });
        }
        Err(TryLockError::Error(e)) => return Err(io_err("lock", &lock_path)(e)),
    }

    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            fs::remove_file(path).map_err(io_err("remove the stale socket", path))?
        }
        Ok(_) => {
            return Err(BindError::NotASocket {
                path: path.to_owned(),
            });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("read", path)(e)),
    }
    let listener = std::os::unix::net::UnixListener::bind(path).map_err(io_err("bind", path))?;
    let guard = SocketGuard {
        path: path.to_owned(),
        _lock: lock,
    };
    fs::set_permissions(path, Permissions::from_mode(0o600)).map_err(io_err("chmod", path))?;
    listener
        .set_nonblocking(true)
        .map_err(io_err("configure", path))?;
    Ok((listener, guard))
}

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
/// response to `events.subscribe`.
async fn connection(stream: UnixStream, core: Arc<Core>, max_bytes: usize) -> io::Result<()> {
    let (read, write) = stream.into_split();
    let (tx, rx) = mpsc::channel(QUEUE);
    let writer = tokio::spawn(write_lines(write, rx));
    let mut forwarder = None;
    let read_result = read_messages(read, &core, max_bytes, &tx, &mut forwarder).await;
    if let Some((closing, task)) = forwarder {
        // Events already waiting, such as those the last message caused,
        // still go out; then the forwarder stops.
        let _ = closing.send(());
        let _ = task.await;
    }
    // The writer sends what's queued, then ends once every sender is gone.
    drop(tx);
    let write_result = writer.await.unwrap_or_else(|e| Err(io::Error::other(e)));
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
    forwarder: &mut Option<(oneshot::Sender<()>, JoinHandle<()>)>,
) -> io::Result<()> {
    let mut reader = BufReader::new(read);
    let mut session = Session::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        let limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let n = (&mut reader)
            .take(limit)
            .read_until(b'\n', &mut line)
            .await?;
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
        if !line.iter().all(u8::is_ascii_whitespace)
            && let Some(reply) = dispatch::handle_message(core, &mut session, &line).await
            && tx.send(reply).await.is_err()
        {
            // The writer is gone: the client stopped listening.
            return Ok(());
        }
        if let Some(events) = session.take_subscription() {
            let (closing, closed) = oneshot::channel();
            let task = tokio::spawn(forward_events(events, closed, core.clone(), tx.clone()));
            *forwarder = Some((closing, task));
        }
        if !complete {
            return Ok(());
        }
    }
}

/// Sends each event as an `events.event` notification. Once `closed`
/// fires, it sends the events already waiting and stops.
async fn forward_events(
    mut events: broadcast::Receiver<Event>,
    mut closed: oneshot::Receiver<()>,
    core: Arc<Core>,
    tx: mpsc::Sender<String>,
) {
    use broadcast::error::{RecvError, TryRecvError};
    let mut closing = false;
    loop {
        let event = if closing {
            match events.try_recv() {
                Ok(e) => e,
                Err(TryRecvError::Lagged(missed)) => core.lagged(missed),
                Err(TryRecvError::Empty | TryRecvError::Closed) => return,
            }
        } else {
            tokio::select! {
                biased;
                received = events.recv() => match received {
                    Ok(e) => e,
                    Err(RecvError::Lagged(missed)) => core.lagged(missed),
                    Err(RecvError::Closed) => return,
                },
                _ = &mut closed => {
                    closing = true;
                    continue;
                }
            }
        };
        let line = dispatch::encode(&Request::notification::<EventNotice>(&event));
        if tx.send(line).await.is_err() {
            return;
        }
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
