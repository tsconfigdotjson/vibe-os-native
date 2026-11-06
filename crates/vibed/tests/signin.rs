//! Signing in with one click, and tools that install themselves (#74). The flows are
//! stand-ins, which say what the real `gh` 2.103.3 and Claude Code 2.1.318 say with no
//! terminal. What `vibed` does with them, and with its children, is real.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Golden, golden_checks, write_script};
#[cfg(target_os = "linux")]
use vibe_core::model::{CheckId, CheckState};
use vibe_core::model::{GhState, SignInState, SignInWhat, ToolboxState};
#[cfg(target_os = "linux")]
use vibe_core::rpc::DoctorFixParams;
use vibe_core::rpc::{
    ErrorCode, NoParams, SignIn, SignInAnswerParams, SignInGetParams, SignInParams,
    SignInStartParams,
};
use vibe_env::testing::TestToolbox;
use vibed::core::Core;
use vibed::doctor::FixedChecks;

struct World {
    golden: Golden,
    core: Arc<Core>,
    bin: PathBuf,
}

fn world_with(checks: impl FnOnce(&Path, FixedChecks) -> FixedChecks) -> World {
    let golden = Golden::new();
    let bin = golden.dir.path().join("flows");
    fs::create_dir_all(&bin).unwrap();
    let mut config = golden.config();
    config.gh = bin.join("gh").into_os_string();
    config.checks = Arc::new(checks(&bin, golden_checks()));
    let core = Arc::new(Core::open(config).unwrap());
    World { golden, core, bin }
}

const GH: &str = r#"#!/bin/sh
d=$(dirname "$0")
case "$1 $2" in
"auth login")
  echo "$*" > "$d/gh-login-args"
  env > "$d/gh-login-env"
  echo
  echo '! First copy your one-time code: B177-C539' >&2
  echo 'Open this URL to continue in your web browser: https://github.com/login/device' >&2
  # Until a person's finished at the page.
  i=0
  while [ ! -e "$d/approved" ]; do
    i=$((i + 1)); [ "$i" -gt 400 ] && { echo 'it timed out' >&2; exit 1; }
    sleep 0.05
  done
  if [ -e "$d/denied" ]; then echo 'error: access_denied' >&2; exit 1; fi
  echo logged-in > "$d/gh-logged-in"
  ;;
"auth token") echo gho_the_token ;;
"auth status")
  if [ -e "$d/gh-logged-in" ]; then
    echo '  ✓ Logged in to github.com account me (keyring)'
  else
    echo 'You are not logged into any GitHub hosts. To log in, run: gh auth login' >&2
    exit 1
  fi
  ;;
*) ;;
esac
"#;

const CLAUDE: &str = r#"#!/bin/sh
d=$(dirname "$0")
echo 'Opening browser to sign in…'
echo "If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c"
printf 'Paste code here if prompted > '
while read -r code; do
  echo "$code" > "$d/claude-code"
  [ "$code" = good ] && exit 0
  # As the real one does: says so on stderr, and goes on waiting.
  echo 'Invalid code. Please make sure the full code was copied.' >&2
done
exit 3
"#;

impl World {
    async fn start(&self, what: SignInWhat) -> SignIn {
        self.core
            .sign_in_start(SignInStartParams { host: None, what })
            .await
            .unwrap()
    }

