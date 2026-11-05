//! Getting a project onto the host: cloning a GitHub repo with the host's
//! `gh` login, or checking a local repo (PRD: Key user flows, Add a
//! project). The registry itself is in `vibe_core::store`.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;
use vibe_core::model::GithubRepo;
use vibe_core::rpc::{ErrorCode, ErrorObject};

/// How long a clone may take before it's given up.
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a quick git query may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How many of the last lines of what a failed clone said are kept.
const WORDS_KEPT: usize = 8;

fn invalid(message: String) -> ErrorObject {
    ErrorObject::new(ErrorCode::InvalidParams, message)
}

fn failed(message: String) -> ErrorObject {
    ErrorObject::new(ErrorCode::CommandFailed, message)
}

/// Kills a process group when dropped, unless disarmed: `gh repo clone`
/// runs `git clone`, which must not outlive a clone that was given up on.
struct GroupKiller(Option<i32>);

impl Drop for GroupKiller {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            // SAFETY: killpg has no memory-safety preconditions; at worst the
            // group is already gone and it fails with ESRCH.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

/// Runs a command with no terminal to prompt on, in a process group of its
/// own, and gives up after `timeout`. Giving up, or dropping the future,
/// kills the command and everything it started.
async fn run(program: &OsStr, args: &[&OsStr], timeout: Duration) -> io::Result<Output> {
    let child = Command::new(program)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let mut killer = GroupKiller(child.id().and_then(|id| i32::try_from(id).ok()));
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => {
            killer.0 = None;
            out
        }
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("it didn't finish in {}s", timeout.as_secs()),
        )),
    }
}

/// The last thing a failed command said, in a line.
fn last_words(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect())
        .unwrap_or_else(|| format!("exit status {}", out.status))
}

/// All a failed `gh` said, git's words with its own, in a few lines of what matters:
/// "failed to run git: exit status 128" alone tells nobody anything. Its progress
/// isn't kept, and nor is a line with a token in it.
fn what_it_said(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("Cloning into ")
                && ![
                    "gho_",
                    "ghp_",
                    "ghs_",
                    "ghu_",
                    "ghr_",
                    "github_pat_",
                    "x-access-token:",
                ]
                .iter()
                .any(|token| l.contains(token))
        })
        .collect();
    if lines.is_empty() {
        return format!("exit status {}", out.status);
    }
    let last = &lines[lines.len().saturating_sub(WORDS_KEPT)..];
    vibe_core::model::clean_alert(&last.join(" "))
}

/// `git -C <dir> <args>`, its trimmed stdout if it succeeds, its last words
/// if not.
pub(crate) async fn git(dir: &Path, args: &[&str]) -> Result<Result<String, String>, ErrorObject> {
    git_within(dir, args, GIT_TIMEOUT).await
}

/// [`git`], given `timeout` to finish.
pub(crate) async fn git_within(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Result<String, String>, ErrorObject> {
    let mut all: Vec<&OsStr> = vec![OsStr::new("-C"), dir.as_os_str()];
    all.extend(args.iter().map(OsStr::new));
    let out = run(OsStr::new("git"), &all, timeout)
        .await
        .map_err(|e| failed(format!("can't run git: {e}")))?;
    Ok(if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(last_words(&out))
    })
}

fn utf8(path: PathBuf) -> Result<String, ErrorObject> {
    path.into_os_string()
        .into_string()
        .map_err(|p| invalid(format!("{} isn't a UTF-8 path", Path::new(&p).display())))
}

/// Checks that `path` is the top of a git checkout and returns it,
/// canonical.
pub async fn local_repo(path: &str) -> Result<String, ErrorObject> {
    let given = Path::new(path);
    if !given.is_absolute() {
        return Err(invalid(format!("{path} isn't an absolute path")));
    }
    let canonical = fs::canonicalize(given).map_err(|e| invalid(format!("{path}: {e}")))?;
    if !canonical.is_dir() {
        return Err(invalid(format!("{path} isn't a directory")));
    }
    let top = git(&canonical, &["rev-parse", "--show-toplevel"])
        .await?
        .map_err(|why| invalid(format!("{path} isn't a git checkout: {why}")))?;
    let top = fs::canonicalize(&top).unwrap_or_else(|_| PathBuf::from(&top));
    if top != canonical {
        return Err(invalid(format!(
            "{path} is inside the git checkout at {}; add that instead",
            top.display()
        )));
    }
    utf8(canonical)
}

