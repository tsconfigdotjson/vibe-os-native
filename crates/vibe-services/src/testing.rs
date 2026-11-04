//! A stand-in for a service and its OAuth server, in one small HTTP server on
//! loopback, behaving as Linear's was seen to: streamable HTTP that wants a
//! Bearer, sessions, SSE answers, protected-resource and authorization-server
//! metadata, dynamic registration of public clients, PKCE with S256, and
//! refresh tokens that rotate on every use, with a replay grace.
//!
//! Every secret it hands out holds [`SECRET_MARK`], so a test can search
//! anything for a leak.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, Response};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::oauth::{REDIRECT_URI, challenge};
use crate::registry::Service;
use crate::util::{form, parse_form, query_of};

/// What every secret this hands out holds.
pub const SECRET_MARK: &str = "FAKESECRET";

/// One request to its MCP endpoints, as it saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub path: String,
    pub method: String,
    pub tool: Option<String>,
    pub bearer: Option<String>,
    pub session: Option<String>,
    /// Whether it was let through.
    pub status: u16,
}

#[derive(Default)]
struct Inner {
    clients: HashSet<String>,
    /// Codes given out, with what they were for.
    codes: HashMap<String, (String, String)>,
    access: HashSet<String>,
    /// The refresh token that's current.
    refresh: HashSet<String>,
    /// Refresh tokens used, and what they gave: taken again in the grace.
    used_refresh: HashMap<String, (String, String)>,
    grace: bool,
    next: u32,
    sessions: HashSet<String>,
    tools: Vec<Value>,
    seen: Vec<Seen>,
    refreshes: usize,
    registrations: usize,
    revocations: Vec<String>,
    /// PKCE verifiers it was given, so a test can look for them elsewhere.
    verifiers: Vec<String>,
    /// Codes taken for a stand-in's fixed answers, with no page to get them.
    fixed_code: Option<String>,
    log: Vec<String>,
    refuse_refresh: Option<String>,
    /// Every token is turned down, new ones too, while refreshes still work:
    /// access revoked at the service.
    reject_all: bool,
    refresh_delay: Option<Duration>,
    /// The token endpoint answers 503, as in an outage.
    token_outage: bool,
    held_answers: HashMap<String, oneshot::Sender<Value>>,
    hold_streams: bool,
    /// Tokens from before now are turned down, and any a refresh makes, as when
    /// a login's revoked. One from a new sign-in is good.
    reject_old_logins: bool,
    /// The tokens a sign-in made, which that doesn't touch.
    signed_in: HashSet<String>,
    /// Its streams stay open after their answers, as a server's may.
    linger_streams: bool,
    lingering: Vec<mpsc::Sender<Bytes>>,
    /// What it tells clients at `initialize`.
    instructions: Option<String>,
    /// A refused refresh's description repeats the token it was given.
    echo_refresh: bool,
}

/// The stand-in, serving until it's dropped.
#[derive(Clone)]
pub struct FakeService {
    pub base: String,
    inner: Arc<Mutex<Inner>>,
    release: Arc<Notify>,
    task: Arc<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for FakeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeService")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Drop for FakeService {
    fn drop(&mut self) {
        if Arc::strong_count(&self.task) == 1 {
            self.task.abort();
        }
    }
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": {}},
    })
}

/// The tools it starts with: all but the first would write, or misbehave.
fn tools() -> Vec<Value> {
    vec![
        tool("list_issues", "Lists your issues."),
        tool("create_issue", "Makes an issue."),
        tool(
            "stream_issues",
            "Answers as an event stream, with progress.",
        ),
        tool("ask_user", "Asks the client something, then answers."),
        tool("boom", "Always fails, as a tool."),
        tool("no_answer", "Closes its stream without answering."),
        tool("accepts_only", "Says 202 to a request, and never answers."),
    ]
}

/// A body fed from a channel, so an event can go out while more is to come.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|c| Ok(Frame::data(c))))
    }
}

fn whole(status: u16, content_type: &str, body: String) -> Response<ChannelBody> {
    let (tx, rx) = mpsc::channel(1);
    let _ = tx.try_send(Bytes::from(body));
    Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .body(ChannelBody(rx))
        .expect("a response")
}

fn json_reply(status: u16, v: Value) -> Response<ChannelBody> {
    whole(status, "application/json", v.to_string())
}

fn oauth_error(code: &str) -> Response<ChannelBody> {
    json_reply(
        400,
        json!({"error": code, "error_description": "said the stand-in"}),
    )
}

