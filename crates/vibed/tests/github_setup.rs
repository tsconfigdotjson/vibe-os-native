//! Adding a GitHub project on a fresh host (#73): it works the first time, nobody
//! runs anything, and no path turns off SSH's checking of a host key.
//!
//! Every process is real but the network: real git, with a stand-in for `ssh` that
//! does what OpenSSH does with `known_hosts`, and one for `gh` that clones as gh
//! does, by its protocol, and fails as it does. `doctor::Machine` runs the real
//! `git config` and that `gh auth setup-git` in a fresh home.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use common::{Golden, write_script};
use vibe_core::model::{GhState, ProjectSource};
use vibe_core::rpc::{ErrorCode, GhStatusParams, ProjectAddParams};
use vibed::core::Core;
use vibed::doctor::Machine;
use vibed::gh::GITHUB_HOST_KEYS;

/// What the stand-in for `ssh` does for each of the things that can be wrong.
const SSH: &str = r#"#!/bin/sh
# As OpenSSH does, and with no terminal to ask on: a host it has no key for is
# refused. Anything asking it not to check is what must never happen.
printf '%s\n' "$*" >>"$WORLD/ssh-args"
case "$*" in
  *StrictHostKeyChecking*|*UserKnownHostsFile*|*GlobalKnownHostsFile*|*VerifyHostKeyDNS*)
    echo "ssh was told not to check: $*" >>"$WORLD/ssh-tampered"
    exit 99 ;;
esac
if [ -f "$WORLD/ssh-host-key-changed" ]; then
  echo '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@' >&2
  echo '@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @' >&2
  echo 'Host key verification failed.' >&2
  exit 255
fi
if ! grep -q '^github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl$' "$HOME/.ssh/known_hosts" 2>/dev/null; then
  echo 'No ED25519 host key is known for github.com and you have requested strict checking.' >&2
  echo 'Host key verification failed.' >&2
  exit 255
fi
if [ -f "$WORLD/ssh-no-key" ]; then
  echo 'git@github.com: Permission denied (publickey).' >&2
  exit 255
fi
# Past the host key, and let in. As OpenSSH reads them: options, as git sends
# `-o SendEnv=GIT_PROTOCOL`, then the host, then `git-upload-pack 'owner/name.git'`.
while [ $# -gt 0 ]; do
  case $1 in
  --) shift; break ;;
  -[BDEFIJLOPQRSWbceilmopw]) shift 2 ;;
  -*) shift ;;
  *) break ;;
  esac
done
shift
# Joined first: `${*##*/}` is each argument's in some shells, macOS's `sh` among them.
command=$*
name=${command##*/}
name=${name%.git\'}
exec git-upload-pack "$WORLD/remotes/$name"
"#;

/// What the stand-in for `gh` does: its login is `$WORLD/logged-in`, and its
/// protocol its real config's. `repo clone` asks git, by that protocol, as gh does,
/// and says "failed to run git" as gh does.
const GH: &str = r#"#!/bin/sh
config="$HOME/.config/gh/protocol"
case "$1 $2" in
"auth status")
  if [ -f "$WORLD/bad-token" ] || [ -f "$WORLD/network-down" ]; then
    # As Ubuntu's gh 2.46 does: it says the login failed, and exits 0.
    echo "github.com"
    echo "  X Failed to log in to github.com using token (hosts.yml)"
    echo "  - To re-authenticate, run: gh auth login -h github.com"
    exit 0
  fi
  if [ -f "$WORLD/logged-in" ]; then
    echo "  ✓ Logged in to github.com account me (keyring)"
    exit 0
  fi
  echo "You are not logged into any GitHub hosts. To log in, run: gh auth login" >&2
  exit 1 ;;
"auth setup-git")
  if [ ! -f "$WORLD/logged-in" ]; then
    echo 'You are not logged into the GitHub host "github.com". Run gh auth login -h github.com' >&2
    exit 1
  fi
  echo "ran" >>"$WORLD/setup-git"
  exec git config --global --add credential.https://github.com.helper '!gh auth git-credential' ;;
