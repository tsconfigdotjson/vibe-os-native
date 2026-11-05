//! Installing what a host's agents need and don't have, for nobody to have to:
//! GitHub's `gh` and Claude Code, each from its publisher's own releases, each checked
//! against the sums published with it, linked from `~/.local/bin`. `gh` goes under
//! VibeOS's data directory. Claude Code installs itself from the checked build, as its
//! own installer has it do, so it goes on updating itself as ever. No root, and never
//! over something that isn't VibeOS's: one of the owner's on the `PATH` is used as it
//! is, and this isn't called at all.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::images::{self, ImageError, Result, fetch, fetch_bounded, sha256_file};
use crate::pi_release::{check_link, point_link};

/// Where GitHub publishes `gh`.
pub const GH_RELEASES: &str = "https://github.com/cli/cli/releases";

/// The `gh` this build installs: known, rather than whatever's newest, as it's run
/// and read here. A new one comes with a new VibeOS.
pub const GH_PINNED: &str = "2.103.3";

fn err(message: impl Into<String>) -> ImageError {
    ImageError(message.into())
}

/// What `gh`'s releases call the build for this machine, with how it's packed.
fn gh_build_here() -> Option<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(("linux_amd64", "tar.gz")),
        ("linux", "aarch64") => Some(("linux_arm64", "tar.gz")),
        ("macos", "x86_64") => Some(("macOS_amd64", "zip")),
        ("macos", "aarch64") => Some(("macOS_arm64", "zip")),
        _ => None,
    }
}

/// What Claude Code's releases call the build for this machine.
fn claude_build_here() -> Option<&'static str> {
    // Alpine and the like: the usual build wouldn't run.
    let musl = cfg!(target_os = "linux")
        && fs::read_dir("/lib").is_ok_and(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
        });
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if musl => Some("linux-x64-musl"),
        ("linux", "aarch64") if musl => Some("linux-arm64-musl"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// A clean directory to work in, for as long as `work` takes.
fn in_scratch<T>(root: &Path, name: &str, work: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let scratch = root.join(name);
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| err(format!("create {}: {e}", scratch.display())))?;
    let done = work(&scratch);
    let _ = fs::remove_dir_all(&scratch);
    done
}

/// Installs the pinned `gh`: `<root>/gh-<version>/gh`, linked at `<home>/.local/bin/gh`.
pub fn install_gh(releases: &str, root: &Path, home: &Path) -> Result<PathBuf> {
    let (build, pack) = gh_build_here().ok_or_else(|| err("gh has no build for this machine"))?;
    install_gh_build(releases, GH_PINNED, build, pack, root, home)
}