fn event(v: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {v}\n\n"))
}

impl FakeService {
    /// Starts one on a port of its own.
    pub async fn start() -> FakeService {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let inner = Arc::new(Mutex::new(Inner {
            grace: true,
            tools: tools(),
            ..Inner::default()
        }));
        let release = Arc::new(Notify::new());
        let (shared, b, r) = (inner.clone(), base.clone(), release.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (inner, base, release) = (shared.clone(), b.clone(), r.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        let (inner, base, release) = (inner.clone(), base.clone(), release.clone());
                        async move { Ok::<_, Infallible>(route(req, inner, base, release).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        FakeService {
            base,
            inner,
            release,
            task: Arc::new(task),
        }
    }

    fn state(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Linear, here.
    pub fn service(&self) -> Service {
        Service::linear_at(&self.base)
    }

    /// Plays the owner signing in at `authorize_url`: the URL the app's web
    /// authentication session would end on.
    pub async fn sign_in(&self, authorize_url: &str) -> String {
        let http = crate::http::Http::new();
        let reply = http
            .send(
                Method::GET,
                authorize_url,
                &[],
                None,
                Duration::from_secs(20),
            )
            .await
            .expect("the authorization page answers");
        assert_eq!(reply.status, 302, "the authorization page redirects");
        reply
            .header("location")
            .expect("it says where to")
            .to_owned()
    }

    /// Everything its MCP endpoints have seen.
    pub fn seen(&self) -> Vec<Seen> {
        self.state().seen.clone()
    }

    /// The tool calls among them.
    pub fn calls(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == "tools/call")
            .collect()
    }

    pub fn refreshes(&self) -> usize {
        self.state().refreshes
    }

    pub fn registrations(&self) -> usize {
        self.state().registrations
    }

    pub fn revocations(&self) -> Vec<String> {
        self.state().revocations.clone()
    }

    /// The PKCE verifiers exchanges brought.
    pub fn verifiers(&self) -> Vec<String> {
        self.state().verifiers.clone()
    }

    /// Takes `code` from any client, with any verifier: for the contract's
    /// examples, which play no browser.
    pub fn accept_code(&self, code: &str) {
        self.state().fixed_code = Some(code.to_owned());
    }

    /// One on a thread and a runtime of its own, for a caller with neither,
    /// such as the contract's world. It serves for as long as the process runs.
    pub fn start_detached() -> FakeService {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async move {
                let fake = FakeService::start().await;
                let _ = tx.send(fake.clone());
                std::future::pending::<()>().await;
            });
        });
        rx.recv().expect("it started")
    }

    /// Adds to the running log of what happened, in order: this stand-in
    /// writes `refresh gave <access>` and `used <access>` there.
    pub fn log(&self, line: impl Into<String>) {
        self.state().log.push(line.into());
    }

    pub fn log_lines(&self) -> Vec<String> {
        self.state().log.clone()
    }

    /// Every access token runs out, so the next call gets a 401.
    pub fn expire_access(&self) {
        self.state().access.clear();
    }

    /// Refreshes are refused with this OAuth error from now on.
    pub fn refuse_refresh(&self, code: &str) {
        self.state().refuse_refresh = Some(code.to_owned());
    }

    /// As when the owner revokes access: every token, however new, gets a 401.
    pub fn reject_all_tokens(&self, reject: bool) {
        self.state().reject_all = reject;
    }

    /// It forgets registered clients, as Linear may collect old ones.
    pub fn forget_clients(&self) {
        self.state().clients.clear();
    }

    /// It forgets its sessions, so the next call with one gets a 404.
    pub fn forget_sessions(&self) {
        self.state().sessions.clear();
    }

    pub fn set_tools(&self, tools: Vec<Value>) {
        self.state().tools = tools;
    }

    /// Whether a used refresh token can be replayed in the grace period.
    pub fn set_replay_grace(&self, grace: bool) {
        self.state().grace = grace;
    }

    /// Each refresh takes this long.
    pub fn slow_refreshes(&self, delay: Duration) {
        self.state().refresh_delay = Some(delay);
    }

    /// Whether its token endpoint is out of order.
    pub fn token_outage(&self, out: bool) {
        self.state().token_outage = out;
    }

    /// Every token so far is turned down, and any a refresh makes, as when a
    /// login's revoked. One from signing in again is good.
    pub fn reject_old_logins(&self) {
        let mut state = self.state();
        state.reject_old_logins = true;
        state.signed_in.clear();
    }

    /// Its event streams, `initialize`'s too, stay open after their answers.
    pub fn linger_streams(&self) {
        self.state().linger_streams = true;
    }

    pub fn set_instructions(&self, text: &str) {
        self.state().instructions = Some(text.to_owned());
    }

    /// A refused refresh's description repeats the token, as a careless server's
    /// might.
    pub fn echo_refresh_in_errors(&self) {
        self.state().echo_refresh = true;
    }

    /// `stream_issues` waits after its first event, until [`Self::release`].
    pub fn hold_streams(&self) {
        self.state().hold_streams = true;
    }

    pub fn release(&self) {
        self.state().hold_streams = false;
        self.release.notify_waiters();
    }
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn route(
    req: Request<Incoming>,
    inner: Arc<Mutex<Inner>>,
    base: String,
    release: Arc<Notify>,
) -> Response<ChannelBody> {
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned).unwrap_or_default();
    let method = req.method().clone();
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let session = req
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = req
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes().to_vec())
        .unwrap_or_default();
    match (method, path.as_str()) {
        (Method::GET, "/.well-known/oauth-protected-resource/mcp") => json_reply(
            200,
            json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}),
        ),
        (Method::GET, "/.well-known/oauth-authorization-server") => json_reply(
            200,
            json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "revocation_endpoint": format!("{base}/revoke"),
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": true,
            }),
        ),
        (Method::POST, "/register") => register(&inner, &body),
        (Method::GET, "/authorize") => authorize(&inner, &base, &query),
        (Method::POST, "/token") => token(&inner, &base, &body).await,
        (Method::POST, "/revoke") => {
            let form = parse_form(&String::from_utf8_lossy(&body));
            if let Some((_, t)) = form.iter().find(|(k, _)| k == "token") {
                lock(&inner).revocations.push(t.clone());
            }
            whole(200, "text/plain", String::new())
        }
        (Method::POST, "/mcp" | "/mcp/readonly") => {
            mcp(&inner, &path, bearer, session, &body, release).await
        }
        _ => whole(404, "text/plain", "not here".into()),
    }
}

