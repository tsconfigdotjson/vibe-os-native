//! Where a host keeps its login to a service: the Keychain on a Mac, a private
//! file on a Linux host, in the same trust class as the `gh` and Claude logins
//! already on that machine. One item per service, replaced whole, so a login is
//! never half-written.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use vibe_core::model::ServiceAuth;

use crate::util::{base64_decode, base64_standard};

/// A login. Nothing but the store and the broker sees it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    pub auth: ServiceAuth,
    /// The access token, or the API key.
    pub access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
    /// When `access` runs out, in seconds since the epoch, if it was said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// The client registered for it, and where it refreshes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

// So no log line, panic or test failure can ever print a token.
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret")
            .field("auth", &self.auth)
            .field("expires_at", &self.expires_at)
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// Keeps logins.
pub trait SecretStore: Send + Sync + fmt::Debug {
    fn load(&self, service: &str) -> io::Result<Option<Secret>>;
    /// Replaces the login whole. Once this returns it's durable.
    fn store(&self, service: &str, secret: &Secret) -> io::Result<()>;
    fn delete(&self, service: &str) -> io::Result<()>;
}

fn encode(secret: &Secret) -> io::Result<String> {
    Ok(base64_standard(&serde_json::to_vec(secret)?))
}

fn decode(text: &str) -> io::Result<Secret> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "a kept login makes no sense");
    let bytes = base64_decode(text.trim()).ok_or_else(bad)?;
    // Not the error's own text: it may quote what it read.
    serde_json::from_slice(&bytes).map_err(|_| bad())
}

/// The Keychain item's service, as for the Claude token.
const KEYCHAIN_SERVICE: &str = "VibeOS";
const SECURITY: &str = "/usr/bin/security";
/// What `security` exits with when there's no such item.
const NOT_FOUND: i32 = 44;
/// `security -i` reads lines of at most 4 KB. The tokens seen in practice are
/// far shorter; a longer one is refused here rather than cut short there.
const MAX_LINE: usize = 4000;

/// The login Keychain, through `/usr/bin/security`, for writing and reading
/// alike, so the item's access list lets `vibed` read it without a prompt. A
/// login never goes on a command line: `security -i` reads the command that
/// stores it from a pipe.
#[derive(Debug, Clone, Copy, Default)]
pub struct Keychain;

impl Keychain {
    fn account(service: &str) -> io::Result<String> {
        if service.is_empty()
            || !service
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("`{service}` can't name a Keychain item"),
            ));
        }
        Ok(format!("service-{service}"))
    }

    fn security() -> Command {
        let mut c = Command::new(SECURITY);
        c.stdin(Stdio::null()).stdout(Stdio::piped());
        c.stderr(Stdio::piped());
        c
    }
}

impl SecretStore for Keychain {
    fn load(&self, service: &str) -> io::Result<Option<Secret>> {
        let account = Keychain::account(service)?;
        let out = Keychain::security()
            .args([
                "find-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                &account,
                "-w",
            ])
            .output()?;
        if out.status.code() == Some(NOT_FOUND) {
            return Ok(None);
        }
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "security can't read {service}'s login: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        decode(&String::from_utf8_lossy(&out.stdout)).map(Some)
    }

    fn store(&self, service: &str, secret: &Secret) -> io::Result<()> {
        let account = Keychain::account(service)?;
        // `-U` replaces the item if there is one, as one operation. Base64
        // needs no quoting.
        let line = format!(
            "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {account} -l {KEYCHAIN_SERVICE}-{account} -w {}\n",
            encode(secret)?
        );
        if line.len() > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{service}'s login is too long for the Keychain through security"),
            ));
        }
        let mut child = Command::new(SECURITY)
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(line.as_bytes())?;
        // `security -i` exits 0 whatever its commands did: what it kept is the
        // proof. What it said isn't repeated, as it may quote the command.
        child.wait()?;
        if self.load(service)?.as_ref() != Some(secret) {
            return Err(io::Error::other(format!(
                "the Keychain didn't keep {service}'s login"
            )));
        }
        Ok(())
    }

    fn delete(&self, service: &str) -> io::Result<()> {
        let account = Keychain::account(service)?;
        let out = Keychain::security()
            .args([
                "delete-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                &account,
            ])
            .output()?;
        if out.status.success() || out.status.code() == Some(NOT_FOUND) {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "security can't forget {service}'s login: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// How a Linux host keeps logins: a private directory that only `vibed`'s user
/// can enter, one `0600` file per login. Each is replaced by rename, and synced
/// before and after, so a crash leaves the old login or the new one whole, and
/// once [`SecretStore::store`] returns the new one survives a power cut.
#[derive(Debug, Clone)]
pub struct FileStore(pub PathBuf);

impl FileStore {
    fn file(&self, service: &str) -> io::Result<PathBuf> {
        // It's only ever a name from the registry, but it names a file.
        if service.is_empty()
            || !service
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("`{service}` can't name a login"),
            ));
        }
        Ok(self.0.join(format!("{service}.login")))
    }

    /// The directory, made private if it's new, and refused if it's open to
    /// anyone else, as a login in it wouldn't be private.
    fn ensure_dir(&self) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.0)?;
        let mode = fs::metadata(&self.0)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is open to other users (mode {:o}); run `chmod 700` on it",
                    self.0.display(),
                    mode & 0o777
                ),
            ));
        }
        Ok(())
    }
}