fn install_gh_build(
    releases: &str,
    version: &str,
    build: &str,
    pack: &str,
    root: &Path,
    home: &Path,
) -> Result<PathBuf> {
    let link = home.join(".local/bin/gh");
    check_link(&link, root)?;
    let dest = root.join(format!("gh-{version}"));
    let binary = dest.join("gh");
    if !binary.is_file() {
        in_scratch(root, ".gh-download", |scratch| {
            let base = format!("{}/download/v{version}", releases.trim_end_matches('/'));
            let sums_name = format!("gh_{version}_checksums.txt");
            let sums_path = scratch.join(&sums_name);
            fetch_bounded(&format!("{base}/{sums_name}"), &sums_path)?;
            let sums = fs::read_to_string(&sums_path)
                .map_err(|e| err(format!("read gh's checksums: {e}")))?;
            let folder = format!("gh_{version}_{build}");
            let name = format!("{folder}.{pack}");
            let want = crate::pi_release::sum_of(&sums, &name)
                .ok_or_else(|| err(format!("gh's checksums don't list {name}")))?;
            let archive = scratch.join(&name);
            fetch_bounded(&format!("{base}/{name}"), &archive)?;
            let got = sha256_file(&archive).map_err(|e| err(format!("read {name}: {e}")))?;
            if !got.eq_ignore_ascii_case(want) {
                return Err(err(format!(
                    "{name} has SHA-256 {got}, but gh's checksums say {want}"
                )));
            }
            let out = if pack == "zip" {
                Command::new("/usr/bin/unzip")
                    .arg("-q")
                    .arg(&archive)
                    .arg("-d")
                    .arg(scratch)
                    .stdin(Stdio::null())
                    .output()
            } else {
                Command::new("tar")
                    .arg("-xzf")
                    .arg(&archive)
                    .arg("-C")
                    .arg(scratch)
                    .stdin(Stdio::null())
                    .output()
            }
            .map_err(|e| err(format!("unpack {name}: {e}")))?;
            if !out.status.success() {
                return Err(err(format!(
                    "can't unpack {name}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let _ = fs::remove_dir_all(&dest);
            fs::create_dir_all(&dest)
                .map_err(|e| err(format!("create {}: {e}", dest.display())))?;
            fs::rename(scratch.join(&folder).join("bin/gh"), &binary)
                .map_err(|e| err(format!("{name} has no bin/gh to run: {e}")))
        })?;
    }
    point_link(&link, &binary)?;
    prune_gh(root, &dest);
    Ok(binary)
}

/// The `gh`s of pins gone by, once the link's moved on.
fn prune_gh(root: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("gh-") && entry.path() != keep {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether the `gh` found at `found` is the one [`install_gh`] linked, of a version this
/// build doesn't pin: it's VibeOS's, so it follows. The owner's own is left alone.
pub fn gh_needs_update(found: &Path, root: &Path, home: &Path) -> bool {
    let real = |p: &Path| fs::canonicalize(p).ok();
    let (Some(found), Some(linked), Some(root)) =
        (real(found), real(&home.join(".local/bin/gh")), real(root))
    else {
        return false;
    };
    found == linked
        && linked.starts_with(&root)
        && linked != root.join(format!("gh-{GH_PINNED}")).join("gh")
}

/// How long Claude Code may take to install itself, as it downloads a build too.
const CLAUDE_INSTALL_FOR: Duration = Duration::from_secs(20 * 60);

/// Installs the newest Claude Code, the way its own installer does: downloads the build,
/// checks it, and has it install itself, which keeps its versions and links
/// `<home>/.local/bin/claude`. So it updates itself from then on as it always does, and
/// tidies up what it no longer needs. Says which version.
pub fn install_claude(releases: &str, root: &Path, home: &Path) -> Result<(String, PathBuf)> {
    let build =
        claude_build_here().ok_or_else(|| err("Claude Code has no build for this machine"))?;
    install_claude_build(releases, build, root, home)
}

fn install_claude_build(
    releases: &str,
    build: &str,
    root: &Path,
    home: &Path,
) -> Result<(String, PathBuf)> {
    let link = home.join(".local/bin/claude");
    check_link(&link, root)?;
    let releases = releases.trim_end_matches('/');
    in_scratch(root, ".claude-download", |scratch| {
        let (version, checksum) = images::claude_release(releases, build, scratch)?;
        let bootstrap = scratch.join("claude");
        // However long it takes: it's big, and a slow link's not a stalled one.
        fetch(&format!("{releases}/{version}/{build}/claude"), &bootstrap)?;
        let got = sha256_file(&bootstrap).map_err(|e| err(format!("read Claude Code: {e}")))?;
        if !got.eq_ignore_ascii_case(&checksum) {
            return Err(err(format!(
                "Claude Code {version} has SHA-256 {got}, but its manifest says {checksum}"
            )));
        }
        fs::set_permissions(&bootstrap, fs::Permissions::from_mode(0o755))
            .map_err(|e| err(format!("make Claude Code runnable: {e}")))?;
        // To a file, as a pipe left unread would stop it when full.
        let said_at = scratch.join("install.log");
        let said = fs::File::create(&said_at)
            .map_err(|e| err(format!("keep what Claude Code's install says: {e}")))?;
        // A link to nothing, from an install long gone, isn't in its way.
        if fs::symlink_metadata(&link).is_ok() && fs::metadata(&link).is_err() {
            let _ = fs::remove_file(&link);
        }
        let mut child = Command::new(&bootstrap)
            .arg("install")
            .env("HOME", home)
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(said)
            .spawn()
            .map_err(|e| err(format!("run Claude Code's install: {e}")))?;
        let begun = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if begun.elapsed() > CLAUDE_INSTALL_FOR => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(err("Claude Code's install took too long"));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(err(format!("Claude Code's install: {e}"))),
            }
        };
        if !status.success() {
            let said = fs::read_to_string(&said_at).unwrap_or_default();
            return Err(err(format!(
                "Claude Code's install failed: {}",
                said.lines()
                    .rfind(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
            )));
        }
        if !link.exists() {
            return Err(err(format!(
                "Claude Code's install finished with nothing at {}",
                link.display()
            )));
        }
        Ok((version, link.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish_gh(root: &Path, version: &str, build: &str, tamper: bool) {
        let release = root.join(format!("download/v{version}"));
        let folder = format!("gh_{version}_{build}");
        let staging = root.join("staging");
        fs::create_dir_all(staging.join(&folder).join("bin")).unwrap();
        fs::create_dir_all(&release).unwrap();
        let bin = staging.join(&folder).join("bin/gh");
        fs::write(&bin, "#!/bin/sh\necho gh\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let name = format!("{folder}.tar.gz");
        let tarball = release.join(&name);
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&tarball)
                .arg("-C")
                .arg(&staging)
                .arg(&folder)
                .status()
                .unwrap()
                .success()
        );
        let sum = if tamper {
            "0".repeat(64)
        } else {
            sha256_file(&tarball).unwrap()
        };
        fs::write(
            release.join(format!("gh_{version}_checksums.txt")),
            format!("{sum}  {name}\n"),
        )
        .unwrap();
    }

    #[test]
    fn gh_goes_in_checked_and_never_over_someone_elses() {
        let tmp = tempfile::tempdir().unwrap();
        let (releases, root, home) = (
            tmp.path().join("releases"),
            tmp.path().join("data"),
            tmp.path().join("home"),
        );
        fs::create_dir_all(&root).unwrap();
        let url = format!("file://{}", releases.display());
        let install = || install_gh_build(&url, "2.0.0", "linux_amd64", "tar.gz", &root, &home);

        // Nothing there, and nothing's kept of a failed try.
        assert!(install().is_err());
        assert!(!home.join(".local/bin/gh").exists());

        publish_gh(&releases, "2.0.0", "linux_amd64", true);
        let e = install().unwrap_err().to_string();
        assert!(e.contains("SHA-256"), "{e}");
        assert!(!root.join("gh-2.0.0/gh").exists());
        assert!(!root.join(".gh-download").exists());

        publish_gh(&releases, "2.0.0", "linux_amd64", false);
        let binary = install().unwrap();
        assert_eq!(binary, root.join("gh-2.0.0/gh"));
        assert_eq!(fs::read_link(home.join(".local/bin/gh")).unwrap(), binary);
        // Again, it's all there, with no download: nothing's published now.
        fs::remove_dir_all(&releases).unwrap();
        assert_eq!(install().unwrap(), binary);

        // One of the owner's is left as it is.
        fs::remove_file(home.join(".local/bin/gh")).unwrap();
        fs::write(home.join(".local/bin/gh"), "mine").unwrap();
        let e = install().unwrap_err().to_string();
        assert!(e.contains("isn't VibeOS's"), "{e}");
        assert_eq!(
            fs::read_to_string(home.join(".local/bin/gh")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn claude_goes_in_checked_and_never_over_someone_elses() {
        let tmp = tempfile::tempdir().unwrap();
        let (releases, root, home) = (
            tmp.path().join("releases"),
            tmp.path().join("data"),
            tmp.path().join("home"),
        );
        fs::create_dir_all(&root).unwrap();
        let url = format!("file://{}", releases.display());
        fs::create_dir_all(&home).unwrap();
        let publish = |body: &str, claim: Option<&str>| {
            let build = releases.join("2.1.400/linux-x64");
            fs::create_dir_all(&build).unwrap();
            let bin = build.join("claude");
            fs::write(&bin, body).unwrap();
            let sum = claim
                .map(str::to_owned)
                .unwrap_or_else(|| sha256_file(&bin).unwrap());
            fs::write(
                releases.join("2.1.400/manifest.json"),
                serde_json::json!({
                    "version": "2.1.400",
                    "platforms": {"linux-x64": {"checksum": sum, "size": body.len()}},
                })
                .to_string(),
            )
            .unwrap();
            fs::write(releases.join("latest"), "2.1.400\n").unwrap();
        };
        // As the real one does: it keeps itself under the home, and links that.
        let installs_itself = "#!/bin/sh\n[ \"$1\" = install ] || exit 9\n\
                               mkdir -p \"$HOME/.local/share/claude\" \"$HOME/.local/bin\"\n\
                               cp \"$0\" \"$HOME/.local/share/claude/2.1.400\"\n\
                               ln -sf \"$HOME/.local/share/claude/2.1.400\" \"$HOME/.local/bin/claude\"\n";
        let install = || install_claude_build(&url, "linux-x64", &root, &home);
        publish(installs_itself, Some(&"0".repeat(64)));
        let e = install().unwrap_err().to_string();
        assert!(e.contains("SHA-256"), "{e}");
        assert!(!home.join(".local/bin/claude").exists());

        publish("#!/bin/sh\necho 'no room' >&2\nexit 1\n", None);
        let e = install().unwrap_err().to_string();
        assert!(e.contains("no room"), "{e}");
        assert!(!root.join(".claude-download").exists());

        // What's left of an old install, a link to nothing, is no tool of anyone's.
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::os::unix::fs::symlink("/nowhere/claude", home.join(".local/bin/claude")).unwrap();
        publish(installs_itself, None);
        let (version, link) = install().unwrap();
        assert_eq!(version, "2.1.400");
        assert_eq!(link, home.join(".local/bin/claude"));
        assert!(link.exists());
        // It's Claude's own layout, for it to update.
        assert!(home.join(".local/share/claude/2.1.400").is_file());
        assert!(!root.join(".claude-download").exists());

        // One of the owner's is left as it is.
        fs::remove_file(&link).unwrap();
        fs::write(&link, "mine").unwrap();
        assert!(
            install()
                .unwrap_err()
                .to_string()
                .contains("isn't VibeOS's")
        );
    }

    #[test]
    fn only_vibeos_own_gh_on_an_old_pin_is_moved() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let (root, home) = (tmp.path().join("data"), tmp.path().join("home"));
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        fs::create_dir_all(root.join("gh-0.1.0")).unwrap();
        fs::write(root.join("gh-0.1.0/gh"), "").unwrap();
        let link = home.join(".local/bin/gh");
        symlink(root.join("gh-0.1.0/gh"), &link).unwrap();
        assert!(gh_needs_update(&link, &root, &home));
        let pinned = root.join(format!("gh-{GH_PINNED}"));
        fs::create_dir_all(&pinned).unwrap();
        fs::write(pinned.join("gh"), "").unwrap();
        fs::remove_file(&link).unwrap();
        symlink(pinned.join("gh"), &link).unwrap();
        assert!(!gh_needs_update(&link, &root, &home));
        let own = tmp.path().join("own-gh");
        fs::write(&own, "").unwrap();
        assert!(!gh_needs_update(&own, &root, &home));
    }
}
