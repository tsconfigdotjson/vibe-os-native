//! The helper itself, as `vibed` runs it, on a port a test may have.
#![cfg(target_os = "linux")]

use std::net::TcpListener;
use std::process::{Command, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_vibe-bind");

fn free_port() -> u16 {
    TcpListener::bind(("::", 0))
        .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn helper(port: u16) -> Command {
    let mut c = Command::new(HELPER);
    c.env_clear()
        .env("VIBE_BIND_TEST_PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

#[test]
fn check_says_it_could_and_a_port_in_use_is_still_allowed() {
    let port = free_port();
    assert_eq!(
        helper(port).arg("--check").status().unwrap().code(),
        Some(0)
    );
    // Something else has it, but this was allowed to try.
    let held = TcpListener::bind(("::", port))
        .or_else(|_| TcpListener::bind(("0.0.0.0", port)))
        .unwrap();
    assert_eq!(
        helper(port).arg("--check").status().unwrap().code(),
        Some(0)
    );
    // Asked for the socket, it's told apart from being refused.
    assert_eq!(helper(port).status().unwrap().code(), Some(98));
    drop(held);
}

#[test]
fn it_takes_no_other_instructions() {
    for args in [&["--port"][..], &["80"][..], &["--check", "x"][..]] {
        let code = helper(free_port()).args(args).status().unwrap().code();
        // Anything but the one flag is refused; extra words after it aren't read.
        if args[0] == "--check" {
            assert_eq!(code, Some(0));
        } else {
            assert_eq!(code, Some(2), "{args:?}");
        }
    }
}
