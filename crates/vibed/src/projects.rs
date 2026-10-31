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
    let args = [
        OsStr::new("repo"),
        OsStr::new("clone"),
        OsStr::new(&name),
        partial.as_os_str(),
    ];
    let out = run(gh, &args, CLONE_TIMEOUT).await;
    let ok = matches!(&out, Ok(o) if o.status.success()) && partial.is_dir();
    if !ok {
        let _ = fs::remove_dir_all(&partial);
        return Err(match out {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                failed("can't run gh: it isn't installed".into())
            }
            Err(e) => failed(format!("gh repo clone {repo} failed: {e}")),
            Ok(o) => failed(format!("gh repo clone {repo} failed: {}", last_words(&o))),
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
}
