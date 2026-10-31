//! Turns one JSON-RPC message into its response, through the core
//! handlers. Apart from the core's state it's pure, so the contract examples
//! in `protocol/examples/` replay through it exactly.

use serde_json::Value;
use tokio::sync::broadcast;
use vibe_core::rpc::{
    AgentsCheck, AgentsDelete, AgentsInterrupt, AgentsList, AgentsMessages, AgentsOutput,
    AgentsSend, AgentsStart, ErrorCode, ErrorObject, Event, EventsSubscribe, GhStatusGet, Hello,
    HelloParams, HelloResult, HostsAdd, HostsExternal, HostsList, HostsRemove, HostsUpgrade, Id,
    KeysCreate, KeysList, KeysRevoke, KeysSync, KeysVerify, LimitsGet, LimitsSet, McpGet, McpSet,
    Method, PROTOCOL_VERSION, ProfilesCreate, ProfilesDelete, ProfilesList, ProfilesUpdate,
    ProjectsAdd, ProjectsList, ProjectsRemove, ProtocolMismatchData, Request, Response,
    SessionsAttach, SessionsList, Subscribed, ToolboxLoginsGet, ToolboxOpen, ToolboxRebuild,
    ToolboxStatusGet, ToolboxUpdate, TunnelPrepare,
};

use crate::core::Core;
pub use crate::core::DaemonInfo;

/// What one connection carries between messages.
#[derive(Debug, Default)]
pub struct Session {
    subscribed: bool,
    /// It's another host's link (its hello says so), which pings: one that
    /// goes quiet was cut off, as by a pulled network.
    link: bool,
    /// A subscription made by the last message, for the connection to start
    /// forwarding once that message's response is out.
    new_subscription: Option<(u64, broadcast::Receiver<Event>)>,
}

impl Session {
    /// The subscription the last message made, if it made one, and the
    /// `seq` of the last event before it. Events published since are waiting
    /// in it.
    pub fn take_subscription(&mut self) -> Option<(u64, broadcast::Receiver<Event>)> {
        self.new_subscription.take()
    }

    /// How long the connection may stay quiet before it's taken for gone:
    /// for a link, a few of its pings; otherwise for ever.
    pub fn quiet_limit(&self) -> Option<std::time::Duration> {
        self.link.then_some(LINK_QUIET)
    }
}

/// How long a link may send nothing before its connection is closed: its
/// pings come every [`crate::link::PING_EVERY`], and an `ssh` cut off by a
/// pulled network leaves its `vibe rpc` here, connected, until then.
pub const LINK_QUIET: std::time::Duration = std::time::Duration::from_secs(60);

/// What a link's hello says it is.
const LINK_CLIENT: &str = "vibed link";

impl Session {
    fn hello_from(&mut self, client: &str) {
        if client.starts_with(LINK_CLIENT) {
            self.link = true;
        }
    }
}

/// Handles one message, a line without its newline, and returns the line to
/// send back, if any. Notifications, and batches of only notifications, get
/// nothing. A batch's calls run in order.
pub async fn handle_message(core: &Core, session: &mut Session, message: &[u8]) -> Option<String> {
    let value: Value = match serde_json::from_slice(message) {
        Ok(v) => v,
        Err(_) => {
            let error = ErrorObject::new(ErrorCode::ParseError, "Parse error");
            return Some(encode(&Response::error(Id::Null, error)));
        }
    };
    match value {
        Value::Array(items) if items.is_empty() => {
            Some(encode(&Response::error(Id::Null, invalid_request())))
        }
        Value::Array(items) => {
            let mut responses = Vec::new();
            for item in items {
                if let Some(r) = handle_value(core, session, item).await {
                    responses.push(r);
                }
            }
            (!responses.is_empty()).then(|| encode(&responses))
        }
        value => handle_value(core, session, value).await.map(|r| encode(&r)),
    }
}

/// Encodes a response, or a batch of them, as one line without its newline.
pub fn encode(response: &impl serde::Serialize) -> String {
    serde_json::to_string(response).expect("responses serialize to JSON")
}

fn invalid_request() -> ErrorObject {
    ErrorObject::new(ErrorCode::InvalidRequest, "Invalid Request")
}

fn invalid_params(detail: &str) -> ErrorObject {
    ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("Invalid params: {detail}"),
    )
}

async fn handle_value(core: &Core, session: &mut Session, value: Value) -> Option<Response> {
    // An invalid request still gets its id back when the id itself is valid.
    let id_hint = value
        .get("id")
        .and_then(|id| serde_json::from_value::<Id>(id.clone()).ok())
        .unwrap_or(Id::Null);
    let Ok(request) = serde_json::from_value::<Request>(value) else {
        return Some(Response::error(id_hint, invalid_request()));
    };
    if matches!(&request.params, Some(p) if !p.is_object() && !p.is_array()) {
        return Some(Response::error(id_hint, invalid_request()));
    }
    let outcome = call(core, session, &request.method, request.params).await;
    let id = request.id?;
    Some(match outcome {
        Ok(result) => Response::result(id, result),
        Err(error) => Response::error(id, error),
    })
}

