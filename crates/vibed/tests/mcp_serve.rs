//! MCP on a Linux host's tailnet name (#151): `mcp.serve.get` and `mcp.serve.set` against a
//! stand-in for `tailscale`. No test here, or anywhere, reaches the machine's own.

mod common;

#[path = "../../vibe-mcp/tests/client/mod.rs"]
mod client;

use std::sync::Arc;
use std::time::{Duration, Instant};

use client::post_raw;
use common::{FakeTailscale, GOLDEN_TAILSCALE, Golden};
use serde_json::json;
use vibe_core::limits::MCP_PORT;
use vibe_core::model::{AdminNeed, EventKind, LOCAL_HOST};
use vibe_core::rpc::{
    ErrorCode, HostParams, KeyCreateParams, McpServe, McpServeSetParams, McpSetParams,
};
use vibed::core::Core;

const URL: &str = "https://vps.tailnet-example.ts.net/mcp";

struct World {
    golden: Golden,
    core: Arc<Core>,
    fake: FakeTailscale,
}

impl World {
    /// A host with `mcp_tailscale` off, and Tailscale up with nothing served.
    fn new() -> World {
        Self::with(|_| {})
    }

    fn with(change: impl FnOnce(&mut vibed::core::Config)) -> World {
        let golden = Golden::new();
        let fake = FakeTailscale::new(&golden.dir.path().canonicalize().unwrap().join("ts"));
        fake.write(
            "after-set.json",
            &FakeTailscale::serving(&[("/mcp", &FakeTailscale::our_target(MCP_PORT))]),
        );
        let mut config = golden.config();
        config.info.os = "linux".into();
        config.toolbox = None;
        config.tailscale_cli = Some(fake.program.clone().into_os_string());
        config.tailscale_wait = Duration::from_secs(10);
        change(&mut config);
        let core = Arc::new(Core::open(config).unwrap());
        World { golden, core, fake }
    }

    fn host_toml(&self) -> std::path::PathBuf {
        self.golden.home.join(".config/vibeos/host.toml")
    }

    /// `mcp_tailscale` as `host.toml` has it.
    fn flag(&self) -> bool {
        self.core.mcp_settings().unwrap().1
    }

    fn set_flag(&self, on: bool) {
        self.core
            .set_mcp(McpSetParams {
                host: None,
                tailscale: Some(on),
            })
            .unwrap();
    }

    async fn get(&self) -> McpServe {
        self.core.mcp_serve(HostParams::default()).await.unwrap()
    }

    async fn set(&self, on: bool) -> Result<McpServe, vibe_core::rpc::ErrorObject> {
        self.core
            .set_mcp_serve(McpServeSetParams { host: None, on })
            .await
    }

    fn needs_operator(&self) -> bool {
        self.core
            .hosts()
            .unwrap()
            .hosts
            .into_iter()
            .find(|h| h.name == LOCAL_HOST)
            .unwrap()
            .needs_admin
            .contains(&AdminNeed::TailscaleOperator)
    }
}

fn set_command() -> String {
    format!(
        "serve --bg --https=443 --set-path=/mcp {}",
        FakeTailscale::our_target(MCP_PORT)
    )
}

const OFF_COMMAND: &str = "serve --https=443 --set-path=/mcp off";

