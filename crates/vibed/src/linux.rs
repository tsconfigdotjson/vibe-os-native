//! Keeping the images `linux` agents boot current (PRD: Key user flows,
//! Install and setup; Exceptions to "set up once, then frozen"): the base
//! release this `vibed` ships with, downloaded once, and the managed layer,
//! which takes each new Claude Code release, checked when `vibed` starts and
//! then daily. The work is `vibe_env::images`; this is when it runs.

use std::path::Path;
use std::time::Duration;

use vibe_core::paths::Layout;
use vibe_env::images::{self, Images};

/// How often a newer Claude Code is looked for.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Where the images come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    /// The directory of base releases, each in `base-<n>/`.
    pub base_releases: String,
    pub base_release: u32,
    /// Claude Code's releases.
    pub claude_releases: String,
}

impl Sources {
    /// The published ones, unless `VIBE_BASE_RELEASES` or
    /// `VIBE_CLAUDE_RELEASES` names others (a `file://` directory with a
    /// base image built by `scripts/base-image.sh`, say).
    pub fn from_env() -> Sources {
        let var = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        Sources {
            base_releases: var("VIBE_BASE_RELEASES", images::BASE_RELEASES),
            base_release: images::BASE_RELEASE,
            claude_releases: var("VIBE_CLAUDE_RELEASES", images::CLAUDE_RELEASES),
        }
    }
}

/// One round, in this order: the base release if it isn't installed; the
/// managed layers no agent's `vm.json` names any more, but the newest; and
/// a newer Claude Code, as a new layer. Pruning goes before the update, so
/// a start that just took the newest layer never loses it. Each step is
/// logged, and one failing doesn't stop the next.
pub fn refresh(images: &Images, agents_dir: &Path, sources: &Sources) {
    let url = images::base_url(&sources.base_releases, sources.base_release);
    let had_base = images
        .base()
        .is_some_and(|b| b.version >= sources.base_release);
    if !had_base {
        match images.install_base(&url, sources.base_release) {
            Ok(base) => crate::log!(
                "vibed: Linux base image {} installed from {url}",
                base.version
            ),
            Err(e) => crate::log!("vibed: can't install the Linux base image: {e}"),
        }
    }
    for gone in images.prune_unused(agents_dir) {
        crate::log!("vibed: removed {}, which no agent uses", gone.display());
    }
    match images.update_managed(&sources.claude_releases) {
        Ok(Some(version)) => crate::log!(
            "vibed: Claude Code {version} is the managed layer for linux agents' next boot"
        ),
        Ok(None) => {}
        Err(e) => crate::log!("vibed: can't update Claude Code for linux agents: {e}"),
    }
}

/// Runs [`refresh`] now and then every `every`, off the runtime's threads.
pub fn spawn(layout: Layout, sources: Sources, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let (layout, sources) = (layout.clone(), sources.clone());
            let done = tokio::task::spawn_blocking(move || {
                refresh(
                    &Images::new(layout.linux_dir()),
                    &layout.agents_dir(),
                    &sources,
                );
            })
            .await;
            if let Err(e) = done {
                crate::log!("vibed: the Linux image check failed: {e}");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn sha256(path: &Path) -> String {
        images::sha256_file(path).unwrap()
    }

    fn publish_claude(dir: &Path, version: &str, binary: &[u8]) {
        let release = dir.join(version);
        fs::create_dir_all(release.join("linux-arm64")).unwrap();
        let bin = release.join("linux-arm64/claude");
        fs::write(&bin, binary).unwrap();
        let manifest = serde_json::json!({
            "version": version,
            "platforms": {"linux-arm64": {"checksum": sha256(&bin), "size": binary.len()}},
        });
        fs::write(release.join("manifest.json"), manifest.to_string()).unwrap();
        fs::write(dir.join("latest"), version).unwrap();
    }

    fn publish_base(dir: &Path) {
        let release = dir.join("base-1");
        fs::create_dir_all(&release).unwrap();
        fs::write(release.join("Image"), "kernel").unwrap();
        fs::write(release.join("initrd"), "initrd").unwrap();
        let mut tree = vibe_env::ext4::Tree::new();
        tree.dir("etc", 0o755);
        vibe_env::ext4::write(&tree, &release.join("base.img"), "base", 1).unwrap();
        assert!(
            Command::new("gzip")
                .arg(release.join("base.img"))
                .status()
                .unwrap()
                .success()
        );
        let mut files = serde_json::Map::new();
        for name in ["Image", "initrd", "base.img.gz"] {
            let p = release.join(name);
            files.insert(
                name.into(),
                serde_json::json!({"sha256": sha256(&p), "size": fs::metadata(&p).unwrap().len()}),
            );
        }
        fs::write(
            release.join("base.json"),
            serde_json::json!({"version": 1, "files": files}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_round_installs_the_base_and_takes_new_claude_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path().join("home"), "macos");
        let published = tmp.path().join("published");
        publish_base(&published.join("base"));
        publish_claude(&published.join("claude"), "2.1.308", b"first");
        let sources = Sources {
            base_releases: format!("file://{}", published.join("base").display()),
            base_release: 1,
            claude_releases: format!("file://{}", published.join("claude").display()),
        };
        let images = Images::new(layout.linux_dir());
        refresh(&images, &layout.agents_dir(), &sources);
        let layers = images.layers().unwrap();
        assert_eq!(layers.base.version, 1);
        assert_eq!(layers.managed.as_ref().unwrap().0, "2.1.308");

        // The next day: a new release, the next boot's layer. The old one,
        // which no agent uses, goes the round after.
        publish_claude(&published.join("claude"), "2.1.314", b"second");
        refresh(&images, &layout.agents_dir(), &sources);
        assert_eq!(images.managed().unwrap().0, "2.1.314");
        let old = layout.linux_dir().join("managed-2.1.308.img");
        assert!(old.exists());
        refresh(&images, &layout.agents_dir(), &sources);
        assert!(!old.exists());
        assert_eq!(images.managed().unwrap().0, "2.1.314");

        // Unreachable sources are logged, not fatal, and nothing is lost.
        let gone = Sources {
            base_releases: "file:///nowhere".into(),
            base_release: 2,
            claude_releases: "file:///nowhere".into(),
        };
        refresh(&images, &layout.agents_dir(), &gone);
        assert_eq!(images.base().unwrap().version, 1);
        assert_eq!(images.managed().unwrap().0, "2.1.314");
    }
}