async fn call(
    core: &Core,
    session: &mut Session,
    method: &str,
    params: Option<Value>,
) -> Result<Value, ErrorObject> {
    // A call for a Linux host goes to its vibed, over its link. While the
    // link is down, its agents are listed as they were last seen.
    if crate::hosts::FORWARDED.contains(&method)
        && let Some(link) = core.link_for(params.as_ref())
    {
        if method == AgentsList::NAME {
            // Its params are checked here, as no host will.
            decode::<AgentsList>(params)?;
            return encode_result(Ok(core.remote_agent_list(&link).await));
        }
        let params = params.unwrap_or_else(|| Value::Object(Default::default()));
        if !params.is_object() {
            return Err(invalid_params("params must be an object"));
        }
        return core.forward(&link, method, params).await;
    }
    match method {
        Hello::NAME => {
            // The version is checked before the params are decoded strictly,
            // so a client from a newer protocol, whose hello may have other
            // fields, still learns which version this daemon speaks.
            if let Some(theirs) = params
                .as_ref()
                .and_then(|p| p.get("protocol"))
                .and_then(Value::as_u64)
                && theirs != u64::from(PROTOCOL_VERSION)
            {
                return Err(protocol_mismatch(theirs));
            }
            let params = decode::<Hello>(params)?;
            session.hello_from(&params.client);
            encode_result(hello(core.info(), params))
        }
        HostsList::NAME => {
            decode::<HostsList>(params)?;
            encode_result(core.hosts())
        }
        HostsAdd::NAME => encode_result(core.add_host(decode::<HostsAdd>(params)?).await),
        HostsUpgrade::NAME => {
            encode_result(core.upgrade_host(decode::<HostsUpgrade>(params)?).await)
        }
        HostsRemove::NAME => encode_result(core.remove_host(decode::<HostsRemove>(params)?).await),
        HostsExternal::NAME => {
            encode_result(core.set_external(decode::<HostsExternal>(params)?).await)
        }
        TunnelPrepare::NAME => {
            decode::<TunnelPrepare>(params)?;
            encode_result(core.prepare_tunnel())
        }
        ProjectsList::NAME => encode_result(core.projects(decode::<ProjectsList>(params)?)),
        ProjectsAdd::NAME => encode_result(core.add_project(decode::<ProjectsAdd>(params)?).await),
        ProjectsRemove::NAME => {
            encode_result(core.remove_project(decode::<ProjectsRemove>(params)?))
        }
        ProfilesList::NAME => encode_result(core.profiles(decode::<ProfilesList>(params)?)),
        ProfilesCreate::NAME => {
            encode_result(core.create_profile(decode::<ProfilesCreate>(params)?))
        }
        ProfilesUpdate::NAME => {
            encode_result(core.update_profile(decode::<ProfilesUpdate>(params)?))
        }
        ProfilesDelete::NAME => {
            encode_result(core.delete_profile(decode::<ProfilesDelete>(params)?))
        }
        LimitsGet::NAME => encode_result(core.limits(decode::<LimitsGet>(params)?)),
        LimitsSet::NAME => encode_result(core.set_limits(decode::<LimitsSet>(params)?)),
        McpGet::NAME => encode_result(core.mcp(decode::<McpGet>(params)?)),
        McpSet::NAME => encode_result(core.set_mcp(decode::<McpSet>(params)?)),
        KeysList::NAME => {
            decode::<KeysList>(params)?;
            encode_result(core.keys())
        }
        KeysCreate::NAME => encode_result(core.create_key(decode::<KeysCreate>(params)?)),
        KeysRevoke::NAME => encode_result(core.revoke_key(decode::<KeysRevoke>(params)?)),
        KeysVerify::NAME => encode_result(core.verify_key_call(decode::<KeysVerify>(params)?)),
        KeysSync::NAME => encode_result(core.sync_keys(decode::<KeysSync>(params)?)),
        GhStatusGet::NAME => {
            let p = decode::<GhStatusGet>(params)?;
            encode_result(Ok(core.gh_status(p).await))
        }
        EventsSubscribe::NAME => {
            decode::<EventsSubscribe>(params)?;
            if session.subscribed {
                return Err(ErrorObject::new(
                    ErrorCode::Conflict,
                    "this connection is already subscribed",
                ));
            }
            let (seq, rx) = core.subscribe();
            session.subscribed = true;
            session.new_subscription = Some((seq, rx));
            encode_result(Ok(Subscribed { seq }))
        }
        SessionsList::NAME => encode_result(core.sessions(decode::<SessionsList>(params)?).await),
        SessionsAttach::NAME => {
            encode_result(core.attach_session(decode::<SessionsAttach>(params)?).await)
        }
        AgentsStart::NAME => encode_result(core.start_agent(decode::<AgentsStart>(params)?).await),
        AgentsList::NAME => encode_result(core.agents(decode::<AgentsList>(params)?).await),
        AgentsDelete::NAME => {
            encode_result(core.delete_agent(decode::<AgentsDelete>(params)?).await)
        }
        AgentsInterrupt::NAME => encode_result(
            core.interrupt_agent(decode::<AgentsInterrupt>(params)?)
                .await,
        ),
        AgentsSend::NAME => encode_result(core.send_call(decode::<AgentsSend>(params)?).await),
        AgentsCheck::NAME => encode_result(core.check_call(decode::<AgentsCheck>(params)?).await),
        AgentsOutput::NAME => {
            encode_result(core.output_call(decode::<AgentsOutput>(params)?).await)
        }
        AgentsMessages::NAME => {
            encode_result(core.messages_call(decode::<AgentsMessages>(params)?).await)
        }
        ToolboxStatusGet::NAME => encode_result(
            core.toolbox_status(decode::<ToolboxStatusGet>(params)?)
                .await,
        ),
        ToolboxOpen::NAME => encode_result(core.open_toolbox(decode::<ToolboxOpen>(params)?).await),
        ToolboxUpdate::NAME => {
            encode_result(core.update_toolbox(decode::<ToolboxUpdate>(params)?).await)
        }
        ToolboxRebuild::NAME => encode_result(
            core.rebuild_toolbox(decode::<ToolboxRebuild>(params)?)
                .await,
        ),
        ToolboxLoginsGet::NAME => encode_result(
            core.toolbox_logins(decode::<ToolboxLoginsGet>(params)?)
                .await,
        ),
        _ => Err(ErrorObject::new(
            ErrorCode::MethodNotFound,
            format!("Method not found: {method}"),
        )),
    }
}

