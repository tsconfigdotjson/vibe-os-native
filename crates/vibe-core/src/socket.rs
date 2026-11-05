//! Binding a Unix socket safely: in a private directory, with a lock beside
//! it so only one process serves it, replacing a socket left behind by one
//! that died. `vibed` serves its API this way, and each `vibe-shim` its
//! session.

use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum BindError {
    /// The socket's directory is open to other users.
    InsecureDir { dir: PathBuf, mode: u32 },
    /// The socket's directory belongs to another user, who could swap the
    /// socket.
    NotOwned { dir: PathBuf, uid: u32 },
    /// Another process holds the lock for this socket.
    AlreadyRunning { path: PathBuf },
    /// Something other than a socket is at the socket's path.
    NotASocket { path: PathBuf },
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindError::InsecureDir { dir, mode } => write!(
                f,
                "{} is open to other users (mode {mode:o}); run `chmod 700` on it",
                dir.display()
            ),
            BindError::NotOwned { dir, uid } => {
                write!(f, "{} belongs to uid {uid}, not this user", dir.display())
            }
            BindError::AlreadyRunning { path } => {
                write!(f, "another process is already serving {}", path.display())
            }
            BindError::NotASocket { path } => {
                write!(
                    f,
                    "{} exists and isn't a socket; not touching it",
                    path.display()
                )
            }
            BindError::Io { what, path, source } => {
                write!(f, "can't {what} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for BindError {}

/// Holds the socket's lock for as long as the server runs, and removes the
/// socket when dropped.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    _lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The lock file that makes one server per socket: `<socket>.lock`.
pub fn lock_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Whether a process holds the lock beside `socket`, as whatever's serving it
/// does, or why that can't be told.
pub fn lock_state(socket: &Path) -> io::Result<bool> {
    let lock = match OpenOptions::new().write(true).open(lock_path(socket)) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match lock.try_lock() {
        // Released as `lock` drops.
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// [`lock_state`], for deciding what to do to a socket: if it can't be told, the
/// lock is taken as held, so nothing is removed or replaced on a guess.
pub fn lock_held(socket: &Path) -> bool {
    lock_state(socket).unwrap_or(true)
}

/// Binds the socket at `path`, readable and writable only by this user.
///
/// The directory is created `0700` if missing, and refused if it's open to
/// other users or owned by one, since the socket is as good as a shell. A
/// lock next to the socket keeps a second server from stealing it; with the
/// lock held, any socket left at `path` is stale and is replaced.
pub fn bind(path: &Path) -> Result<(std::os::unix::net::UnixListener, SocketGuard), BindError> {
    let io_err = |what, path: &Path| {
        let path = path.to_owned();
        move |source| BindError::Io { what, path, source }
    };
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    match fs::metadata(dir) {
        // Not one to judge by its owner and mode, which would have the owner
        // `chmod` a file for nothing.
        Ok(meta) if !meta.is_dir() => {
            return Err(io_err("use", dir)(io::Error::from_raw_os_error(
                libc::ENOTDIR,
            )));
        }
        Ok(meta) => {
            // SAFETY: geteuid has no preconditions and can't fail.
            let me = unsafe { libc::geteuid() };
            if meta.uid() != me {
                return Err(BindError::NotOwned {
                    dir: dir.to_owned(),
                    uid: meta.uid(),
                });
            }
            let mode = meta.permissions().mode() & 0o7777;
            if mode & 0o077 != 0 {
                return Err(BindError::InsecureDir {
                    dir: dir.to_owned(),
                    mode,
                });
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_err("create", dir))?,
        Err(e) => return Err(io_err("read", dir)(e)),
    }

    let lock_path = lock_path(path);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(io_err("open", &lock_path))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(BindError::AlreadyRunning {
                path: path.to_owned(),
            });
        }
        Err(TryLockError::Error(e)) => return Err(io_err("lock", &lock_path)(e)),
    }

    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            fs::remove_file(path).map_err(io_err("remove the stale socket", path))?
        }
        Ok(_) => {
            return Err(BindError::NotASocket {
                path: path.to_owned(),
            });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("read", path)(e)),
    }
    let listener = std::os::unix::net::UnixListener::bind(path).map_err(io_err("bind", path))?;
    let guard = SocketGuard {
        path: path.to_owned(),
        _lock: lock,
    };
    fs::set_permissions(path, Permissions::from_mode(0o600)).map_err(io_err("chmod", path))?;
    listener
        .set_nonblocking(true)
        .map_err(io_err("configure", path))?;
    Ok((listener, guard))
}