"auth git-credential")
  exit 0 ;;
"api meta")
  if [ -f "$WORLD/network-down" ] || [ -f "$WORLD/bad-token" ] || [ -f "$WORLD/meta-down" ] || [ ! -f "$WORLD/logged-in" ]; then
    echo 'gh: Requires authentication (HTTP 401)' >&2
    exit 1
  fi
  echo "ran" >>"$WORLD/asked-meta"
  exec cat "$WORLD/published-keys" ;;
"api user")
  if [ -f "$WORLD/network-down" ]; then
    echo 'Get "https://api.github.com/user": dial tcp: connect: connection refused' >&2
    exit 1
  fi
  if [ -f "$WORLD/bad-token" ]; then
    echo 'gh: Bad credentials (HTTP 401)' >&2
    exit 1
  fi
  echo me ;;
"config set")
  # The owner's setting: nothing here is to touch it.
  echo "$*" >>"$WORLD/config-writes"
  printf '%s\n' "$4" >"$config"
  exit 0 ;;
"repo clone")
  if [ -f "$WORLD/network-down" ]; then
    echo 'Post "https://api.github.com/graphql": dial tcp: connect: connection refused' >&2
    exit 1
  fi
  if [ -f "$WORLD/bad-token" ]; then
    echo 'HTTP 401: Bad credentials (https://api.github.com/graphql)' >&2
    echo 'Try authenticating with:  gh auth login' >&2
    exit 1
  fi
  if [ ! -f "$WORLD/logged-in" ]; then
    echo 'To get started with GitHub CLI, please run:  gh auth login' >&2
    echo 'Alternatively, populate the GH_TOKEN environment variable with a GitHub API authentication token.' >&2
    exit 4
  fi
  # A URL says how, as it does to gh. A name goes by gh's setting.
  case $3 in
  https://github.com/*)
    repo=${3#https://github.com/}
    how=https ;;
  *)
    repo=$3
    how=$(cat "$config") ;;
  esac
  name=${repo#*/}
  if [ ! -d "$WORLD/remotes/$name" ]; then
    echo "GraphQL: Could not resolve to a Repository with the name '$repo'. (repository)" >&2
    exit 1
  fi
  echo "$how" >>"$WORLD/clones"
  case $how in
  ssh) git clone -q "git@github.com:$repo.git" "$4" ;;
  *)
    # Over HTTPS, GitHub is a directory here, and its name is put back after.
    git clone -q "$WORLD/remotes/$name" "$4" &&
      git -C "$4" remote set-url origin "https://github.com/$repo.git" ;;
  esac || {
    echo "failed to run git: exit status 128" >&2
    exit 1
  } ;;
*)
  echo "stand-in gh: unexpected $*" >&2
  exit 2 ;;
esac
"#;

struct Fresh {
    golden: Golden,
    core: Arc<Core>,
    /// Holds what the stand-ins look at, and what they note.
    world: PathBuf,
}