/// Calls `method` on this host, with `params`, as a connection of its own
/// would. The tunnel's socket runs its calls through this, with no `host`
/// in them, so none goes on to a Linux host.
pub(crate) async fn call_here(
    core: &Core,
    method: &str,
    params: Value,
) -> Result<Value, ErrorObject> {
    call(core, &mut Session::default(), method, Some(params)).await
}

/// Decodes `M`'s params, by name only; absent means `{}`.
fn decode<M: Method>(params: Option<Value>) -> Result<M::Params, ErrorObject> {
    let params = match params {
        None => Value::Object(Default::default()),
        Some(p @ Value::Object(_)) => p,
        Some(_) => return Err(invalid_params("params must be an object")),
    };
    serde_json::from_value(params).map_err(|e| invalid_params(&e.to_string()))
}

fn encode_result<T: serde::Serialize>(
    result: Result<T, ErrorObject>,
) -> Result<Value, ErrorObject> {
    serde_json::to_value(result?)
        .map_err(|e| ErrorObject::new(ErrorCode::InternalError, format!("Internal error: {e}")))
}

fn protocol_mismatch(theirs: u64) -> ErrorObject {
    ErrorObject::new(
        ErrorCode::ProtocolMismatch,
        format!(
            "Protocol version mismatch: the client speaks {theirs}, vibed speaks {PROTOCOL_VERSION}"
        ),
    )
    .with_data(ProtocolMismatchData {
        protocol: PROTOCOL_VERSION,
    })
}

