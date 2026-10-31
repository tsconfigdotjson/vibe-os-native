//! The disk images a `linux` agent boots, in `~/VibeOS/linux/` (PRD: What's
//! inside `linux`; Exceptions to "set up once, then frozen"):
//!
//! ```text
//! base-<n>/                 Image, initrd and base.img: base release <n>,
//!                           downloaded once per app release
//! managed-<version>.img     the managed layer: Claude Code <version>
//! template-<n>.img          the Linux template, from the toolbox (#10)
//! home-<n>.img              the Linux home, from the toolbox (#10)
//! downloads/                downloads under way
//! ```
//!
//! An image is never changed in place, because a sleeping agent's snapshot
//! depends on the exact disks it was saved with: each new version is a new
//! file, written beside its final name and renamed into place. New agents
//! boot the newest of each; an agent keeps the ones in its `vm.json` until
//! its next fresh boot. Old managed layers go once no agent's `vm.json`
//! names them.
//!
//! Downloads go through `curl`, over HTTPS (or `file://`, for a base image
//! built locally and for tests), and are checked against the checksums
//! their manifest gives before anything is kept.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ext4;
use crate::vm::VmConfig;

/// Where Claude Code's releases are published, as its own installer
/// fetches them.
pub const CLAUDE_RELEASES: &str = "https://downloads.claude.ai/claude-code-releases";

/// The build the managed layer takes: Linux on arm64, for the Debian base
/// (glibc), as the managed layer's install verifies it.
pub const CLAUDE_PLATFORM: &str = "linux-arm64";

/// Where the managed layer puts Claude Code: `vibe-guest` puts this
/// directory first on `PATH`.
pub const MANAGED_BIN: &str = "opt/vibeos/bin";

/// The base release this build of VibeOS boots: a new app release may ship
/// a new one (PRD: Base image updates).
pub const BASE_RELEASE: u32 = 1;

/// Where base releases are published, each in `base-<n>/`, as
/// `scripts/base-image.sh` makes them.
pub const BASE_RELEASES: &str =
    "https://github.com/tsconfigdotjson/vibe-os-native/releases/download";

/// The URL of base release `n` under `releases`.
pub fn base_url(releases: &str, n: u32) -> String {
    format!("{}/base-{n}", releases.trim_end_matches('/'))
}

/// The base image's manifest, beside its files.
pub const BASE_MANIFEST: &str = "base.json";

/// The files of a base release, as they're kept.
pub const KERNEL: &str = "Image";
pub const INITRD: &str = "initrd";
pub const BASE_DISK: &str = "base.img";

/// Why an image couldn't be had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageError(pub String);

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImageError {}

impl From<ImageError> for crate::EnvError {
    fn from(e: ImageError) -> Self {
        crate::EnvError::new(e.0)
    }
}

fn err(message: impl Into<String>) -> ImageError {
    ImageError(message.into())
}

fn io_err(what: impl fmt::Display) -> impl FnOnce(io::Error) -> ImageError {
    move |e| err(format!("{what}: {e}"))
}

pub type Result<T> = std::result::Result<T, ImageError>;

/// A base release, installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub version: u32,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub disk: PathBuf,
}

/// The newest of each layer: what a fresh boot takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layers {
    pub base: Base,
    pub template: Option<PathBuf>,
    /// Claude Code's version, and its layer.
    pub managed: Option<(String, PathBuf)>,
    pub home: Option<PathBuf>,
}

/// The images in one directory, `~/VibeOS/linux/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Images {
    dir: PathBuf,
}