#[tokio::test]
async fn on_sets_both_and_off_takes_only_mcp_off_and_clears_it() {
    let w = World::new();
    let other = ("/", "http://127.0.0.1:3000");
    w.fake
        .write("serve.json", &FakeTailscale::serving(&[other]));
    w.fake.write(
        "after-set.json",
        &FakeTailscale::serving(&[other, ("/mcp", &FakeTailscale::our_target(MCP_PORT))]),
    );
    w.fake
        .write("after-off.json", &FakeTailscale::serving(&[other]));
    std::fs::create_dir_all(w.host_toml().parent().unwrap()).unwrap();
    std::fs::write(w.host_toml(), "# mine\ncpu_cores = 2\n").unwrap();

    let got = w.get().await;
    assert_eq!(
        got,
        McpServe {
            on: false,
            available: true,
            url: None,
            note: None
        }
    );

    let (_, mut events) = w.core.subscribe();
    let on = w.set(true).await.unwrap();
    assert_eq!(
        on,
        McpServe {
            on: true,
            available: true,
            url: Some(URL.to_owned()),
            note: None
        }
    );
    assert!(w.flag());
    assert_eq!(w.fake.changes(), [set_command()]);
    assert_eq!(events.try_recv().unwrap().kind, EventKind::McpChanged);
    assert!(events.try_recv().is_err(), "one event for the one change");
    // It's the Tailscale address that's the target, the listener Serve can reach, not loopback.
    assert!(!w.fake.calls().iter().any(|c| c.contains("127.0.0.1")));
    let text = std::fs::read_to_string(w.host_toml()).unwrap();
    assert!(text.starts_with("# mine\ncpu_cores = 2\n"), "{text}");
    assert_eq!(w.get().await, on);

    let off = w.set(false).await.unwrap();
    assert_eq!(
        off,
        McpServe {
            on: false,
            available: true,
            url: None,
            note: None
        }
    );
    assert!(!w.flag());
    assert_eq!(w.fake.changes(), [set_command(), OFF_COMMAND.to_owned()]);
    assert_eq!(
        w.fake.read("serve.json"),
        FakeTailscale::serving(&[other]),
        "what else it serves stays"
    );
    assert_eq!(events.try_recv().unwrap().kind, EventKind::McpChanged);
}

/// Exactly what's on the live host, by hand: on, and it stays as it is. No write of any kind.
#[tokio::test]
async fn on_when_its_already_so_doesnt_touch_serve() {
    let w = World::new();
    let serving = FakeTailscale::serving(&[("/mcp", &FakeTailscale::our_target(MCP_PORT))]);
    w.fake.write("serve.json", &serving);
    w.set_flag(true);
    let got = w.get().await;
    assert!(got.on, "{got:?}");
    assert_eq!(got.url.as_deref(), Some(URL));
    w.set(true).await.unwrap();
    assert_eq!(w.fake.changes(), Vec::<String>::new());
    assert_eq!(w.fake.read("serve.json"), serving);
    assert!(w.flag());
}

/// The real CLI overwrites `/mcp` with no complaint and exit 0, so it mustn't get the chance.
#[tokio::test]
async fn a_mcp_that_isnt_this_hosts_stops_on_and_off_and_is_left_alone() {
    for target in [
        "http://127.0.0.1:9999/other".to_owned(),
        // Loopback, this host's own port: the Host gate rejects it, and it isn't what's asked.
        format!("http://127.0.0.1:{MCP_PORT}/mcp"),
        // This host's address, on a port it's not on any more.
        format!("http://{GOLDEN_TAILSCALE}:1234/mcp"),
    ] {
        let w = World::new();
        let serving = FakeTailscale::serving(&[("/mcp", &target)]);
        w.fake.write("serve.json", &serving);

        let got = w.get().await;
        assert!(!got.on && got.available, "{got:?}");
        assert!(got.url.is_none());
        let note = got.note.unwrap();
        assert!(note.contains(&target), "{note}");

        let e = w.set(true).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict.code(), "{e:?}");
        assert!(e.message.contains(&target), "{}", e.message);
        assert!(e.message.contains("leaves it alone"), "{}", e.message);
        assert!(!w.flag(), "{target}: nothing changed");

        w.set_flag(true);
        let e = w.set(false).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict.code(), "{e:?}");
        assert!(w.flag(), "{target}: off stopped too");

        assert_eq!(w.fake.changes(), Vec::<String>::new());
        assert_eq!(w.fake.read("serve.json"), serving);
    }
}

