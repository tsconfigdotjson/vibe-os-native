//! What backends whose agents run natively on the host share: the test
//! backend, `macos` and the Linux host's cgroups ([`crate::cgroups`]).
//! Their agents run under the shim's PTY, in the shim's session.

use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{EnvError, Result, Run, RunOutput};

/// The most output a [`run`] keeps: the end of it.
pub const MAX_OUTPUT: usize = 1 << 20;

/// How long, once the command has exited, its output may take to end:
/// something it left running may hold the pipe open.
const DRAIN: Duration = Duration::from_secs(2);

/// Runs `r` to completion in a process group of its own, with stdout and
/// stderr on one pipe, and `env` set on top of `r.env`. Whatever it leaves
/// running in its group when it ends is killed, and so is everything once
/// it runs out of time.
pub fn run(r: &Run, env: &[(String, String)]) -> Result<RunOutput> {
    let (program, args) = r
        .argv
        .split_first()
        .ok_or_else(|| EnvError::new("nothing to run"))?;
    let (mut reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&r.cwd)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .envs(r.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| EnvError::new(format!("can't run {program}: {e}")))?;
    // Our copies of the pipe's write end go with the command, so the pipe
    // ends when the command and its children have all closed theirs.
    drop(cmd);
    let pgid = child.id() as i32;
    let output = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    {
        let output = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut out = output.lock().unwrap_or_else(|e| e.into_inner());
                        out.extend_from_slice(&buf[..n]);
                        if out.len() > MAX_OUTPUT {
                            let cut = out.len() - MAX_OUTPUT;
                            out.drain(..cut);
                        }
                    }
                }
            }
            let _ = done_tx.send(());
        });
    }
    let deadline = Instant::now() + r.timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(pgid);
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    kill_group(pgid);
    let _ = done_rx.recv_timeout(DRAIN);
    let output = std::mem::take(&mut *output.lock().unwrap_or_else(|e| e.into_inner()));
    Ok(RunOutput {
        code: status.code(),
        signal: status.signal(),
        timed_out,
        output,
    })
}

fn kill_group(pgid: i32) {
    if pgid > 1 {
        // SAFETY: killpg has no memory-safety preconditions; an empty group
        // makes it fail with ESRCH.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> RunOutput {
        let tmp = std::env::temp_dir();
        run(
            &Run {
                argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
                env: vec![("VIBE_A".into(), "from run".into())],
                cwd: tmp,
                timeout,
            },
            &[
                ("VIBE_A".into(), "from the backend".into()),
                ("VIBE_B".into(), "b".into()),
            ],
        )
        .unwrap()
    }

    #[test]
    fn output_codes_and_environment() {
        let out = sh(
            "echo out; echo err >&2; echo \"$VIBE_A/$VIBE_B\"; exit 3",
            Duration::from_secs(10),
        );
        assert_eq!(out.code, Some(3));
        assert!(!out.success() && !out.timed_out);
        let text = String::from_utf8(out.output).unwrap();
        assert_eq!(text, "out\nerr\nfrom run/b\n");
        assert!(sh("true", Duration::from_secs(10)).success());
        let killed = sh("kill -TERM $$", Duration::from_secs(10));
        assert_eq!((killed.code, killed.signal), (None, Some(libc::SIGTERM)));
    }

    #[test]
    fn time_runs_out_for_everything_it_started() {
        let started = Instant::now();
        let out = sh("sleep 30 & echo started; wait", Duration::from_millis(300));
        assert!(out.timed_out);
        assert_eq!(out.signal, Some(libc::SIGKILL));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(String::from_utf8(out.output).unwrap(), "started\n");
    }

    #[test]
    fn what_it_leaves_running_doesnt_hold_it_up() {
        let started = Instant::now();
        let out = sh("sleep 30 & echo done", Duration::from_secs(20));
        assert!(out.success());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let err = run(
            &Run {
                argv: vec!["/nonexistent/x".into()],
                env: vec![],
                cwd: std::env::temp_dir(),
                timeout: Duration::from_secs(1),
            },
            &[],
        )
        .unwrap_err();
        assert!(err.message().contains("can't run /nonexistent/x"), "{err}");
        assert!(
            run(
                &Run {
                    argv: vec![],
                    env: vec![],
                    cwd: std::env::temp_dir(),
                    timeout: Duration::from_secs(1),
                },
                &[],
            )
            .is_err()
        );
    }
}