pub(crate) fn hello(info: &DaemonInfo, params: HelloParams) -> Result<HelloResult, ErrorObject> {
    if params.protocol != PROTOCOL_VERSION {
        return Err(protocol_mismatch(params.protocol.into()));
    }
    crate::log!(
        "vibed: hello from {} (protocol {})",
        params.client,
        params.protocol
    );
    Ok(HelloResult {
        daemon: "vibed".to_owned(),
        version: info.version.clone(),
        protocol: PROTOCOL_VERSION,
        os: info.os.clone(),
        arch: info.arch.clone(),
        pid: info.pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Config;
    use serde_json::json;
    use vibe_core::paths::Layout;

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: "9.9.9".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            pid: 1,
        }
    }

    fn core(dir: &tempfile::TempDir) -> Core {
        Core::open(Config::new(info(), Layout::new(dir.path(), "linux"))).unwrap()
    }

    fn reply(message: &str) -> Option<Value> {
        reply_bytes(message.as_bytes()).map(|s| serde_json::from_str(&s).unwrap())
    }

    fn reply_bytes(message: &[u8]) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let core = core(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(handle_message(&core, &mut Session::default(), message));
        if let Some(s) = &out {
            assert!(!s.contains('\n'), "a response is one line");
        }
        out
    }

    #[test]
    fn current_info_describes_this_process() {
        let i = DaemonInfo::current();
        assert_eq!(i.pid, std::process::id());
        assert_eq!(i.os, std::env::consts::OS);
        assert_eq!(i.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn hello_reports_the_daemon() {
        let r = reply(
            r#"{"jsonrpc":"2.0","id":1,"method":"daemon.hello","params":{"client":"t","protocol":1}}"#,
        );
        assert_eq!(
            r,
            Some(json!({"jsonrpc": "2.0", "id": 1, "result": {
                "daemon": "vibed", "version": "9.9.9", "protocol": 1,
                "os": "linux", "arch": "x86_64", "pid": 1}}))
        );
    }

    #[test]
    fn invalid_utf8_is_a_parse_error() {
        let r = reply_bytes(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"\xff\"}").unwrap();
        assert!(r.contains("-32700"), "{r}");
    }

    #[test]
    fn missing_params_mean_an_empty_object() {
        let r = reply(r#"{"jsonrpc":"2.0","id":1,"method":"daemon.hello"}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602);
        assert_eq!(
            r["error"]["message"],
            "Invalid params: missing field `client`"
        );
    }

    #[test]
    fn unknown_param_fields_are_invalid_params() {
        let r = reply(r#"{"jsonrpc":"2.0","id":1,"method":"daemon.hello","params":{"client":"t","protocol":1,"x":0}}"#)
            .unwrap();
        assert_eq!(r["error"]["code"], -32602);
    }

    #[test]
    fn invalid_request_with_unreadable_id_gets_null() {
        let r = reply(r#"{"jsonrpc":"2.0","id":{"a":1},"method":"daemon.hello"}"#).unwrap();
        assert_eq!(r["id"], Value::Null);
        assert_eq!(r["error"]["code"], -32600);
    }

    #[test]
    fn invalid_notification_still_gets_an_error() {
        let r = reply(r#"{"jsonrpc":"2.0","method":"daemon.hello","params":5}"#).unwrap();
        assert_eq!(r["id"], Value::Null);
        assert_eq!(r["error"]["code"], -32600);
    }

    #[test]
    fn nested_batches_are_invalid_requests() {
        let r = reply("[[]]").unwrap();
        assert_eq!(
            r,
            json!([{"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "Invalid Request"}}])
        );
    }

    #[test]
    fn methods_without_params_take_an_empty_object_or_none() {
        for m in [
            r#"{"jsonrpc":"2.0","id":1,"method":"hosts.list"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"hosts.list","params":{}}"#,
        ] {
            assert_eq!(
                reply(m).unwrap()["result"]["hosts"][0]["name"],
                "local",
                "{m}"
            );
        }
        let r =
            reply(r#"{"jsonrpc":"2.0","id":1,"method":"hosts.list","params":{"x":1}}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602);
    }

    #[test]
    fn subscribing_twice_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let core = core(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut session = Session::default();
        let sub = br#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#;
        let first = rt
            .block_on(handle_message(&core, &mut session, sub))
            .unwrap();
        assert!(first.contains(r#""result":{"seq":0}"#), "{first}");
        assert!(session.take_subscription().is_some());
        assert!(session.take_subscription().is_none());
        let second = rt
            .block_on(handle_message(&core, &mut session, sub))
            .unwrap();
        assert!(second.contains("-32002"), "{second}");
        assert!(session.take_subscription().is_none());
    }

    #[test]
    fn a_links_connection_may_go_quiet_only_so_long() {
        let dir = tempfile::tempdir().unwrap();
        let core = core(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let hello = |client: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"daemon.hello","params":{{"client":"{client}","protocol":1}}}}"#
            )
        };
        for (client, limit) in [
            ("VibeOS.app", None),
            ("vibe-cli", None),
            ("vibed link from mac-mini", Some(LINK_QUIET)),
            ("vibed link calls from mac-mini", Some(LINK_QUIET)),
        ] {
            let mut session = Session::default();
            assert_eq!(session.quiet_limit(), None);
            rt.block_on(handle_message(
                &core,
                &mut session,
                hello(client).as_bytes(),
            ))
            .unwrap();
            assert_eq!(session.quiet_limit(), limit, "{client}");
        }
        assert!(LINK_QUIET > 2 * crate::link::PING_EVERY);
    }

    #[test]
    fn scalars_are_invalid_requests() {
        for m in ["1", "\"x\"", "null", "true"] {
            assert_eq!(reply(m).unwrap()["error"]["code"], -32600, "{m}");
        }
    }
}