impl SecretStore for FileStore {
    fn load(&self, service: &str) -> io::Result<Option<Secret>> {
        let file = self.file(service)?;
        let meta = match fs::symlink_metadata(&file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be a file only its owner can read; run `chmod 600` on it",
                    file.display()
                ),
            ));
        }
        decode(&fs::read_to_string(&file)?).map(Some)
    }

    fn store(&self, service: &str, secret: &Secret) -> io::Result<()> {
        self.ensure_dir()?;
        let file = self.file(service)?;
        let tmp = file.with_extension("new");
        // Whatever a crash left there isn't to be trusted, its mode included.
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let written = (|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(encode(secret)?.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, &file)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        // The rename itself must last too.
        fs::File::open(&self.0)?.sync_all()
    }

    fn delete(&self, service: &str) -> io::Result<()> {
        match fs::remove_file(self.file(service)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        match fs::File::open(&self.0) {
            Ok(dir) => dir.sync_all(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Logins in memory, for tests.
#[derive(Debug, Default)]
pub struct MemoryStore(Mutex<HashMap<String, Secret>>);

impl SecretStore for MemoryStore {
    fn load(&self, service: &str) -> io::Result<Option<Secret>> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(service)
            .cloned())
    }

    fn store(&self, service: &str, secret: &Secret) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(service.to_owned(), secret.clone());
        Ok(())
    }

    fn delete(&self, service: &str) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(service);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(access: &str) -> Secret {
        Secret {
            auth: ServiceAuth::Oauth,
            access: access.into(),
            refresh: Some("r".into()),
            expires_at: Some(5),
            client_id: Some("c".into()),
            token_endpoint: None,
            revocation_endpoint: None,
            scopes: vec!["read".into()],
        }
    }

    #[test]
    fn a_secrets_debug_shows_no_token() {
        let shown = format!("{:?}", secret("the-access-token"));
        assert!(!shown.contains("the-access-token"), "{shown}");
        assert!(!shown.contains("\"r\""), "{shown}");
    }

    fn mode(path: &std::path::Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_file_store_keeps_logins_private_and_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("services"));
        assert_eq!(store.load("linear").unwrap(), None);
        store.store("linear", &secret("a")).unwrap();
        store.store("linear", &secret("b")).unwrap();
        assert_eq!(store.load("linear").unwrap(), Some(secret("b")));
        assert_eq!(mode(&store.file("linear").unwrap()), 0o600);
        assert_eq!(mode(&store.0), 0o700);
        // Nothing's left beside it.
        let names: Vec<_> = fs::read_dir(&store.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["linear.login"]);
        store.delete("linear").unwrap();
        store.delete("linear").unwrap();
        assert_eq!(store.load("linear").unwrap(), None);
    }

    #[test]
    fn a_crash_mid_write_leaves_the_old_login_and_no_loose_file_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("services"));
        store.store("linear", &secret("old")).unwrap();
        // What a crash leaves: half a new file, as it happens readable by all.
        let tmp = store.file("linear").unwrap().with_extension("new");
        fs::write(&tmp, "half a lo").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.load("linear").unwrap(), Some(secret("old")));
        store.store("linear", &secret("new")).unwrap();
        assert_eq!(store.load("linear").unwrap(), Some(secret("new")));
        assert_eq!(mode(&store.file("linear").unwrap()), 0o600);
    }

    #[test]
    fn a_file_store_wont_keep_or_read_a_login_others_can_reach() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("services"));
        store.store("linear", &secret("a")).unwrap();
        let file = store.file("linear").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.load("linear").unwrap_err();
        assert!(err.to_string().contains("chmod 600"), "{err}");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&store.0, fs::Permissions::from_mode(0o755)).unwrap();
        let err = store.store("linear", &secret("b")).unwrap_err();
        assert!(err.to_string().contains("chmod 700"), "{err}");
    }

    #[test]
    fn a_file_stores_logins_are_only_ever_plainly_named() {
        let store = FileStore(PathBuf::from("/nowhere"));
        for bad in ["", "../x", "a/b", "a b", ".."] {
            assert!(store.load(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_login_that_makes_no_sense_says_nothing_of_itself() {
        let err = decode("c2VjcmV0LXN0dWZm").unwrap_err();
        assert!(!err.to_string().contains("secret-stuff"), "{err}");
    }

    #[test]
    fn keychain_items_are_only_ever_plainly_named() {
        assert_eq!(Keychain::account("linear").unwrap(), "service-linear");
        for bad in ["", "a b", "a;rm", "a\nb", "a/b"] {
            assert!(Keychain::account(bad).is_err(), "{bad:?}");
        }
    }

    // Only a Mac has a Keychain. A long login, near what `security -i` takes,
    // so a limit lower than thought shows up here, not at a user's refresh.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_keychain_keeps_a_login_whole() {
        let name = format!("test-{}", std::process::id());
        let store = Keychain;
        let long = "x".repeat(2800);
        store.store(&name, &secret(&long)).unwrap();
        let kept = store.load(&name);
        let replaced = store.store(&name, &secret("short"));
        let kept2 = store.load(&name);
        store.delete(&name).unwrap();
        assert_eq!(kept.unwrap(), Some(secret(&long)));
        replaced.unwrap();
        assert_eq!(kept2.unwrap(), Some(secret("short")));
        assert_eq!(store.load(&name).unwrap(), None);
    }
}