/// It's never turned on, `/mcp` isn't added to a name that has it, and the real CLI would
/// turn it off in passing, which isn't this toggle's to do either.
#[tokio::test]
async fn funnel_stops_on_and_is_left_alone_and_off_still_works() {
    let w = World::new();
    let funnel = format!(
        r#"{{"AllowFunnel":{{"{}:443":true}},"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"{}:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:3000"}}}}}}}}}}"#,
        FakeTailscale::NAME,
        FakeTailscale::NAME,
    );
    w.fake.write("serve.json", &funnel);
    let got = w.get().await;
    assert!(!got.on);
    assert!(got.note.unwrap().contains("Funnel"));
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict.code());
    assert!(e.message.contains("public internet"), "{}", e.message);
    assert_eq!(w.fake.changes(), Vec::<String>::new());
    assert!(!w.flag());
    assert_eq!(w.fake.read("serve.json"), funnel);

    // Funnel turned on by hand, on a name that already serves /mcp: the toggle says what
    // that means, and can always take it off.
    let ours = FakeTailscale::our_target(MCP_PORT);
    let exposed = format!(
        r#"{{"AllowFunnel":{{"{}:443":true}},"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"{}:443":{{"Handlers":{{"/mcp":{{"Proxy":"{ours}"}}}}}}}}}}"#,
        FakeTailscale::NAME,
        FakeTailscale::NAME,
    );
    w.fake.write("serve.json", &exposed);
    w.set_flag(true);
    let got = w.get().await;
    assert!(got.on);
    assert!(got.note.unwrap().contains("public internet"));
    assert_eq!(
        w.set(true).await.unwrap_err().code,
        ErrorCode::Conflict.code()
    );
    assert_eq!(w.fake.changes(), Vec::<String>::new());
    w.set(false).await.unwrap();
    assert_eq!(w.fake.changes(), [OFF_COMMAND.to_owned()]);
    assert!(!w.flag());
    for call in w.fake.calls() {
        assert!(!call.contains("funnel"), "{call}");
    }
}

#[tokio::test]
async fn refused_for_want_of_operator_says_how_and_offers_allow_until_it_works() {
    let w = World::new();
    // As tailscale 1.104.5 said it to a user who isn't its operator.
    w.fake.write(
        "on-set.sh",
        "echo 'sending serve config: Access denied: serve config denied' >&2; exit 1\n",
    );
    let (_, mut events) = w.core.subscribe();
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert!(
        e.message.contains("sudo tailscale set --operator="),
        "{}",
        e.message
    );
    assert!(e.message.contains("Allow…"), "{}", e.message);
    assert!(e.message.contains("Access denied"), "{}", e.message);
    assert!(!w.flag(), "put back");
    assert!(w.needs_operator(), "hosts.admin's flow offers it");
    assert_eq!(events.try_recv().unwrap().kind, EventKind::HostLink);

    // Allowed: the same toggle now works, and it's not asked for again.
    std::fs::remove_file(w.fake.dir.join("on-set.sh")).unwrap();
    assert!(w.set(true).await.unwrap().on);
    assert!(!w.needs_operator());
    assert!(w.flag());

    // Refused taking it off: still on, as it was.
    w.fake.write(
        "on-off.sh",
        "echo 'sending serve config: Access denied: serve config denied' >&2; exit 1\n",
    );
    let e = w.set(false).await.unwrap_err();
    assert!(e.message.contains("--operator="), "{}", e.message);
    assert!(w.flag());
    assert!(w.get().await.on);
    assert!(w.needs_operator());
}

/// `tailscale serve` printing a link and exiting 0, as it does where it can't wait for
/// someone to turn HTTPS on: that's not success.
#[tokio::test]
async fn exit_0_with_https_off_is_tailscales_link_not_success() {
    let w = World::new();
    w.fake.write(
        "on-set.sh",
        r#"printf '\nServe is not enabled on your tailnet.\nAsk your admin to enable it:\n\n         https://login.tailscale.com/f/serve?node=abc123\n\n'
exit 0
"#,
    );
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert!(
        e.message.contains("Serve is not enabled on your tailnet"),
        "{}",
        e.message
    );
    assert!(
        e.message
            .contains("https://login.tailscale.com/f/serve?node=abc123"),
        "{}",
        e.message
    );
    assert_eq!(
        e.data,
        Some(json!({
            "tailscale_enable_url": "https://login.tailscale.com/f/serve?node=abc123"
        }))
    );
    assert!(!w.flag(), "put back");
    assert!(!w.needs_operator());
}

