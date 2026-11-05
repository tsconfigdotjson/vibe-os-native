//! `gh auth status`, which `vibed` runs at startup and every hour so the menu
//! can show whether the host's GitHub login works (PRD: Auth, security and
//! trust). And what git and `gh` need of a host to reach GitHub, which `vibed`
//! sets up itself (PRD: Key user flows, Add a project): GitHub's published SSH
//! host keys, pinned here so that SSH's checking of them stays on.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::process::Command;
use vibe_core::model::GhState;

/// How long `gh auth status` may take; it asks GitHub whether the token
/// still works.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// GitHub's SSH host keys, as it publishes them in `ssh_keys` at
/// <https://api.github.com/meta> and on its "SSH key fingerprints" page. These are
/// what a first connection is checked against, so nothing has to accept a key on
/// first sight. The fingerprints are held to in a test. If GitHub ever rotates
/// them, a clone falls back to HTTPS until a new build has the new ones, and one it
/// has stopped publishing, as it did a leaked one in 2023, isn't added by a build
/// that's still in the field: see [`published_host_keys`].
pub const GITHUB_HOST_KEYS: [&str; 3] = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
    "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=",
    "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCj7ndNxQowgcQnjshcLrqPEiiphnt+VTTvDP6mHBL9j1aNUkY4Ue1gvwnGLVlOhGeYrnZaMgRK6+PKCUXaDbC7qtbW8gIkhL7aGCsOr/C56SJMy/BCZfxd1nWzAOxSDPgVsmerOBYfNqltV9/hWCqBywINIR+5dIg6JTJ72pcEpEjcYgXkE2YEFXV1JHnsKgbLWNlhScqb2UmyRkQyytRLtL+38TGxkxCflmO+5Z8CSSNY7GidjMIZ7Q4zMjA2n1nGrlTDkzwDCsw+wqFPGQA179cnfGWOWRVruj16z6XyvxvjJwbz0wQZ75XK5tKSb7FNyeIEs4TT4jk+S4dhPeAUC5y+bDYirYgM4GC7uEnztnZyaVWQ7B381AK4Qdrwt51ZqExKbQpTUNn+EjqoTwvqNj4kqx5QUCI0ThS/YkOxJCXmPUWZbhjpCg56i+2aB6CmK2JGhn57K5mj0MNdBXA4/WnwH6XoPWJzK5Nyu2zB3nAZp+S5hpQs+p1vN1/wsjk=",
];

/// The names SSH looks GitHub up by: its own, and GitHub's documented way round a
/// blocked port 22 (`HostName ssh.github.com`, `Port 443`), which has the same keys.
const GITHUB_SSH_NAMES: [&str; 2] = ["github.com", "[ssh.github.com]:443"];

/// What `known_hosts` has for `name` of keys of type `kind`: each one, and whether it's
/// marked, as `@revoked` is. A hashed line can't be told without hashing, so it doesn't
/// count, and costs one plain line beside it, once.
fn held<'a>(
    known_hosts: &'a str,
    name: &'a str,
    kind: &'a str,
) -> impl Iterator<Item = (bool, &'a str)> {
    known_hosts.lines().filter_map(move |line| {
        let mut fields = line.split_whitespace().peekable();
        let marked = fields.next_if(|f| f.starts_with('@')).is_some();
        let hosts = fields.next()?;
        if hosts.starts_with('#') || !hosts.split(',').any(|h| h == name) {
            return None;
        }
        (fields.next() == Some(kind)).then(|| (marked, fields.next().unwrap_or("")))
    })
}

/// The lines `known_hosts` wants of `keys`: each under each name that has no key of its
/// type. A key of that type for github.com that isn't this one, as a newer key is after a
/// rotation, or one marked `@revoked`, is the owner's say for that type, under both names.
fn wanted<'a>(known_hosts: &str, keys: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut lines = Vec::new();
    for key in keys {
        let (kind, blob) = key.split_once(' ').unwrap_or((key, ""));
        if held(known_hosts, GITHUB_SSH_NAMES[0], kind).any(|(marked, k)| marked || k != blob) {
            continue;
        }
        for name in GITHUB_SSH_NAMES {
            if held(known_hosts, name, kind).next().is_none() {
                lines.push(format!("{name} {key}\n"));
            }
        }
    }
    lines
}

