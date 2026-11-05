//! What backends whose agents run natively on the host share: the test
//! backend, `macos` and the Linux host's cgroups ([`crate::cgroups`]).
//! Their agents run under the shim's PTY, in the shim's session.

use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{AgentHome, EnvError, Handle, Launch, Result, Run, RunOutput, pi};

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
    run_in(r, env, false)
}

/// [`run`], for a command in `h`'s compartment. A Pi agent's start from the
/// same allowlist it does, so `.vibe/setup.sh` gets no stray provider key.
pub fn run_for(h: &Handle, r: &Run, env: &[(String, String)]) -> Result<RunOutput> {
    run_in(r, env, matches!(h.home, AgentHome::Pi(_)))
}

/// What Pi says of `model`, asked with what the agent would start with. Pi
/// that won't run or doesn't say is left to try when the agent starts.
pub fn pi_auth_check(
    program: &std::path::Path,
    h: &Handle,
    env: &[(String, String)],
    own: Vec<(String, String)>,
    model: &str,
) -> Option<pi::AuthCheck> {
    let launch = Launch {
        agent: vibe_core::model::AgentKind::Pi,
        args: pi::auth_check_args(model),
        // `-e` goes before everything, and `auth` has to come first.
        env: env
            .iter()
            .filter(|(name, _)| name != pi::SERVICES_ENV)
            .cloned()
            .collect(),
        cwd: h.worktree.clone(),
    };
    let AgentCommand { command, env, .. } = agent_command(program, h, &launch, own).ok()?;
    let run = Run {
        argv: command,
        env,
        cwd: h.worktree.clone(),
        timeout: PI_AUTH_CHECK_TIMEOUT,
    };
    let out = run_in(&run, &[], true).ok()?;
    pi::read_auth_check(model, &String::from_utf8_lossy(&out.output))
}