/// A fresh host: a home with no `.ssh`, no git config and `gh` with `protocol`.
fn fresh_host(protocol: &str, logged_in: bool) -> Fresh {
    let golden = Golden::new();
    let root = golden.dir.path().canonicalize().unwrap();
    let world = root.join("world");
    let bin = root.join("host-bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&world).unwrap();
    assert!(!golden.home.join(".ssh").exists(), "it's fresh");
    assert!(!golden.home.join(".gitconfig").exists(), "it's fresh");
    fs::create_dir_all(golden.home.join(".config/gh")).unwrap();
    fs::write(golden.home.join(".config/gh/protocol"), protocol).unwrap();
    fs::create_dir_all(world.join("remotes")).unwrap();
    common::repo(&world.join("remotes/web"));
    if logged_in {
        fs::write(world.join("logged-in"), "").unwrap();
    }
    // What GitHub publishes, as `gh api meta` would say.
    fs::write(
        world.join("published-keys"),
        GITHUB_HOST_KEYS.join("\n") + "\n",
    )
    .unwrap();
    // `gh` and `ssh` find what they need, wherever `vibed` runs them from, and see
    // this fresh home as a real gh and ssh would see the real one.
    let env = format!(
        "#!/bin/sh\nexport WORLD={w} HOME={h} GIT_SSH={b}/ssh-real \
         GIT_CONFIG_NOSYSTEM=1 GIT_SSH_VARIANT=ssh PATH={b}:$PATH\n",
        w = world.display(),
        h = golden.home.display(),
        b = bin.display()
    );
    let body = |script: &str| format!("{env}{}", script.strip_prefix("#!/bin/sh\n").unwrap());
    write_script(&bin.join("gh"), &body(GH));
    write_script(&bin.join("ssh-real"), &body(SSH));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut config = golden.config();
    config.gh = bin.join("gh").into();
    config.checks = Arc::new(Machine::new(&golden.home).with_path(path));
    let core = Arc::new(Core::open(config).unwrap());
    Fresh {
        golden,
        core,
        world,
    }
}

impl Fresh {
    async fn add(&self) -> Result<vibe_core::rpc::Project, vibe_core::rpc::ErrorObject> {
        self.core
            .add_project(ProjectAddParams {
                repo: Some("me/web".into()),
                ..Default::default()
            })
            .await
    }

    fn known_hosts(&self) -> String {
        fs::read_to_string(self.golden.home.join(".ssh/known_hosts")).unwrap_or_default()
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.world.join(name)).unwrap_or_default()
    }

    fn protocol(&self) -> String {
        fs::read_to_string(self.golden.home.join(".config/gh/protocol"))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn origin(&self, path: &str) -> String {
        let out = std::process::Command::new("git")
            .args(["-C", path, "remote", "get-url", "origin"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// What's asked of every path: nothing told SSH not to check a host key, on its
    /// command line or anywhere it or git would read it from.
    fn assert_ssh_checks_host_keys(&self) {
        assert_eq!(self.read("ssh-tampered"), "");
        assert!(!self.golden.home.join(".ssh/config").exists());
        let git_config =
            fs::read_to_string(self.golden.home.join(".gitconfig")).unwrap_or_default();
        assert!(!git_config.contains("sshCommand"), "{git_config}");
    }

    /// `gh`'s setting, ssh in these, is the owner's: nothing writes to it.
    fn assert_gh_left_alone(&self) {
        assert_eq!(self.protocol(), "ssh");
        assert_eq!(self.read("config-writes"), "");
    }

    /// git asks gh, in the home, for GitHub's credentials.
    fn assert_git_uses_gh(&self) {
        let config = fs::read_to_string(self.golden.home.join(".gitconfig")).unwrap();
        assert!(config.contains("!gh auth git-credential"), "{config}");
    }
}

/// `#73`, as it happened: ssh, and no github.com in `known_hosts`.
#[tokio::test]
async fn ssh_with_no_known_hosts_clones_the_first_time() {
    let w = fresh_host("ssh", true);
    let project = w.add().await.unwrap();
    assert_eq!(project.source, ProjectSource::Github);
    // Over SSH, as it was asked to be, and at the first try.
    assert_eq!(w.read("clones"), "ssh\n");
    w.assert_gh_left_alone();
    assert_eq!(w.origin(&project.path), "git@github.com:me/web.git");
    let known = w.known_hosts();
    for key in GITHUB_HOST_KEYS {
        assert!(known.contains(&format!("github.com {key}\n")), "{known}");
    }
    w.assert_git_uses_gh();
    w.assert_ssh_checks_host_keys();
    assert!(!w.read("ssh-args").is_empty(), "it went through ssh");
}

/// gh's other protocol, with `gh auth setup-git` never run.
#[tokio::test]
async fn https_with_no_setup_git_clones_the_first_time() {
    let w = fresh_host("https", true);
    let project = w.add().await.unwrap();
    assert_eq!(w.read("clones"), "https\n");
    assert_eq!(w.origin(&project.path), "https://github.com/me/web.git");
    w.assert_git_uses_gh();
    assert_eq!(w.read("setup-git"), "ran\n");
    // Not asked to do it again.
    let again = fresh_again(&w).await;
    assert_eq!(again, "ran\n");
    assert_eq!(w.read("ssh-args"), "", "it had no call to use ssh");
}

async fn fresh_again(w: &Fresh) -> String {
    w.core.check_gh().await;
    w.read("setup-git")
}

/// No SSH key this account knows: over HTTPS, with gh's login, for this clone. gh's
/// own setting is the owner's, and stays.
#[tokio::test]
async fn ssh_with_no_key_goes_over_https_and_leaves_gh_as_it_is() {
    let w = fresh_host("ssh", true);
    fs::write(w.world.join("ssh-no-key"), "").unwrap();
    let project = w.add().await.unwrap();
    assert_eq!(w.read("clones"), "ssh\nhttps\n");
    w.assert_gh_left_alone();
    assert_eq!(w.origin(&project.path), "https://github.com/me/web.git");
    w.assert_ssh_checks_host_keys();
}

/// One add's going round SSH doesn't get in another's way, however they overlap.
#[tokio::test]
async fn adds_at_once_all_go_round_ssh_with_no_key() {
    let w = fresh_host("ssh", true);
    fs::write(w.world.join("ssh-no-key"), "").unwrap();
    for name in ["one", "two"] {
        common::repo(&w.world.join("remotes").join(name));
    }
    let add = |repo: &str| {
        w.core.add_project(ProjectAddParams {
            repo: Some(repo.into()),
            ..Default::default()
        })
    };
    let (web, one, two) = tokio::join!(add("me/web"), add("me/one"), add("me/two"));
    for added in [web, one, two] {
        added.unwrap();
    }
    w.assert_gh_left_alone();
}

/// A credential helper the owner keeps just for GitHub is theirs. `gh auth setup-git`
/// would replace it, at startup and every hour after.
#[tokio::test]
async fn a_helper_of_the_owners_own_for_github_stays() {
    let w = fresh_host("https", true);
    let mine = "[credential \"https://github.com\"]\n\thelper = /opt/custom-helper\n";
    let config = w.golden.home.join(".gitconfig");
    fs::write(&config, mine).unwrap();
    w.core.check_gh().await;
    w.add().await.unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap(), mine);
    assert_eq!(w.read("setup-git"), "");

    // And it's called what it is.
    use vibe_core::model::CheckId;
    let report = w.core.doctor().await;
    let check = report.checks.iter().find(|c| c.id == CheckId::GhGit);
    let detail = &check.unwrap().detail;
    assert!(detail.contains("of your own"), "{detail}");
    assert!(!detail.contains("asks gh"), "{detail}");
}

/// A host key that doesn't match what's pinned, as a rotation or someone in the way
/// would look: SSH keeps refusing it, with nothing turned off, and HTTPS has its own
/// check of who it's talking to.
#[tokio::test]
async fn a_host_key_that_changed_is_never_accepted() {
    let w = fresh_host("ssh", true);
    fs::write(w.world.join("ssh-host-key-changed"), "").unwrap();
    let project = w.add().await.unwrap();
    assert_eq!(w.read("clones"), "ssh\nhttps\n");
    assert_eq!(w.origin(&project.path), "https://github.com/me/web.git");
    w.assert_gh_left_alone();
    w.assert_ssh_checks_host_keys();
    let args = w.read("ssh-args");
    assert!(args.contains("git@github.com "), "{args}");
    assert_eq!(args.lines().count(), 1, "tried once: {args}");
}

/// It's the one thing only the owner can do: said plainly, with gh's own words, and
/// with nothing changed on the way, nor a retry that couldn't help.
#[tokio::test]
async fn logged_out_says_so_with_what_gh_said() {
    let w = fresh_host("ssh", false);
    let e = w.add().await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert!(
        e.message.starts_with("GitHub isn't signed in on mac-mini"),
        "{}",
        e.message
    );
    assert!(e.message.contains("me/web"), "{}", e.message);
    assert!(
        e.message.contains("To get started with GitHub CLI"),
        "gh's own words: {}",
        e.message
    );
    assert_eq!(w.read("clones"), "", "no clone was tried");
    w.assert_gh_left_alone();
    assert!(
        !w.golden.home.join("projects/web").exists()
            && !w.golden.home.join("VibeOS/projects/web").exists()
    );
    // The app learns it too. GitHub can't be asked which of its keys still stand, so
    // none goes in yet.
    let status = w.core.gh_status(GhStatusParams::default()).await;
    assert_eq!(status.state, GhState::LoggedOut);
    assert_eq!(w.known_hosts(), "");

    // Signed in, the very same thing just works.
    fs::write(w.world.join("logged-in"), "").unwrap();
    w.add().await.unwrap();
    w.assert_git_uses_gh();
    assert!(w.known_hosts().contains("github.com ssh-ed25519"));
}

/// A token gone bad reads as a working login to whoever trusts gh's exit, on a gh as
/// old as Ubuntu's. It's the same thing to ask of the owner.
#[tokio::test]
async fn a_bad_token_is_said_as_a_sign_in_not_a_failed_command() {
    let w = fresh_host("ssh", true);
    fs::write(w.world.join("bad-token"), "").unwrap();
    let e = w.add().await.unwrap_err();
    assert!(
        e.message.starts_with("GitHub isn't signed in on mac-mini"),
        "{}",
        e.message
    );
    assert!(
        e.message.contains("HTTP 401: Bad credentials"),
        "{}",
        e.message
    );
    w.assert_gh_left_alone();
    let status = w.core.gh_status(GhStatusParams::default()).await;
    assert_eq!(status.state, GhState::LoggedOut);
}

/// A good token with no network: real gh's `auth status` calls that a failed login
/// and an invalid token, and it's no use telling the owner to sign in.
#[tokio::test]
async fn a_network_that_is_down_is_not_called_signed_out() {
    let w = fresh_host("ssh", true);
    fs::write(w.world.join("network-down"), "").unwrap();
    let e = w.add().await.unwrap_err();
    assert!(
        e.message.starts_with("gh repo clone me/web failed: "),
        "{}",
        e.message
    );
    assert!(e.message.contains("connection refused"), "{}", e.message);
    assert!(!e.message.contains("gh auth login"), "{}", e.message);
    w.assert_gh_left_alone();
    assert_eq!(w.core.check_gh().await.state, GhState::Error);
}

/// A repo that isn't there is no fault of SSH's, so there's nothing to go round.
#[tokio::test]
async fn a_missing_repo_changes_nothing_and_says_what_gh_said() {
    let w = fresh_host("ssh", true);
    let e = w
        .core
        .add_project(ProjectAddParams {
            repo: Some("me/missing".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        e.message,
        "gh repo clone me/missing failed: GraphQL: Could not resolve to a Repository with the \
         name 'me/missing'. (repository)"
    );
    w.assert_gh_left_alone();
}

/// What wouldn't clone either way says git's real words, not just "exit status 128",
/// and gh is as it was.
#[tokio::test]
async fn a_clone_that_fails_either_way_leaves_gh_as_it_was() {
    let w = fresh_host("ssh", true);
    fs::write(w.world.join("ssh-no-key"), "").unwrap();
    // Nothing for HTTPS to fetch either.
    let remotes = w.world.join("remotes/web");
    fs::remove_dir_all(remotes.join(".git")).ok();
    fs::write(remotes.join("HEAD"), "not a repo").unwrap();
    let e = w.add().await.unwrap_err();
    assert_eq!(w.read("clones"), "ssh\nhttps\n");
    w.assert_gh_left_alone();
    assert!(e.message.contains("exit status 128"), "{}", e.message);
    assert!(
        e.message.contains("fatal:") || e.message.contains("does not appear"),
        "git's own words: {}",
        e.message
    );
}

/// At startup, with nothing asked: what `vibed` does for a host as it comes up.
#[tokio::test]
async fn a_check_of_gh_sets_it_all_up_by_itself() {
    let w = fresh_host("ssh", true);
    w.core.check_gh().await;
    assert!(w.known_hosts().contains("github.com ssh-ed25519"));
    w.assert_git_uses_gh();
    // Quiet about it, each time after.
    let (known, config) = (
        w.known_hosts(),
        fs::read_to_string(w.golden.home.join(".gitconfig")).unwrap(),
    );
    w.core.check_gh().await;
    assert_eq!(w.known_hosts(), known);
    assert_eq!(
        fs::read_to_string(w.golden.home.join(".gitconfig")).unwrap(),
        config
    );
    assert_eq!(w.read("setup-git"), "ran\n");

    assert_eq!(w.read("asked-meta"), "ran\n", "only while one was missing");

    // Logged out, the keys and git wait for it.
    let w = fresh_host("https", false);
    w.core.check_gh().await;
    assert_eq!(w.known_hosts(), "");
    assert!(!w.golden.home.join(".gitconfig").exists());
    assert_eq!(w.read("setup-git"), "");
}

/// A build in the field, after GitHub rotates a key: the one it no longer publishes
/// stays out, and a clone over SSH goes on as it would have.
#[tokio::test]
async fn a_key_github_has_dropped_is_not_pinned_by_a_build_still_carrying_it() {
    let w = fresh_host("ssh", true);
    let [ed25519, ecdsa, rsa] = GITHUB_HOST_KEYS;
    fs::write(
        w.world.join("published-keys"),
        format!("{ed25519}\n{ecdsa}\n"),
    )
    .unwrap();
    let project = w.add().await.unwrap();
    let known = w.known_hosts();
    assert!(
        known.contains(&format!("github.com {ed25519}\n")),
        "{known}"
    );
    assert!(!known.contains(rsa), "{known}");
    assert_eq!(w.origin(&project.path), "git@github.com:me/web.git");
    w.assert_ssh_checks_host_keys();
}

/// GitHub can't be asked: nothing goes in on the strength of what the build remembers,
/// and SSH refusing an unknown host still leaves HTTPS.
#[tokio::test]
async fn no_answer_from_github_pins_nothing_and_the_clone_goes_on() {
    let w = fresh_host("ssh", true);
    fs::write(w.world.join("meta-down"), "").unwrap();
    let project = w.add().await.unwrap();
    assert_eq!(w.known_hosts(), "");
    assert_eq!(w.origin(&project.path), "https://github.com/me/web.git");
    w.assert_gh_left_alone();
    w.assert_ssh_checks_host_keys();
}

/// `vibe doctor` has nothing to say about what's been done for the owner.
#[tokio::test]
async fn the_doctor_lists_nothing_vibeos_can_do_itself() {
    use vibe_core::model::{CheckId, CheckState};
    let w = fresh_host("https", true);
    let report = w.core.doctor().await;
    let check = report
        .checks
        .iter()
        .find(|c| c.id == CheckId::GhGit)
        .unwrap();
    // gh's status here is what `vibed` was last told, and the doctor looks again.
    assert_eq!(check.state, CheckState::Pass, "{check:?}");
    assert_eq!(check.hint, None);
    assert!(!check.fixable);
    w.assert_git_uses_gh();

    let w = fresh_host("https", false);
    let report = w.core.doctor().await;
    let check = report
        .checks
        .iter()
        .find(|c| c.id == CheckId::GhGit)
        .unwrap();
    assert_eq!(check.state, CheckState::Optional, "{check:?}");
    assert_eq!(check.hint, None);
    assert!(!check.fixable);
}
