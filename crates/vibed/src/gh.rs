//! `gh auth status`, which `vibed` runs at startup and every hour so the menu
//! can show whether the host's GitHub login works (PRD: Auth, security and
//! trust).

use std::ffi::OsStr;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use vibe_core::model::GhState;

/// How long `gh auth status` may take; it asks GitHub whether the token
/// still works.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub state: GhState,
    pub account: Option<String>,
    pub detail: String,
}

/// Runs `<gh> auth status` for github.com's active account, so a stale
/// second account doesn't count. A `gh` too old for `--active` (before 2.40)
/// is asked about every account instead.
pub async fn check(gh: &OsStr) -> Check {
    match run(gh, true).await {
        Ok((false, text)) if text.contains("unknown flag") => match run(gh, false).await {
            Ok((ok, text)) => interpret(ok, &text),
            Err(check) => check,
        },
        Ok((ok, text)) => interpret(ok, &text),
        Err(check) => check,
    }
}

/// Whether `gh auth status` succeeded, and everything it printed.
async fn run(gh: &OsStr, active: bool) -> Result<(bool, String), Check> {
    let mut args = vec!["auth", "status", "--hostname", "github.com"];
    if active {
        args.push("--active");
    }
    let run = Command::new(gh)
        .args(&args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(TIMEOUT, run).await {
        Err(_) => Err(Check {
            state: GhState::Error,
            account: None,
            detail: format!("gh auth status didn't finish in {}s", TIMEOUT.as_secs()),
        }),
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => Err(Check {
            state: GhState::NotInstalled,
            account: None,
            detail: "gh isn't installed".into(),
        }),
        Ok(Err(e)) => Err(Check {
            state: GhState::Error,
            account: None,
            detail: format!("can't run gh: {e}"),
        }),
        Ok(Ok(out)) => {
            // gh has printed its report to stdout and to stderr, depending
            // on the version.
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok((out.status.success(), text))
        }
    }
}

/// Reads `gh auth status` output. Token lines are never kept.
pub fn interpret(success: bool, output: &str) -> Check {
    let lines: Vec<&str> = output
        .lines()
        .map(|l| l.trim_start_matches(|c: char| c.is_whitespace() || "✓X✗-!*".contains(c)))
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.to_ascii_lowercase().contains("token"))
        .collect();
    let find = |needles: &[&str]| {
        lines.iter().copied().find(|l| {
            let lower = l.to_ascii_lowercase();
            needles.iter().any(|n| lower.contains(n))
        })
    };
    if success {
        let line = find(&["logged in to"]);
        let account = line.and_then(account_of);
        return Check {
            state: GhState::LoggedIn,
            account,
            detail: line
                .or(lines.first().copied())
                .unwrap_or("logged in")
                .to_owned(),
        };
    }
    let logged_out = find(&[
        "not logged in",
        "failed to log in",
        "gh auth login",
        "invalid",
    ]);
    Check {
        state: if logged_out.is_some() {
            GhState::LoggedOut
        } else {
            GhState::Error
        },
        account: None,
        detail: logged_out
            .or(lines.first().copied())
            .unwrap_or("gh auth status failed")
            .chars()
            .take(300)
            .collect(),
    }
}

/// The account in `Logged in to github.com account NAME (keyring)`, or the
/// older `Logged in to github.com as NAME (…)`.
fn account_of(line: &str) -> Option<String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    words
        .windows(2)
        .find(|w| w[0] == "account" || w[0] == "as")
        .map(|w| w[1].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_in_current_gh() {
        let out = "github.com\n  ✓ Logged in to github.com account octocat (keyring)\n  - Active account: true\n  - Git operations protocol: https\n  - Token: gho_************************************\n  - Token scopes: 'gist', 'read:org', 'repo'\n";
        let c = interpret(true, out);
        assert_eq!(c.state, GhState::LoggedIn);
        assert_eq!(c.account.as_deref(), Some("octocat"));
        assert_eq!(
            c.detail,
            "Logged in to github.com account octocat (keyring)"
        );
    }

    #[test]
    fn logged_in_older_gh() {
        let out = "github.com\n  ✓ Logged in to github.com as octocat (/home/u/.config/gh/hosts.yml)\n  ✓ Token: *******************\n";
        let c = interpret(true, out);
        assert_eq!(c.account.as_deref(), Some("octocat"));
        assert!(!c.detail.contains("Token"));
    }

    #[test]
    fn logged_out() {
        let c = interpret(
            false,
            "You are not logged into any GitHub hosts. To log in, run: gh auth login\n",
        );
        assert_eq!(c.state, GhState::LoggedOut);
        assert_eq!(c.account, None);
        assert!(
            c.detail.starts_with("You are not logged into"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn bad_token() {
        let out = "github.com\n  X Failed to log in to github.com account octocat (keyring)\n  - Active account: true\n  - The token in keyring is invalid.\n  - To re-authenticate, run: gh auth login -h github.com\n";
        let c = interpret(false, out);
        assert_eq!(c.state, GhState::LoggedOut);
        assert_eq!(
            c.detail,
            "Failed to log in to github.com account octocat (keyring)"
        );
    }

    #[test]
    fn anything_else_is_an_error() {
        let c = interpret(false, "error connecting to api.github.com\n");
        assert_eq!(c.state, GhState::Error);
        assert_eq!(c.detail, "error connecting to api.github.com");
        assert_eq!(interpret(false, "").detail, "gh auth status failed");
    }

    fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn asks_about_the_active_account() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = script(
            tmp.path(),
            r#"case "$*" in
*--active*) echo "  ✓ Logged in to github.com account me (keyring)" ;;
*) echo "  X Failed to log in to github.com account old (keyring)" >&2; exit 1 ;;
esac"#,
        );
        let c = check(gh.as_os_str()).await;
        assert_eq!(
            (c.state, c.account.as_deref()),
            (GhState::LoggedIn, Some("me"))
        );
    }

    #[tokio::test]
    async fn an_old_gh_without_active_is_asked_plainly() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = script(
            tmp.path(),
            r#"case "$*" in
*--active*) echo "unknown flag: --active" >&2; exit 1 ;;
*) echo "  ✓ Logged in to github.com as me (hosts.yml)" ;;
esac"#,
        );
        let c = check(gh.as_os_str()).await;
        assert_eq!(
            (c.state, c.account.as_deref()),
            (GhState::LoggedIn, Some("me"))
        );
    }

    #[tokio::test]
    async fn a_missing_gh_is_not_installed() {
        let c = check(OsStr::new("/nonexistent/gh")).await;
        assert_eq!(c.state, GhState::NotInstalled);
    }
}