/// Where it can wait, it waits for good, with what it said on stdout. It's not waited for.
#[tokio::test]
async fn a_serve_that_waits_is_killed_with_what_it_said_kept() {
    let w = World::with(|c| c.tailscale_wait = Duration::from_millis(1500));
    w.fake.write(
        "on-set.sh",
        r#"printf '\nServe is not enabled on your tailnet.\nTo enable, visit:\n\n         https://login.tailscale.com/f/serve?node=abc123\n\n'
sleep 300 &
echo $! > "$d/child"
wait
"#,
    );
    let started = Instant::now();
    let e = w.set(true).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert!(
        e.message
            .contains("https://login.tailscale.com/f/serve?node=abc123"),
        "{}",
        e.message
    );
    assert_eq!(
        e.data,
        Some(json!({
            "tailscale_enable_url": "https://login.tailscale.com/f/serve?node=abc123"
        }))
    );
    assert!(!w.flag(), "put back");
    // What it started went with it.
    let child = w.fake.read("child").trim().parse::<i32>().unwrap();
    let mut gone = false;
    for _ in 0..100 {
        // SAFETY: signal 0 only asks whether the process is there.
        if unsafe { libc::kill(child, 0) } != 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "pid {child} outlived the command");
}

#[tokio::test]
async fn a_command_that_says_it_worked_but_didnt_isnt_believed() {
    // On: exits 0 and /mcp isn't there.
    let w = World::new();
    w.fake.write("after-set.json", "{}");
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert!(!w.flag(), "put back");

    // Off: exits 0 and /mcp is still there, so it's still on.
    let w = World::new();
    let serving = FakeTailscale::serving(&[("/mcp", &FakeTailscale::our_target(MCP_PORT))]);
    w.fake.write("serve.json", &serving);
    w.fake.write("after-off.json", &serving);
    w.set_flag(true);
    assert!(w.set(false).await.is_err());
    assert!(w.flag());

    // On: someone else's turned up at /mcp between the look and the read back.
    let w = World::new();
    w.fake.write(
        "after-set.json",
        &FakeTailscale::serving(&[("/mcp", "http://127.0.0.1:1")]),
    );
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict.code());
    assert!(!w.flag(), "put back");
}

#[tokio::test]
async fn on_with_the_flag_already_on_leaves_it_on_if_serve_fails() {
    let w = World::new();
    w.set_flag(true);
    w.fake
        .write("on-set.sh", "echo 'port 443 is in use' >&2; exit 1\n");
    let e = w.set(true).await.unwrap_err();
    assert!(e.message.contains("port 443 is in use"), "{}", e.message);
    assert!(w.flag(), "as it was by hand, before");
}

#[tokio::test]
async fn off_with_nothing_on_serve_runs_nothing_and_clears_the_flag() {
    let w = World::new();
    w.set_flag(true);
    w.set(false).await.unwrap();
    assert!(!w.flag());
    assert_eq!(w.fake.changes(), Vec::<String>::new());
}

#[tokio::test]
async fn a_status_that_doesnt_read_stops_it_before_any_change() {
    for (file, text) in [
        ("serve.json", "this isn't json"),
        ("serve.json", ""),
        ("status.json", "<html>"),
    ] {
        let w = World::new();
        w.fake.write(file, text);
        let got = w.get().await;
        assert!(!got.available, "{file} {text:?}: {got:?}");
        assert!(w.set(true).await.is_err(), "{file} {text:?}");
        w.set_flag(true);
        assert!(w.set(false).await.is_err(), "{file} {text:?}");
        assert!(w.flag());
        assert_eq!(w.fake.changes(), Vec::<String>::new());
    }
}