/// Its check is local: longer than this and it's left to the screen to say.
const PI_AUTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn run_in(r: &Run, env: &[(String, String)], clean: bool) -> Result<RunOutput> {
    let (program, args) = r
        .argv
        .split_first()
        .ok_or_else(|| EnvError::new("nothing to run"))?;
    let (mut reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new(program);
    if clean {
        let inherited = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
        cmd.env_clear().envs(pi::allowed_env(inherited));
    } else {
        vibe_core::keychain::withhold_allow(&mut cmd);
    }
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

/// What a native agent's terminal runs, with the environment it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// `env` is all it gets, rather than what it gets on top of `vibed`'s.
    pub clean_env: bool,
}

/// `program` for `launch`, with `own`, the backend's own environment. Claude
/// gets those on top of `vibed`'s. Pi starts from what [`pi::allowed_env`]
/// keeps of `vibed`'s, so no stray key reaches it, and is told where its
/// sessions go; its shared services' extension is written beside the agent, if
/// `launch` offers them.
pub fn agent_command(
    program: &std::path::Path,
    h: &Handle,
    launch: &Launch,
    own: Vec<(String, String)>,
) -> Result<AgentCommand> {
    let inherited = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    agent_command_in(inherited, program, h, launch, own)
}

/// [`agent_command`], with `inherited` as `vibed`'s environment.
pub fn agent_command_in(
    inherited: impl IntoIterator<Item = (String, String)>,
    program: &std::path::Path,
    h: &Handle,
    launch: &Launch,
    own: Vec<(String, String)>,
) -> Result<AgentCommand> {
    let mut command = vec![program.to_string_lossy().into_owned()];
    let AgentHome::Pi(home) = &h.home else {
        command.extend(launch.args.iter().cloned());
        let mut env = own;
        env.extend(launch.env.iter().cloned());
        return Ok(AgentCommand {
            command,
            env,
            clean_env: false,
        });
    };
    let mut env = pi::allowed_env(inherited);
    env.extend(own);
    if pi::offers_services(&launch.env) {
        let extension = pi::write_extension(&h.agent_dir).map_err(|e| {
            EnvError::new(format!("can't write Pi's shared services extension: {e}"))
        })?;
        command.extend(pi::service_args(&launch.env, &extension.to_string_lossy()));
    }
    command.extend(launch.args.iter().cloned());
    env.extend(launch.env.iter().cloned());
    // Last, so `pi.env` can't undo them or move what's looked for. As in a VM.
    env.extend(pi::quiet_env());
    env.push((
        pi::AGENT_DIR_ENV.to_owned(),
        home.dir.to_string_lossy().into_owned(),
    ));
    env.push((
        pi::SESSION_DIR_ENV.to_owned(),
        home.sessions.to_string_lossy().into_owned(),
    ));
    Ok(AgentCommand {
        command,
        env,
        clean_env: true,
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
    use crate::{ClaudeHome, PiHome};
    use std::path::Path;

    fn stray() -> Vec<(String, String)> {
        [
            ("HOME", "/vibed-home"),
            ("USER", "me"),
            ("XDG_RUNTIME_DIR", "/run/user/501"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/501/bus"),
            ("OPENROUTER_API_KEY", "stray"),
            ("ANTHROPIC_API_KEY", "stray"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "stray"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec()
    }

    fn handle(home: crate::AgentHome, dir: &Path) -> Handle {
        Handle {
            agent_ref: "q".into(),
            agent_dir: dir.to_owned(),
            worktree: "/w/q".into(),
            home,
            session: None,
        }
    }

    fn pi_handle(dir: &Path) -> Handle {
        handle(
            crate::AgentHome::Pi(PiHome::user("/h/.pi/agent".into(), Path::new("/w/q"))),
            dir,
        )
    }

    fn launch(agent: vibe_core::model::AgentKind, services: Option<&str>) -> Launch {
        let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), "1".to_owned())];
        // As `vibed` offers them: in the environment, and nothing else.
        env.extend(services.map(|s| (pi::SERVICES_ENV.to_owned(), s.to_owned())));
        Launch {
            agent,
            args: vec!["--".into(), "hi".into()],
            env,
            cwd: "/w/q".into(),
        }
    }

    /// Pi's own answer about a model, from the real launch path and the
    /// stand-in, whose `auth check` prints what Pi's does.
    #[test]
    fn pi_is_asked_about_a_model_with_what_it_would_start_with() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("pi");
        fs::write(&program, crate::testing::PI_STAND_IN).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let dir = tmp.path().join("agent");
        fs::create_dir_all(&dir).unwrap();
        let mut h = handle(
            crate::AgentHome::Pi(PiHome::at(dir.clone(), dir.join("sessions/x"))),
            tmp.path(),
        );
        h.worktree = tmp.path().to_owned();
        let own = vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
        let ask = |model: &str| pi_auth_check(&program, &h, &[], own.clone(), model);
        assert!(matches!(
            ask("mock/m"),
            Some(pi::AuthCheck::NotReady(why)) if why.contains("signed in to mock")
        ));
        fs::write(dir.join("auth.json"), r#"{"mock":{"type":"oauth"}}"#).unwrap();
        assert_eq!(ask("mock/m"), Some(pi::AuthCheck::Ready));
        // A login for another provider isn't one for this.
        assert!(matches!(ask("other/m"), Some(pi::AuthCheck::NotReady(_))));
        // Of a model it doesn't know, which an extension's may be, it says nothing.
        assert_eq!(ask("unknown/m"), None);
        // Services don't get in the way of `auth` coming first.
        let offered = [(pi::SERVICES_ENV.to_owned(), "{}".to_owned())];
        assert_eq!(
            pi_auth_check(&program, &h, &offered, own.clone(), "mock/m"),
            Some(pi::AuthCheck::Ready)
        );
        // A Pi that can't be run is left to try.
        assert_eq!(
            pi_auth_check(&tmp.path().join("nope"), &h, &[], own.clone(), "mock/m"),
            None
        );
    }

    #[test]
    fn pi_gets_an_allowlisted_environment_and_claude_what_it_always_did() {
        use vibe_core::model::AgentKind;
        let tmp = tempfile::tempdir().unwrap();
        let own = vec![("PATH".to_owned(), "/agents/bin".to_owned())];
        let c = agent_command_in(
            stray(),
            Path::new("/bin/pi"),
            &pi_handle(tmp.path()),
            &launch(AgentKind::Pi, None),
            own.clone(),
        )
        .unwrap();
        assert!(c.clean_env);
        let get = |name: &str| {
            c.env
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("OPENROUTER_API_KEY"), None);
        assert_eq!(get("ANTHROPIC_API_KEY"), None);
        assert_eq!(get("CLAUDE_CODE_OAUTH_TOKEN"), None);
        // What a Linux host's scope needs goes through, as does the backend's.
        assert_eq!(get("XDG_RUNTIME_DIR"), Some("/run/user/501"));
        assert!(get("DBUS_SESSION_BUS_ADDRESS").is_some());
        assert_eq!(get("PATH"), Some("/agents/bin"));
        assert_eq!(get("GIT_CONFIG_COUNT"), Some("1"));
        assert_eq!(get("PI_TELEMETRY"), Some("0"));
        assert_eq!(
            get(pi::SESSION_DIR_ENV),
            Some("/h/.pi/agent/sessions/--w-q--"),
            "always said, so a sessionDir setting can't lose the transcript"
        );
        assert_eq!(c.command, ["/bin/pi", "--", "hi"]);

        let claude = handle(
            crate::AgentHome::Claude(ClaudeHome::user(Path::new("/h"))),
            tmp.path(),
        );
        let c = agent_command_in(
            stray(),
            Path::new("/bin/claude"),
            &claude,
            &launch(AgentKind::Claude, Some("{}")),
            own,
        )
        .unwrap();
        assert!(!c.clean_env, "as it was: on top of vibed's");
        // No `-e`: the extension is Pi's, whatever its environment holds.
        assert_eq!(c.command, ["/bin/claude", "--", "hi"]);
    }

    /// `vibed`'s `allow` isn't for a `.vibe/setup.sh`. Run again by itself, with
    /// `allow` around it, as this can't be changed for a process with other
    /// threads. What runs is a plain `sh`, with nothing here reaching for a Keychain.
    #[test]
    fn allow_isnt_passed_on() {
        use vibe_core::keychain::GUARD_ENV;
        const AGAIN: &str = "VIBE_TEST_ALLOW_AROUND";
        if std::env::var_os(AGAIN).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["native::tests::allow_isnt_passed_on", "--exact"])
                .env(AGAIN, "1")
                .env(GUARD_ENV, "allow")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{said}");
            assert!(said.contains("1 passed"), "{said}");
            return;
        }
        assert_eq!(std::env::var(GUARD_ENV).as_deref(), Ok("allow"));
        let out = sh(&format!("echo \"[${GUARD_ENV}]\""), Duration::from_secs(10));
        assert_eq!(String::from_utf8_lossy(&out.output).trim(), "[]");
    }

    #[test]
    fn a_native_pi_loads_the_extension_written_beside_it() {
        use vibe_core::model::AgentKind;
        let tmp = tempfile::tempdir().unwrap();
        let c = agent_command_in(
            stray(),
            Path::new("/bin/pi"),
            &pi_handle(tmp.path()),
            &launch(AgentKind::Pi, Some(r#"{"mcpServers":{}}"#)),
            Vec::new(),
        )
        .unwrap();
        let extension = tmp.path().join(pi::EXTENSION_NAME);
        assert_eq!(fs_read(&extension), pi::EXTENSION);
        assert_eq!(
            c.command,
            ["/bin/pi", "-e", extension.to_str().unwrap(), "--", "hi"]
        );
        assert!(c.env.contains(&(
            pi::SERVICES_ENV.to_owned(),
            r#"{"mcpServers":{}}"#.to_owned()
        )));
        // Off: neither the flag nor the variable.
        let off = agent_command_in(
            stray(),
            Path::new("/bin/pi"),
            &pi_handle(tmp.path()),
            &launch(AgentKind::Pi, None),
            Vec::new(),
        )
        .unwrap();
        assert!(!off.command.iter().any(|w| w == "-e"));
        assert!(!off.env.iter().any(|(k, _)| k == pi::SERVICES_ENV));
    }

    fn fs_read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

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
