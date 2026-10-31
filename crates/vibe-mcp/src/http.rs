//! MCP over Streamable HTTP, at `/mcp` (PRD: MCP interface, Transport).
//!
//! Every request needs an API key as `Authorization: Bearer <key>`; there's
//! no unauthenticated mode, not even on loopback (PRD: Auth is always on).
//! Behind that gate, `rmcp`'s Streamable HTTP service answers statelessly:
//! each call stands alone, and the ref it names is the only handle. A
//! result that comes with progress (`vibe_wait`'s) is sent as an event
//! stream, everything else as plain JSON.
//!
//! On loopback, the `Host` header must be a loopback name, so a web page
//! can't reach the server through DNS rebinding; and a request from a
//! browser, which carries an `Origin`, is refused everywhere.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_service::Service as _;

use crate::tools::Caller;
use crate::{Daemon, PATH, Server, net};

type Body = BoxBody<Bytes, Infallible>;

/// How long an idle event stream goes between keep-alives.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The largest request body: a prompt can be long, but not this long.
const MAX_BODY: usize = 4 << 20;

/// The `rmcp` service behind the gate. `check_host` holds the `Host`
/// header to loopback names.
fn service<D: Daemon>(
    daemon: Arc<D>,
    check_host: bool,
) -> StreamableHttpService<Server<D>, NeverSessionManager> {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(Some(KEEP_ALIVE))
        .with_max_request_body_bytes(MAX_BODY)
        .enforce_origin_validation();
    if !check_host {
        config = config.disable_allowed_hosts();
    }
    StreamableHttpService::new(
        move || Ok(Server::new(Arc::clone(&daemon))),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

fn plain(status: StatusCode, text: &str) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(format!("{text}\n"))).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

/// The key a request carries, as `Authorization: Bearer <key>`.
fn bearer(req: &Request<Incoming>) -> Option<&str> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, key) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| key.trim())
}

async fn handle<D: Daemon>(
    mut req: Request<Incoming>,
    svc: StreamableHttpService<Server<D>, NeverSessionManager>,
    daemon: Arc<D>,
) -> Response<Body> {
    if req.uri().path() != PATH {
        return plain(StatusCode::NOT_FOUND, "VibeOS serves MCP at /mcp");
    }
    let caller = match bearer(&req) {
        Some(key) => {
            let key = key.to_owned();
            let d = Arc::clone(&daemon);
            tokio::task::spawn_blocking(move || d.verify_key(&key))
                .await
                .ok()
                .flatten()
        }
        None => None,
    };
    let Some(caller) = caller else {
        let mut r = plain(
            StatusCode::UNAUTHORIZED,
            "an API key is needed: send it as Authorization: Bearer vk_… (Settings → API keys, or vibe key create)",
        );
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer realm=\"vibeos\""),
        );
        return r;
    };
    req.extensions_mut().insert(Caller(caller));
    let mut svc = svc;
    match svc.call(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    }
}

/// Serves MCP on `listener` until the task is aborted. Connections already
/// open finish on their own.
pub async fn serve<D: Daemon>(listener: TcpListener, daemon: Arc<D>, check_host: bool) {
    let svc = service(Arc::clone(&daemon), check_host);
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                daemon.log(&format!("mcp: can't accept a connection: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (svc, daemon) = (svc.clone(), Arc::clone(&daemon));
        tokio::spawn(async move {
            let conn = hyper::service::service_fn(move |req| {
                let (svc, daemon) = (svc.clone(), Arc::clone(&daemon));
                async move { Ok::<_, Infallible>(handle(req, svc, daemon).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), conn)
                .await;
        });
    }
}

/// What to listen on: the port, and whether the Tailscale address too.
/// Asked again on every check, as `host.toml` can change at any time.
pub type Settings = dyn Fn() -> (u16, bool) + Send + Sync;

/// Keeps MCP served where the settings say, checking every `every`: on
/// loopback always, and on the Tailscale address while that's on and the
/// machine has one. A listener whose address is no longer wanted stops; one
/// that can't bind is tried again on the next check.
pub fn spawn_listeners<D: Daemon>(
    daemon: Arc<D>,
    settings: Arc<Settings>,
    every: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut running: HashMap<SocketAddr, JoinHandle<()>> = HashMap::new();
        let mut failing: HashMap<SocketAddr, String> = HashMap::new();
        let stopper = Stopper(&mut running);
        loop {
            let (port, tailscale) = settings();
            let wanted = net::listen_addresses(port, tailscale);
            stopper.0.retain(|addr, task| {
                let keep = wanted.contains(addr) && !task.is_finished();
                if !keep {
                    task.abort();
                    daemon.log(&format!("mcp: stopped serving http://{addr}{PATH}"));
                }
                keep
            });
            failing.retain(|addr, _| wanted.contains(addr));
            for addr in wanted {
                if stopper.0.contains_key(&addr) {
                    continue;
                }
                match TcpListener::bind(addr).await {
                    Ok(listener) => {
                        failing.remove(&addr);
                        daemon.log(&format!("mcp: serving http://{addr}{PATH}"));
                        let task = tokio::spawn(serve(
                            listener,
                            Arc::clone(&daemon),
                            addr.ip().is_loopback(),
                        ));
                        stopper.0.insert(addr, task);
                    }
                    Err(e) => {
                        let why = e.to_string();
                        if failing.get(&addr) != Some(&why) {
                            daemon.log(&format!("mcp: can't serve on {addr}: {why}; trying again"));
                            failing.insert(addr, why);
                        }
                    }
                }
            }
            tokio::time::sleep(every).await;
        }
    })
}

/// Stops every listener when the supervisor ends, aborted or not.
struct Stopper<'a>(&'a mut HashMap<SocketAddr, JoinHandle<()>>);

impl Drop for Stopper<'_> {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}
