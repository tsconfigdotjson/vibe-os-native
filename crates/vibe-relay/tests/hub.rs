//! The hub, with a stand-in for the Mac at the far end of its relay, on a
//! socket pair as it would be on vsock or through `vibe services`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use vibe_relay::frame::{Frame, PROTOCOL, read_frame, write_frame};
use vibe_relay::hub::{Admit, Hub, Timing};
use vibe_relay::local::{self, Hello};

fn anyone() -> Admit {
    Box::new(|_, _| Ok(()))
}

fn hub(admit: Admit, timing: Timing) -> (Hub, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("services.sock");
    let hub = Hub::new(admit, timing);
    let listener = UnixListener::bind(&socket).unwrap();
    let h = hub.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let h = h.clone();
            std::thread::spawn(move || h.serve(conn.unwrap()));
        }
    });
    (hub, socket, dir)
}

/// The Mac's end of a relay given to `hub`.
fn dial(hub: &Hub) -> UnixStream {
    let (here, there) = UnixStream::pair().unwrap();
    here.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let h = hub.clone();
    let closer = there.try_clone().unwrap();
    let writer = there.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = h.carry(there, writer, move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        });
    });
    let mut here2 = here.try_clone().unwrap();
    write_frame(&mut here2, &Frame::Hello { protocol: PROTOCOL }).unwrap();
    assert_eq!(
        read_frame(&mut here2).unwrap(),
        Some(Frame::Hello { protocol: PROTOCOL })
    );
    relay_up(hub);
    here
}

