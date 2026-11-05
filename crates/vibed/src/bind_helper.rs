//! Port 443 for a `vibed` that runs as its user (PRD: External access): what it
//! asks of `vibe-bind`, the small root-owned helper that holds the one privilege
//! needed, and what it takes to put that in place. It's in place with nobody asked
//! if this user has passwordless `sudo`; otherwise one button on the Mac runs the
//! same steps, with `sudo`'s own password prompt there.
//!
//! The helper is the way chosen, as the least: only its owner's group can run it,
//! and it can open port 443 and nothing else. Upgrading `vibed` doesn't touch it.
//! Only where a file capability can't work (no `setcap`, a `nosuid` mount, some
//! containers, or a helper's directory someone else could change),
//! `net.ipv4.ip_unprivileged_port_start=443` is the fallback, which lets every user
//! on the server open ports from 443 up. Anything else going wrong, such as a build
//! that isn't the right one or a password refused, stops there and says so.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::remote::shell_quote;

/// Where the root-owned helper is: nowhere this user can write.
pub const HELPER: &str = "/usr/local/lib/vibeos/vibe-bind-1";

/// The build of it that's installed with `vibed`, as this user's, in the same
/// directory: what's copied to [`HELPER`]. It holds no capability there.
pub const SOURCE: &str = "vibe-bind";

/// Where the fallback's setting is kept.
const SYSCTL_FILE: &str = "/etc/sysctl.d/60-vibeos.conf";

/// How long the helper has to hand over the socket.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long trying to put it in place may take, with a `sudo` that's stuck asking
/// a directory service, say.
const GRANT_TIMEOUT: Duration = Duration::from_secs(60);

