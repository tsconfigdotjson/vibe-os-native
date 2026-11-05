//! Keeping the images `linux` agents boot current (PRD: Key user flows,
//! Install and setup; Exceptions to "set up once, then frozen"): the base
//! release this `vibed` ships with, downloaded once, and the managed layer,
//! which takes each new Claude Code release, checked when `vibed` starts and
//! then daily, and the Pi layer, the same way, once a profile runs Pi in a VM.
//! The work is `vibe_env::images`; this is when it runs.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vibe_core::model::{CheckId, CheckState};
use vibe_core::paths::Layout;
use vibe_core::rpc::DoctorCheck;
use vibe_env::images::{self, Images};

use crate::core::lock;

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
    /// Pi's releases.
    pub pi_releases: String,
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
            pi_releases: crate::pi::releases_from_env(),
        }
    }
}

/// How a [`refresh`] went: why the base image or Claude Code couldn't be
/// had, if it couldn't.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refreshed {
    pub base_error: Option<String>,
    pub claude_error: Option<String>,
    pub pi_error: Option<String>,
}

/// One round, in this order: the base release if it isn't installed; the
/// managed layers no agent's `vm.json` names any more, but the newest; and
/// a newer Claude Code, as a new layer; and with `pi`, the Pi this build pins,
/// the same way. Pruning goes before the update, so a start that just took the newest
/// layer never loses it. Each step is logged, and one failing doesn't stop
/// the next.
pub fn refresh(images: &Images, agents_dir: &Path, sources: &Sources, pi: bool) -> Refreshed {
    let mut out = Refreshed::default();
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
            Err(e) => {
                crate::log!("vibed: can't install the Linux base image: {e}");
                out.base_error = Some(e.to_string());
            }
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
        Err(e) => {
            crate::log!("vibed: can't update Claude Code for linux agents: {e}");
            out.claude_error = Some(e.to_string());
        }
    }
    if pi {
        match images.update_pi(&sources.pi_releases, vibe_env::pi_release::PINNED) {
            Ok(Some(version)) => {
                crate::log!("vibed: Pi {version} is the layer for linux Pi agents' next boot");
            }
            Ok(None) => {}
            Err(e) => {
                crate::log!("vibed: can't update Pi for linux agents: {e}");
                out.pi_error = Some(e.to_string());
            }
        }
    }
    out
}

/// The Linux images on a Mac, kept current in the background, and how
/// that's going, for the doctor's `linux` check and the first run's
/// download step (PRD: Key user flows, Install and setup).
#[derive(Debug)]
pub struct Watch {
    images: Images,
    agents_dir: std::path::PathBuf,
    sources: Sources,
    state: Mutex<WatchState>,
    kick: tokio::sync::Notify,
    /// A profile runs Pi in a VM, so its layer is kept current. A layer that's
    /// there says so too.
    want_pi: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Default)]
struct WatchState {
    /// A round is under way.
    running: bool,
    /// A round is asked for, and hasn't started yet.
    pending: bool,
    /// Whether a round has finished since `vibed` started.
    rounds: u32,
    last: Refreshed,
}