fn read_known_hosts(path: &Path) -> io::Result<String> {
    match fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Whether a key this build pins looks to be missing from `<home>/.ssh/known_hosts`, so
/// that GitHub's worth asking. Run often, so it's only a read. One that can't be read
/// says yes, so that [`pin_host_keys`] gets to say why.
pub fn pins_missing(home: &Path) -> bool {
    read_known_hosts(&home.join(".ssh/known_hosts")).map_or(true, |there| {
        !wanted(&there, GITHUB_HOST_KEYS.into_iter()).is_empty()
    })
}

/// The keys GitHub publishes now, asked for over TLS with `gh`'s login. A build can be
/// out in the field long after GitHub has dropped a key, so what's pinned goes in only
/// if it's still listed here. Only that: a key new to this build isn't trusted from here.
pub async fn published_host_keys(gh: &OsStr) -> Result<Vec<String>, String> {
    let args = [
        "api",
        "meta",
        "--hostname",
        "github.com",
        "--jq",
        ".ssh_keys[]",
    ];
    match output(gh, &args, "gh api meta").await {
        Ok((true, text)) => Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()),
        Ok((false, text)) => Err(format!(
            "gh api meta failed: {}",
            text.lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("it didn't say why")
        )),
        Err(check) => Err(check.detail),
    }
}

/// Adds to `<home>/.ssh/known_hosts` those of GitHub's keys still in `published` that it
/// wants, and says if it added any. Nothing that's there is touched.
pub fn pin_host_keys(home: &Path, published: &[String]) -> io::Result<bool> {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = home.join(".ssh");
    let path = dir.join("known_hosts");
    let there = read_known_hosts(&path)?;
    let missing = wanted(
        &there,
        GITHUB_HOST_KEYS
            .into_iter()
            .filter(|key| published.iter().any(|p| p == key)),
    );
    if missing.is_empty() {
        return Ok(false);
    }
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let mut lines = String::new();
    if !there.is_empty() && !there.ends_with('\n') {
        lines.push('\n');
    }
    lines.extend(missing);
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)?
        .write_all(lines.as_bytes())?;
    Ok(true)
}

/// Whether a clone failed for a reason of SSH's that HTTPS, with `gh`'s login, gets
/// round: a host key it won't accept, no SSH key this account knows, a blocked port,
/// or no `ssh` at all. It only ever goes round: no check is turned off. Not a network
/// that's down, since that's no better over HTTPS.
pub fn ssh_in_the_way(said: &str) -> bool {
    let said = said.to_ascii_lowercase();
    !said.contains("could not resolve hostname")
        && [
            "host key verification failed",
            "remote host identification has changed",
            "permission denied (publickey",
            "could not read from remote repository",
            "ssh: connect to host",
            "kex_exchange_identification",
            "connection closed by",
            "cannot run ssh",
        ]
        .iter()
        .any(|sign| said.contains(sign))
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub state: GhState,
    pub account: Option<String>,
    pub detail: String,
}

/// Runs `<gh> auth status` for github.com's active account, so a stale
/// second account doesn't count. A `gh` too old for `--active`, as Ubuntu's 2.46
/// is, is asked about every account instead.
pub async fn check(gh: &OsStr) -> Check {
    let check = match run(gh, true).await {
        Ok((false, text)) if text.contains("unknown flag") => match run(gh, false).await {
            Ok((ok, text)) => interpret(ok, &text),
            Err(check) => check,
        },
        Ok((ok, text)) => interpret(ok, &text),
        Err(check) => check,
    };
    if check.state == GhState::LoggedOut && check.detail.starts_with("Failed to log in") {
        confirm_failed_login(gh, check).await
    } else {
        check
    }
}