/// Tailscale's install and login are the owner's: it's said, and nothing runs them.
#[tokio::test]
async fn logged_out_or_absent_tailscale_is_said_and_never_set_up() {
    for state in ["NeedsLogin", "Stopped", "NeedsMachineAuth"] {
        let w = World::new();
        w.fake.write(
            "status.json",
            &format!(r#"{{"BackendState":"{state}","Self":{{"DNSName":""}}}}"#),
        );
        let got = w.get().await;
        assert!(!got.available && !got.on, "{got:?}");
        assert!(got.note.unwrap().contains(state));
        assert!(w.set(true).await.unwrap_err().message.contains(state));
        assert!(!w.flag());
        assert_eq!(w.fake.changes(), Vec::<String>::new());
    }

    let w = World::with(|c| c.tailscale_cli = Some("vibe-test-no-such-tailscale".into()));
    let got = w.get().await;
    assert!(!got.available);
    assert!(got.note.unwrap().contains("isn't installed"));
    let e = w.set(true).await.unwrap_err();
    assert!(e.message.contains("isn't installed"), "{}", e.message);
    assert!(!w.flag());
    // Off's flag can still go: there's no Serve to take it off of.
    w.set_flag(true);
    assert!(!w.set(false).await.unwrap().on);
    assert!(!w.flag());
}

#[tokio::test]
async fn with_no_tailscale_address_to_listen_on_on_is_refused() {
    let w = World::with(|c| c.tailscale = Arc::new(Vec::new));
    let got = w.get().await;
    assert!(!got.available);
    assert!(got.note.unwrap().contains("no Tailscale address"));
    let e = w.set(true).await.unwrap_err();
    assert!(e.message.contains("no Tailscale address"), "{}", e.message);
    assert_eq!(w.fake.changes(), Vec::<String>::new());
    assert!(!w.flag());
}

/// A wildcard or public "address" never becomes a target, whatever the machine reports.
#[tokio::test]
async fn only_a_tailscale_address_is_ever_the_target() {
    let w = World::with(|c| {
        c.tailscale = Arc::new(|| {
            ["0.0.0.0", "203.0.113.9", GOLDEN_TAILSCALE, "::"]
                .iter()
                .map(|a| a.parse().unwrap())
                .collect()
        });
    });
    w.set(true).await.unwrap();
    assert_eq!(w.fake.changes(), [set_command()]);
}

#[tokio::test]
async fn a_port_of_its_own_is_the_targets() {
    let w = World::new();
    std::fs::create_dir_all(w.host_toml().parent().unwrap()).unwrap();
    std::fs::write(w.host_toml(), "mcp_port = 9123\n").unwrap();
    w.fake.write(
        "after-set.json",
        &FakeTailscale::serving(&[("/mcp", &FakeTailscale::our_target(9123))]),
    );
    assert!(w.set(true).await.unwrap().on);
    assert_eq!(
        w.fake.changes(),
        [format!(
            "serve --bg --https=443 --set-path=/mcp http://{GOLDEN_TAILSCALE}:9123/mcp"
        )]
    );
}

#[tokio::test]
async fn a_broken_host_toml_stops_it_before_serve_is_touched() {
    let w = World::new();
    std::fs::create_dir_all(w.host_toml().parent().unwrap()).unwrap();
    std::fs::write(w.host_toml(), "cpu_core = 4\n").unwrap();
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::HostConfigInvalid.code());
    assert!(w.fake.calls().is_empty(), "{:?}", w.fake.calls());
}

/// The Mac's toggle is `mcp.set`, as it always was: nothing here reaches `tailscale`, and the
/// Serve toggle isn't for it.
#[tokio::test]
async fn a_mac_has_its_own_toggle_and_no_serve() {
    let golden = Golden::new();
    let core = Core::open(golden.config()).unwrap();
    let got = core.mcp_serve(HostParams::default()).await.unwrap();
    assert!(!got.on && !got.available, "{got:?}");
    assert!(got.note.unwrap().contains("Linux host"));
    for on in [true, false] {
        let e = core
            .set_mcp_serve(McpServeSetParams { host: None, on })
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams.code(), "{e:?}");
    }
    let s = core
        .set_mcp(McpSetParams {
            host: None,
            tailscale: Some(true),
        })
        .unwrap();
    assert!(s.tailscale);
    assert_eq!(
        s.tailscale_urls,
        [format!("http://{GOLDEN_TAILSCALE}:{MCP_PORT}/mcp")]
    );
    // mcp.set alone, on a Linux host, doesn't reach Tailscale either.
    let w = World::new();
    w.set_flag(true);
    assert!(w.fake.calls().is_empty());
}

