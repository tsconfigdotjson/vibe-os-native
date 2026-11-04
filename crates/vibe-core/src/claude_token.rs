//! The Claude login for `linux` agents (PRD: Exceptions to "set up once,
//! then frozen"; Auth, security and trust): one long-lived token from
//! `claude setup-token`, kept in the Mac's Keychain. `vibe setup-token`
//! puts it there, and `vibed` reads it for each launch and hands it to the
//! VM as [`TOKEN_ENV`], so nothing in a VM refreshes a login or writes it
//! down.
//!
//! The Keychain is reached through `/usr/bin/security`, for writing and
//! reading alike, so the item's access list (the program that made it) lets
//! `vibed` read it without a prompt. The token never goes on a command
//! line: `security -i` reads the command that stores it from a pipe.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::keychain::Target;

/// What Claude Code reads its token from.
pub const TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// For tests only, and only in debug builds: keep the token in this file
/// instead of the Keychain.
pub const TEST_FILE_ENV: &str = "VIBE_TEST_TOKEN_FILE";

/// The Keychain item: a generic password with this service and account.
pub const SERVICE: &str = "VibeOS";
pub const ACCOUNT: &str = "claude-setup-token";

/// What `security` exits with when there's no such item.
const NOT_FOUND: i32 = 44;

/// How a `setup-token` token starts.
pub const PREFIX: &str = "sk-ant-oat";

/// Where the token is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStore {
    /// The Keychain, under this service and account: the login Keychain, unless
    /// a test keeps it in a file of its own.
    Keychain {
        service: String,
        account: String,
        keychain: Target,
    },
    /// A private file. Only for tests, through [`TEST_FILE_ENV`].
    File(PathBuf),
}

impl TokenStore {
    /// The Keychain item, unless [`TEST_FILE_ENV`] names a file.
    pub fn current() -> TokenStore {
        // Release builds keep it in the Keychain, whatever the environment.
        let test_file = if cfg!(debug_assertions) {
            std::env::var_os(TEST_FILE_ENV).filter(|v| !v.is_empty())
        } else {
            None
        };
        match test_file {
            Some(path) => TokenStore::File(PathBuf::from(path)),
            None => TokenStore::Keychain {
                service: SERVICE.into(),
                account: ACCOUNT.into(),
                keychain: Target::Login,
            },
        }
    }

