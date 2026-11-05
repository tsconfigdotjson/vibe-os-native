//! Pi's releases (PRD: What's inside `linux`; Adding a Linux host): the
//! standalone builds on GitHub, which need no Node and no Homebrew. Each is a
//! directory, `pi/`, holding the binary and the assets it needs beside it.
//! Everything is checked against the release's `SHA256SUMS` before it's used.
//!
//! `curl` sets no quarantine flag on a Mac, so an ad-hoc signed build runs as
//! the owner's own `curl` download of it does.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::images::{ImageError, Result, Version, fetch_bounded as fetch, sha256_file};

/// Where Pi's releases are published.
pub const RELEASES: &str = "https://github.com/earendil-works/pi/releases";

/// The Pi this build of VibeOS runs and installs: not the newest, which could
/// change what its agent dir holds, its logins, its transcripts or its screen
/// before any of that is checked. A new one is a new release of VibeOS, after
/// `scripts/check-full.sh`'s `pi-e2e` and a fresh look at what it reads and runs
/// (PRD: Who gets which credentials).
pub const PINNED: &str = "1.5.0";

/// The `fd` the Pi layer has, as Pi would otherwise download one to `bin/` in
/// its agent dir, which a VM can't write. Pinned, with its release's hash, as
/// that publishes no sums.
pub const FD_VERSION: &str = "10.3.0";
const FD_SHA256: &str = "66f297e404400a3358e9a0c0b2f3f4725956e7e4435427a9ae56e22adbe73a68";
const FD_RELEASES: &str = "https://github.com/sharkdp/fd/releases";

/// The build `linux` VMs run: arm64 on the Debian base (glibc).
pub const VM_PLATFORM: &str = "linux-arm64";

/// The build for the machine this runs on, if Pi publishes one.
pub fn platform_here() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        _ => None,
    }
}

fn err(message: impl Into<String>) -> ImageError {
    ImageError(message.into())
}

/// What `SHA256SUMS` says of `name`.
pub(crate) fn sum_of<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let (sum, file) = l.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then_some(sum.trim())
    })
}

/// A build, downloaded and checked, unpacked in `scratch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub version: String,
    /// The `pi/` directory: the binary and its assets.
    pub dir: PathBuf,
    /// The tarball's SHA-256.
    pub sha256: String,
    /// `fd` for the layer, for a VM's build.
    pub fd: Option<PathBuf>,
}