#[tokio::test]
async fn a_host_that_isnt_here_is_refused() {
    let w = World::new();
    let e = w
        .core
        .mcp_serve(HostParams {
            host: Some("vps".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound.code());
    let e = w
        .core
        .set_mcp_serve(McpServeSetParams {
            host: Some("vps".into()),
            on: true,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound.code());
    assert!(w.fake.calls().is_empty());
}

/// Two toggles at once take turns, so the second sees what the first did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes_take_turns() {
    let w = World::new();
    w.fake.write("on-set.sh", "sleep 0.3\n");
    let (a, b) = tokio::join!(w.set(true), w.set(true));
    assert!(a.unwrap().on && b.unwrap().on);
    assert_eq!(w.fake.changes(), [set_command()], "set once");
}

/// What Serve sends: an authenticated call with the `ts.net` name as its `Host`, to the
/// listener on the Tailscale address, with the real core and a real key. The key check runs
/// before the `Host` check, so only an authenticated call proves this; a 401 proves nothing.
/// Loopback's listener refuses the same call, which is why Serve can't target it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_authenticated_call_with_the_tailnet_name_gets_through_only_on_the_tailscale_listener() {
    let w = World::new();
    let key = w
        .core
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap()
        .secret;
    let auth = format!("Bearer {key}");
    let host = ("Host", "vps.tailnet-example.ts.net");
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {},
        }
    }})
    .to_string();
    let headers = [
        ("Authorization", auth.as_str()),
        host,
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", "tools/list"),
    ];

    // As spawn_listeners serves any address that isn't loopback, and loopback.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tailscale_like = listener.local_addr().unwrap();
    tokio::spawn(vibe_mcp::http::serve(listener, Arc::clone(&w.core), false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let loopback = listener.local_addr().unwrap();
    tokio::spawn(vibe_mcp::http::serve(listener, Arc::clone(&w.core), true));

    let reply = post_raw(tailscale_like, vibe_mcp::PATH, &headers, &call).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("vibe_list_agents"), "{}", reply.body);

    let refused = post_raw(loopback, vibe_mcp::PATH, &headers, &call).await;
    assert_eq!(refused.status, 403, "{}", refused.body);

    // No key: refused before anything else, wherever.
    let anon = post_raw(tailscale_like, vibe_mcp::PATH, &[host], &call).await;
    assert_eq!(anon.status, 401);
}

/// The real CLI replaces `/mcp/` when told to set `/mcp`, so the toggle mustn't get that far.
#[tokio::test]
async fn a_slashed_mcp_stops_it_and_says_how_to_remove_that_one() {
    let w = World::new();
    let serving = format!(
        r#"{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"{}:443":{{"Handlers":{{"/mcp/":{{"Path":"/srv/www"}}}}}}}}}}"#,
        FakeTailscale::NAME
    );
    w.fake.write("serve.json", &serving);
    let got = w.get().await;
    assert!(!got.on && got.available, "{got:?}");
    assert!(got.note.unwrap().contains("--set-path=/mcp/ off"));
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict.code());
    assert!(e.message.contains("--set-path=/mcp/ off"), "{}", e.message);
    assert!(!w.flag());
    w.set_flag(true);
    assert_eq!(
        w.set(false).await.unwrap_err().code,
        ErrorCode::Conflict.code()
    );
    assert_eq!(w.fake.changes(), Vec::<String>::new());
    assert_eq!(w.fake.read("serve.json"), serving);
}

/// An address in Tailscale's range on another interface, as a cloud network can have: not
/// Tailscale's, so not the target, and a `/mcp` pointing at it isn't this host's.
#[tokio::test]
async fn only_what_tailscale_says_is_its_own_address_is_the_target_or_ours() {
    let w = World::with(|c| {
        c.tailscale = Arc::new(|| {
            ["100.64.0.5", GOLDEN_TAILSCALE]
                .iter()
                .map(|a| a.parse().unwrap())
                .collect()
        });
    });
    w.fake.write(
        "serve.json",
        &FakeTailscale::serving(&[("/mcp", &format!("http://100.64.0.5:{MCP_PORT}/mcp"))]),
    );
    assert_eq!(
        w.set(true).await.unwrap_err().code,
        ErrorCode::Conflict.code()
    );
    w.fake.write("serve.json", "{}");
    w.set(true).await.unwrap();
    assert_eq!(w.fake.changes(), [set_command()]);

    // With only such an address, there's nowhere for Serve to reach.
    let w = World::with(|c| c.tailscale = Arc::new(|| vec!["100.64.0.5".parse().unwrap()]));
    assert!(!w.get().await.available);
    assert!(w.set(true).await.is_err());
    assert_eq!(w.fake.changes(), Vec::<String>::new());
}