fn register(inner: &Mutex<Inner>, body: &[u8]) -> Response<ChannelBody> {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let wants_ours = v["redirect_uris"]
        .as_array()
        .is_some_and(|r| r.iter().any(|u| u == REDIRECT_URI));
    if !wants_ours || v["token_endpoint_auth_method"] != "none" {
        return oauth_error("invalid_client_metadata");
    }
    let mut state = lock(inner);
    state.next += 1;
    state.registrations += 1;
    let id = format!("fake-client-{}", state.next);
    state.clients.insert(id.clone());
    json_reply(201, json!({"client_id": id}))
}

fn authorize(inner: &Mutex<Inner>, base: &str, query: &str) -> Response<ChannelBody> {
    let q = query_of(&format!("?{query}"));
    let get = |name: &str| {
        q.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let mut state = lock(inner);
    let fine = state.clients.contains(&get("client_id"))
        && get("redirect_uri") == REDIRECT_URI
        && get("response_type") == "code"
        && get("code_challenge_method") == "S256"
        && !get("code_challenge").is_empty()
        && get("resource") == format!("{base}/mcp");
    if !fine {
        return whole(400, "text/plain", "a bad authorization request".into());
    }
    state.next += 1;
    let code = format!("fake-code-{}", state.next);
    state
        .codes
        .insert(code.clone(), (get("client_id"), get("code_challenge")));
    let location = format!(
        "{REDIRECT_URI}?{}",
        form(&[
            ("code", code.as_str()),
            ("state", get("state").as_str()),
            ("iss", base)
        ])
    );
    let (_tx, rx) = mpsc::channel(1);
    Response::builder()
        .status(302)
        .header("Location", location)
        .body(ChannelBody(rx))
        .expect("a redirect")
}

fn new_tokens(state: &mut Inner) -> (String, String) {
    state.next += 1;
    let access = format!("{SECRET_MARK}-access-{}", state.next);
    let refresh = format!("{SECRET_MARK}-refresh-{}", state.next);
    state.access.insert(access.clone());
    state.refresh.insert(refresh.clone());
    (access, refresh)
}

fn tokens_reply(access: &str, refresh: &str) -> Response<ChannelBody> {
    json_reply(
        200,
        json!({
            "access_token": access,
            "refresh_token": refresh,
            "token_type": "bearer",
            "expires_in": 86400,
            "scope": "read write",
        }),
    )
}

async fn token(inner: &Mutex<Inner>, base: &str, body: &[u8]) -> Response<ChannelBody> {
    let pairs = parse_form(&String::from_utf8_lossy(body));
    let get = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    if lock(inner).token_outage {
        return whole(503, "text/plain", "try later".into());
    }
    if get("resource") != format!("{base}/mcp") {
        return oauth_error("invalid_target");
    }
    match get("grant_type").as_str() {
        "authorization_code" => {
            let mut state = lock(inner);
            if state.fixed_code.as_deref() == Some(get("code").as_str()) {
                let (access, refresh) = new_tokens(&mut state);
                return tokens_reply(&access, &refresh);
            }
            if !state.clients.contains(&get("client_id")) {
                return oauth_error("invalid_client");
            }
            state.verifiers.push(get("code_verifier"));
            let Some((client, wanted)) = state.codes.remove(&get("code")) else {
                return oauth_error("invalid_grant");
            };
            if client != get("client_id")
                || challenge(&get("code_verifier")) != wanted
                || get("redirect_uri") != REDIRECT_URI
            {
                return oauth_error("invalid_grant");
            }
            let (access, refresh) = new_tokens(&mut state);
            state.signed_in.insert(access.clone());
            tokens_reply(&access, &refresh)
        }
        "refresh_token" => {
            let delay = lock(inner).refresh_delay;
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let mut state = lock(inner);
            state.refreshes += 1;
            if let Some(code) = state.refuse_refresh.clone() {
                if state.echo_refresh {
                    return json_reply(
                        400,
                        json!({"error": code, "error_description": format!("no to {}", get("refresh_token"))}),
                    );
                }
                return oauth_error(&code);
            }
            if !state.clients.contains(&get("client_id")) {
                return oauth_error("invalid_client");
            }
            let given = get("refresh_token");
            // Each sign-in is a grant of its own, with a refresh token that rotates.
            if state.refresh.remove(&given) {
                let (access, refresh) = new_tokens(&mut state);
                state
                    .used_refresh
                    .insert(given, (access.clone(), refresh.clone()));
                state.log.push(format!("refresh gave {access}"));
                return tokens_reply(&access, &refresh);
            }
            if state.grace
                && let Some((access, refresh)) = state.used_refresh.get(&given).cloned()
            {
                state.log.push(format!("replay gave {access}"));
                return tokens_reply(&access, &refresh);
            }
            oauth_error("invalid_grant")
        }
        _ => oauth_error("unsupported_grant_type"),
    }
}

fn unauthorized() -> Response<ChannelBody> {
    let (_tx, rx) = mpsc::channel(1);
    Response::builder()
        .status(401)
        .header("WWW-Authenticate", "Bearer realm=\"OAuth\"")
        .body(ChannelBody(rx))
        .expect("a 401")
}

async fn mcp(
    inner: &Mutex<Inner>,
    path: &str,
    bearer: Option<String>,
    session: Option<String>,
    body: &[u8],
    release: Arc<Notify>,
) -> Response<ChannelBody> {
    let message: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let method = message["method"].as_str().unwrap_or_default().to_owned();
    let tool_name = message
        .pointer("/params/name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let mut state = lock(inner);
    let authorized = !state.reject_all
        && bearer.as_deref().is_some_and(|b| {
            state.access.contains(b) && (!state.reject_old_logins || state.signed_in.contains(b))
        });
    let known_session = session.as_ref().is_some_and(|s| state.sessions.contains(s));
    let status = if !authorized {
        401
    } else if method != "initialize" && !known_session {
        404
    } else {
        200
    };
    state.seen.push(Seen {
        path: path.to_owned(),
        method: method.clone(),
        tool: tool_name.clone(),
        bearer: bearer.clone(),
        session: session.clone(),
        status,
    });
    match status {
        401 => return unauthorized(),
        404 => return whole(404, "text/plain", "no such session".into()),
        _ => {}
    }
    // Every use of a token, whatever for.
    if let Some(b) = &bearer {
        let line = format!("used {b}");
        state.log.push(line);
    }
    // A client's answer to something asked of it, in a held stream.
    if message.get("method").is_none() {
        if let Some(tx) = state.held_answers.remove(&id.to_string()) {
            let _ = tx.send(message);
        }
        return whole(202, "text/plain", String::new());
    }
    if id.is_null() {
        return whole(202, "text/plain", String::new());
    }
    let result = |v: Value| json!({"jsonrpc": "2.0", "id": id, "result": v});
    match method.as_str() {
        "initialize" => {
            state.next += 1;
            let new_session = format!("fake-session-{}", state.next);
            state.sessions.insert(new_session.clone());
            let mut answer = json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "fake-linear", "version": "1"},
            });
            if let Some(i) = &state.instructions {
                answer["instructions"] = json!(i);
            }
            let answer = result(answer);
            let (tx, rx) = mpsc::channel(1);
            let _ = tx.try_send(event(&answer));
            if state.linger_streams {
                state.lingering.push(tx);
            }
            Response::builder()
                .status(200)
                .header("Content-Type", "text/event-stream")
                .header("Mcp-Session-Id", new_session)
                .body(ChannelBody(rx))
                .expect("a stream")
        }
        "tools/list" => {
            // A page at a time, to prove they're all read.
            let start: usize = message
                .pointer("/params/cursor")
                .and_then(Value::as_str)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let all: Vec<Value> = if path == "/mcp/readonly" {
                state
                    .tools
                    .iter()
                    .filter(|t| t["name"] == "list_issues")
                    .cloned()
                    .collect()
            } else {
                state.tools.clone()
            };
            let page: Vec<&Value> = all.iter().skip(start).take(2).collect();
            let mut v = json!({"tools": page});
            if start + 2 < all.len() {
                v["nextCursor"] = json!((start + 2).to_string());
            }
            json_reply(200, result(v))
        }
        "tools/call" => {
            let text = |t: String| json!({"content": [{"type": "text", "text": t}]});
            match tool_name.as_deref().unwrap_or_default() {
                "list_issues" => json_reply(200, result(text(format!("issues on {path}")))),
                "create_issue" if path == "/mcp/readonly" => json_reply(
                    200,
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "no such tool here"}}),
                ),
                "create_issue" => json_reply(200, result(text("made one".into()))),
                "boom" => json_reply(
                    200,
                    result(
                        json!({"content": [{"type": "text", "text": "it broke"}], "isError": true}),
                    ),
                ),
                "stream_issues" => {
                    let hold = state.hold_streams;
                    let linger = state.linger_streams;
                    let done = result(text("streamed".into()));
                    let progress = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {"progressToken": id, "progress": 1},
                    });
                    drop(state);
                    let (tx, rx) = mpsc::channel(4);
                    tokio::spawn(async move {
                        let _ = tx.send(Bytes::from_static(b": keep-alive\n\n")).await;
                        let _ = tx.send(event(&progress)).await;
                        if hold {
                            release.notified().await;
                        }
                        let _ = tx.send(event(&done)).await;
                        if linger {
                            std::future::pending::<()>().await;
                        }
                    });
                    Response::builder()
                        .status(200)
                        .header("Content-Type", "text/event-stream")
                        .body(ChannelBody(rx))
                        .expect("a stream")
                }
                "ask_user" => {
                    let ask_id = json!(format!("ask-{}", state.next));
                    let (answered, answer) = oneshot::channel();
                    state.held_answers.insert(ask_id.to_string(), answered);
                    drop(state);
                    let ask = json!({
                        "jsonrpc": "2.0",
                        "id": ask_id,
                        "method": "elicitation/create",
                        "params": {"message": "which one?"},
                    });
                    let call_id = id.clone();
                    let (tx, rx) = mpsc::channel(4);
                    tokio::spawn(async move {
                        let _ = tx.send(event(&ask)).await;
                        let said = answer.await.unwrap_or(Value::Null);
                        let done = json!({"jsonrpc": "2.0", "id": call_id, "result": {
                            "content": [{"type": "text", "text": format!("told {}", said["result"])}],
                        }});
                        let _ = tx.send(event(&done)).await;
                    });
                    Response::builder()
                        .status(200)
                        .header("Content-Type", "text/event-stream")
                        .body(ChannelBody(rx))
                        .expect("a stream")
                }
                "no_answer" => whole(200, "text/event-stream", ": nothing\n\n".into()),
                "accepts_only" => whole(202, "text/plain", String::new()),
                other => json_reply(
                    200,
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("no tool {other}")}}),
                ),
            }
        }
        other => json_reply(
            200,
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no method {other}")}}),
        ),
    }
}