/// `gh auth status` says "Failed to log in", and that the token is invalid, of a
/// good token it couldn't check for want of a network, so GitHub is asked. Only
/// GitHub refusing it says the owner has anything to do.
async fn confirm_failed_login(gh: &OsStr, failed: Check) -> Check {
    let text = match output(gh, &["api", "user", "--jq", ".login"], "gh api user").await {
        Ok((true, text)) => {
            let account = text.trim().to_owned();
            return Check {
                state: GhState::LoggedIn,
                detail: format!("Logged in to github.com account {account}"),
                account: Some(account).filter(|a| !a.is_empty()),
            };
        }
        Ok((false, text)) => text,
        Err(check) => return check,
    };
    if text.contains("HTTP 401") || text.contains("gh auth login") {
        return failed;
    }
    let said = text.lines().map(str::trim).rfind(|l| !l.is_empty());
    Check {
        state: GhState::Error,
        account: None,
        detail: format!(
            "can't tell if gh's login works: {}",
            said.unwrap_or("gh api user failed")
                .chars()
                .take(250)
                .collect::<String>()
        ),
    }
}

/// Whether `gh auth status` succeeded, and everything it printed.
async fn run(gh: &OsStr, active: bool) -> Result<(bool, String), Check> {
    let mut args = vec!["auth", "status", "--hostname", "github.com"];
    if active {
        args.push("--active");
    }
    output(gh, &args, "gh auth status").await
}