/// The owner put it right by hand after a refusal: the toggle doesn't go on asking for `sudo`.
#[tokio::test]
async fn a_toggle_that_ends_up_as_asked_stops_asking_for_allow() {
    let w = World::new();
    w.fake.write(
        "on-set.sh",
        "echo 'sending serve config: Access denied: serve config denied' >&2; exit 1\n",
    );
    assert!(w.set(true).await.is_err());
    assert!(w.needs_operator());
    w.fake.write(
        "serve.json",
        &FakeTailscale::serving(&[("/mcp", &FakeTailscale::our_target(MCP_PORT))]),
    );
    // A read doesn't settle it: refused turning off, it's still on, and still refused.
    w.get().await;
    assert!(w.needs_operator());
    assert!(w.set(true).await.unwrap().on);
    assert!(!w.needs_operator());
}

/// Serve goes first: a refused turn-on never opens the listener, and if `vibed` goes down
/// while `tailscale serve` waits, there's no listener behind a toggle that says off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_dropped_while_serve_waits_leaves_the_flag_and_kills_the_command() {
    let w = World::new();
    w.fake.write(
        "on-set.sh",
        r#"echo $$ > "$d/child"
exec sleep 300
"#,
    );
    let core = Arc::clone(&w.core);
    let change = tokio::spawn(async move {
        core.set_mcp_serve(McpServeSetParams {
            host: None,
            on: true,
        })
        .await
    });
    for _ in 0..200 {
        if !w.fake.read("child").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!w.fake.read("child").is_empty(), "serve was never run");
    assert!(!w.flag(), "not until Serve has it");
    change.abort();
    let _ = change.await;
    assert!(!w.flag());
    let command = w.fake.read("child").trim().parse::<i32>().unwrap();
    let mut gone = false;
    for _ in 0..250 {
        // SAFETY: signal 0 only asks whether the process is there.
        if unsafe { libc::kill(command, 0) } != 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "pid {command} outlived the change");
}

/// `host.toml` broken between the look and the write: `/mcp` comes back off, as it wasn't on.
#[tokio::test]
async fn a_flag_that_cant_be_written_takes_mcp_back_off_serve() {
    let w = World::new();
    std::fs::create_dir_all(w.host_toml().parent().unwrap()).unwrap();
    w.fake.write(
        "on-set.sh",
        &format!("echo 'cpu_core = 4' > '{}'\n", w.host_toml().display()),
    );
    let e = w.set(true).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::HostConfigInvalid.code(), "{e:?}");
    assert_eq!(w.fake.changes(), [set_command(), OFF_COMMAND.to_owned()]);
    assert_eq!(w.fake.read("serve.json"), "{}");
}

/// The file setting, by hand: said, and the toggle is how to go further.
#[tokio::test]
async fn the_flag_alone_is_said_and_isnt_called_on() {
    let w = World::new();
    w.set_flag(true);
    let got = w.get().await;
    assert!(!got.on && got.url.is_none(), "{got:?}");
    assert!(got.note.unwrap().contains("mcp_tailscale is on"));
    assert_eq!(w.fake.changes(), Vec::<String>::new());
}

/// Funnel on a name with `/mcp` on Serve but its listener off: not called public.
#[tokio::test]
async fn funnel_with_the_listener_off_isnt_called_public_mcp() {
    let w = World::new();
    let ours = FakeTailscale::our_target(MCP_PORT);
    w.fake.write(
        "serve.json",
        &format!(
            r#"{{"AllowFunnel":{{"{n}:443":true}},"Web":{{"{n}:443":{{"Handlers":{{"/mcp":{{"Proxy":"{ours}"}}}}}}}}}}"#,
            n = FakeTailscale::NAME
        ),
    );
    let note = w.get().await.note.unwrap();
    assert!(
        !note.contains("this MCP is on the public internet"),
        "{note}"
    );
    assert!(note.contains("Funnel"), "{note}");
}

/// `tailscale serve` worked, and then Tailscale couldn't be read: not told as a refusal.
#[tokio::test]
async fn a_read_back_that_fails_isnt_called_a_refusal() {
    let w = World::new();
    // Its own script breaks the status it reads next.
    w.fake.write(
        "on-set.sh",
        r#"printf 'not json' > "$d/status.json"
"#,
    );
    let e = w.set(true).await.unwrap_err();
    assert!(e.message.contains("can't be read back"), "{}", e.message);
    assert!(!w.flag());
}
