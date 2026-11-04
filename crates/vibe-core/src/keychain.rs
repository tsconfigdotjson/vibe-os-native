//! The one door to the macOS Keychain: everything that runs `security` gets its
//! command here (PRD: Auth, security and trust).
//!
//! A test must never reach the Keychain of the person running it. A `vibed`
//! with a temporary `HOME` that asks to keep a login puts a "Keychain Not Found"
//! dialog on their screen, and one with the real `HOME` writes among their real
//! items. So the login Keychain is closed unless it's plainly a real install:
//!
//! - A release is let in, unless [`GUARD_ENV`] says `never`.
//! - A debug build is kept out, whoever started it and whatever its `HOME`, unless
//!   it's inside an app bundle or [`GUARD_ENV`] says `allow`.
//! - Everything `cargo` runs has [`GUARD_ENV`] forced to `never`, in
//!   `.cargo/config.toml`, so `cargo test --release` is kept out too, and so is a
//!   shell that exported `allow`.
//!
//! The variable can only close a release, never send one elsewhere. A test that
//! wants the real `security` uses a keychain file of its own: [`TempKeychain`].

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The program every Keychain call runs.
pub const SECURITY: &str = "/usr/bin/security";

/// `never` closes the login Keychain, in any build; `allow` opens it to a debug
/// build.
pub const GUARD_ENV: &str = "VIBE_LOGIN_KEYCHAIN";

/// What `security` is pointed at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Target {
    /// The login Keychain of whoever's running this, if [`login_open`].
    #[default]
    Login,
    /// A keychain file, and nothing else: never the login Keychain, whatever
    /// the guard says.
    File(PathBuf),
}

impl Target {
    /// The real `security`, pointed here. Every call also adds [`Target::arg`]
    /// as its last argument.
    pub fn command(&self) -> io::Result<Command> {
        self.command_with(Path::new(SECURITY))
    }

    /// `program`, pointed here. Only the real `security` is guarded, as nothing
    /// else can reach a Keychain: a stand-in for it can run freely.
    pub fn command_with(&self, program: &Path) -> io::Result<Command> {
        let mut command = Command::new(program);
        match self {
            Target::Login => {
                if program == Path::new(SECURITY) && !login_open() {
                    return Err(closed());
                }
            }
            Target::File(path) => {
                check_file(path)?;
                // Its search list and default keychain, should it look, are
                // beside the file and not anyone's own.
                command.env("HOME", path.parent().unwrap_or(Path::new("/")));
            }
        }
        Ok(command)
    }

    /// The last argument of a call, naming the keychain to use: without one,
    /// `security` uses the login Keychain.
    pub fn arg(&self) -> Option<&str> {
        match self {
            Target::Login => None,
            Target::File(path) => path.to_str(),
        }
    }
}