/// The relay's up just after its hello is through. If another's up, what ends that
/// one's streams says when this has replaced it.
fn relay_up(hub: &Hub) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !hub.connected() {
        assert!(Instant::now() < deadline, "the relay never came up");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn shim(
    socket: &Path,
    agent: Option<&str>,
) -> (UnixStream, BufReader<UnixStream>, Result<(), String>) {
    let conn = UnixStream::connect(socket).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let hello = Hello {
        service: "linear".into(),
        agent: agent.map(str::to_owned),
    };
    (&conn).write_all(hello.line().as_bytes()).unwrap();
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut answer = String::new();
    reader.read_line(&mut answer).unwrap();
    let answer = local::parse_answer(answer.trim_end().as_bytes());
    (conn, reader, answer)
}

fn next_not_ping(mac: &mut UnixStream) -> Frame {
    loop {
        match read_frame(mac).unwrap().expect("the relay is up") {
            Frame::Ping => {}
            f => return f,
        }
    }
}

#[test]
fn without_a_relay_a_shim_is_told_so() {
    let (_hub, socket, _dir) = hub(anyone(), Timing::default());
    let (_c, _r, answer) = shim(&socket, None);
    assert_eq!(answer, Err(local::NO_RELAY.to_owned()));
    assert!(local::NO_RELAY.starts_with("Mac unreachable"));
}

#[test]
fn shims_share_a_relay_each_on_a_stream_of_its_own() {
    let (hub, socket, _dir) = hub(anyone(), Timing::default());
    let mut mac = dial(&hub);
    let (mut one, mut one_r, answer) = shim(&socket, Some("quiet-amber-otter"));
    assert_eq!(answer, Ok(()));
    let Frame::Open {
        stream: s1,
        service,
        agent,
    } = next_not_ping(&mut mac)
    else {
        panic!("not an open")
    };
    assert_eq!(service, "linear");
    assert_eq!(agent.as_deref(), Some("quiet-amber-otter"));
    let (mut two, mut two_r, answer) = shim(&socket, None);
    assert_eq!(answer, Ok(()));
    let Frame::Open { stream: s2, .. } = next_not_ping(&mut mac) else {
        panic!("not an open")
    };
    assert_ne!(s1, s2);

    // Each way, on the right stream.
    writeln!(one, "from one").unwrap();
    assert_eq!(
        next_not_ping(&mut mac),
        Frame::Message {
            stream: s1,
            body: b"from one".to_vec()
        }
    );
    writeln!(two, "from two").unwrap();
    assert_eq!(
        next_not_ping(&mut mac),
        Frame::Message {
            stream: s2,
            body: b"from two".to_vec()
        }
    );
    for (stream, body) in [(s2, &b"to two"[..]), (s1, &b"to one"[..])] {
        write_frame(
            &mut mac,
            &Frame::Message {
                stream,
                body: body.to_vec(),
            },
        )
        .unwrap();
    }
    let mut line = String::new();
    one_r.read_line(&mut line).unwrap();
    assert_eq!(line, "to one\n");
    line.clear();
    two_r.read_line(&mut line).unwrap();
    assert_eq!(line, "to two\n");

    // A shim that goes closes its stream, and only that.
    drop((one, one_r));
    assert_eq!(
        next_not_ping(&mut mac),
        Frame::Close {
            stream: s1,
            reason: None
        }
    );
    writeln!(two, "still here").unwrap();
    assert_eq!(
        next_not_ping(&mut mac),
        Frame::Message {
            stream: s2,
            body: b"still here".to_vec()
        }
    );
}

#[test]
fn the_mac_going_ends_every_stream_and_a_new_relay_serves_again() {
    let (hub, socket, _dir) = hub(anyone(), Timing::default());
    let mac = dial(&hub);
    let (_c, mut r, answer) = shim(&socket, None);
    assert_eq!(answer, Ok(()));
    drop(mac);
    let mut line = String::new();
    assert_eq!(r.read_line(&mut line).unwrap(), 0, "the stream ends");
    // Soon it says so to new shims too.
    let mut told = false;
    for _ in 0..200 {
        let (_c, _r, a) = shim(&socket, None);
        if a.is_err() {
            told = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(told);
    assert!(!hub.connected());

    let _mac = dial(&hub);
    let (_c, _r, answer) = shim(&socket, None);
    assert_eq!(answer, Ok(()));
}

#[test]
fn a_new_relay_replaces_an_old_one_that_never_said_it_went() {
    // As after a VM's restore: the old connection is just silent.
    let (hub, socket, _dir) = hub(anyone(), Timing::default());
    let _old = dial(&hub);
    let (_c, mut r, answer) = shim(&socket, None);
    assert_eq!(answer, Ok(()));
    let mut new = dial(&hub);
    let mut line = String::new();
    assert_eq!(r.read_line(&mut line).unwrap(), 0, "its streams end");
    let (_c, _r, answer) = shim(&socket, None);
    assert_eq!(answer, Ok(()));
    assert!(matches!(next_not_ping(&mut new), Frame::Open { .. }));
}

#[test]
fn a_mac_that_stops_answering_pings_is_taken_for_gone() {
    let timing = Timing {
        ping_every: Duration::from_millis(30),
        dead_after: Duration::from_millis(150),
    };
    let (hub, socket, _dir) = hub(anyone(), timing);
    // A stall past `dead_after` ends the relay before the shim reaches it, and the
    // shim's told so: this is about one it did reach.
    let deadline = Instant::now() + Duration::from_secs(120);
    let (mut mac, _c, mut r) = loop {
        let mac = dial(&hub);
        let (c, r, answer) = shim(&socket, None);
        if answer.is_ok() {
            break (mac, c, r);
        }
        assert!(Instant::now() < deadline, "{answer:?}");
    };
    assert_eq!(
        read_frame(&mut mac)
            .unwrap()
            .map(|f| matches!(f, Frame::Open { .. } | Frame::Ping)),
        Some(true)
    );
    // It says nothing back from here on.
    let mut line = String::new();
    assert_eq!(r.read_line(&mut line).unwrap(), 0);
    assert!(!hub.connected());
}

#[test]
fn a_mac_that_answers_pings_stays() {
    let timing = Timing {
        ping_every: Duration::from_millis(30),
        dead_after: Duration::from_secs(1),
    };
    let (hub, _socket, _dir) = hub(anyone(), timing);
    // A machine that stalls this thread or the hub's past `dead_after` ends a relay
    // however it's answered, and rightly. If answers counted for nothing, every
    // relay would end at `dead_after`. So what shows they count is one that lasts.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let mut mac = dial(&hub);
        let began = Instant::now();
        while let Ok(Some(Frame::Ping)) = read_frame(&mut mac) {
            if write_frame(&mut mac, &Frame::Pong).is_err() {
                break;
            }
            if began.elapsed() > timing.dead_after * 3 / 2 && hub.connected() {
                return;
            }
        }
        assert!(Instant::now() < deadline, "no relay outlasted dead_after");
    }
}

#[test]
fn the_hub_can_refuse_a_shim() {
    let admit: Admit = Box::new(|_, agent| match agent {
        Some("off-agent") => Err("shared services are off for this agent's profile".into()),
        _ => Ok(()),
    });
    let (hub, socket, _dir) = hub(admit, Timing::default());
    let _mac = dial(&hub);
    let (_c, _r, answer) = shim(&socket, Some("off-agent"));
    assert_eq!(
        answer,
        Err("shared services are off for this agent's profile".to_owned())
    );
    let (_c, _r, answer) = shim(&socket, Some("on-agent"));
    assert_eq!(answer, Ok(()));
}

#[test]
fn a_process_in_the_guest_cant_pose_as_the_relay() {
    // A relay only ever comes in by vsock: the local socket takes shims alone.
    let (hub, socket, _dir) = hub(anyone(), Timing::default());
    let conn = UnixStream::connect(&socket).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    (&conn).write_all(b"{\"relay\":1}\n").unwrap();
    let mut answer = String::new();
    BufReader::new(conn).read_line(&mut answer).unwrap();
    assert!(local::parse_answer(answer.trim_end().as_bytes()).is_err());
    assert!(!hub.connected());
}

/// A relay's writer that takes its hello, then says it's stuck, and is until its
/// sender goes.
struct Stuck {
    writes: usize,
    stuck: std::sync::mpsc::Sender<()>,
    until: std::sync::mpsc::Receiver<()>,
}

impl Write for Stuck {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        if self.writes > 1 {
            let _ = self.stuck.send(());
            let _ = self.until.recv();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_stuck_relay_doesnt_stop_the_hub_answering() {
    let timing = Timing {
        ping_every: Duration::from_millis(20),
        dead_after: Duration::from_secs(60),
    };
    let hub = Hub::new(anyone(), timing);
    let (here, there) = UnixStream::pair().unwrap();
    let (stuck, is_stuck) = std::sync::mpsc::channel();
    let (_free, until) = std::sync::mpsc::channel();
    let writer = Stuck {
        writes: 0,
        stuck,
        until,
    };
    let h = hub.clone();
    std::thread::spawn(move || {
        let _ = h.carry(there, writer, || {});
    });
    let mut here = here;
    write_frame(&mut here, &Frame::Hello { protocol: PROTOCOL }).unwrap();
    // The watchdog's ping is in its write, for as long as this test lasts.
    is_stuck.recv_timeout(Duration::from_secs(60)).unwrap();
    // On a thread of its own, as a hub holding its state through that write would
    // never answer.
    let (answered, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = answered.send(hub.connected());
    });
    assert_eq!(answer.recv_timeout(Duration::from_secs(60)), Ok(true));
}

/// A connection to the Mac whose writes start failing when it's told to, part
/// way through a frame as a send timeout would.
struct Breaks {
    to: UnixStream,
    broken: Arc<AtomicBool>,
}

impl Write for Breaks {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.to.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.to.flush()
    }
}

/// The Mac's end of a relay whose hub writes through [`Breaks`].
fn dial_breaking(hub: &Hub) -> (UnixStream, Arc<AtomicBool>) {
    let (mut here, there) = UnixStream::pair().unwrap();
    here.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let broken = Arc::new(AtomicBool::new(false));
    let (h, closer, to) = (
        hub.clone(),
        there.try_clone().unwrap(),
        there.try_clone().unwrap(),
    );
    let writer = Breaks {
        to,
        broken: broken.clone(),
    };
    std::thread::spawn(move || {
        let _ = h.carry(there, writer, move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        });
    });
    write_frame(&mut here, &Frame::Hello { protocol: PROTOCOL }).unwrap();
    read_frame(&mut here).unwrap().unwrap();
    relay_up(hub);
    (here, broken)
}

/// The relay's gone, for the Mac, the hub and whoever was on it.
fn relay_ended(hub: &Hub, mac: &mut UnixStream) {
    while let Ok(Some(_)) = read_frame(mac) {}
    for _ in 0..500 {
        if !hub.connected() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the relay is still up");
}

#[test]
fn a_failed_write_from_a_shim_ends_the_relay_rather_than_leave_part_of_a_frame() {
    let (hub, socket, _dir) = hub(anyone(), Timing::default());
    let (mut mac, broken) = dial_breaking(&hub);
    let (mut conn, mut reader, answer) = shim(&socket, None);
    assert_eq!(answer, Ok(()));
    next_not_ping(&mut mac);

    broken.store(true, Ordering::SeqCst);
    writeln!(conn, "this one fails").unwrap();
    relay_ended(&hub, &mut mac);
    let mut rest = String::new();
    assert_eq!(reader.read_line(&mut rest).unwrap(), 0, "the shim's told");
}

#[test]
fn a_failed_ping_ends_the_relay_rather_than_leave_part_of_a_frame() {
    let (hub, _socket, _dir) = hub(
        anyone(),
        Timing {
            ping_every: Duration::from_millis(30),
            // Long enough that only the write failing can be what ends it.
            dead_after: Duration::from_secs(300),
        },
    );
    let (mut mac, broken) = dial_breaking(&hub);
    broken.store(true, Ordering::SeqCst);
    relay_ended(&hub, &mut mac);
}