/// How a clone went wrong.
enum Failed {
    NoGh,
    /// `gh` wouldn't run, or didn't finish.
    Ran(io::Error),
    /// What it said as it failed.
    Said(String),
}

/// One `gh repo clone` of `repo`, by name or URL, which leaves `partial` a directory
/// if it worked.
async fn clone_into(gh: &OsStr, repo: &str, partial: &Path) -> Result<(), Failed> {
    let args = [
        OsStr::new("repo"),
        OsStr::new("clone"),
        OsStr::new(repo),
        partial.as_os_str(),
    ];
    match run(gh, &args, CLONE_TIMEOUT).await {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Failed::NoGh),
        Err(e) => Err(Failed::Ran(e)),
        Ok(o) if o.status.success() && partial.is_dir() => Ok(()),
        Ok(o) => Err(Failed::Said(what_it_said(&o))),
    }
}

/// Puts `repo` at `dest`, cloning it with `gh` unless a clone of it is
/// already there (from a project that was removed and is being added
/// again). Anything else at `dest` is left alone and refused.
pub async fn github_clone(
    gh: &OsStr,
    repo: &GithubRepo,
    dest: &Path,
) -> Result<String, ErrorObject> {
    if fs::symlink_metadata(dest).is_ok() {
        let origin = git(dest, &["remote", "get-url", "origin"]).await?;
        return match origin {
            Ok(url) if dest.is_dir() && repo.matches_remote(&url) => utf8(
                fs::canonicalize(dest).map_err(|e| failed(format!("{}: {e}", dest.display())))?,
            ),
            _ => Err(ErrorObject::new(
                ErrorCode::Conflict,
                format!(
                    "{} already exists and isn't a clone of {repo}; move it, or add the project under another name",
                    dest.display()
                ),
            )),
        };
    }
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| failed(format!("can't create {}: {e}", parent.display())))?;
    // Clone beside the destination and move it into place once it's
    // complete, so an interrupted clone is never taken for a finished one.
    let mut partial_name = OsStr::new(".").to_owned();
    partial_name.push(dest.file_name().unwrap_or_default());
    partial_name.push(".cloning");
    let partial = parent.join(partial_name);
    if fs::symlink_metadata(&partial).is_ok() {
        fs::remove_dir_all(&partial)
            .map_err(|e| failed(format!("can't clear {}: {e}", partial.display())))?;
    }
    let name = repo.to_string();
    let mut out = clone_into(gh, &name, &partial).await;
    // SSH in the way, whatever was done about it beforehand: this clone goes over
    // HTTPS, with `gh`'s login, which a URL asks for. `gh`'s own setting is the owner's
    // and stays as it is, so nothing here can race, or last beyond this clone.
    if let Err(Failed::Said(said)) = &out
        && crate::gh::ssh_in_the_way(said)
    {
        let said = said.clone();
        let _ = fs::remove_dir_all(&partial);
        out = clone_into(gh, &format!("https://github.com/{repo}"), &partial).await;
        if out.is_ok() {
            crate::log!("vibed: {repo} wouldn't clone over SSH, so it came over HTTPS: {said}");
        }
    }
    if let Err(failure) = out {
        let _ = fs::remove_dir_all(&partial);
        return Err(match failure {
            Failed::NoGh => failed(
                "can't run gh: it isn't installed on this host yet. VibeOS installs it by itself: \
                 add the project again in a moment"
                    .into(),
            ),
            Failed::Ran(e) => failed(format!("gh repo clone {repo} failed: {e}")),
            Failed::Said(said) => failed(format!("gh repo clone {repo} failed: {said}")),
        });
    }
    if let Err(e) = fs::rename(&partial, dest) {
        let _ = fs::remove_dir_all(&partial);
        return Err(failed(format!(
            "can't move the clone to {}: {e}",
            dest.display()
        )));
    }
    utf8(fs::canonicalize(dest).map_err(|e| failed(format!("{}: {e}", dest.display())))?)
}

