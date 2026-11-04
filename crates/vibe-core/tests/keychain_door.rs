//! Nothing goes around `vibe_core::keychain`: the guard on the login Keychain
//! only holds if it's the one way there. A new call to `security` anywhere in
//! the workspace fails here until it goes through that door.

use std::fs;
use std::path::{Path, PathBuf};

/// `(file, what it's allowed)`: the door itself, and `check-full.sh`'s one read
/// of the Claude token, behind VIBE_CHECK_KEYCHAIN_TOKEN.
const ALLOWED: &[(&str, &str)] = &[
    ("crates/vibe-core/src/keychain.rs", "\"/usr/bin/security\""),
    (
        "scripts/check-full.sh",
        "token=$(security find-generic-password -s VibeOS -a claude-setup-token -w 2>/dev/null) || {",
    ),
];

const THIS_FILE: &str = "crates/vibe-core/tests/keychain_door.rs";

/// What starts `security`, in Rust and in shell.
fn reaches_keychain(line: &str) -> bool {
    if line.contains("\"/usr/bin/security\"")
        || line.contains("Command::new(\"security\")")
        || line.contains("/usr/bin/security ")
    {
        return true;
    }
    const VERBS: &[&str] = &[
        "find-generic-password",
        "add-generic-password",
        "delete-generic-password",
        "find-internet-password",
        "default-keychain",
        "list-keychains",
        "login-keychain",
        "unlock-keychain",
    ];
    VERBS
        .iter()
        .any(|verb| line.contains(&format!("security {verb}")))
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
                .position(|(f, what)| *f == name && (trimmed == *what || trimmed.contains(*what)));
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
        "x=$(security find-generic-password -s a -w)",
        "/usr/bin/security -i",
        "security add-generic-password -U",
    ] {
        assert!(reaches_keychain(line), "{line}");
    }
    for line in [
        "//! Auth, security and trust",
        "\"the Keychain\"",
        "security didn't answer in 10s",
    ] {
        assert!(!reaches_keychain(line), "{line}");
    }
}