    /// The token, or `None` if none is kept.
    pub fn load(&self) -> io::Result<Option<String>> {
        let text = match self {
            TokenStore::File(path) => match fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            },
            TokenStore::Keychain {
                service,
                account,
                keychain,
            } => {
                let out = security(keychain)?
                    .args(["find-generic-password", "-s", service, "-a", account, "-w"])
                    .args(keychain.arg())
                    .stdin(Stdio::null())
                    .output()?;
                if out.status.code() == Some(NOT_FOUND) {
                    return Ok(None);
                }
                if !out.status.success() {
                    return Err(io::Error::other(format!(
                        "security can't read the Claude token: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                String::from_utf8(out.stdout).map_err(|_| bad("isn't text"))?
            }
        };
        let token = text.trim().to_owned();
        validate(&token).map_err(bad)?;
        Ok(Some(token))
    }

    /// Keeps `token`, replacing any kept before.
    pub fn store(&self, token: &str) -> io::Result<()> {
        validate(token).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        match self {
            TokenStore::File(path) => {
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(path)?;
                f.write_all(token.as_bytes())?;
                f.write_all(b"\n")?;
                f.sync_all()?;
            }
            TokenStore::Keychain {
                service,
                account,
                keychain,
            } => {
                for name in [service, account] {
                    if !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("`{name}` can't name a Keychain item"),
                        ));
                    }
                }
                // `-U` replaces the item if there is one. The token's
                // characters need no quoting (see `validate`).
                let mut line = format!(
                    "add-generic-password -U -s {service} -a {account} -l {service}-{account} -w {token}"
                );
                if let Some(file) = keychain.arg() {
                    line.push(' ');
                    line.push_str(file);
                }
                line.push('\n');
                let mut child = security(keychain)?
                    .arg("-i")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()?;
                child
                    .stdin
                    .take()
                    .expect("piped")
                    .write_all(line.as_bytes())?;
                let mut stderr = String::new();
                if let Some(mut e) = child.stderr.take() {
                    e.read_to_string(&mut stderr)?;
                }
                child.wait()?;
                // `security -i` exits 0 whatever its commands did: what it
                // kept is the proof.
                if self.load()?.as_deref() != Some(token) {
                    return Err(io::Error::other(format!(
                        "the Keychain didn't keep the Claude token: {}",
                        stderr.trim()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Forgets the token. True if there was one.
    pub fn remove(&self) -> io::Result<bool> {
        match self {
            TokenStore::File(path) => match fs::remove_file(path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            },
            TokenStore::Keychain {
                service,
                account,
                keychain,
            } => {
                let out = security(keychain)?
                    .args(["delete-generic-password", "-s", service, "-a", account])
                    .args(keychain.arg())
                    .stdin(Stdio::null())
                    .output()?;
                match out.status.code() {
                    Some(0) => Ok(true),
                    Some(NOT_FOUND) => Ok(false),
                    _ => Err(io::Error::other(format!(
                        "security can't remove the Claude token: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))),
                }
            }
        }
    }
}

fn security(keychain: &Target) -> io::Result<Command> {
    if !cfg!(target_os = "macos") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the Claude token for linux agents is kept in the Keychain, which only a Mac has",
        ));
    }
    keychain.command()
}

fn bad(why: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the kept Claude token {why}; run `vibe setup-token` again"),
    )
}

/// Checks a token looks like one `claude setup-token` makes: letters,
/// digits, `-` and `_`, starting [`PREFIX`].
pub fn validate(token: &str) -> Result<(), String> {
    if !token.starts_with(PREFIX) {
        return Err(format!("isn't a Claude token: it doesn't start {PREFIX}"));
    }
    if token.len() < 40 || token.len() > 512 {
        return Err("isn't a Claude token: its length is wrong".into());
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("isn't a Claude token: it has characters a token can't".into());
    }
    Ok(())
}

/// The last token in what `claude setup-token` printed, terminal escapes
/// and all. One its terminal wrapped onto the next lines is joined up again:
/// a run of token characters that ends a line goes on with the next line
/// when that line, past its indent, is token characters to its end.
pub fn find(output: &[u8]) -> Option<String> {
    let text = strip_escapes(output);
    let is_token = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let run = |s: &str| s.find(|c: char| !is_token(c)).unwrap_or(s.len());
    let line_break = |s: &str| {
        let crs = s.len() - s.trim_start_matches('\r').len();
        s[crs..].starts_with('\n').then_some(crs + 1)
    };
    let mut found = None;
    let mut rest = text.as_str();
    while let Some(at) = rest.find(PREFIX) {
        let tail = &rest[at..];
        let mut token = String::new();
        let mut i = 0;
        loop {
            let end = i + run(&tail[i..]);
            token.push_str(&tail[i..end]);
            i = end;
            let Some(nl) = line_break(&tail[end..]) else {
                break;
            };
            let next = &tail[end + nl..];
            let indent = next.len() - next.trim_start_matches(' ').len();
            let body = &next[indent..];
            let more = run(body);
            // A line that starts a token anew is a redraw, not the rest.
            if more == 0
                || body.starts_with(PREFIX)
                || !(body[more..].is_empty() || line_break(&body[more..]).is_some())
            {
                break;
            }
            i = end + nl + indent;
        }
        if validate(&token).is_ok() {
            found = Some(token);
        }
        rest = &tail[i.max(1)..];
    }
    found
}

/// `bytes` without terminal escape sequences (CSI, OSC and two-byte ones).
fn strip_escapes(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // Parameters, then one final byte in @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                // Up to BEL or ST.
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abcdefghij";

    #[test]
    fn tokens_are_checked() {
        assert!(validate(TOKEN).is_ok());
        assert!(validate("sk-ant-oat01-short").is_err());
        assert!(validate(&format!("{TOKEN} rm -rf")).is_err());
        assert!(validate(&format!("{TOKEN}\n-w x")).is_err());
        assert!(validate(&TOKEN.replace("sk-ant-oat", "sk-ant-api")).is_err());
    }

    #[test]
    fn the_token_is_found_in_what_setup_token_printed() {
        let screen = format!(
            "\x1b[?25l\x1b[2K\x1b[1mLong-lived authentication token created successfully!\x1b[22m\r\n\
             \x1b]0;claude\x07Your OAuth token (valid for 1 year):\r\n\r\n\x1b[32m{TOKEN}\x1b[39m\r\n\
             Store this token securely.\r\n"
        );
        assert_eq!(find(screen.as_bytes()).as_deref(), Some(TOKEN));
        assert_eq!(find(b"no token here, nor sk-ant-oat01-tiny"), None);
        // Wrapped by a narrow terminal, indented, it's joined up again; the
        // prose after it isn't taken for more of it.
        let wrapped = format!(
            "Your OAuth token:\r\n\r\n  {}\r\n  {}\r\n  {}\r\n\r\nStore this token securely.\r\n",
            &TOKEN[..25],
            &TOKEN[25..50],
            &TOKEN[50..]
        );
        assert_eq!(find(wrapped.as_bytes()).as_deref(), Some(TOKEN));
        let then_word = format!("{TOKEN}\nDone\n");
        assert_eq!(
            find(then_word.as_bytes()).map(|t| t.len()),
            Some(TOKEN.len() + 4),
            "a lone word on the next line can't be told from a wrap"
        );
        // A redraw printed it twice; the last whole one wins.
        let twice = format!("{TOKEN}\r\n\x1b[1A\x1b[2K{TOKEN}\r\n");
        assert_eq!(find(twice.as_bytes()).as_deref(), Some(TOKEN));
    }

    #[test]
    fn a_file_store_for_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::File(tmp.path().join("token"));
        assert_eq!(store.load().unwrap(), None);
        store.store(TOKEN).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(TOKEN));
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tmp.path().join("token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(store.store("not a token").is_err());
        assert_eq!(store.load().unwrap().as_deref(), Some(TOKEN));
        fs::write(tmp.path().join("token"), "garbage").unwrap();
        let e = store.load().unwrap_err();
        assert!(e.to_string().contains("vibe setup-token"), "{e}");
        assert!(store.remove().unwrap());
        assert!(!store.remove().unwrap());
    }

    /// The real `security`, on a keychain of the test's own.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_keychain_keeps_the_token() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let temp = crate::keychain::TempKeychain::create(dir.path()).unwrap();
        let store = TokenStore::Keychain {
            service: SERVICE.into(),
            account: ACCOUNT.into(),
            keychain: temp.target(),
        };
        assert_eq!(store.load().unwrap(), None);
        store.store(TOKEN).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(TOKEN));
        let other = TOKEN.replace("AbCd", "ZyXw");
        store.store(&other).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(other.as_str()));
        assert!(store.remove().unwrap());
        assert_eq!(store.load().unwrap(), None);
    }

    /// Reading, keeping and forgetting alike, and on the Mac before `security`
    /// is started: this is what `vibe setup-token` and the `linux` backend use.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_test_cant_reach_the_login_keychains_token() {
        // First, and an item of no use to anyone, as with the door open, what this
        // tries would be real.
        assert!(!crate::keychain::login_open());
        let store = TokenStore::Keychain {
            service: "VibeOS-test".into(),
            account: format!("closed-door-{}", std::process::id()),
            keychain: Target::Login,
        };
        for e in [
            store.load().unwrap_err(),
            store.store(TOKEN).unwrap_err(),
            store.remove().unwrap_err(),
        ] {
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn only_a_mac_has_the_keychain() {
        let store = TokenStore::Keychain {
            service: SERVICE.into(),
            account: ACCOUNT.into(),
            keychain: Target::Login,
        };
        let e = store.load().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
    }
}