/// Downloads `platform`'s build of `version` into `scratch`, an empty
/// directory, checks it against the release's `SHA256SUMS` and unpacks it.
pub fn download(releases: &str, version: &str, platform: &str, scratch: &Path) -> Result<Build> {
    if Version::parse(version).is_none() {
        return Err(err(format!("`{version}` isn't a Pi version")));
    }
    let base = format!("{}/download/v{version}", releases.trim_end_matches('/'));
    let sums_path = scratch.join("SHA256SUMS");
    fetch(&format!("{base}/SHA256SUMS"), &sums_path)?;
    let sums = fs::read_to_string(&sums_path)
        .map_err(|e| err(format!("read Pi {version}'s SHA256SUMS: {e}")))?;
    let name = format!("pi-{platform}.tar.gz");
    let want = sum_of(&sums, &name)
        .ok_or_else(|| err(format!("Pi {version}'s SHA256SUMS doesn't list {name}")))?;
    let tarball = scratch.join(&name);
    fetch(&format!("{base}/{name}"), &tarball)?;
    let got = sha256_file(&tarball).map_err(|e| err(format!("read {name}: {e}")))?;
    if !got.eq_ignore_ascii_case(want) {
        let _ = fs::remove_file(&tarball);
        return Err(err(format!(
            "Pi {version}'s {name} has SHA-256 {got}, but its SHA256SUMS says {want}"
        )));
    }
    let unpacked = scratch.join("unpacked");
    fs::create_dir_all(&unpacked)
        .map_err(|e| err(format!("create {}: {e}", unpacked.display())))?;
    let out = Command::new("tar")
        .args(["-xzf"])
        .arg(&tarball)
        .arg("-C")
        .arg(&unpacked)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(format!("run tar: {e}")))?;
    if !out.status.success() {
        return Err(err(format!(
            "can't unpack {name}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let dir = unpacked.join("pi");
    let binary = dir.join("pi");
    let runs =
        fs::metadata(&binary).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if !runs {
        return Err(err(format!("{name} has no pi/pi to run")));
    }
    Ok(Build {
        version: version.to_owned(),
        dir,
        sha256: got,
        fd: None,
    })
}

/// Downloads the pinned `fd` for a VM into `scratch`, checks it and unpacks it,
/// and says where the binary is. A `file://` directory has one in `fd/`, with
/// its sum beside it, as tests' do.
pub fn download_fd(releases: &str, scratch: &Path) -> Result<PathBuf> {
    let target = format!("fd-v{FD_VERSION}-aarch64-unknown-linux-gnu");
    let name = format!("{target}.tar.gz");
    let tarball = scratch.join(&name);
    let want = if releases.starts_with("file://") {
        let base = format!("{}/fd", releases.trim_end_matches('/'));
        fetch(&format!("{base}/{name}"), &tarball)?;
        let sum = scratch.join("fd.sha256");
        fetch(&format!("{base}/{name}.sha256"), &sum)?;
        fs::read_to_string(&sum)
            .map_err(|e| err(format!("read {}: {e}", sum.display())))?
            .trim()
            .to_owned()
    } else {
        fetch(
            &format!("{FD_RELEASES}/download/v{FD_VERSION}/{name}"),
            &tarball,
        )?;
        FD_SHA256.to_owned()
    };
    let got = sha256_file(&tarball).map_err(|e| err(format!("read {name}: {e}")))?;
    if !got.eq_ignore_ascii_case(&want) {
        let _ = fs::remove_file(&tarball);
        return Err(err(format!(
            "{name} has SHA-256 {got}, but {want} was expected"
        )));
    }
    let unpacked = scratch.join("fd-unpacked");
    fs::create_dir_all(&unpacked)
        .map_err(|e| err(format!("create {}: {e}", unpacked.display())))?;
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&unpacked)
        .arg(format!("{target}/fd"))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(format!("run tar: {e}")))?;
    if !out.status.success() {
        return Err(err(format!(
            "can't unpack {name}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(unpacked.join(target).join("fd"))
}

/// What a native install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    /// The binary, in its own directory.
    pub binary: PathBuf,
    /// The link to it on `PATH`.
    pub link: PathBuf,
}

/// Whether a `pi` found at `path` is one [`install`] put under `root`, which
/// it can replace, rather than the owner's own.
pub fn is_ours(path: &Path, root: &Path) -> bool {
    let name_ok = |p: &Path| {
        p.strip_prefix(root).is_ok_and(|rest| {
            rest.components()
                .next()
                .is_some_and(|c| c.as_os_str().to_str().is_some_and(|n| n.starts_with("pi-")))
        })
    };
    name_ok(path) || fs::read_link(path).is_ok_and(|to| name_ok(&to))
}

/// Whether the `pi` agents find at `found` is the one [`install`] linked at `link`,
/// under `root`, of a version but `version`: the build VibeOS pins has moved on
/// since, and it keeps what's its own in step. Links are followed all the way, so
/// one of the owner's to that link still follows it. A `pi` that's anything else
/// is left alone, as moving `link` wouldn't change what runs.
pub fn needs_update(found: &Path, link: &Path, root: &Path, version: &str) -> bool {
    let real = |p: &Path| fs::canonicalize(p).ok();
    let (Some(found), Some(linked), Some(root)) = (real(found), real(link), real(root)) else {
        return false;
    };
    found == linked
        && is_ours(&linked, &root)
        && linked != root.join(format!("pi-{version}")).join("pi")
}

/// Installs `version` for this machine for the owner's own use, on a Mac or a
/// Linux host: `<root>/pi-<version>/` and a link at `<home>/.local/bin/pi`. A
/// `pi` there that isn't one of these is the owner's, and is left alone:
/// nothing is installed over it. With `prune`, for a host with no Pi agent
/// running from one, older installs go but for the one just before this.
pub fn install(
    releases: &str,
    version: &str,
    platform: &str,
    root: &Path,
    home: &Path,
    prune_older: bool,
) -> Result<Installed> {
    let link = home.join(".local/bin/pi");
    check_link(&link, root)?;
    let binary = keep(releases, version, platform, root)?;
    point_link(&link, &binary)?;
    if prune_older {
        prune(root, version);
    }
    Ok(Installed {
        version: version.to_owned(),
        binary,
        link,
    })
}

/// Where [`keep`] puts `version` under `root`.
pub fn kept_binary(root: &Path, version: &str) -> PathBuf {
    root.join(format!("pi-{version}")).join("pi")
}

/// Has `version` for this machine under `root`, checked, downloading it if it isn't
/// there, and says where. No link: a `pi` of the owner's own stays what they run.
pub fn keep(releases: &str, version: &str, platform: &str, root: &Path) -> Result<PathBuf> {
    fs::create_dir_all(root).map_err(|e| err(format!("create {}: {e}", root.display())))?;
    let binary = kept_binary(root, version);
    if binary.is_file() {
        return Ok(binary);
    }
    let scratch = root.join(".pi-download");
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| err(format!("create {}: {e}", scratch.display())))?;
    let result = (|| {
        let dest = root.join(format!("pi-{version}"));
        let build = download(releases, version, platform, &scratch)?;
        let _ = fs::remove_dir_all(&dest);
        fs::rename(&build.dir, &dest).map_err(|e| err(format!("keep {}: {e}", dest.display())))?;
        Ok(binary)
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// Says why `link` can't be pointed at an install under `root`, if it can't.
pub(crate) fn check_link(link: &Path, root: &Path) -> Result<()> {
    match fs::symlink_metadata(link) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(format!("look at {}: {e}", link.display()))),
        Ok(_) => {
            let ours = fs::read_link(link).is_ok_and(|to| to.starts_with(root));
            // A link to nothing isn't anyone's tool: what it was is gone.
            let dangling = fs::metadata(link).is_err();
            if ours || dangling {
                Ok(())
            } else {
                Err(err(format!(
                    "{} is there already and isn't VibeOS's, so it's left as it is",
                    link.display()
                )))
            }
        }
    }
}

/// Points `link` at `binary`, in one step.
pub(crate) fn point_link(link: &Path, binary: &Path) -> Result<()> {
    let parent = link.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(parent).map_err(|e| err(format!("create {}: {e}", parent.display())))?;
    let mut tmp = link.as_os_str().to_owned();
    tmp.push(format!(".vibe-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(binary, &tmp)
        .and_then(|()| fs::rename(&tmp, link))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            err(format!("link {}: {e}", link.display()))
        })
}

/// Removes every `pi-<version>` under `root` but `keep` and the newest other.
fn prune(root: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut others: Vec<(Version, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let v = name.strip_prefix("pi-")?;
            (v != keep).then(|| Some((Version::parse(v)?, e.path())))?
        })
        .collect();
    others.sort();
    others.pop();
    for (_, path) in others {
        let _ = fs::remove_dir_all(path);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn only_vibeos_own_older_install_needs_updating() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for v in ["1.4.0", "1.5.0"] {
            fs::create_dir_all(root.join(format!("pi-{v}"))).unwrap();
            fs::write(root.join(format!("pi-{v}/pi")), "").unwrap();
        }
        let link = bin.join("pi");
        let point = |to: &str| {
            let _ = fs::remove_file(&link);
            symlink(root.join(format!("pi-{to}/pi")), &link).unwrap();
        };
        let needs = |found: &Path| needs_update(found, &link, &root, "1.5.0");
        point("1.4.0");
        assert!(needs(&link));
        point("1.5.0");
        assert!(!needs(&link));

        // A link of the owner's to VibeOS's link follows it.
        point("1.4.0");
        let chain = bin.join("chain");
        symlink(&link, &chain).unwrap();
        assert!(needs(&chain));

        // A link of the owner's to one old build, earlier on the PATH, is theirs:
        // moving VibeOS's link wouldn't change it, so it must not be tried forever.
        point("1.5.0");
        let theirs = bin.join("theirs");
        symlink(root.join("pi-1.4.0/pi"), &theirs).unwrap();
        assert!(!needs(&theirs));

        // The owner's own, wherever it is, is left alone.
        let own = bin.join("own");
        fs::write(&own, "").unwrap();
        assert!(!needs(&own));
        fs::remove_file(&link).unwrap();
        fs::write(&link, "").unwrap();
        assert!(!needs(&link));
        assert!(!needs(&bin.join("nothing")));
    }
    use crate::images::sha256_file;

    /// Publishes `version` of `platform` under `root` as a `file://` release,
    /// with `binary` as its `pi/pi`, and an `fd`.
    pub(crate) fn publish(root: &Path, version: &str, platform: &str, binary: &str) -> String {
        let release = root.join(format!("download/v{version}"));
        let staging = root.join(format!("staging-{version}/pi"));
        fs::create_dir_all(staging.join("theme")).unwrap();
        fs::create_dir_all(&release).unwrap();
        fs::write(staging.join("pi"), binary).unwrap();
        fs::set_permissions(staging.join("pi"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(staging.join("theme/dark.json"), "{}").unwrap();
        fs::write(staging.join("package.json"), "{}").unwrap();
        let name = format!("pi-{platform}.tar.gz");
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(release.join(&name))
            .arg("-C")
            .arg(root.join(format!("staging-{version}")))
            .arg("pi")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let sum = sha256_file(&release.join(&name)).unwrap();
        fs::write(release.join("SHA256SUMS"), format!("{sum}  {name}\n")).unwrap();
        let target = format!("fd-v{FD_VERSION}-aarch64-unknown-linux-gnu");
        let staging = root.join("staging-fd");
        fs::create_dir_all(staging.join(&target)).unwrap();
        fs::write(staging.join(&target).join("fd"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            staging.join(&target).join("fd"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::create_dir_all(root.join("fd")).unwrap();
        let tarball = root.join("fd").join(format!("{target}.tar.gz"));
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&staging)
            .arg(&target)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let sum = sha256_file(&tarball).unwrap();
        fs::write(root.join("fd").join(format!("{target}.tar.gz.sha256")), sum).unwrap();
        format!("file://{}", root.display())
    }

    #[test]
    fn a_build_is_checked_and_unpacked() {
        let tmp = tempfile::tempdir().unwrap();
        let releases = publish(&tmp.path().join("r"), "1.5.0", "linux-arm64", "#!/bin/sh\n");
        let scratch = tmp.path().join("s");
        fs::create_dir_all(&scratch).unwrap();
        let build = download(&releases, "1.5.0", "linux-arm64", &scratch).unwrap();
        assert_eq!(build.version, "1.5.0");
        assert!(build.dir.join("pi").is_file());
        assert!(
            build.dir.join("theme/dark.json").is_file(),
            "its assets too"
        );
    }

    #[test]
    fn a_tampered_build_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        let releases = publish(&root, "1.5.0", "linux-arm64", "#!/bin/sh\n");
        let tarball = root.join("download/v1.5.0/pi-linux-arm64.tar.gz");
        fs::write(&tarball, b"tampered").unwrap();
        let scratch = tmp.path().join("s");
        fs::create_dir_all(&scratch).unwrap();
        let e = download(&releases, "1.5.0", "linux-arm64", &scratch).unwrap_err();
        assert!(e.0.contains("SHA-256"), "{e}");
        assert!(!scratch.join("pi-linux-arm64.tar.gz").exists());
        // A platform the release doesn't have says so.
        let e = download(&releases, "1.5.0", "linux-x64", &scratch).unwrap_err();
        assert!(e.0.contains("doesn't list"), "{e}");
        // Nor is a version anything but a version.
        let e = download(&releases, "../x", "linux-x64", &scratch).unwrap_err();
        assert!(e.0.contains("isn't a Pi version"), "{e}");
    }

    #[test]
    fn a_native_install_links_it_and_keeps_one_older() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        let (data, home) = (tmp.path().join("data"), tmp.path().join("home"));
        let mut releases = String::new();
        for v in ["1.4.0", "1.5.0", "1.6.0"] {
            releases = publish(&root, v, "linux-x64", v);
            let done = install(&releases, v, "linux-x64", &data, &home, true).unwrap();
            assert_eq!(done.version, v);
            assert_eq!(fs::read_to_string(&done.link).unwrap(), v);
            assert_eq!(fs::read_link(&done.link).unwrap(), done.binary);
        }
        assert!(!data.join("pi-1.4.0").exists(), "older ones go");
        assert!(data.join("pi-1.5.0").exists(), "the last stays");
        assert!(data.join("pi-1.6.0/theme/dark.json").is_file());
        assert!(!data.join(".pi-download").exists());
        // Again, with nothing new, is fine.
        install(&releases, "1.6.0", "linux-x64", &data, &home, true).unwrap();
        // With an agent running from an older one, none go.
        let releases = publish(&root, "1.7.0", "linux-x64", "1.7.0");
        install(&releases, "1.7.0", "linux-x64", &data, &home, false).unwrap();
        assert!(data.join("pi-1.5.0").exists() && data.join("pi-1.6.0").exists());
        assert!(is_ours(&home.join(".local/bin/pi"), &data));
        assert!(is_ours(&data.join("pi-1.7.0/pi"), &data));
        assert!(!is_ours(Path::new("/usr/local/bin/pi"), &data));
    }

    #[test]
    fn fd_is_checked_like_pi() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        let releases = publish(&root, "1.5.0", "linux-arm64", "#!/bin/sh\n");
        let scratch = tmp.path().join("s");
        fs::create_dir_all(&scratch).unwrap();
        let fd = download_fd(&releases, &scratch).unwrap();
        assert!(fd.is_file() && fd.ends_with("fd"));
        fs::write(
            root.join("fd")
                .join(format!("fd-v{FD_VERSION}-aarch64-unknown-linux-gnu.tar.gz")),
            "tampered",
        )
        .unwrap();
        let e = download_fd(&releases, &scratch).unwrap_err();
        assert!(e.0.contains("SHA-256"), "{e}");
    }

    #[test]
    fn the_owners_own_pi_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let releases = publish(&tmp.path().join("r"), "1.5.0", "linux-x64", "new");
        let (data, home) = (tmp.path().join("data"), tmp.path().join("home"));
        let link = home.join(".local/bin/pi");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&link, "mine").unwrap();
        let e = install(&releases, "1.5.0", "linux-x64", &data, &home, true).unwrap_err();
        assert!(e.0.contains("isn't VibeOS's"), "{e}");
        assert_eq!(fs::read_to_string(&link).unwrap(), "mine");
        assert!(!data.join("pi-1.5.0").exists());
    }

    #[test]
    fn sums_are_read_as_sha256sum_writes_them() {
        let sums = "aa  pi-a.tar.gz\nbb *pi-b.tar.gz\n";
        assert_eq!(sum_of(sums, "pi-a.tar.gz"), Some("aa"));
        assert_eq!(sum_of(sums, "pi-b.tar.gz"), Some("bb"));
        assert_eq!(sum_of(sums, "pi-c.tar.gz"), None);
    }
}
