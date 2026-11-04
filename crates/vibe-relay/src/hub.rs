//! A machine's end of the relay: every shim here, over the one connection the
//! Mac opened. `vibe-guest` runs one in a VM, and `vibed` on a Linux host.
//!
//! A shim connects to the local socket and is given a stream on the relay,
//! if there is one; if not, it's told so, and says so to its agent. When the
//! relay goes, every stream goes with it, and each shim reconnects on its next
//! call. A newer relay replaces an older one: after a VM's restore the old
//! connection may never say it's gone.
//!
//! This runs on plain threads, one per connection, as `vibe-guest` does
//! everything: shims are few and their messages small.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::frame::{Frame, PROTOCOL, read_frame, write_frame};
use crate::local::{self, Hello};
use crate::read_line;

/// How often the Mac is pinged, and how long it may go without answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub ping_every: Duration,
    pub dead_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ping_every: Duration::from_secs(15),
            dead_after: Duration::from_secs(45),
        }
    }
}

/// How long a write to a shim may block before it's dropped.
const SHIM_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a client gets to say its hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Says whether a shim may have what it asks for.
pub type Admit = Box<dyn Fn(&str, Option<&str>) -> Result<(), String> + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// The relay now in use.
struct Carrier {
    generation: u64,
    writer: SharedWriter,
    /// Ends the connection, which ends its reader.
    close: Box<dyn Fn() + Send + Sync>,
    last_heard: Instant,
    /// A write end per shim, to send to it and to close it.
    streams: HashMap<u32, UnixStream>,
}

#[derive(Default)]
struct State {
    carrier: Option<Carrier>,
}