/// The listening socket from the helper at `helper`. `NotFound` if it isn't there,
/// `PermissionDenied` if it holds no privilege that works here, `AddrInUse` if
/// something else has the port. `test_port` is for a debug build's tests.
pub fn open(helper: &Path, test_port: Option<u16>) -> io::Result<std::net::TcpListener> {
    let (ours, theirs) = UnixStream::pair()?;
    ours.set_read_timeout(Some(TIMEOUT))?;
    let mut command = Command::new(helper);
    command
        .env_clear()
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(port) = test_port {
        command.env("VIBE_BIND_TEST_PORT", port.to_string());
    }
    let mut child = command.spawn()?;
    // It holds this end of the pair too. Gone, a helper that exits ends the read.
    drop(command);
    let got = receive(&ours);
    if matches!(&got, Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
    {
        let _ = child.kill();
    }
    let out = child.wait_with_output()?;
    match (got, out.status.code()) {
        (Ok(Some(fd)), _) => {
            let listener = std::net::TcpListener::from(fd);
            listener.set_nonblocking(true)?;
            Ok(listener)
        }
        (_, Some(13)) => Err(io::ErrorKind::PermissionDenied.into()),
        (_, Some(98)) => Err(io::ErrorKind::AddrInUse.into()),
        (Err(e), _) => Err(e),
        (Ok(None), code) => Err(io::Error::other(format!(
            "vibe-bind ended with {code:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// The descriptor `sock`'s peer sends, if it sends one.
fn receive(sock: &UnixStream) -> io::Result<Option<OwnedFd>> {
    // SAFETY: the control buffer is sized for one descriptor and aligned for a
    // cmsghdr, the iovec and its byte outlive the call, and a descriptor found in a
    // SCM_RIGHTS message is ours to own from then on.
    unsafe {
        let mut control = [0u64; 4];
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&raw mut byte).cast(),
            iov_len: 1,
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(&control) as _;
        // Not to be inherited: agents' shims outlive `vibed`, and one holding this
        // would keep the port from the next `vibed`, and could accept on it.
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let n = libc::recvmsg(sock.as_raw_fd(), &raw mut msg, flags);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&raw const msg);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Ok(None);
        }
        let fd = libc::CMSG_DATA(header)
            .cast::<libc::c_int>()
            .read_unaligned();
        let fd = OwnedFd::from_raw_fd(fd);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        Ok(Some(fd))
    }
}

/// What [`GRANT`] exits with when a file capability can't work here, so only then
/// is the fallback tried. Nothing else says this: not `sudo`, which says 1 for a
/// password refused, nor a build that isn't the right one.
const CANT_WORK_HERE: u8 = 75;

/// What's run as root to put the helper in place: `sh -c <this> sh <source> <group>
/// <sha256> <helper>`. If a directory above it is one someone else could change,
/// "root-owned" would mean nothing there. The copy is only given its group and made
/// runnable once the sum has been checked, on a root-owned file in a directory only
/// root can change, so a source swapped for a link to something private shows nobody
/// anything. The capability goes last of all, as `chown` would clear it. With an
/// empty sum, for `sudo -n`, whoever can run this could run anything as root already.
const GRANT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin
set -eu
helper=$4
dir=$(dirname "$helper")
new=$helper.new
p=$dir
while [ "$p" != / ]; do
  p=$(dirname "$p")
  if [ -z "$(find "$p" -maxdepth 0 -user root ! -perm /022)" ]; then
    echo "$p isn't only root's, so a helper there would mean nothing" >&2
    exit 75
  fi
done
if [ -L "$dir" ]; then
  echo "$dir is a link, so a helper there would mean nothing" >&2
  exit 75
fi
install -d -o root -g root -m 0755 "$dir"
chown root:root "$dir"
chmod 0755 "$dir"
rm -f "$new"
install -o root -g root -m 0700 "$1" "$new"
if [ -n "$3" ] && [ "$(sha256sum "$new" | cut -d " " -f 1)" != "$3" ]; then
  rm -f "$new"
  echo "vibe-bind there isn't the build this VibeOS installs: nothing was changed" >&2
  exit 1
fi
chgrp "$2" "$new"
chmod 0750 "$new"
if ! setcap cap_net_bind_service=+ep "$new"; then
  rm -f "$new"
  echo "this server won't take a file capability" >&2
  exit 75
fi
mv -f "$new" "$helper"
"#;

/// What's run as root when the helper can't work: `sh -c <this> sh <file>`. Never
/// raises a setting that's lower already. It's kept only once it's taken, so a
/// server that refuses it isn't left with it for its next boot.
const FALLBACK: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin
set -eu
key=net.ipv4.ip_unprivileged_port_start
now=$(sysctl -n "$key")
[ -n "$now" ]
if [ "$now" -le 443 ]; then
  exit 0
fi
sysctl -q -w "$key=443"
printf '%s=443\n' "$key" > "$1"
"#;

/// The shell lines that get port 443 usable by this user, ending in success or not:
/// as `sudo` is called (`sudo`, or `sudo -n` to never ask), from the helper build at
/// `source`, a shell word. `sum` is the sum to hold it to, as a shell word, and may
/// be empty. The fallback is only for what can't work otherwise.
pub fn grant_lines(sudo: &str, source: &str, sum: &str, helper_path: &Path) -> String {
    let helper = helper_path.to_string_lossy();
    format!(
        "{sudo} sh -c {grant} sh {source} \"$(id -gn)\" {sum} {path} </dev/null\n\
         s=$?\n\
         if [ \"$s\" = 0 ]; then\n\
           {path} --check </dev/null\n\
           s=$?\n\
           if [ \"$s\" = 13 ]; then s={CANT_WORK_HERE}; fi\n\
         fi\n\
         if [ \"$s\" = {CANT_WORK_HERE} ]; then\n\
           {sudo} sh -c {fallback} sh {file} </dev/null\n\
           s=$?\n\
         fi\n\
         [ \"$s\" = 0 ]\n",
        grant = shell_quote(GRANT),
        fallback = shell_quote(FALLBACK),
        path = shell_quote(&helper),
        file = shell_quote(SYSCTL_FILE),
    )
}

/// Tries to get port 443 usable without asking anyone, with `sudo -n`. `source` is
/// the build beside this `vibed`. Says if it could have worked. Whoever calls this
/// keeps it to once a run: a refused `sudo -n` is in the server's logs.
pub fn grant_quietly(source: &Path, helper: &Path, path: Option<&std::ffi::OsStr>) -> bool {
    let line = grant_lines(
        "sudo -n",
        &shell_quote(&source.to_string_lossy()),
        "''",
        helper,
    );
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(line)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            crate::log!("vibed: can't try allowing port 443: {e}");
            return false;
        }
    };
    let begun = Instant::now();
    while matches!(child.try_wait(), Ok(None)) {
        if begun.elapsed() > GRANT_TIMEOUT {
            // SAFETY: only signals the group this child leads.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => {
            crate::log!("vibed: port 443 is allowed now, with no password asked for");
            true
        }
        Ok(out) => {
            crate::log!(
                "vibed: can't allow port 443 without a password: {}",
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("it took too long")
            );
            false
        }
        Err(e) => {
            crate::log!("vibed: can't try allowing port 443: {e}");
            false
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// A directory of stand-ins, first on the `PATH`, logging what each is given.
    struct Stubs {
        dir: tempfile::TempDir,
    }

    impl Stubs {
        fn new() -> Stubs {
            let s = Stubs {
                dir: tempfile::tempdir().unwrap(),
            };
            // As what's asked of them on a server that's as it should be.
            s.stub("find", "echo \"$1\"");
            s.stub("chown", "exit 0");
            s.stub("chgrp", "exit 0");
            s.stub("chmod", "exit 0");
            s.stub("setcap", "exit 0");
            s.stub(
                "install",
                "[ \"$1\" = -d ] && { eval \"d=\\${$#}\"; mkdir -p \"$d\"; exit 0; }\n\
                 eval \"to=\\${$#}\"; n=$(($#-1)); eval \"from=\\${$n}\"; cp \"$from\" \"$to\"",
            );
            s
        }

        fn stub(&self, name: &str, body: &str) {
            let p = self.dir.path().join(name);
            fs::write(
                &p,
                format!(
                    "#!/bin/sh\necho \"{name} $*\" >> \"{}/log\"\n{body}\n",
                    self.dir.path().display()
                ),
            )
            .unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn log(&self) -> String {
            fs::read_to_string(self.path("log")).unwrap_or_default()
        }

        /// `script`, with its own `PATH` taken out so these are what it finds.
        fn run(&self, script: &str, args: &[&Path]) -> Option<i32> {
            let script = script.replacen("PATH=/usr/sbin:/usr/bin:/sbin:/bin\n", "", 1);
            assert!(!script.contains("PATH="));
            self.run_as_it_is(&script, args)
        }

        /// `script` itself: for what `sudo`, a stand-in, never runs.
        fn run_as_it_is(&self, script: &str, args: &[&Path]) -> Option<i32> {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .arg("sh")
                .args(args)
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", self.dir.path().display()),
                )
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .code()
        }

        fn grant(&self, source: &Path, sum: &str, helper: &Path) -> Option<i32> {
            self.run(
                GRANT,
                &[source, Path::new("mygroup"), Path::new(sum), helper],
            )
        }
    }

    use std::path::PathBuf;

    fn source(s: &Stubs) -> (PathBuf, String) {
        let p = s.path("vibe-bind");
        fs::write(&p, "the build").unwrap();
        let sum = crate::remote::sha256_hex(b"the build");
        (p, sum)
    }

    #[test]
    fn the_right_build_is_put_in_place_with_its_capability_last() {
        let s = Stubs::new();
        let (src, sum) = source(&s);
        let helper = s.path("lib/vibe-bind-1");
        assert_eq!(s.grant(&src, &sum, &helper), Some(0));
        assert!(helper.is_file());
        assert!(!s.path("lib/vibe-bind-1.new").exists());
        let log = s.log();
        let at = |what: &str| log.find(what).unwrap_or_else(|| panic!("{what}: {log}"));
        assert!(at("install -o root") < at("chgrp mygroup"), "{log}");
        assert!(at("chgrp mygroup") < at("chmod 0750"), "{log}");
        // chown would clear it, so nothing that touches ownership comes after.
        assert!(at("chmod 0750") < at("setcap"), "{log}");
    }

    #[test]
    fn another_build_or_none_is_refused_and_is_no_reason_for_the_wider_way() {
        let s = Stubs::new();
        let (src, _) = source(&s);
        let helper = s.path("lib/vibe-bind-1");
        assert_eq!(s.grant(&src, &"0".repeat(64), &helper), Some(1));
        assert!(!helper.exists());
        assert!(!s.path("lib/vibe-bind-1.new").exists());
        assert!(!s.log().contains("setcap"), "{}", s.log());
        assert_eq!(
            s.grant(&s.path("nothing-here"), &"0".repeat(64), &helper),
            Some(1)
        );
    }

    #[test]
    fn what_cant_hold_a_capability_says_so_as_the_one_reason_for_the_fallback() {
        let s = Stubs::new();
        let (src, sum) = source(&s);
        let helper = s.path("lib/vibe-bind-1");
        s.stub("setcap", "exit 1");
        assert_eq!(s.grant(&src, &sum, &helper), Some(75));
        assert!(!helper.exists());
        assert!(!s.path("lib/vibe-bind-1.new").exists());
    }

    #[test]
    fn a_directory_above_that_someone_else_could_change_is_no_place_for_it() {
        let s = Stubs::new();
        let (src, sum) = source(&s);
        // find says nothing: it isn't root's alone.
        s.stub("find", "exit 0");
        let helper = s.path("lib/vibe-bind-1");
        assert_eq!(s.grant(&src, &sum, &helper), Some(75));
        assert!(!s.log().contains("install"), "{}", s.log());
    }

    #[test]
    fn the_fallback_never_raises_a_setting_and_keeps_only_what_was_taken() {
        let s = Stubs::new();
        let file = s.path("60-vibeos.conf");
        let args = [file.as_path()];
        // Lower already.
        s.stub("sysctl", "echo 80");
        assert_eq!(s.run(FALLBACK, &args), Some(0));
        assert!(!file.exists());
        // Refused by the kernel, or a key that isn't there: nothing for next boot.
        s.stub(
            "sysctl",
            "[ \"$1\" = -n ] && { echo 1024; exit 0; }; exit 1",
        );
        assert_ne!(s.run(FALLBACK, &args), Some(0));
        assert!(!file.exists());
        s.stub("sysctl", "exit 0");
        assert_ne!(s.run(FALLBACK, &args), Some(0));
        assert!(!file.exists());
        // Taken, and kept.
        s.stub("sysctl", "[ \"$1\" = -n ] && echo 1024; exit 0");
        assert_eq!(s.run(FALLBACK, &args), Some(0));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "net.ipv4.ip_unprivileged_port_start=443\n"
        );
    }

    /// How the two are put together: by a stand-in `sudo` that exits as it's told,
    /// and a helper that says how its check went.
    fn granted(sudo_exit: i32, check_exit: i32) -> (Option<i32>, String) {
        let s = Stubs::new();
        s.stub("sudo", &format!("exit {sudo_exit}"));
        s.stub("id", "echo mygroup");
        let helper = s.path("helper");
        fs::write(&helper, format!("#!/bin/sh\nexit {check_exit}\n")).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let lines = grant_lines("sudo -n", "src", "''", &helper);
        let code = s.run_as_it_is(&lines, &[]);
        (code, s.log())
    }

    fn sudo_calls(log: &str) -> usize {
        log.lines().filter(|l| l.starts_with("sudo ")).count()
    }

    #[test]
    fn only_a_capability_that_cant_work_goes_the_wider_way() {
        // All's well: just the once.
        let (code, log) = granted(0, 0);
        assert_eq!((code, sudo_calls(&log)), (Some(0), 1), "{log}");
        // Refused, or a password mistyped, or the wrong build: it stops there, and a
        // refused `sudo` isn't tried twice for the server's log.
        let (code, log) = granted(1, 0);
        assert_eq!((code, sudo_calls(&log)), (Some(1), 1), "{log}");
        // It says it can't.
        let (code, log) = granted(75, 0);
        assert_eq!(sudo_calls(&log), 2, "{log}");
        // ...and the stand-in says that of the fallback too, so it isn't called done.
        assert_eq!(code, Some(1), "{log}");
        // In place, and it doesn't work here (a nosuid mount).
        let (_, log) = granted(0, 13);
        assert_eq!(sudo_calls(&log), 2, "{log}");
    }

    /// B1 of the review: a socket a shim inherits outlives `vibed`, and keeps the
    /// port from the next one. The real helper, and a real child after it.
    #[test]
    fn the_socket_handed_over_isnt_inherited_by_what_vibed_starts() {
        let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            })
            .join("vibe-bind");
        let helper = std::env::var_os("CARGO_TARGET_DIR")
            .map(|d| PathBuf::from(d).join("debug/vibe-bind"))
            .unwrap_or(helper);
        if !helper.is_file() {
            eprintln!("skipped: {} isn't built", helper.display());
            return;
        }
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let listener = open(&helper, Some(port)).unwrap();
        // SAFETY: only reads the flags of a descriptor this test holds.
        let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        // A child outliving it, as a shim does: the port's free for the next one.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        drop(listener);
        let again = open(&helper, Some(port));
        let _ = child.kill();
        let _ = child.wait();
        again.unwrap();
    }

    #[test]
    fn a_helper_that_fails_says_why_at_once_and_one_stuck_is_let_go() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let p = tmp.path().join(name);
            fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let begun = Instant::now();
        let denied = open(&write("denied", "exit 13"), None).unwrap_err();
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
        let in_use = open(&write("in-use", "exit 98"), None).unwrap_err();
        assert_eq!(in_use.kind(), io::ErrorKind::AddrInUse);
        let other = open(&write("other", "echo boom >&2; exit 1"), None).unwrap_err();
        assert!(other.to_string().contains("boom"), "{other}");
        assert!(
            begun.elapsed() < Duration::from_secs(4),
            "{:?}",
            begun.elapsed()
        );

        let begun = Instant::now();
        open(&write("stuck", "exec sleep 30"), None).unwrap_err();
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "{:?}",
            begun.elapsed()
        );
    }
}