impl Watch {
    pub fn new(layout: &Layout, sources: Sources) -> Arc<Watch> {
        Arc::new(Watch {
            images: Images::new(layout.linux_dir()),
            agents_dir: layout.agents_dir(),
            sources,
            state: Mutex::new(WatchState::default()),
            kick: tokio::sync::Notify::new(),
            want_pi: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// From now on, keeps the Pi layer current.
    /// Why the Pi layer couldn't be had the last time it was tried.
    pub fn pi_error(&self) -> Option<String> {
        lock(&self.state).last.pi_error.clone()
    }

    pub fn want_pi(&self) {
        self.want_pi
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// One [`refresh`], noted for [`Watch::check`].
    pub fn refresh(&self) -> Refreshed {
        {
            let mut st = lock(&self.state);
            st.running = true;
            st.pending = false;
        }
        let pi =
            self.want_pi.load(std::sync::atomic::Ordering::SeqCst) || self.images.pi().is_some();
        let done = refresh(&self.images, &self.agents_dir, &self.sources, pi);
        let mut st = lock(&self.state);
        st.running = false;
        st.rounds += 1;
        st.last = done.clone();
        done
    }

    /// Asks for a round now, rather than at the next daily one. It shows
    /// as under way at once.
    pub fn kick(&self) {
        lock(&self.state).pending = true;
        self.kick.notify_one();
    }

    /// The doctor's `linux` check: ready once the base release this
    /// `vibed` boots and a managed layer are installed; otherwise how the
    /// download is going, or why it stopped.
    pub fn check(&self) -> DoctorCheck {
        let base = self
            .images
            .base()
            .filter(|b| b.version >= self.sources.base_release);
        let managed = self.images.managed();
        let st = lock(&self.state);
        let busy = st.running || st.pending;
        let check = |state, detail: String, hint: Option<&str>, progress| DoctorCheck {
            id: CheckId::Linux,
            state,
            detail,
            hint: hint.map(str::to_owned),
            fixable: state != CheckState::Pass && state != CheckState::Working,
            progress,
        };
        if let (Some(base), Some((claude, _))) = (&base, &managed) {
            // A template keeps the base it was built on, so one from before
            // shared services stays there until it's rebuilt. Nothing's broken,
            // and the download can't help: it says what will.
            let boots = self.images.layers().map(|l| l.base.version);
            if self.sources.base_release >= images::SERVICES_BASE
                && boots.as_ref().is_ok_and(|v| *v < images::SERVICES_BASE)
            {
                return DoctorCheck {
                    fixable: false,
                    ..check(
                        CheckState::Optional,
                        format!(
                            "Linux agents have no shared MCP services yet: the template is on \
                             base image {}, and they need {}.",
                            boots.unwrap_or(0),
                            images::SERVICES_BASE
                        ),
                        Some(
                            "Rebuild Template in Settings → Linux Toolbox, or run `vibe toolbox \
                             rebuild`.",
                        ),
                        None,
                    )
                };
            }
            return check(
                CheckState::Pass,
                format!(
                    "Linux base image {} and Claude Code {claude} are ready for linux agents.",
                    base.version
                ),
                None,
                None,
            );
        }
        let error = match (&base, &managed) {
            (None, _) => st.last.base_error.as_deref(),
            _ => st.last.claude_error.as_deref(),
        };
        if !busy && let Some(e) = error {
            return check(
                CheckState::Fail,
                sentence(e),
                Some("VibeOS tries again every hour. Check this Mac is online."),
                None,
            );
        }
        let progress = base
            .is_none()
            .then(|| self.images.base_progress(self.sources.base_release))
            .flatten()
            .map(|(done, total)| percent(done, total));
        let detail = match (&base, busy || st.rounds == 0) {
            (None, true) => "Downloading the Linux base image.".to_owned(),
            (None, false) => "The Linux base image isn't downloaded yet.".to_owned(),
            (Some(_), true) => "Downloading Claude Code for linux agents.".to_owned(),
            (Some(_), false) => "Claude Code for linux agents isn't downloaded yet.".to_owned(),
        };
        if busy || st.rounds == 0 {
            check(CheckState::Working, detail, None, progress)
        } else {
            check(
                CheckState::Fail,
                detail,
                Some("Try again to download it now."),
                None,
            )
        }
    }
}

/// `done` of `total` in whole percent, under 100 until it's all there.
fn percent(done: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    let p = (u128::from(done) * 100 / u128::from(total)) as u8;
    if done < total { p.min(99) } else { 100 }
}

/// `e` as a sentence: capitalized, with a full stop.
fn sentence(e: &str) -> String {
    let mut out: String = e.trim().to_owned();
    if let Some(first) = out.get(..1) {
        out = first.to_uppercase() + &out[1..];
    }
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// Runs a round now, then every `every`, and whenever [`Watch::kick`]
/// asks, off the runtime's threads.
pub fn spawn(watch: Arc<Watch>, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                () = watch.kick.notified() => {}
            }
            let round = Arc::clone(&watch);
            if let Err(e) = tokio::task::spawn_blocking(move || round.refresh()).await {
                let mut st = lock(&watch.state);
                st.running = false;
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
    fn the_doctor_says_to_rebuild_a_template_from_before_shared_services() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path().join("home"), "macos");
        let dir = layout.linux_dir();
        for n in [2, 3] {
            let base = dir.join(format!("base-{n}"));
            fs::create_dir_all(&base).unwrap();
            for f in ["Image", "initrd", "base.img"] {
                fs::write(base.join(f), f).unwrap();
            }
        }
        let bin = tmp.path().join("claude-bin");
        fs::write(&bin, "claude").unwrap();
        Images::new(dir.clone())
            .add_managed("2.1.308", &bin, "x")
            .unwrap();
        let sources = || Sources {
            base_releases: "file:///nowhere".into(),
            base_release: vibe_env::images::SERVICES_BASE,
            claude_releases: "file:///nowhere".into(),
            pi_releases: "file:///nowhere".into(),
        };

        // No template yet: new agents boot the newest base, and all's well.
        let check = Watch::new(&layout, sources()).check();
        assert_eq!(check.state, CheckState::Pass, "{check:?}");

        // A template built on the older one keeps that one's guest.
        fs::write(dir.join("template-1.img"), "t").unwrap();
        fs::write(dir.join("template-1.base"), "2").unwrap();
        let check = Watch::new(&layout, sources()).check();
        assert_eq!(check.state, CheckState::Optional, "{check:?}");
        assert!(check.detail.contains("no shared MCP services"), "{check:?}");
        assert!(
            check.hint.as_deref().unwrap().contains("Rebuild Template"),
            "{check:?}"
        );
        assert!(!check.fixable, "downloading can't fix it: {check:?}");

        // Rebuilt, on the base that has them.
        fs::write(dir.join("template-2.img"), "t").unwrap();
        fs::write(dir.join("template-2.base"), "3").unwrap();
        let check = Watch::new(&layout, sources()).check();
        assert_eq!(check.state, CheckState::Pass, "{check:?}");
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
            pi_releases: "file:///nowhere".into(),
        };
        let images = Images::new(layout.linux_dir());
        refresh(&images, &layout.agents_dir(), &sources, false);
        let layers = images.layers().unwrap();
        assert_eq!(layers.base.version, 1);
        assert_eq!(layers.managed.as_ref().unwrap().0, "2.1.308");

        // The next day: a new release, the next boot's layer. The old one,
        // which no agent uses, goes the round after.
        publish_claude(&published.join("claude"), "2.1.314", b"second");
        refresh(&images, &layout.agents_dir(), &sources, false);
        assert_eq!(images.managed().unwrap().0, "2.1.314");
        let old = layout.linux_dir().join("managed-2.1.308.img");
        assert!(old.exists());
        refresh(&images, &layout.agents_dir(), &sources, false);
        assert!(!old.exists());
        assert_eq!(images.managed().unwrap().0, "2.1.314");

        // Unreachable sources are logged, not fatal, and nothing is lost.
        let gone = Sources {
            base_releases: "file:///nowhere".into(),
            base_release: 2,
            claude_releases: "file:///nowhere".into(),
            pi_releases: "file:///nowhere".into(),
        };
        refresh(&images, &layout.agents_dir(), &gone, false);
        assert_eq!(images.base().unwrap().version, 1);
        assert_eq!(images.managed().unwrap().0, "2.1.314");
    }

    #[test]
    fn the_watch_says_how_the_download_goes() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path().join("home"), "macos");
        let published = tmp.path().join("published");
        let sources = Sources {
            base_releases: format!("file://{}", published.join("base").display()),
            base_release: 1,
            claude_releases: format!("file://{}", published.join("claude").display()),
            pi_releases: "file:///nowhere".into(),
        };
        let watch = Watch::new(&layout, sources);
        // Before the first round: working, with no progress yet, and
        // nothing to retry.
        let c = watch.check();
        assert_eq!(
            (c.state, c.fixable, c.progress),
            (CheckState::Working, false, None)
        );
        assert_eq!(c.detail, "Downloading the Linux base image.");

        // Nothing published: the round fails, and says so, retryably.
        let done = watch.refresh();
        assert!(done.base_error.is_some() && done.claude_error.is_some());
        let c = watch.check();
        assert_eq!((c.state, c.fixable), (CheckState::Fail, true));
        assert!(
            c.detail.starts_with("Can't download file://"),
            "{}",
            c.detail
        );
        assert!(c.detail.ends_with('.'));
        assert!(c.hint.is_some());

        // A retry asked for shows as under way at once, and stays so
        // while a round that began before it ends.
        lock(&watch.state).running = true;
        watch.kick();
        assert_eq!(watch.check().state, CheckState::Working);
        lock(&watch.state).running = false;
        assert_eq!(watch.check().state, CheckState::Working);

        // Mid-download, the manifest says how far along it is.
        let downloads = layout.linux_dir().join("downloads");
        fs::create_dir_all(&downloads).unwrap();
        fs::write(
            downloads.join("base.json"),
            r#"{"version": 1, "files": {"Image": {"sha256": "0", "size": 50},
                "initrd": {"sha256": "0", "size": 50}, "base.img.gz": {"sha256": "0", "size": 900}}}"#,
        )
        .unwrap();
        fs::write(downloads.join("base.img.gz"), [0u8; 450]).unwrap();
        // The kernel's checked and kept in the staging directory.
        let staged = layout.linux_dir().join(".base-1.partial");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("Image"), "k").unwrap();
        assert_eq!(watch.check().progress, Some(50));
        fs::remove_dir_all(&downloads).unwrap();
        fs::remove_dir_all(&staged).unwrap();

        // Published, the next round has both, and the check passes.
        publish_base(&published.join("base"));
        publish_claude(&published.join("claude"), "2.1.314", b"claude");
        assert_eq!(watch.refresh(), Refreshed::default());
        let c = watch.check();
        assert_eq!(
            (c.state, c.fixable, c.hint),
            (CheckState::Pass, false, None)
        );
        assert_eq!(
            c.detail,
            "Linux base image 1 and Claude Code 2.1.314 are ready for linux agents."
        );
    }

    #[test]
    fn percents_stay_under_100_until_the_end() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(0, 10), 0);
        assert_eq!(percent(999, 1000), 99);
        assert_eq!(percent(1000, 1000), 100);
        assert_eq!(percent(2000, 1000), 100);
        assert_eq!(percent(u64::MAX - 1, u64::MAX), 99);
        assert_eq!(sentence("can't download x"), "Can't download x.");
    }

    #[test]
    fn only_a_mac_that_runs_pi_downloads_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pi");
        std::fs::create_dir_all(&root).unwrap();
        let sources = Sources {
            base_releases: "file:///nowhere".into(),
            base_release: 1,
            claude_releases: "file:///nowhere".into(),
            pi_releases: format!("file://{}", root.display()),
        };
        let layout = Layout::new(tmp.path(), "macos");
        let watch = Watch::new(&layout, sources);
        // Pi publishes nothing here, so a round that asks would say so.
        assert_eq!(watch.refresh().pi_error, None, "not asked for");
        watch.want_pi();
        assert!(watch.refresh().pi_error.is_some());
    }
}