/// The clone at `path` that removing its project deletes, on a Linux host
/// (PRD: Key user flows, Add a project), checked so that nothing else can
/// go: it must be a git repo directly inside `root` (the projects folder),
/// itself and not through a symlink, and neither `root`, `/`, nor `home` or
/// a folder holding it. `None` if it's gone already. An error says why it
/// isn't safe to delete; then it's left alone.
pub fn clone_to_delete(root: &Path, home: &Path, path: &Path) -> Result<Option<PathBuf>, String> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("{shown} isn't an absolute path"));
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    if meta.file_type().is_symlink() {
        return Err(format!("{shown} is a symlink"));
    }
    if !meta.is_dir() {
        return Err(format!("{shown} isn't a directory"));
    }
    let root = fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let real = fs::canonicalize(path).map_err(|e| format!("{shown}: {e}"))?;
    // The path as given, and where it really is, must both be one level
    // inside the projects folder: that rules out `..`, a symlink anywhere
    // on the way, the folder itself and anything deeper or outside it.
    let given_parent = path
        .parent()
        .map(fs::canonicalize)
        .transpose()
        .map_err(|e| format!("{shown}: {e}"))?;
    let inside = real.parent() == Some(root.as_path())
        && given_parent.as_deref() == Some(root.as_path())
        && real.file_name() == path.file_name();
    if !inside {
        return Err(format!("{shown} isn't a folder in {}", root.display()));
    }
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
    if real == Path::new("/") || home.starts_with(&real) || root.starts_with(&real) {
        return Err(format!("{shown} holds {}", home.display()));
    }
    if fs::symlink_metadata(real.join(".git")).is_err() {
        return Err(format!("{shown} isn't a git repo"));
    }
    Ok(Some(real))
}

/// Whether the clone at `dir` is still `repo`'s: its `origin` must be the
/// repo the project was added from, as adding it again checks, so a folder
/// that has become something else is never deleted for it. Says why not.
pub async fn clone_is_of(dir: &Path, repo: &GithubRepo) -> Result<(), String> {
    match git(dir, &["remote", "get-url", "origin"]).await {
        Ok(Ok(url)) if repo.matches_remote(&url) => Ok(()),
        Ok(Ok(url)) => Err(format!("{}'s origin is {url}, not {repo}", dir.display())),
        Ok(Err(why)) => Err(format!("{} has no origin to check: {why}", dir.display())),
        Err(e) => Err(e.message),
    }
}

/// Where [`move_aside`] puts a clone: beside it, under a name no project
/// can have.
fn aside_of(dir: &Path) -> PathBuf {
    let mut aside = OsStr::new(".").to_owned();
    aside.push(dir.file_name().unwrap_or_default());
    aside.push(".removing");
    dir.with_file_name(aside)
}

/// Moves the clone `dir` that [`clone_to_delete`] passed out of the
/// project's name, in one rename, so a delete cut short never leaves a
/// half-deleted repo under it, and returns where it went. Whatever an
/// earlier delete left there goes first: a folder, or a file or symlink
/// (only the link). An error means nothing was moved.
pub fn move_aside(dir: &Path) -> Result<PathBuf, String> {
    let aside = aside_of(dir);
    if let Ok(meta) = fs::symlink_metadata(&aside) {
        let cleared = if meta.is_dir() {
            fs::remove_dir_all(&aside)
        } else {
            fs::remove_file(&aside)
        };
        cleared.map_err(|e| format!("can't clear {}: {e}", aside.display()))?;
    }
    fs::rename(dir, &aside).map_err(|e| format!("can't move {} aside: {e}", dir.display()))?;
    Ok(aside)
}