/// Whether `<gh> <args>` succeeded, and everything it printed. `what` is what it's
/// called if it can't be said to have finished.
async fn output(gh: &OsStr, args: &[&str], what: &str) -> Result<(bool, String), Check> {
    let run = Command::new(gh)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(TIMEOUT, run).await {
        Err(_) => Err(Check {
            state: GhState::Error,
            account: None,
            detail: format!("{what} didn't finish in {}s", TIMEOUT.as_secs()),
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
    let bare = output
        .lines()
        .map(|l| l.trim_start_matches(|c: char| c.is_whitespace() || "✓X✗-!*".contains(c)))
        .map(str::trim);
    // A gh as old as Ubuntu's 2.46 exits 0 even when its only login failed, with a
    // missing or bad token, so its words count over its exit. That line can end
    // "using token", which isn't one, and is left out of what's kept. A timeout is the
    // same line, from a newer gh, and says nothing against the login.
    let said = |words: &str| {
        bare.clone()
            .find(|l| l.to_ascii_lowercase().contains(words))
            .map(|l| l.split(" using token").next().unwrap_or(l))
    };
    let failed_login = said("failed to log in");
    let timed_out = said("timeout trying to log in");
    let lines: Vec<&str> = bare
        .filter(|l| !l.is_empty() && !l.to_ascii_lowercase().contains("token"))
        .collect();
    let find = |needles: &[&str]| {
        lines.iter().copied().find(|l| {
            let lower = l.to_ascii_lowercase();
            needles.iter().any(|n| lower.contains(n))
        })
    };
    if success && (failed_login.is_none() || find(&["logged in to"]).is_some()) {
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
    let logged_out = failed_login.or_else(|| find(&["not logged in", "gh auth login", "invalid"]));
    Check {
        state: if logged_out.is_some() {
            GhState::LoggedOut
        } else {
            GhState::Error
        },
        account: None,
        detail: logged_out
            .map(without_the_command)
            .or(timed_out)
            .or(lines.first().copied())
            .unwrap_or("gh auth status failed")
            .chars()
            .take(300)
            .collect(),
    }
}

/// What gh said of a login, without the command it goes on to give, as this is shown
/// beside the one click that does it.
fn without_the_command(line: &str) -> &str {
    let said = [
        "To log in, run",
        "To re-authenticate, run",
        "Run gh auth login",
    ]
    .iter()
    .fold(line, |said, command| {
        said.split(command).next().unwrap_or(said)
    })
    .trim();
    if said.is_empty() {
        "gh's login no longer works"
    } else {
        said
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

    /// The fingerprints GitHub publishes, read off `ssh_key_fingerprints` at
    /// api.github.com/meta and held to here, so a slip in a key can't get through.
    #[test]
    fn the_pinned_keys_are_the_ones_github_publishes() {
        use sha2::{Digest, Sha256};
        let want = [
            "+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU",
            "p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM",
            "uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s",
        ];
        for (key, want) in GITHUB_HOST_KEYS.iter().zip(want) {
            let (kind, blob) = key.split_once(' ').unwrap();
            let blob = decode_base64(blob);
            let kept = String::from_utf8_lossy(&blob[4..4 + kind.len()]).into_owned();
            assert_eq!(kept, kind, "the key says what it is");
            assert_eq!(encode_base64(&Sha256::digest(&blob)), want, "{kind}");
        }
    }

    const BASE64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn decode_base64(text: &str) -> Vec<u8> {
        let (mut bits, mut have, mut out) = (0u32, 0, Vec::new());
        for c in text.bytes().filter(|c| *c != b'=') {
            bits = bits << 6 | BASE64.iter().position(|b| *b == c).unwrap() as u32;
            have += 6;
            if have >= 8 {
                have -= 8;
                out.push((bits >> have) as u8);
                bits &= (1 << have) - 1;
            }
        }
        out
    }

    /// Without padding, as SSH writes a fingerprint.
    fn encode_base64(bytes: &[u8]) -> String {
        let (mut bits, mut have, mut out) = (0u32, 0, String::new());
        for b in bytes {
            bits = bits << 8 | u32::from(*b);
            have += 8;
            while have >= 6 {
                have -= 6;
                out.push(BASE64[(bits >> have) as usize] as char);
                bits &= (1 << have) - 1;
            }
        }
        if have > 0 {
            out.push(BASE64[(bits << (6 - have)) as usize] as char);
        }
        out
    }

    fn all_published() -> Vec<String> {
        GITHUB_HOST_KEYS.map(str::to_owned).to_vec()
    }

    #[test]
    fn a_fresh_home_gets_the_keys_privately_and_only_once() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        assert!(pins_missing(tmp.path()));
        assert!(pin_host_keys(tmp.path(), &all_published()).unwrap());
        let dir = tmp.path().join(".ssh");
        let file = dir.join("known_hosts");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file), 0o600);
        let first = fs::read_to_string(&file).unwrap();
        let want: String = GITHUB_HOST_KEYS
            .iter()
            .flat_map(|k| {
                [
                    format!("github.com {k}\n"),
                    format!("[ssh.github.com]:443 {k}\n"),
                ]
            })
            .collect();
        assert_eq!(first, want);
        // Nothing to do, so nothing done.
        assert!(!pins_missing(tmp.path()));
        assert!(!pin_host_keys(tmp.path(), &all_published()).unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), first);
    }

    /// A key GitHub has stopped publishing, as it did the leaked RSA key in 2023, isn't
    /// added by a build still carrying it, whatever else it adds.
    #[test]
    fn a_key_github_no_longer_publishes_is_not_added() {
        let [ed25519, ecdsa, rsa] = GITHUB_HOST_KEYS;
        let tmp = tempfile::tempdir().unwrap();
        let listed = vec![ed25519.to_owned(), ecdsa.to_owned(), "ssh-rsa NEWER".into()];
        assert!(pin_host_keys(tmp.path(), &listed).unwrap());
        let now = fs::read_to_string(tmp.path().join(".ssh/known_hosts")).unwrap();
        assert!(!now.contains(rsa), "{now}");
        assert!(!now.contains("NEWER"), "only what's pinned goes in: {now}");
        assert_eq!(now.lines().count(), 4, "{now}");

        // Having asked and been told nothing, nothing goes in.
        let tmp = tempfile::tempdir().unwrap();
        assert!(!pin_host_keys(tmp.path(), &[]).unwrap());
        assert!(!tmp.path().join(".ssh").exists());
    }

    /// Ubuntu's gh 2.46 and this build's own both take this.
    #[tokio::test]
    async fn the_published_keys_are_asked_for_as_gh_takes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = script(
            tmp.path(),
            r#"[ "$*" = "api meta --hostname github.com --jq .ssh_keys[]" ] || exit 9
echo "ssh-ed25519 AAAA"
echo "ssh-rsa BBBB""#,
        );
        assert_eq!(
            published_host_keys(gh.as_os_str()).await,
            Ok(vec![
                "ssh-ed25519 AAAA".to_owned(),
                "ssh-rsa BBBB".to_owned()
            ])
        );
        let gh = script(
            tmp.path(),
            "echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1",
        );
        let why = published_host_keys(gh.as_os_str()).await.unwrap_err();
        assert!(why.contains("HTTP 401"), "{why}");
    }

    #[test]
    fn what_a_known_hosts_has_already_stays_as_it_is() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".ssh");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let file = dir.join("known_hosts");
        // No newline at its end; another host; one of GitHub's by another name
        // with others; a comment and a hashed line, which don't count.
        let [ed25519, ecdsa, rsa] = GITHUB_HOST_KEYS;
        let there = format!(
            "gitlab.com ssh-ed25519 AAAA\n# github.com {rsa}\n|1|a=|b= {rsa}\n\
             1.2.3.4,github.com {ed25519}"
        );
        fs::write(&file, &there).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(pin_host_keys(tmp.path(), &all_published()).unwrap());
        let now = fs::read_to_string(&file).unwrap();
        let other = "[ssh.github.com]:443";
        assert_eq!(
            now,
            format!(
                "{there}\n{other} {ed25519}\ngithub.com {ecdsa}\n{other} {ecdsa}\n\
                 github.com {rsa}\n{other} {rsa}\n"
            )
        );
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&dir), mode(&file)), (0o755, 0o644), "the owner's");
        assert!(!pin_host_keys(tmp.path(), &all_published()).unwrap());

        // A key of that type that isn't GitHub's pin, such as a newer one after a
        // rotation, is theirs to have put there: the pin doesn't go in beside it, under
        // either name.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".ssh")).unwrap();
        let file = tmp.path().join(".ssh/known_hosts");
        fs::write(&file, "github.com ssh-rsa NEWER\n").unwrap();
        assert!(pin_host_keys(tmp.path(), &all_published()).unwrap());
        let now = fs::read_to_string(&file).unwrap();
        assert!(!now.contains(rsa), "{now}");
        assert_eq!(now.lines().count(), 5, "{now}");
        assert!(!pins_missing(tmp.path()));

        // A key the owner has revoked is theirs to have said, for both names.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".ssh")).unwrap();
        let revoked = format!("@revoked github.com {rsa}\n");
        let file = tmp.path().join(".ssh/known_hosts");
        fs::write(&file, &revoked).unwrap();
        assert!(pin_host_keys(tmp.path(), &all_published()).unwrap());
        let now = fs::read_to_string(&file).unwrap();
        assert_eq!(now.matches(rsa).count(), 1, "{now}");
    }

    #[test]
    fn a_known_hosts_that_cant_be_written_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".ssh"), "a file").unwrap();
        assert!(pins_missing(tmp.path()), "so that it gets to say why");
        assert!(pin_host_keys(tmp.path(), &all_published()).is_err());
    }

    #[test]
    fn what_ssh_says_that_https_gets_round() {
        for said in [
            "Host key verification failed.\nfatal: Could not read from remote repository.",
            "git@github.com: Permission denied (publickey).",
            "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!",
            "ssh: connect to host github.com port 22: Connection timed out",
            "kex_exchange_identification: Connection closed by remote host",
            "ERROR: Repository not found.\nfatal: Could not read from remote repository.",
            "cannot run ssh: No such file or directory\nfatal: unable to fork",
        ] {
            assert!(ssh_in_the_way(said), "{said}");
        }
        for said in [
            "GraphQL: Could not resolve to a Repository with the name 'me/x'. (repository)",
            "ssh: Could not resolve hostname github.com: Name or service not known\n\
             fatal: Could not read from remote repository.",
            "To get started with GitHub CLI, please run:  gh auth login",
            "fatal: unable to access 'https://github.com/me/x/': Could not resolve host",
            "",
        ] {
            assert!(!ssh_in_the_way(said), "{said}");
        }
    }

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
        // Shown beside the one click that does it, so without the command.
        assert_eq!(c.detail, "You are not logged into any GitHub hosts.");
        let c = interpret(
            false,
            "You are not logged into the GitHub host \"github.com\". Run gh auth login -h github.com\n",
        );
        assert_eq!(
            c.detail,
            "You are not logged into the GitHub host \"github.com\"."
        );
    }

    /// Words from gh 2.103.3's source, the one this build installs.
    #[test]
    fn a_rejected_refresh_token_is_a_sign_in_without_the_command() {
        let out = "github.com\n  X Refresh token for github.com account me was rejected\n  - Active account: true\n  - The stored expiring credential was removed because its refresh token is no longer valid.\n  - To re-authenticate, run: gh auth login -h github.com\n";
        let c = interpret(false, out);
        assert_eq!(c.state, GhState::LoggedOut);
        assert_eq!(c.detail, "gh's login no longer works");
    }

    /// Words from gh 2.103.3's source: it says this when GitHub didn't answer in time.
    #[test]
    fn a_timeout_says_so_and_is_not_a_sign_in() {
        let out = "github.com\n  X Timeout trying to log in to github.com account me (keyring)\n  - Active account: true\n";
        let c = interpret(false, out);
        assert_eq!(c.state, GhState::Error);
        assert_eq!(
            c.detail,
            "Timeout trying to log in to github.com account me (keyring)"
        );
        let out =
            "github.com\n  X Timeout trying to log in to github.com using token (hosts.yml)\n";
        let c = interpret(false, out);
        assert_eq!(c.state, GhState::Error);
        assert_eq!(c.detail, "Timeout trying to log in to github.com");
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

    /// As read off Ubuntu's gh 2.46, with no token and with a bad one.
    #[test]
    fn an_old_gh_exits_0_when_its_login_failed() {
        let out = "github.com\n  X Failed to log in to github.com using token (hosts.yml)\n  - Active account: true\n  - The token in hosts.yml is invalid.\n  - To re-authenticate, run: gh auth login -h github.com\n  - To forget about this account, run: gh auth logout -h github.com -u \n";
        let c = interpret(true, out);
        assert_eq!(c.state, GhState::LoggedOut);
        assert_eq!(c.account, None);
        assert_eq!(c.detail, "Failed to log in to github.com");
        // With a login that works too, it's logged in.
        let out = "github.com\n  ✓ Logged in to github.com account me (keyring)\n  - Active account: true\n  X Failed to log in to github.com account old (keyring)\n";
        let c = interpret(true, out);
        assert_eq!(
            (c.state, c.account.as_deref()),
            (GhState::LoggedIn, Some("me"))
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

    /// Words as real gh 2.46 said them, in each case.
    #[tokio::test]
    async fn a_failed_login_is_only_believed_if_github_refuses_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let answering = |api: &str| {
            format!(
                r#"case "$1" in
auth) echo "  X Failed to log in to github.com account me (hosts.yml)"; exit 0 ;;
api) {api} ;;
esac"#
            )
        };
        let ask = |api: &str| {
            let gh = script(tmp.path(), &answering(api));
            async move { check(gh.as_os_str()).await }
        };
        for refused in [
            r#"echo '{"status":"401"}'; echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1"#,
            r#"echo 'gh: Requires authentication (HTTP 401)' >&2; exit 1"#,
            r#"echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4"#,
        ] {
            let c = ask(refused).await;
            assert_eq!(c.state, GhState::LoggedOut, "{refused}");
            assert!(c.detail.starts_with("Failed to log in"), "{}", c.detail);
        }
        // A good token, and no network: nothing for the owner to sign in to.
        let c = ask(
            r#"echo 'Get "https://api.github.com/user": proxyconnect tcp: dial tcp 127.0.0.1:9: connect: connection refused' >&2; exit 1"#,
        )
        .await;
        assert_eq!(c.state, GhState::Error);
        assert!(c.detail.contains("connection refused"), "{}", c.detail);
        // GitHub says it's fine, whatever `auth status` thought.
        let c = ask("echo me").await;
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