impl Images {
    pub fn new(dir: impl Into<PathBuf>) -> Images {
        Images { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The newest base release installed, if any.
    pub fn base(&self) -> Option<Base> {
        self.versions("base-", "")
            .into_iter()
            .filter_map(|(n, _)| self.installed_base(n))
            .next_back()
    }

    fn installed_base(&self, version: u32) -> Option<Base> {
        let dir = self.dir.join(format!("base-{version}"));
        let base = Base {
            version,
            kernel: dir.join(KERNEL),
            initrd: dir.join(INITRD),
            disk: dir.join(BASE_DISK),
        };
        (base.kernel.is_file() && base.initrd.is_file() && base.disk.is_file()).then_some(base)
    }

    /// The newest managed layer: Claude Code's version and the image.
    pub fn managed(&self) -> Option<(String, PathBuf)> {
        self.managed_all().pop()
    }

    /// Every managed layer, oldest first.
    fn managed_all(&self) -> Vec<(String, PathBuf)> {
        let mut found: Vec<(Version, String, PathBuf)> = self
            .names()
            .into_iter()
            .filter_map(|name| {
                let v = name.strip_prefix("managed-")?.strip_suffix(".img")?;
                let parsed = Version::parse(v)?;
                Some((parsed, v.to_owned(), self.dir.join(&name)))
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, v, p)| (v, p)).collect()
    }

    /// The newest `template-<n>.img` or `home-<n>.img`.
    fn newest(&self, kind: &str) -> Option<PathBuf> {
        self.versions(&format!("{kind}-"), ".img")
            .pop()
            .map(|(_, p)| p)
    }

    /// The newest of each layer, or why there's no base to boot.
    pub fn layers(&self) -> Result<Layers> {
        let base = self.base().ok_or_else(|| {
            err(format!(
                "no Linux base image in {} yet: vibed downloads it when it starts",
                self.dir.display()
            ))
        })?;
        Ok(Layers {
            base,
            template: self.newest("template"),
            managed: self.managed(),
            home: self.newest("home"),
        })
    }

    fn names(&self) -> Vec<String> {
        fs::read_dir(&self.dir)
            .map(|d| {
                d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `<prefix><n><suffix>` entries, by `n`, oldest first.
    fn versions(&self, prefix: &str, suffix: &str) -> Vec<(u32, PathBuf)> {
        let mut found: Vec<(u32, PathBuf)> = self
            .names()
            .into_iter()
            .filter_map(|name| {
                let n = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
                // Digits only: no sign, no leading zero to make two names
                // for one version.
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || n.starts_with('0') {
                    return None;
                }
                Some((n.parse().ok()?, self.dir.join(&name)))
            })
            .collect();
        found.sort();
        found
    }

    fn make_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(io_err(format!("create {}", self.dir.display())))
    }

    /// A fresh scratch directory for a download, emptied of any that a
    /// stopped `vibed` left.
    fn downloads(&self) -> Result<PathBuf> {
        let dir = self.dir.join("downloads");
        match fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(err(format!("clear {}: {e}", dir.display())));
            }
            _ => {}
        }
        fs::create_dir_all(&dir).map_err(io_err(format!("create {}", dir.display())))?;
        Ok(dir)
    }

    /// Installs base release `version` from `url`, a directory holding its
    /// [`BASE_MANIFEST`] and the files it names, unless it's installed
    /// already. Every file is checked against the manifest's size and
    /// SHA-256 before the release is kept.
    pub fn install_base(&self, url: &str, version: u32) -> Result<Base> {
        if let Some(base) = self.installed_base(version) {
            return Ok(base);
        }
        self.make_dir()?;
        let scratch = self.downloads()?;
        let url = url.trim_end_matches('/');
        let manifest_path = scratch.join(BASE_MANIFEST);
        fetch(&format!("{url}/{BASE_MANIFEST}"), &manifest_path)?;
        let manifest: BaseManifest = serde_json::from_slice(
            &fs::read(&manifest_path).map_err(io_err("read the base manifest"))?,
        )
        .map_err(|e| err(format!("{url}/{BASE_MANIFEST} isn't a base manifest: {e}")))?;
        if manifest.version != version {
            return Err(err(format!(
                "{url} has base release {}, not {version}",
                manifest.version
            )));
        }
        let staged = self.dir.join(format!(".base-{version}.partial"));
        let _ = fs::remove_dir_all(&staged);
        fs::create_dir_all(&staged).map_err(io_err(format!("create {}", staged.display())))?;
        let result = (|| {
            for (name, keep) in [(KERNEL, KERNEL), (INITRD, INITRD)] {
                let file = manifest.file(name)?;
                let got = scratch.join(name);
                fetch(&format!("{url}/{name}"), &got)?;
                file.check(name, &got)?;
                fs::rename(&got, staged.join(keep)).map_err(io_err(format!("keep {name}")))?;
            }
            let disk = staged.join(BASE_DISK);
            if let Some(file) = manifest.files.get("base.img.gz") {
                let got = scratch.join("base.img.gz");
                fetch(&format!("{url}/base.img.gz"), &got)?;
                file.check("base.img.gz", &got)?;
                gunzip_sparse(&got, &disk)?;
                let _ = fs::remove_file(&got);
            } else {
                let file = manifest.file(BASE_DISK)?;
                let got = scratch.join(BASE_DISK);
                fetch(&format!("{url}/{BASE_DISK}"), &got)?;
                file.check(BASE_DISK, &got)?;
                fs::rename(&got, &disk).map_err(io_err("keep base.img"))?;
            }
            let mut head = vec![0u8; 2048];
            File::open(&disk)
                .and_then(|f| f.read_exact_at(&mut head, 0))
                .map_err(io_err("read base.img"))?;
            if !ext4::is_ext4(&head) {
                return Err(err("base.img isn't an ext4 image"));
            }
            readonly(&staged.join(KERNEL))?;
            readonly(&staged.join(INITRD))?;
            readonly(&disk)?;
            let dest = self.dir.join(format!("base-{version}"));
            fs::rename(&staged, &dest).map_err(io_err(format!("keep {}", dest.display())))?;
            Ok(())
        })();
        let _ = fs::remove_dir_all(&scratch);
        if result.is_err() {
            let _ = fs::remove_dir_all(&staged);
        }
        result?;
        self.installed_base(version).ok_or_else(|| {
            err(format!(
                "base release {version} vanished as it was installed"
            ))
        })
    }

    /// Checks `releases` (normally [`CLAUDE_RELEASES`]) for a Claude Code
    /// newer than the newest managed layer, and if there is one, downloads
    /// its [`CLAUDE_PLATFORM`] build, checks it against the release
    /// manifest's size and SHA-256, and makes it a new layer. Returns the
    /// new version, or `None` if the newest is already a layer.
    pub fn update_managed(&self, releases: &str) -> Result<Option<String>> {
        let releases = releases.trim_end_matches('/');
        self.make_dir()?;
        let scratch = self.downloads()?;
        let result = (|| {
            let latest = scratch.join("latest");
            fetch(&format!("{releases}/latest"), &latest)?;
            let version = fs::read_to_string(&latest)
                .map_err(io_err("read the latest version"))?
                .trim()
                .to_owned();
            if Version::parse(&version).is_none() {
                return Err(err(format!(
                    "{releases}/latest isn't a version: {:?}",
                    version.chars().take(40).collect::<String>()
                )));
            }
            if let Some((newest, _)) = self.managed()
                && Version::parse(&newest) >= Version::parse(&version)
            {
                return Ok(None);
            }
            let manifest_path = scratch.join("manifest.json");
            fetch(
                &format!("{releases}/{version}/manifest.json"),
                &manifest_path,
            )?;
            let manifest: ClaudeManifest = serde_json::from_slice(
                &fs::read(&manifest_path).map_err(io_err("read Claude's manifest"))?,
            )
            .map_err(|e| err(format!("Claude {version}'s manifest can't be read: {e}")))?;
            if manifest.version != version {
                return Err(err(format!(
                    "Claude {version}'s manifest is for {}",
                    manifest.version
                )));
            }
            let file = manifest
                .platforms
                .get(CLAUDE_PLATFORM)
                .ok_or_else(|| err(format!("Claude {version} has no {CLAUDE_PLATFORM} build")))?;
            let binary = scratch.join("claude");
            fetch(
                &format!("{releases}/{version}/{CLAUDE_PLATFORM}/claude"),
                &binary,
            )?;
            FileSum {
                sha256: file.checksum.clone(),
                size: file.size,
            }
            .check(&format!("Claude {version}"), &binary)?;
            self.add_managed(&version, &binary, &file.checksum)?;
            Ok(Some(version))
        })();
        let _ = fs::remove_dir_all(&scratch);
        result
    }

    /// Makes `binary`, Claude Code `version` (already checked against
    /// `sha256`), a managed layer: `/opt/vibeos/bin/claude`, with a note of
    /// what it is beside it. Returns the layer.
    pub fn add_managed(&self, version: &str, binary: &Path, sha256: &str) -> Result<PathBuf> {
        if Version::parse(version).is_none() {
            return Err(err(format!("`{version}` isn't a Claude Code version")));
        }
        self.make_dir()?;
        let dest = self.dir.join(format!("managed-{version}.img"));
        if dest.exists() {
            return Ok(dest);
        }
        let partial = self.dir.join(format!(
            ".managed-{version}.img.{}.partial",
            std::process::id()
        ));
        let _ = fs::remove_file(&partial);
        let note = serde_json::json!({
            "claude": version,
            "platform": CLAUDE_PLATFORM,
            "sha256": sha256,
        });
        let mut tree = ext4::Tree::new();
        tree.file(
            &format!("{MANAGED_BIN}/claude"),
            ext4::Source::Path(binary.to_owned()),
            0o755,
        )
        .file(
            "opt/vibeos/managed.json",
            ext4::Source::Bytes(format!("{note}\n").into_bytes()),
            0o644,
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs().min(u64::from(u32::MAX)) as u32);
        let built = ext4::write(&tree, &partial, "managed", now)
            .map_err(io_err(format!("build the layer for Claude {version}")))
            .and_then(|_| readonly(&partial))
            .and_then(|()| {
                fs::rename(&partial, &dest).map_err(io_err(format!("keep {}", dest.display())))
            });
        if built.is_err() {
            let _ = fs::remove_file(&partial);
        }
        built.map(|()| dest)
    }

    /// Removes every managed layer but the newest that no path in `in_use`
    /// names, and anything a stopped build left. Returns what went.
    pub fn prune_managed(&self, in_use: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let mut all = self.managed_all();
        all.pop();
        let mut gone = Vec::new();
        for (_, path) in all {
            if !in_use.contains(&path) && fs::remove_file(&path).is_ok() {
                gone.push(path);
            }
        }
        for name in self.names() {
            if name.starts_with(".managed-") && name.ends_with(".partial") {
                let path = self.dir.join(&name);
                // Only one left by a vibed that's gone: its pid is in the name.
                let pid = name
                    .trim_end_matches(".partial")
                    .rsplit('.')
                    .next()
                    .and_then(|p| p.parse::<u32>().ok());
                if pid != Some(std::process::id()) && fs::remove_file(&path).is_ok() {
                    gone.push(path);
                }
            }
        }
        gone
    }
}

/// Every disk image the agents in `agents_dir` boot with, by their
/// `vm.json`: what a sleeping agent's snapshot depends on.
pub fn disks_in_use(agents_dir: &Path) -> HashSet<PathBuf> {
    let mut used = HashSet::new();
    let Ok(dirs) = fs::read_dir(agents_dir) else {
        return used;
    };
    for dir in dirs.filter_map(|e| e.ok()) {
        let Ok(text) = fs::read_to_string(dir.path().join("vm.json")) else {
            continue;
        };
        match serde_json::from_str::<VmConfig>(&text) {
            Ok(config) => used.extend(config.disks.into_iter().map(|d| d.path)),
            // One that can't be read keeps every layer: better too much
            // disk than a snapshot that can't restore.
            Err(_) => {
                used.insert(PathBuf::from("*"));
            }
        }
    }
    used
}

impl Images {
    /// [`Images::prune_managed`] for the agents in `agents_dir`; nothing goes
    /// if any of their `vm.json` can't be read.
    pub fn prune_unused(&self, agents_dir: &Path) -> Vec<PathBuf> {
        let used = disks_in_use(agents_dir);
        if used.contains(Path::new("*")) {
            return Vec::new();
        }
        self.prune_managed(&used)
    }
}

/// A file's size and SHA-256, as a manifest gives them.
#[derive(Debug, Clone, Deserialize)]
struct FileSum {
    sha256: String,
    size: u64,
}

impl FileSum {
    fn check(&self, what: &str, path: &Path) -> Result<()> {
        let size = fs::metadata(path).map_err(io_err(what))?.len();
        if size != self.size {
            return Err(err(format!(
                "{what} is {size} bytes, but its manifest says {}",
                self.size
            )));
        }
        let got = sha256_file(path).map_err(io_err(what))?;
        if !got.eq_ignore_ascii_case(&self.sha256) {
            return Err(err(format!(
                "{what} has SHA-256 {got}, but its manifest says {}",
                self.sha256
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct BaseManifest {
    version: u32,
    files: std::collections::BTreeMap<String, FileSum>,
}

impl BaseManifest {
    fn file(&self, name: &str) -> Result<&FileSum> {
        self.files
            .get(name)
            .ok_or_else(|| err(format!("the base manifest has no {name}")))
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeManifest {
    version: String,
    platforms: std::collections::BTreeMap<String, ClaudeFile>,
}

#[derive(Debug, Deserialize)]
struct ClaudeFile {
    checksum: String,
    size: u64,
}

/// A Claude Code version, `1.2.3` or `1.2.3-tag`, ordered as releases are.
/// Nothing else is ever a file name here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    numbers: [u64; 3],
    /// A release without a tag sorts after its pre-releases.
    release: bool,
    tag: String,
}

impl Version {
    fn parse(s: &str) -> Option<Version> {
        let (core, tag) = match s.split_once('-') {
            Some((core, tag)) => (core, Some(tag)),
            None => (s, None),
        };
        let mut numbers = [0u64; 3];
        let mut parts = core.split('.');
        for n in &mut numbers {
            let part = parts.next()?;
            if part.is_empty() || part.len() > 9 || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *n = part.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        if let Some(tag) = tag
            && (tag.is_empty()
                || tag.len() > 40
                || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
        {
            return None;
        }
        Some(Version {
            numbers,
            release: tag.is_none(),
            tag: tag.unwrap_or_default().to_owned(),
        })
    }
}

/// Downloads `url` to `dest` with `curl`, over HTTPS or from a local file.
fn fetch(url: &str, dest: &Path) -> Result<()> {
    let out = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "--proto", "=https,file", "-o"])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(io_err("run curl"))?;
    if out.status.success() {
        return Ok(());
    }
    let _ = fs::remove_file(dest);
    Err(err(format!(
        "can't download {url}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn readonly(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))
        .map_err(io_err(format!("make {} read-only", path.display())))
}

/// Unpacks a gzip file with `gzip -dc`, writing only blocks that aren't all
/// zeros, so an ext4 image stays as sparse as it was made.
fn gunzip_sparse(src: &Path, dest: &Path) -> Result<()> {
    let mut child = Command::new("gzip")
        .arg("-dc")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_err("run gzip"))?;
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(io_err(format!("create {}", dest.display())))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut buf = vec![0u8; ext4::BLOCK as usize];
    let mut at = 0u64;
    let copied = (|| -> io::Result<()> {
        loop {
            let mut got = 0;
            while got < buf.len() {
                match stdout.read(&mut buf[got..])? {
                    0 => break,
                    n => got += n,
                }
            }
            if got == 0 {
                break;
            }
            if buf[..got].iter().any(|&b| b != 0) {
                out.write_all_at(&buf[..got], at)?;
            }
            at += got as u64;
            if got < buf.len() {
                break;
            }
        }
        out.set_len(at)?;
        out.sync_all()
    })();
    let status = child.wait().map_err(io_err("wait for gzip"))?;
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let result = copied
        .map_err(io_err(format!("unpack {}", src.display())))
        .and_then(|()| {
            if status.success() {
                Ok(())
            } else {
                Err(err(format!(
                    "can't unpack {}: {}",
                    src.display(),
                    stderr.trim()
                )))
            }
        });
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A release directory for `file://` downloads, as Claude's and a base
    /// image's are laid out.
    struct Releases {
        dir: tempfile::TempDir,
    }

    impl Releases {
        fn new() -> Releases {
            Releases {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn url(&self) -> String {
            format!("file://{}", self.dir.path().display())
        }

        /// Publishes Claude `version` with `binary` as its linux-arm64 build;
        /// the manifest says `size` and `sum` if given, else the truth.
        fn claude(&self, version: &str, binary: &[u8], lie: Option<(u64, &str)>) {
            let dir = self.dir.path().join(version);
            fs::create_dir_all(dir.join(CLAUDE_PLATFORM)).unwrap();
            fs::write(dir.join(CLAUDE_PLATFORM).join("claude"), binary).unwrap();
            let truth = hex(&Sha256::digest(binary));
            let (size, sum) = lie.unwrap_or((binary.len() as u64, &truth));
            let manifest = serde_json::json!({
                "version": version,
                "platforms": {
                    CLAUDE_PLATFORM: {"binary": "claude", "checksum": sum, "size": size},
                    "darwin-arm64": {"binary": "claude", "checksum": "00", "size": 1},
                },
            });
            fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
            fs::write(self.dir.path().join("latest"), format!("{version}\n")).unwrap();
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn e2fsck_cat(image: &Path, path: &str) -> Option<Vec<u8>> {
        let debugfs = ["/sbin/debugfs", "/usr/sbin/debugfs"]
            .into_iter()
            .find(|p| Path::new(p).exists())?;
        let out = Command::new(debugfs)
            .args(["-R", &format!("cat {path}")])
            .arg(image)
            .output()
            .unwrap();
        Some(out.stdout)
    }

    #[test]
    fn versions_order_as_releases_do() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("2.1.314") > v("2.1.308"));
        assert!(v("2.10.0") > v("2.9.99"));
        assert!(v("2.1.0") > v("2.1.0-beta.1"));
        assert!(v("2.1.0-beta.2") > v("2.1.0-beta.1"));
        for bad in [
            "",
            "2.1",
            "2.1.3.4",
            "2.1.x",
            "-1.2.3",
            "2.1.3-",
            "2.1.3-a/b",
            "../2.1.3",
            "2.1.3\n",
            "<html>",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_new_claude_release_becomes_a_new_layer() {
        let releases = Releases::new();
        let tmp = tempfile::tempdir().unwrap();
        let images = Images::new(tmp.path().join("linux"));
        assert_eq!(images.managed(), None);

        let first = b"#!/bin/sh\necho claude 2.1.308\n".repeat(1000);
        releases.claude("2.1.308", &first, None);
        assert_eq!(
            images.update_managed(&releases.url()).unwrap().as_deref(),
            Some("2.1.308")
        );
        let (version, layer) = images.managed().unwrap();
        assert_eq!(version, "2.1.308");
        assert_eq!(layer, images.dir().join("managed-2.1.308.img"));
        if let Some(got) = e2fsck_cat(&layer, "/opt/vibeos/bin/claude") {
            assert_eq!(got, first);
            let note = e2fsck_cat(&layer, "/opt/vibeos/managed.json").unwrap();
            assert!(
                String::from_utf8(note)
                    .unwrap()
                    .contains("\"claude\":\"2.1.308\"")
            );
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&layer).unwrap().permissions().mode() & 0o777,
            0o444,
            "an image is never changed in place"
        );

        // Nothing new: nothing done.
        assert_eq!(images.update_managed(&releases.url()).unwrap(), None);

        // A new release is a new layer beside the old, for the next boot.
        let second = b"claude 2.1.314".to_vec();
        releases.claude("2.1.314", &second, None);
        assert_eq!(
            images.update_managed(&releases.url()).unwrap().as_deref(),
            Some("2.1.314")
        );
        assert_eq!(images.managed().unwrap().0, "2.1.314");
        assert!(layer.exists(), "the old layer stays until it's unused");
        // Nothing of the downloads is left.
        let mut names = images.names();
        names.sort();
        assert_eq!(names, ["managed-2.1.308.img", "managed-2.1.314.img"]);

        // An older "latest" (a rollback upstream) doesn't replace a newer layer.
        releases.claude("2.1.300", b"old", None);
        assert_eq!(images.update_managed(&releases.url()).unwrap(), None);
    }

    #[test]
    fn a_download_that_doesnt_match_its_manifest_is_refused() {
        let releases = Releases::new();
        let tmp = tempfile::tempdir().unwrap();
        let images = Images::new(tmp.path().join("linux"));
        releases.claude("2.1.400", b"tampered", Some((8, &"ab".repeat(32))));
        let e = images.update_managed(&releases.url()).unwrap_err();
        assert!(e.0.contains("SHA-256"), "{e}");
        releases.claude("2.1.401", b"short", Some((99, "x")));
        let e = images.update_managed(&releases.url()).unwrap_err();
        assert!(e.0.contains("bytes"), "{e}");
        assert_eq!(images.managed(), None);
        assert_eq!(images.names(), Vec::<String>::new(), "nothing kept");

        // No linux-arm64 build, a manifest for another version, or a
        // "latest" that isn't a version: refused too.
        let dir = releases.dir.path();
        fs::write(dir.join("latest"), "<html>busy</html>").unwrap();
        assert!(
            images
                .update_managed(&releases.url())
                .unwrap_err()
                .0
                .contains("isn't a version")
        );
        fs::create_dir_all(dir.join("2.1.402")).unwrap();
        fs::write(dir.join("latest"), "2.1.402").unwrap();
        fs::write(
            dir.join("2.1.402/manifest.json"),
            r#"{"version":"2.1.402","platforms":{}}"#,
        )
        .unwrap();
        assert!(
            images
                .update_managed(&releases.url())
                .unwrap_err()
                .0
                .contains("no linux-arm64")
        );
        fs::write(
            dir.join("2.1.402/manifest.json"),
            r#"{"version":"2.1.9","platforms":{}}"#,
        )
        .unwrap();
        assert!(
            images
                .update_managed(&releases.url())
                .unwrap_err()
                .0
                .contains("is for 2.1.9")
        );
        // And a release that isn't there at all.
        assert!(images.update_managed("file:///nonexistent/dir").is_err());
    }

    #[test]
    fn layers_are_pruned_once_no_agent_uses_them() {
        let tmp = tempfile::tempdir().unwrap();
        let images = Images::new(tmp.path().join("linux"));
        let bin = tmp.path().join("claude");
        fs::write(&bin, "c").unwrap();
        let old = images.add_managed("2.1.1", &bin, "x").unwrap();
        let used = images.add_managed("2.1.2", &bin, "x").unwrap();
        let newest = images.add_managed("2.1.3", &bin, "x").unwrap();
        // Adding one again keeps it as it is.
        assert_eq!(images.add_managed("2.1.3", &bin, "x").unwrap(), newest);
        fs::write(images.dir().join(".managed-2.1.9.img.1.partial"), "").unwrap();

        // An agent asleep on 2.1.2.
        let agents = tmp.path().join("agents");
        let agent = agents.join("quiet-amber-otter");
        fs::create_dir_all(&agent).unwrap();
        let config = crate::vm::VmConfig {
            version: crate::vm::CONFIG_VERSION,
            agent: "quiet-amber-otter".into(),
            cpus: 1,
            memory: 1 << 30,
            kernel: "/k".into(),
            initrd: None,
            cmdline: String::new(),
            machine_id: None,
            mac: None,
            disks: vec![crate::vm::Disk {
                id: "managed".into(),
                path: used.clone(),
                read_only: true,
            }],
            shares: vec![],
            console_log: None,
            snapshot: "/s".into(),
        };
        config.save(&agent.join("vm.json")).unwrap();
        let gone = images.prune_unused(&agents);
        assert!(gone.contains(&old), "{gone:?}");
        assert!(!old.exists());
        assert!(used.exists() && newest.exists());
        assert!(!images.dir().join(".managed-2.1.9.img.1.partial").exists());

        // A vm.json that can't be read keeps everything.
        fs::write(agent.join("vm.json"), "{").unwrap();
        assert!(images.prune_unused(&agents).is_empty());
        assert!(used.exists());
    }

    /// Publishes base release `version` in `dir`, with a real ext4 image.
    fn publish_base(dir: &Path, version: u32, gzip: bool) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(KERNEL), b"ARM64 kernel").unwrap();
        fs::write(dir.join(INITRD), b"initrd").unwrap();
        let img = dir.join("base.img");
        let mut tree = ext4::Tree::new();
        tree.file(
            "etc/debian_version",
            ext4::Source::Bytes(b"13\n".to_vec()),
            0o644,
        );
        ext4::write(&tree, &img, "base", 1).unwrap();
        let disk_name = if gzip {
            let out = Command::new("gzip").arg("-k").arg(&img).status().unwrap();
            assert!(out.success());
            fs::remove_file(&img).unwrap();
            "base.img.gz"
        } else {
            "base.img"
        };
        let mut files = serde_json::Map::new();
        for name in [KERNEL, INITRD, disk_name] {
            let p = dir.join(name);
            files.insert(
                name.into(),
                serde_json::json!({
                    "sha256": sha256_file(&p).unwrap(),
                    "size": fs::metadata(&p).unwrap().len(),
                }),
            );
        }
        let manifest = serde_json::json!({"version": version, "files": files});
        fs::write(dir.join(BASE_MANIFEST), manifest.to_string()).unwrap();
    }

    #[test]
    fn a_base_release_installs_once_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let images = Images::new(tmp.path().join("linux"));
        assert!(
            images
                .layers()
                .unwrap_err()
                .0
                .contains("no Linux base image")
        );
        let release = tmp.path().join("release-1");
        publish_base(&release, 1, true);
        let url = format!("file://{}", release.display());
        let base = images.install_base(&url, 1).unwrap();
        assert_eq!(base.version, 1);
        assert_eq!(fs::read(&base.kernel).unwrap(), b"ARM64 kernel");
        let mut head = vec![0u8; 2048];
        File::open(&base.disk)
            .unwrap()
            .read_exact_at(&mut head, 0)
            .unwrap();
        assert!(ext4::is_ext4(&head));
        if let Some(v) = e2fsck_cat(&base.disk, "/etc/debian_version") {
            assert_eq!(v, b"13\n");
        }
        let layers = images.layers().unwrap();
        assert_eq!(layers.base, base);
        assert_eq!(
            (layers.template, layers.managed, layers.home),
            (None, None, None)
        );
        // Installed, it isn't fetched again.
        assert_eq!(images.install_base("file:///nowhere", 1).unwrap(), base);

        // Template and home versions from the toolbox are found, newest first.
        for name in [
            "template-2.img",
            "template-10.img",
            "home-1.img",
            "template-01.img",
        ] {
            fs::write(images.dir().join(name), "").unwrap();
        }
        let layers = images.layers().unwrap();
        assert_eq!(layers.template, Some(images.dir().join("template-10.img")));
        assert_eq!(layers.home, Some(images.dir().join("home-1.img")));

        // A newer release, uncompressed, is the base new agents get.
        let release2 = tmp.path().join("release-2");
        publish_base(&release2, 2, false);
        images
            .install_base(&format!("file://{}", release2.display()), 2)
            .unwrap();
        assert_eq!(images.base().unwrap().version, 2);
    }

    #[test]
    fn a_bad_base_release_is_refused_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let images = Images::new(tmp.path().join("linux"));
        let release = tmp.path().join("r");
        publish_base(&release, 3, true);
        let url = format!("file://{}", release.display());
        // The wrong version.
        assert!(
            images
                .install_base(&url, 4)
                .unwrap_err()
                .0
                .contains("not 4")
        );
        // A file changed after the manifest was made.
        let mut f = OpenOptions::new()
            .append(true)
            .open(release.join(INITRD))
            .unwrap();
        f.write_all(b"!").unwrap();
        let e = images.install_base(&url, 3).unwrap_err();
        assert!(e.0.contains("initrd"), "{e}");
        assert_eq!(images.base(), None);
        assert_eq!(images.names(), Vec::<String>::new());
    }

    #[test]
    fn gunzip_keeps_images_sparse() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = tmp.path().join("raw");
        let f = File::create(&raw).unwrap();
        f.set_len(64 << 20).unwrap();
        f.write_all_at(b"data", 10 << 20).unwrap();
        f.write_all_at(b"tail", (64 << 20) - 4).unwrap();
        drop(f);
        assert!(
            Command::new("gzip")
                .arg("-k")
                .arg(&raw)
                .status()
                .unwrap()
                .success()
        );
        let out = tmp.path().join("out");
        gunzip_sparse(&tmp.path().join("raw.gz"), &out).unwrap();
        assert_eq!(sha256_file(&out).unwrap(), sha256_file(&raw).unwrap());
        use std::os::unix::fs::MetadataExt;
        assert!(fs::metadata(&out).unwrap().blocks() * 512 < 1 << 20);
        // A broken archive leaves nothing.
        fs::write(tmp.path().join("bad.gz"), "not gzip").unwrap();
        assert!(gunzip_sparse(&tmp.path().join("bad.gz"), &tmp.path().join("bad")).is_err());
        assert!(!tmp.path().join("bad").exists());
    }
}