/// A machine's shims and its relay.
#[derive(Clone)]
pub struct Hub {
    state: Arc<Mutex<State>>,
    next_stream: Arc<AtomicU32>,
    next_generation: Arc<AtomicU64>,
    admit: Arc<Admit>,
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub")
            .field("connected", &self.connected())
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// A hub that keeps watch on its relay with `timing`, on a thread of its
    /// own, for as long as a clone of it lives.
    pub fn new(admit: Admit, timing: Timing) -> Hub {
        let hub = Hub {
            state: Arc::default(),
            next_stream: Arc::new(AtomicU32::new(1)),
            next_generation: Arc::new(AtomicU64::new(1)),
            admit: Arc::new(admit),
        };
        let watched = Arc::downgrade(&hub.state);
        let _ = std::thread::Builder::new()
            .name("relay-watch".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(timing.ping_every);
                    let Some(state) = watched.upgrade() else {
                        return;
                    };
                    let mut state = lock(&state);
                    let Some(carrier) = &state.carrier else {
                        continue;
                    };
                    if carrier.last_heard.elapsed() >= timing.dead_after {
                        if let Some(dead) = state.carrier.take() {
                            (dead.close)();
                            close_streams(dead.streams);
                        }
                        continue;
                    }
                    // Never wait on a relay that's stuck: its silence is what
                    // ends it.
                    if let Ok(mut w) = carrier.writer.try_lock() {
                        let _ = write_frame(&mut *w, &Frame::Ping);
                    }
                }
            });
        hub
    }

    /// Whether a relay is up.
    pub fn connected(&self) -> bool {
        lock(&self.state).carrier.is_some()
    }

    /// Carries the relay on this connection until it ends, replacing any
    /// other. `close` must make the connection's reads end.
    pub fn carry(
        &self,
        mut reader: impl Read,
        writer: impl Write + Send + 'static,
        close: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<()> {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        write_frame(&mut *lock(&writer), &Frame::Hello { protocol: PROTOCOL })?;
        match read_frame(&mut reader)? {
            Some(Frame::Hello { protocol }) if protocol == PROTOCOL => {}
            Some(Frame::Hello { protocol }) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the Mac speaks relay protocol {protocol}, not {PROTOCOL}"),
                ));
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the Mac opened with {other:?}, not a hello"),
                ));
            }
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        {
            let mut state = lock(&self.state);
            if let Some(old) = state.carrier.take() {
                (old.close)();
                close_streams(old.streams);
            }
            state.carrier = Some(Carrier {
                generation,
                writer,
                close: Box::new(close),
                last_heard: Instant::now(),
                streams: HashMap::new(),
            });
        }
        let ended = self.read_carrier(&mut reader, generation);
        let mut state = lock(&self.state);
        if state
            .carrier
            .as_ref()
            .is_some_and(|c| c.generation == generation)
            && let Some(old) = state.carrier.take()
        {
            (old.close)();
            close_streams(old.streams);
        }
        ended
    }

    fn read_carrier(&self, reader: &mut impl Read, generation: u64) -> io::Result<()> {
        while let Some(frame) = read_frame(reader)? {
            let mut state = lock(&self.state);
            let Some(carrier) = state
                .carrier
                .as_mut()
                .filter(|c| c.generation == generation)
            else {
                // Replaced, and this one's done.
                return Ok(());
            };
            carrier.last_heard = Instant::now();
            match frame {
                Frame::Message { stream, body } => {
                    if let Some(mut shim) = carrier
                        .streams
                        .get(&stream)
                        .and_then(|s| s.try_clone().ok())
                    {
                        // Not under the lock: a slow shim holds up only the
                        // relay's reader, and only until its write times out.
                        drop(state);
                        let wrote = shim
                            .write_all(&body)
                            .and_then(|()| shim.write_all(b"\n"))
                            .and_then(|()| shim.flush());
                        if wrote.is_err() {
                            let _ = shim.shutdown(std::net::Shutdown::Both);
                        }
                    }
                }
                Frame::Close { stream, .. } => {
                    if let Some(shim) = carrier.streams.remove(&stream) {
                        let _ = shim.shutdown(std::net::Shutdown::Both);
                    }
                }
                Frame::Ping => {
                    let writer = carrier.writer.clone();
                    drop(state);
                    write_frame(&mut *lock(&writer), &Frame::Pong)?;
                }
                Frame::Pong | Frame::Hello { .. } | Frame::Open { .. } => {}
            }
        }
        Ok(())
    }

    /// Gives a shim its stream, if there's a relay.
    fn open(
        &self,
        service: &str,
        agent: Option<&str>,
        shim: &UnixStream,
    ) -> Result<(u32, SharedWriter, u64), String> {
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let mut state = lock(&self.state);
        let carrier = state
            .carrier
            .as_mut()
            .ok_or_else(|| local::NO_RELAY.to_owned())?;
        let clone = shim.try_clone().map_err(|e| e.to_string())?;
        let _ = clone.set_write_timeout(Some(SHIM_WRITE_TIMEOUT));
        carrier.streams.insert(id, clone);
        let writer = carrier.writer.clone();
        let generation = carrier.generation;
        drop(state);
        let open = Frame::Open {
            stream: id,
            service: service.to_owned(),
            agent: agent.map(str::to_owned),
        };
        write_frame(&mut *lock(&writer), &open)
            .map_err(|e| format!("Mac unreachable: the connection to it broke: {e}"))?;
        Ok((id, writer, generation))
    }

    /// Serves one client of the local socket until it goes: a shim, or if
    /// `relays` lets it, `vibe services` with a relay.
    pub fn serve(&self, conn: UnixStream, relays: bool) {
        if conn.set_read_timeout(Some(HELLO_TIMEOUT)).is_err() {
            return;
        }
        let Ok(reader) = conn.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader);
        let hello = match read_line(&mut reader) {
            Ok(Some(line)) => Hello::parse(&line),
            _ => return,
        };
        let _ = conn.set_read_timeout(None);
        match hello {
            Err(why) => {
                let _ = (&conn).write_all(local::answer(&Err(why)).as_bytes());
            }
            Ok(Hello::Relay) => {
                if !relays {
                    let why = "this socket doesn't take a relay".to_owned();
                    let _ = (&conn).write_all(local::answer(&Err(why)).as_bytes());
                    return;
                }
                if (&conn)
                    .write_all(local::answer(&Ok(())).as_bytes())
                    .is_err()
                {
                    return;
                }
                let Ok(writer) = conn.try_clone() else {
                    return;
                };
                let closer = conn;
                let _ = self.carry(reader, writer, move || {
                    let _ = closer.shutdown(std::net::Shutdown::Both);
                });
            }
            Ok(Hello::Open { service, agent }) => {
                self.serve_shim(conn, reader, &service, agent.as_deref());
            }
        }
    }

    fn serve_shim(
        &self,
        conn: UnixStream,
        mut reader: BufReader<UnixStream>,
        service: &str,
        agent: Option<&str>,
    ) {
        let opened = (self.admit)(service, agent).and_then(|()| self.open(service, agent, &conn));
        let (stream, writer, generation) = match opened {
            Ok(o) => o,
            Err(why) => {
                let _ = (&conn).write_all(local::answer(&Err(why)).as_bytes());
                return;
            }
        };
        if (&conn)
            .write_all(local::answer(&Ok(())).as_bytes())
            .is_err()
        {
            self.close_stream(stream, generation, &writer);
            return;
        }
        while let Ok(Some(body)) = read_line(&mut reader) {
            let message = Frame::Message { stream, body };
            if write_frame(&mut *lock(&writer), &message).is_err() {
                break;
            }
        }
        self.close_stream(stream, generation, &writer);
    }

    fn close_stream(&self, stream: u32, generation: u64, writer: &SharedWriter) {
        let mut state = lock(&self.state);
        if let Some(c) = state
            .carrier
            .as_mut()
            .filter(|c| c.generation == generation)
        {
            c.streams.remove(&stream);
        }
        drop(state);
        let _ = write_frame(
            &mut *lock(writer),
            &Frame::Close {
                stream,
                reason: None,
            },
        );
    }
}

fn close_streams(streams: HashMap<u32, UnixStream>) {
    for shim in streams.into_values() {
        let _ = shim.shutdown(std::net::Shutdown::Both);
    }
}

/// `vibe services`: the Mac's relay, carried from this process's stdin and
/// stdout to the hub at `socket`, until either side ends.
pub fn relay_stdio(socket: &std::path::Path) -> io::Result<()> {
    let conn = UnixStream::connect(socket)?;
    (&conn).write_all(Hello::Relay.line().as_bytes())?;
    let mut reader = BufReader::new(conn.try_clone()?);
    let line = read_line(&mut reader)?
        .ok_or_else(|| io::Error::other("vibed closed the connection without an answer"))?;
    local::parse_answer(&line).map_err(io::Error::other)?;
    let mut up = conn.try_clone()?;
    let upward = std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut up);
        let _ = up.shutdown(std::net::Shutdown::Both);
    });
    let copied = io::copy(&mut reader, &mut io::stdout().lock());
    let _ = conn.shutdown(std::net::Shutdown::Both);
    drop(upward);
    copied.map(|_| ())
}