/// Puts a clone [`move_aside`] moved back, as when its project couldn't
/// be removed after all.
pub fn move_back(aside: &Path, dir: &Path) -> io::Result<()> {
    fs::rename(aside, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git_init(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let ok = StdCommand::new("git")
            .args(["init", "-q"])
            .arg(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    #[tokio::test]
    async fn local_repos_must_be_the_top_of_a_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("app");
        git_init(&repo);
        fs::create_dir(repo.join("sub")).unwrap();
        let canonical = fs::canonicalize(&repo).unwrap();

        let got = local_repo(repo.to_str().unwrap()).await.unwrap();
        assert_eq!(Path::new(&got), canonical);
        // A trailing slash or a `..` is the same place.
        let dotted = format!("{}/sub/..", repo.display());
        assert_eq!(local_repo(&dotted).await.unwrap(), got);

        let sub = local_repo(repo.join("sub").to_str().unwrap())
            .await
            .unwrap_err();
        assert!(sub.message.contains("inside the git checkout at"), "{sub}");
        let plain = local_repo(tmp.path().to_str().unwrap()).await.unwrap_err();
        assert!(plain.message.contains("isn't a git checkout"), "{plain}");
        let missing = local_repo("/nonexistent/x").await.unwrap_err();
        assert_eq!(missing.code, ErrorCode::InvalidParams.code());
        let relative = local_repo("app").await.unwrap_err();
        assert!(relative.message.contains("absolute"), "{relative}");
        let file = repo.join("f");
        fs::write(&file, "").unwrap();
        let file = local_repo(file.to_str().unwrap()).await.unwrap_err();
        assert!(file.message.contains("isn't a directory"), "{file}");
    }

    #[tokio::test]
    async fn giving_up_kills_everything_the_command_started() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        // Like gh running git: a child that would outlive its parent.
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let started = std::time::Instant::now();
        let err = run(
            OsStr::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(&script)],
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only checks that the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the grandchild outlived the timeout"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn failed_with(stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn what_a_failed_clone_said_is_kept_but_for_a_token_or_what_moves_a_cursor() {
        let said = what_it_said(&failed_with(
            "Cloning into 'x'...\nHost key verification failed.\n\
             fatal: Could not read from remote repository.\n\n\
             failed to run git: exit status 128\n",
        ));
        assert_eq!(
            said,
            "Host key verification failed. fatal: Could not read from remote repository. \
             failed to run git: exit status 128"
        );
        for token in [
            "gho_abc",
            "ghp_abc",
            "github_pat_abc",
            "https://x-access-token:v1.abc@github.com/o/n",
        ] {
            let said = what_it_said(&failed_with(&format!("fatal: {token}\nthe end\n")));
            assert_eq!(said, "the end", "{token}");
        }
        let said = what_it_said(&failed_with("a\u{1b}[2Jb\u{202e}c\n"));
        assert_eq!(said, "a[2Jbc");
        assert!(what_it_said(&failed_with("")).contains("exit status"));
    }

    #[tokio::test]
    async fn a_missing_gh_is_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("projects/web");
        let repo = GithubRepo::parse("me/web").unwrap();
        let err = github_clone(OsStr::new("/nonexistent/gh"), &repo, &dest)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CommandFailed.code());
        assert!(err.message.contains("isn't installed"), "{err}");
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn something_else_at_the_destination_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("web");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("keep"), "mine").unwrap();
        let repo = GithubRepo::parse("me/web").unwrap();
        let err = github_clone(OsStr::new("/nonexistent/gh"), &repo, &dest)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict.code());
        assert_eq!(fs::read_to_string(dest.join("keep")).unwrap(), "mine");

        // A clone of another repo is refused too; one of this repo is kept.
        let other = tmp.path().join("other");
        git_init(&other);
        let set = |url: &str| {
            StdCommand::new("git")
                .arg("-C")
                .arg(&other)
                .args(["remote", "add", "origin", url])
                .status()
                .unwrap()
        };
        set("https://github.com/me/other.git");
        let err = github_clone(OsStr::new("gh"), &repo, &other)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict.code());
        let same = tmp.path().join("same");
        git_init(&same);
        StdCommand::new("git")
            .arg("-C")
            .arg(&same)
            .args(["remote", "add", "origin", "git@github.com:me/web.git"])
            .status()
            .unwrap();
        let got = github_clone(OsStr::new("/nonexistent/gh"), &repo, &same)
            .await
            .unwrap();
        assert_eq!(Path::new(&got), fs::canonicalize(&same).unwrap());
    }

    mod deleting {
        use super::super::{clone_is_of, clone_to_delete, move_aside, move_back};
        use std::fs;
        use std::os::unix::fs::symlink;
        use std::path::{Path, PathBuf};
        use vibe_core::model::GithubRepo;

        struct Home {
            _dir: tempfile::TempDir,
            home: PathBuf,
            root: PathBuf,
        }

        fn home() -> Home {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().canonicalize().unwrap().join("home");
            let root = home.join("VibeOS/projects");
            fs::create_dir_all(&root).unwrap();
            Home {
                _dir: dir,
                home,
                root,
            }
        }

        fn clone_at(dir: &Path) {
            fs::create_dir_all(dir.join(".git")).unwrap();
            fs::write(dir.join("README.md"), "hi").unwrap();
        }

        #[test]
        fn a_clone_in_the_projects_folder_goes_and_nothing_else() {
            let h = home();
            let app = h.root.join("app");
            let web = h.root.join("web");
            clone_at(&app);
            clone_at(&web);
            let target = clone_to_delete(&h.root, &h.home, &app).unwrap().unwrap();
            assert_eq!(target, app);
            let aside = move_aside(&target).unwrap();
            assert_eq!(aside, h.root.join(".app.removing"));
            assert!(!app.exists() && aside.join("README.md").exists());
            fs::remove_dir_all(&aside).unwrap();
            assert!(!app.exists());
            assert!(web.join("README.md").exists(), "its neighbour stays");
            assert!(h.root.is_dir(), "the projects folder stays");
            let left: Vec<_> = fs::read_dir(&h.root)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(left, ["web"], "nothing set aside is left");
            // Gone already: nothing to do.
            assert_eq!(clone_to_delete(&h.root, &h.home, &app).unwrap(), None);
        }

        #[test]
        fn moving_aside_clears_what_an_earlier_delete_left_and_can_be_undone() {
            let h = home();
            let app = h.root.join("app");
            clone_at(&app);
            // A file where the clone would go: cleared, not followed.
            let outside = h.home.join("keep.txt");
            fs::write(&outside, "keep").unwrap();
            symlink(&outside, h.root.join(".app.removing")).unwrap();
            let aside = move_aside(&app).unwrap();
            assert!(aside.join("README.md").exists());
            assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
            move_back(&aside, &app).unwrap();
            assert!(app.join("README.md").exists());
            // A folder an interrupted delete left.
            fs::create_dir_all(h.root.join(".app.removing/junk")).unwrap();
            let aside = move_aside(&app).unwrap();
            assert!(!aside.join("junk").exists() && aside.join("README.md").exists());
            // Nothing to move: an error, and nothing changed.
            assert!(move_aside(&h.root.join("gone")).is_err());
        }

        #[tokio::test]
        async fn only_a_clone_of_the_projects_repo_is_its_clone() {
            let h = home();
            let app = h.root.join("app");
            fs::create_dir_all(&app).unwrap();
            let git = |args: &[&str]| {
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&app)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{out:?}");
            };
            git(&["init", "-q"]);
            let repo = GithubRepo::parse("me/app").unwrap();
            let none = clone_is_of(&app, &repo).await.unwrap_err();
            assert!(none.contains("has no origin"), "{none}");
            git(&[
                "remote",
                "add",
                "origin",
                "https://github.com/someone/else.git",
            ]);
            let other = clone_is_of(&app, &repo).await.unwrap_err();
            assert!(other.contains("not me/app"), "{other}");
            git(&["remote", "set-url", "origin", "git@github.com:Me/app.git"]);
            clone_is_of(&app, &repo).await.unwrap();
        }

        #[test]
        fn anything_but_a_clone_directly_inside_is_refused() {
            let h = home();
            clone_at(&h.root.join("app"));
            let outside = h.home.join("src/mine");
            clone_at(&outside);
            let deeper = h.root.join("app/vendor/lib");
            clone_at(&deeper);
            fs::create_dir_all(h.root.join("plain")).unwrap();
            fs::write(h.root.join("file"), "x").unwrap();
            // A symlink in the projects folder to a repo elsewhere.
            symlink(&outside, h.root.join("link")).unwrap();
            // A real folder reached through a symlinked parent.
            symlink(h.home.join("src"), h.root.join("via")).unwrap();
            let cases: Vec<(PathBuf, &str)> = vec![
                (h.root.clone(), "the projects folder itself"),
                (h.root.join("."), "the projects folder, by another name"),
                (h.root.join("app/.."), "the projects folder, by .."),
                (
                    h.root.join("../projects"),
                    "the projects folder, from above",
                ),
                (outside.clone(), "a repo outside it"),
                (h.root.join("../../src/mine"), "a repo outside it, by .."),
                (deeper.clone(), "a repo deeper in"),
                (h.root.join("plain"), "a folder that isn't a repo"),
                (h.root.join("file"), "a file"),
                (h.root.join("link"), "a symlink to a repo"),
                (h.root.join("via/mine"), "a repo through a symlink"),
                (h.home.clone(), "home"),
                (PathBuf::from("/"), "the root"),
                (PathBuf::from("VibeOS/projects/app"), "a relative path"),
            ];
            for (path, what) in cases {
                assert!(
                    clone_to_delete(&h.root, &h.home, &path).is_err(),
                    "{what}: {}",
                    path.display()
                );
            }
            assert!(outside.join("README.md").exists());
            assert!(deeper.join("README.md").exists());
        }

        #[test]
        fn a_projects_folder_that_is_home_or_root_deletes_nothing_holding_home() {
            let h = home();
            // Home is a repo, and the projects folder is its parent.
            clone_at(&h.home);
            let parent = h.home.parent().unwrap().to_owned();
            assert!(clone_to_delete(&parent, &h.home, &h.home).is_err());
            assert!(clone_to_delete(Path::new("/"), &h.home, Path::new("/")).is_err());
        }
    }
}