/// A keychain file's path has to fit on a `security -i` line unquoted.
fn check_file(path: &Path) -> io::Result<()> {
    let plain = path.is_absolute()
        && path.to_str().is_some_and(|text| {
            !text
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "\"'\\".contains(c))
        });
    if plain {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{}` can't name a keychain file", path.display()),
        ))
    }
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "this build stays out of the login Keychain, as it may be a test: \
             {GUARD_ENV}=allow lets a debug build in, outside cargo"
        ),
    )
}

/// Whether this process may reach the login Keychain (see the module's text).
pub fn login_open() -> bool {
    decide(
        cfg!(debug_assertions),
        std::env::var_os(GUARD_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
    )
}

fn decide(debug_build: bool, guard: Option<&OsStr>, exe: Option<&Path>) -> bool {
    if guard == Some(OsStr::new("never")) {
        false
    } else if !debug_build || guard == Some(OsStr::new("allow")) {
        true
    } else {
        exe.is_some_and(in_app_bundle)
    }
}

/// Whether `exe` is a program in an app's `Contents/MacOS`. A debug build there
/// was put there on purpose (`scripts/sign-macos.sh`), and is someone's real
/// VibeOS; nothing's tested from inside a bundle.
fn in_app_bundle(exe: &Path) -> bool {
    let mut parents = exe.ancestors().skip(1);
    parents.next().and_then(Path::file_name) == Some(OsStr::new("MacOS"))
        && parents.next().and_then(Path::file_name) == Some(OsStr::new("Contents"))
        && parents
            .next()
            .and_then(Path::extension)
            .is_some_and(|e| e == "app")
}

/// A keychain file for a test of the real `security`, in a directory of its own
/// and gone when this is dropped. Being outside the login Keychain, it needs no
/// opening of the guard, and being unlocked by its maker, it can't ask anyone
/// for anything.
#[derive(Debug)]
pub struct TempKeychain {
    dir: PathBuf,
    target: Target,
}

impl TempKeychain {
    /// Makes one in `dir`, an empty directory the caller made for it.
    pub fn create(dir: &Path) -> io::Result<TempKeychain> {
        // The temporary directory's own name can be a symlink; securityd keeps
        // the real path.
        let dir = fs::canonicalize(dir)?;
        let target = Target::File(dir.join("test.keychain-db"));
        let made = TempKeychain {
            dir: dir.clone(),
            target,
        };
        let path = made.path().to_owned();
        let password = "test-keychain";
        made.quietly(&["create-keychain", "-p", password, &path])?;
        // Not locked again mid-test, by a timeout or by sleep.
        made.quietly(&["set-keychain-settings", &path])?;
        made.quietly(&["unlock-keychain", "-p", password, &path])?;
        Ok(made)
    }

    /// What to point a store at.
    pub fn target(&self) -> Target {
        self.target.clone()
    }

    fn path(&self) -> &str {
        self.target.arg().expect("a checked path")
    }

    fn quietly(&self, args: &[&str]) -> io::Result<()> {
        let out = self
            .target
            .command()?
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        if out.status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "security {} in {}: {}",
            args[0],
            self.dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

impl Drop for TempKeychain {
    fn drop(&mut self) {
        let path = self.path().to_owned();
        let _ = self.quietly(&["delete-keychain", &path]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TESTS: Option<&str> = Some("/tmp/target/debug/deps/agents-0a1b2c");
    const APP: Option<&str> = Some("/Applications/VibeOS.app/Contents/MacOS/vibed");

    fn open(debug: bool, guard: Option<&str>, exe: Option<&str>) -> bool {
        decide(debug, guard.map(OsStr::new), exe.map(Path::new))
    }

    #[test]
    fn a_debug_build_stays_out_unless_its_told_or_in_an_app() {
        assert!(!open(true, None, TESTS));
        assert!(!open(true, None, None));
        assert!(!open(true, Some(""), TESTS));
        assert!(!open(true, Some("yes"), TESTS));
        assert!(!open(true, None, Some("/tmp/x/bundle/aarch64/vibed")));
        assert!(!open(true, None, Some("/tmp/VibeOS.app/vibed")));
        assert!(open(true, Some("allow"), TESTS));
        assert!(open(true, None, APP));
    }

    #[test]
    fn a_release_goes_in_and_only_never_keeps_it_out() {
        assert!(open(false, None, TESTS));
        assert!(open(false, Some("allow"), TESTS));
        assert!(open(false, Some("anything"), APP));
        assert!(!open(false, Some("never"), APP));
    }

    #[test]
    fn never_beats_everything() {
        assert!(!open(true, Some("never"), APP));
        assert!(!open(false, Some("never"), TESTS));
    }

    /// As these tests are: from cargo in any profile, or run by hand.
    #[test]
    fn a_test_cant_reach_the_login_keychain() {
        assert!(!login_open());
        let e = Target::Login.command().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        assert!(e.to_string().contains(GUARD_ENV), "{e}");
    }

    #[test]
    fn a_stand_in_for_security_needs_no_guard() {
        assert!(Target::Login.command_with(Path::new("/bin/true")).is_ok());
    }

    #[test]
    fn a_file_is_named_on_every_call_and_keeps_home_away() {
        let target = Target::File(PathBuf::from("/tmp/one/test.keychain-db"));
        assert_eq!(target.arg(), Some("/tmp/one/test.keychain-db"));
        let command = target.command().unwrap();
        let home = command.get_envs().find(|(k, _)| *k == "HOME").unwrap().1;
        assert_eq!(home, Some(OsStr::new("/tmp/one")));
        assert_eq!(Target::Login.arg(), None);
    }

    #[test]
    fn a_keychain_files_path_fits_on_a_line() {
        for bad in ["relative", "/a b", "/a\nb", "/a\"b", "/a'b", "/a\\b"] {
            let e = Target::File(PathBuf::from(bad)).command().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    /// The real `security`, on a file of the test's own, and what it leaves.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_temp_keychain_is_made_and_goes() {
        let dir = tempfile::tempdir().unwrap();
        let temp = TempKeychain::create(dir.path()).unwrap();
        let path = PathBuf::from(temp.target().arg().unwrap());
        assert!(path.is_file(), "{path:?}");
        drop(temp);
        assert!(!path.exists(), "{path:?}");
    }
}