    /// Waits until `ok` is true of it.
    async fn until(&self, mut now: SignIn, ok: impl Fn(&SignIn) -> bool) -> SignIn {
        for _ in 0..200 {
            if ok(&now) {
                return now;
            }
            now = self
                .core
                .sign_in_get(SignInGetParams {
                    host: None,
                    id: now.id.clone(),
                    seen: Some(now.seq),
                })
                .await
                .unwrap();
        }
        panic!("it never came: {now:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_says_its_page_and_code_and_a_person_finishes_there() {
    let w = world_with(|_, c| c);
    write_script(&w.bin.join("gh"), GH);
    let check = w.core.check_gh().await;
    assert_eq!(check.state, GhState::LoggedOut, "{check:?}");
    let now = w.start(SignInWhat::Github).await;
    assert_eq!(now.state, SignInState::Running);
    let now = w.until(now, |s| s.url.is_some() && s.code.is_some()).await;
    assert_eq!(now.code.as_deref(), Some("B177-C539"));
    assert_eq!(now.url.as_deref(), Some("https://github.com/login/device"));
    assert!(!now.asks_for_code && !now.opens_itself);
    // `gh`'s protocol is the owner's: it isn't set here (#73).
    let args = fs::read_to_string(w.bin.join("gh-login-args")).unwrap();
    assert!(args.contains("--web"), "{args}");
    assert!(!args.contains("git-protocol"), "{args}");
    // Nothing's waiting on a terminal.
    let env = fs::read_to_string(w.bin.join("gh-login-env")).unwrap();
    assert!(env.contains("GH_PROMPT_DISABLED=1"), "{env}");

    fs::write(w.bin.join("approved"), "").unwrap();
    let now = w.until(now, |s| s.state != SignInState::Running).await;
    assert_eq!((now.state, now.error.clone()), (SignInState::Done, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_says_what_was_said() {
    let w = world_with(|_, c| c);
    write_script(&w.bin.join("gh"), GH);
    fs::write(w.bin.join("denied"), "").unwrap();
    fs::write(w.bin.join("approved"), "").unwrap();
    let now = w.start(SignInWhat::Github).await;
    let now = w.until(now, |s| s.state != SignInState::Running).await;
    assert_eq!(now.state, SignInState::Failed);
    assert_eq!(now.error.as_deref(), Some("error: access_denied"));
}

fn with_claude(bin: &Path, checks: FixedChecks) -> FixedChecks {
    FixedChecks {
        claude: Ok((bin.join("claude"), "2.1.318 (Claude Code)".into())),
        ..checks
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_takes_the_code_its_page_showed() {
    let w = world_with(with_claude);
    write_script(&w.bin.join("claude"), CLAUDE);
    let now = w.start(SignInWhat::Claude).await;
    // Its last line has no end, and it's read all the same.
    let now = w.until(now, |s| s.asks_for_code && s.url.is_some()).await;
    assert!(
        now.url
            .as_deref()
            .unwrap()
            .starts_with("https://claude.com/")
    );
    // What couldn't be a code never reaches it.
    let refused = w
        .core
        .sign_in_answer(SignInAnswerParams {
            host: None,
            id: now.id.clone(),
            value: "two\nlines".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidParams.code());
    assert!(!w.bin.join("claude-code").exists());

    w.core
        .sign_in_answer(SignInAnswerParams {
            host: None,
            id: now.id.clone(),
            value: "good".into(),
        })
        .await
        .unwrap();
    let now = w.until(now, |s| s.state != SignInState::Running).await;
    assert_eq!(now.state, SignInState::Done, "{now:?}");
    assert_eq!(
        fs::read_to_string(w.bin.join("claude-code")).unwrap(),
        "good\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_code_says_so_and_another_can_be_given() {
    let w = world_with(with_claude);
    write_script(&w.bin.join("claude"), CLAUDE);
    let now = w.start(SignInWhat::Claude).await;
    let now = w.until(now, |s| s.asks_for_code).await;
    let answer = |value: &str| {
        w.core.sign_in_answer(SignInAnswerParams {
            host: None,
            id: now.id.clone(),
            value: value.into(),
        })
    };
    answer("bad").await.unwrap();
    let said = w.until(now.clone(), |s| s.error.is_some()).await;
    // It's still going, asking again, with why.
    assert_eq!(said.state, SignInState::Running);
    assert!(said.asks_for_code);
    assert!(said.error.as_deref().unwrap().starts_with("Invalid code"));
    answer("good").await.unwrap();
    let done = w.until(said, |s| s.state != SignInState::Running).await;
    assert_eq!(done.state, SignInState::Done, "{done:?}");
    assert_eq!(done.error, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn giving_up_ends_the_flow_and_starting_again_replaces_it() {
    let w = world_with(with_claude);
    write_script(&w.bin.join("claude"), CLAUDE);
    let first = w.start(SignInWhat::Claude).await;
    let first = w.until(first, |s| s.asks_for_code).await;
    // A second click: there's one at a time.
    let second = w.start(SignInWhat::Claude).await;
    assert_eq!(
        w.core
            .sign_in_get(SignInGetParams {
                host: None,
                id: first.id.clone(),
                seen: None,
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound.code()
    );
    let second = w.until(second, |s| s.asks_for_code).await;
    let gone = w
        .core
        .sign_in_cancel(SignInParams {
            host: None,
            id: second.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(gone.state, SignInState::Cancelled);
    // Nothing's left to tell.
    assert!(!w.bin.join("claude-code").exists());
}

/// A flow that never finishes is let go, and an unknown one is said to be.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_one_is_said_to_be() {
    let w = world_with(|_, c| c);
    let e = w
        .core
        .sign_in_get(SignInGetParams {
            host: None,
            id: "nope".into(),
            seen: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound.code());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linux_agents_github_goes_on_into_the_toolbox_and_leaves_nothing_here() {
    let w = world_with(|_, c| c);
    write_script(&w.bin.join("gh"), GH);
    // Background work only runs where the core knows itself, as in vibed.
    w.core.spawn_links();
    let now = w.start(SignInWhat::ToolboxGithub).await;
    let now = w.until(now, |s| s.code.is_some()).await;
    // In a place of its own: the owner's own login on this Mac isn't touched.
    let env = fs::read_to_string(w.bin.join("gh-login-env")).unwrap();
    let config = env
        .lines()
        .find_map(|l| l.strip_prefix("GH_CONFIG_DIR="))
        .expect("a config of its own")
        .to_owned();
    assert!(Path::new(&config).is_dir());
    let args = fs::read_to_string(w.bin.join("gh-login-args")).unwrap();
    assert!(args.contains("--insecure-storage"), "{args}");

    fs::write(w.bin.join("approved"), "").unwrap();
    let now = w.until(now, |s| s.state != SignInState::Running).await;
    assert_eq!(now.state, SignInState::Done, "{now:?}");
    let layout = vibe_core::paths::Layout::new(&w.golden.home, "macos");
    let toolbox = layout.agent_dir(vibe_env::toolbox::TOOLBOX_REF);
    assert_eq!(
        fs::read_to_string(toolbox.join("github-token")).unwrap(),
        "gho_the_token"
    );
    assert!(!Path::new(&config).exists(), "the scratch went");
}

/// The window closed after approving, with a shell opened during the browser step: the
/// login's kept, the toolbox isn't wedged, and the shell's state isn't disturbed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approved_login_outlasts_the_window_and_waits_for_a_shell_opened_meanwhile() {
    let golden = Golden::new();
    let bin = golden.dir.path().join("flows");
    fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("gh"), GH);
    let release = bin.join("release");
    let shell = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done",
        release.display()
    );
    let layout = vibe_core::paths::Layout::new(&golden.home, "macos");
    let mut config = golden.config();
    config.gh = bin.join("gh").into_os_string();
    config.checks = Arc::new(golden_checks());
    config.toolbox = Some(Arc::new(TestToolbox::new(
        layout.clone(),
        vec!["sh".into(), "-c".into(), shell],
    )));
    let core = Arc::new(Core::open(config).unwrap());
    core.spawn_links();
    let w = World {
        golden,
        core: Arc::clone(&core),
        bin,
    };
    let state = async || core.toolbox_status(NoParams {}).await.unwrap().state;

    let now = w.start(SignInWhat::ToolboxGithub).await;
    let now = w.until(now, |s| s.code.is_some()).await;
    let env = fs::read_to_string(w.bin.join("gh-login-env")).unwrap();
    let scratch = env
        .lines()
        .find_map(|l| l.strip_prefix("GH_CONFIG_DIR="))
        .unwrap()
        .to_owned();
    core.open_toolbox(NoParams {}).await.unwrap();
    fs::write(w.bin.join("approved"), "").unwrap();
    let now = w.until(now, |s| s.finishing).await;

    // Closing the window gives up on what's under way, but not on this.
    let after = core
        .sign_in_cancel(SignInParams {
            host: None,
            id: now.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(after.state, SignInState::Running);
    assert!(after.finishing);
    // Read, so it's not left on disk for however long the shell stays open.
    for _ in 0..200 {
        if !Path::new(&scratch).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!Path::new(&scratch).exists());
    assert_eq!(state().await, ToolboxState::Open);

    fs::write(&release, "").unwrap();
    // As `vibed`'s own poll does, every few seconds.
    let mut closed = false;
    for _ in 0..400 {
        core.check_toolbox().await;
        if state().await == ToolboxState::Closed {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(closed, "the shell's exit was noticed");
    let done = w.until(after, |s| s.state != SignInState::Running).await;
    assert_eq!(done.state, SignInState::Done, "{done:?}");
    assert!(!done.finishing);
    assert_eq!(state().await, ToolboxState::Closed);
    let toolbox = layout.agent_dir(vibe_env::toolbox::TOOLBOX_REF);
    assert_eq!(
        fs::read_to_string(toolbox.join("github-token")).unwrap(),
        "gho_the_token"
    );
}

/// A flow's exit is what counts, even with something it left holding its output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flow_that_leaves_something_behind_still_ends() {
    let w = world_with(|_, c| c);
    write_script(
        &w.bin.join("gh"),
        "#!/bin/sh\n[ \"$1 $2\" = 'auth login' ] || exit 1\n\
         echo '! First copy your one-time code: B177-C539' >&2\nsleep 20 &\nexit 0\n",
    );
    let begun = std::time::Instant::now();
    let now = w.start(SignInWhat::Github).await;
    let now = w.until(now, |s| s.state != SignInState::Running).await;
    assert_eq!(now.state, SignInState::Done, "{now:?}");
    assert!(begun.elapsed() < Duration::from_secs(15));
}

/// A flow that can't start leaves no scratch directory for a login to land in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flow_that_cant_start_leaves_nothing_behind() {
    let w = world_with(|_, c| c);
    let _ = w
        .core
        .sign_in_start(SignInStartParams {
            host: None,
            what: SignInWhat::ToolboxGithub,
        })
        .await;
    let data = vibe_core::paths::Layout::new(&w.golden.home, "macos").data_dir();
    let left: Vec<_> = fs::read_dir(data)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".toolbox-gh-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

// Tools.

#[cfg(target_os = "linux")]
fn publish_gh(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let version = vibe_env::tools::GH_PINNED;
    let build = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux_amd64",
        ("linux", "aarch64") => "linux_arm64",
        _ => return,
    };
    let folder = format!("gh_{version}_{build}");
    let staging = root.join("staging");
    fs::create_dir_all(staging.join(&folder).join("bin")).unwrap();
    let bin = staging.join(&folder).join("bin/gh");
    fs::write(&bin, "#!/bin/sh\necho 'not logged in' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let release = root.join(format!("download/v{version}"));
    fs::create_dir_all(&release).unwrap();
    let name = format!("{folder}.tar.gz");
    let tarball = release.join(&name);
    assert!(
        std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&staging)
            .arg(&folder)
            .status()
            .unwrap()
            .success()
    );
    let sum = vibe_env::images::sha256_file(&tarball).unwrap();
    fs::write(
        release.join(format!("gh_{version}_checksums.txt")),
        format!("{sum}  {name}\n"),
    )
    .unwrap();
}

/// A host with no `gh` and no Claude Code gets them with nobody asked, the doctor
/// saying so all the while, and never a chore.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_a_host_lacks_goes_in_by_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh_releases, claude_releases) = (tmp.path().join("gh"), tmp.path().join("claude"));
    let golden = Golden::new();
    let mut config = golden.config();
    config.gh = "gh".into();
    config.tool_releases = vibed::tool_install::ToolReleases {
        gh: format!("file://{}", gh_releases.display()),
        claude: format!("file://{}", claude_releases.display()),
    };
    config.checks = Arc::new(FixedChecks {
        claude: Err("Claude Code isn't installed, or your login shell can't find it".into()),
        installs_tools: true,
        ..golden_checks()
    });
    let core = Arc::new(Core::open(config).unwrap());
    let row = async |id| {
        core.doctor()
            .await
            .checks
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
    };

    // Nothing published: it says why, with Try Again, and no command.
    core.repair_setup().await;
    let gh_home = golden.home.join(".local/bin/gh");
    assert!(!gh_home.exists());
    let claude = row(CheckId::Claude).await;
    assert_eq!(
        (claude.state, claude.fixable),
        (CheckState::Fail, true),
        "{claude:?}"
    );
    assert!(
        claude.detail.contains("can't be installed yet"),
        "{claude:?}"
    );
    assert!(!claude.hint.unwrap().contains('`'));

    publish_gh(&gh_releases);
    let build = match std::env::consts::ARCH {
        "x86_64" => "linux-x64",
        _ => "linux-arm64",
    };
    let dir = claude_releases.join("2.1.400").join(build);
    fs::create_dir_all(&dir).unwrap();
    // As the real one does: it keeps itself under the home, and links that.
    fs::write(
        dir.join("claude"),
        "#!/bin/sh\n[ \"$1\" = install ] || exit 9\n\
         mkdir -p \"$HOME/.local/share/claude\" \"$HOME/.local/bin\"\n\
         cp \"$0\" \"$HOME/.local/share/claude/2.1.400\"\n\
         ln -sf \"$HOME/.local/share/claude/2.1.400\" \"$HOME/.local/bin/claude\"\n",
    )
    .unwrap();
    fs::write(
        claude_releases.join("2.1.400/manifest.json"),
        serde_json::json!({
            "version": "2.1.400",
            "platforms": {build: {
                "checksum": vibe_env::images::sha256_file(&dir.join("claude")).unwrap(),
                "size": 10,
            }},
        })
        .to_string(),
    )
    .unwrap();
    fs::write(claude_releases.join("latest"), "2.1.400\n").unwrap();

    // Try Again, as the app's and `vibe doctor`'s do.
    core.doctor_fix(DoctorFixParams {
        check: CheckId::Claude,
    })
    .await
    .unwrap();
    assert!(golden.home.join(".local/bin/claude").exists());
    core.repair_setup().await;
    assert!(gh_home.exists());
}
