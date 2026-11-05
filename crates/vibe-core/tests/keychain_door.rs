//! Nothing goes around `vibe_core::keychain`: the guard on the login Keychain
//! only holds if it's the one way there. A new call to `security` in `crates`,
//! `scripts` or `swift` fails here until it goes through that door. It's a check
//! and not a proof: it reads lines, so it can be got around, and it doesn't read
//! a release's own workflow, which no test runs.

use std::fs;
use std::path::{Path, PathBuf};

/// `(file, the whole line it's allowed)`: the door itself; `check-full.sh`'s one read of the
/// Claude token, behind VIBE_CHECK_KEYCHAIN_TOKEN; a release finding the identity
/// to sign with, which is no test and keeps nothing; and a word in a test's list.
const ALLOWED: &[(&str, &str)] = &[
    (
        "crates/vibe-core/src/keychain.rs",
        "const SECURITY: &str = \"/usr/bin/security\";",
    ),
    (
        "scripts/check-full.sh",
        "token=$(security find-generic-password -s VibeOS -a claude-setup-token -w 2>/dev/null) || {",
    ),
    (
        "scripts/release-macos.sh",
        "security find-identity -v -p codesigning | grep -F -- \"$IDENTITY\" | grep -q \"Developer ID Application: .*($APPLE_TEAM_ID)\\\"\" ||",
    ),
    ("crates/vibe-env/src/pi_logins/tests.rs", "\"security\","),
];

const THIS_FILE: &str = "crates/vibe-core/tests/keychain_door.rs";

/// What's only ever a way to the Keychain, in Rust, shell, Python and Swift.
const WAYS: &[&str] = &[
    "/usr/bin/security",
    "keychain::SECURITY",
    "Command::new(\"security\")",
    ".arg(\"security\")",
    "[\"security\"",
    "\"security\",",
    "['security'",
    "[ \"security\"",
    "SecItem",
    "SecKeychain",
    "security_framework",
];

/// What the `security` program does that touches a keychain, which is all but its
/// odds and ends.
const VERBS: &[&str] = &[
    "add-certificates",
    "add-generic-password",
    "add-internet-password",
    "add-trusted-cert",
    "authorizationdb",
    "authorize",
    "cms",
    "create-db",
    "create-filevaultmaster-keychain",
    "create-keychain",
    "create-keypair",
    "default-keychain",
    "delete-certificate",
    "delete-generic-password",
    "delete-identity",
    "delete-internet-password",
    "delete-keychain",
    "dump-keychain",
    "dump-trust-settings",
    "execute-with-privileges",
    "export",
    "find-certificate",
    "find-generic-password",
    "find-identity",
    "find-internet-password",
    "find-key",
    "get-identity-preference",
    "import",
    "list-keychains",
    "lock-keychain",
    "login-keychain",
    "remove-trusted-cert",
    "set-generic-password-partition-list",
    "set-identity-preference",
    "set-internet-password-partition-list",
    "set-key-partition-list",
    "set-keychain-password",
    "set-keychain-settings",
    "show-keychain-info",
    "trust-settings-export",
    "trust-settings-import",
    "unlock-keychain",
    "verify-cert",
];

/// Whether `line` has the word `security` with what makes it the program after it:
/// a verb, an option or a line continued, not the next word of a sentence.
fn runs_the_program(line: &str) -> bool {
    line.match_indices("security").any(|(at, word)| {
        let inside_a_word = line[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || "_-.".contains(c));
        let after = &line[at + word.len()..];
        let next = after.trim_start_matches([' ', '\t']);
        if inside_a_word || next.len() == after.len() {
            return false;
        }
        let next = next.split_whitespace().next().unwrap_or("");
        let verb = next.trim_end_matches(['"', '\'', '`', ')', ';', ',']);
        next == "\\"
            || VERBS.contains(&verb)
            || verb
                .strip_prefix('-')
                .is_some_and(|flag| flag.starts_with(|c: char| c.is_ascii_alphabetic()))
    })
}

/// What starts `security`, or reaches for a keychain some other way, in Rust and
/// in shell. A comment can say what it likes.
fn reaches_keychain(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with("//") || (line.starts_with('#') && !line.starts_with("#!")) {
        return false;
    }
    WAYS.iter().any(|way| line.contains(way)) || runs_the_program(line)
}

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "target" && name != "build" && !name.starts_with('.') {
                sources(&path, found);
            }
        } else if name.ends_with(".rs")
            || name.ends_with(".sh")
            || name.ends_with(".py")
            || name.ends_with(".swift")
        {
            found.push(path);
        }
    }
}

#[test]
fn only_the_door_runs_security() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["crates", "scripts", "swift"] {
        sources(&root.join(dir), &mut files);
    }
    assert!(files.len() > 50, "looked in the wrong place: {files:?}");

    let mut around = Vec::new();
    let mut seen = vec![false; ALLOWED.len()];
    for file in &files {
        let name = file.strip_prefix(&root).unwrap().to_string_lossy();
        if name == THIS_FILE {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            let allowed = ALLOWED
                .iter()
                .position(|(f, what)| *f == name && trimmed == *what);
            if let Some(allowed) = allowed {
                seen[allowed] = true;
            } else if reaches_keychain(trimmed) {
                around.push(format!("{name}:{}: {trimmed}", i + 1));
            }
        }
    }
    assert!(
        around.is_empty(),
        "these reach the Keychain around vibe_core::keychain, where a test could \
         reach the login Keychain:\n{}",
        around.join("\n")
    );
    assert!(seen.iter().all(|s| *s), "the door moved: {seen:?}");
}

#[test]
fn reaching_the_keychain_is_recognized() {
    for line in [
        "Command::new(\"/usr/bin/security\")",
        "Command::new(\"security\").arg(\"-i\")",
        "Command::new(keychain::SECURITY)",
        "Command::new(\"env\").arg(\"security\")",
        "self.run(&[\"security\", \"find-identity\"])",
        "\"security\",",
        "\"security\", \"-i\",",
        ".args([\"env\", \"security\", \"-i\"])",
        "subprocess.run(['security', 'dump-keychain'])",
        "x=$(security find-generic-password -s a -w)",
        "/usr/bin/security -i",
        "exec /usr/bin/security",
        "security add-generic-password -U",
        "security find-identity -v -p codesigning | grep -q x",
        "security dump-keychain",
        "security delete-keychain a.keychain-db",
        "security import cert.p12 -k a",
        "security -i",
        "security -v find-identity",
        "security  find-identity",
        "security\tfind-identity",
        "security \\",
        "printf 'x' | security -i;",
        "sh -c \"security unlock-keychain\"",
        "SecItemAdd(query, nil)",
        "SecKeychainOpen(path, &keychain)",
        "use security_framework::passwords;",
    ] {
        assert!(reaches_keychain(line), "{line}");
    }
    for line in [
        "//! Auth, security and trust",
        "// `security -i` reads lines of at most 4 KB",
        "/// security find-identity",
        "# security find-identity",
        "\"the Keychain\"",
        "security didn't answer in 10s",
        "\"security can't read the Claude token: {}\"",
        "\"security {} in {}: {}\"",
        "\"\"\"Sparkle's side of a release (PRD: Auth, security and",
        "*com.apple.security.virtualization*) ;;",
        "let program = dir.path().join(\"security\");",
        "\"securityLevel\",",
        "\"security_level\",",
        "insecurity -i",
        // HostSupport reads its own entitlement with this.
        "import Security",
    ] {
        assert!(!reaches_keychain(line), "{line}");
    }
}
