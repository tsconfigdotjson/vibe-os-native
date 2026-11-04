//! The hub, with a stand-in for the Mac at the far end of its relay, on a
//! socket pair as it would be on vsock or through `vibe services`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    let hub = hub.clone();
    let closer = there.try_clone().unwrap();
    let writer = there.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = hub.carry(there, writer, move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        });
    });
    let mut here2 = here.try_clone().unwrap();
    write_frame(&mut here2, &Frame::Hello { protocol: PROTOCOL }).unwrap();
    assert_eq!(
        read_frame(&mut here2).unwrap(),
        Some(Frame::Hello { protocol: PROTOCOL })
    );
    // The relay's up just after its hello is through: give the hub its
    // moment, whether another relay is up or not.
    std::thread::sleep(Duration::from_millis(30));
    here
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
    let mut mac = dial(&hub);
    let (_c, mut r, answer) = shim(&socket, None);
    assert_eq!(answer, Ok(()));
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
        dead_after: Duration::from_millis(200),
    };
    let (hub, _socket, _dir) = hub(anyone(), timing);
    let mut mac = dial(&hub);
    for _ in 0..30 {
        assert_eq!(read_frame(&mut mac).unwrap(), Some(Frame::Ping));
        write_frame(&mut mac, &Frame::Pong).unwrap();
    }
    assert!(hub.connected());
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
