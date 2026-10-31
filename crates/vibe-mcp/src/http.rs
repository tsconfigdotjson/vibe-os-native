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
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_service::Service as _;

use crate::tools::Caller;
use crate::{Daemon, PATH, Server};

type Body = BoxBody<Bytes, Infallible>;

/// How long an idle event stream goes between keep-alives.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// How long a request's body may take to arrive, once its headers have.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The most connections open at once on one listener.
const MAX_CONNECTIONS: usize = 256;

/// How long a client has to send a request's headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

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
fn bearer<B>(req: &Request<B>) -> Option<&str> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, key) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| key.trim())
}

/// A request as `rmcp` gets it: read whole first.
type FullRequest = Request<Full<Bytes>>;

/// Reads a request's body whole, up to [`MAX_BODY`] and within
/// [`READ_TIMEOUT`], before anything answers it. A socket closed with a
/// request still unread in it is reset, not closed, and the reset can lose
/// the answer before the client reads it; macOS does so even for a small
/// body that came in a write of its own. Every refusal, the key gate's
/// and `rmcp`'s own Host and Origin checks, comes after this.
async fn read_whole(req: Request<Incoming>) -> Result<FullRequest, Box<Response<Body>>> {
    let (parts, mut body) = req.into_parts();
    let read = async {
        let mut kept = bytes::BytesMut::new();
        let mut too_large = false;
        while let Some(frame) = body.frame().await {
            let Ok(data) = frame.map(|f| f.into_data()) else {
                return Err(Box::new(plain(
                    StatusCode::BAD_REQUEST,
                    "the request ended early",
                )));
            };
            let Ok(data) = data else { continue };
            // Past the limit it's still read, and dropped, so the answer
            // isn't lost to a reset.
            too_large |= kept.len() + data.len() > MAX_BODY;
            if !too_large {
                kept.extend_from_slice(&data);
            }
        }
        if too_large {
            return Err(Box::new(plain(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the request is too large",
            )));
        }
        Ok(Request::from_parts(parts, Full::new(kept.freeze())))
    };
    tokio::time::timeout(READ_TIMEOUT, read)
        .await
        .unwrap_or_else(|_| {
            Err(Box::new(plain(
                StatusCode::REQUEST_TIMEOUT,
                "the request took too long to arrive",
            )))
        })
}

async fn handle<D: Daemon>(
    req: Request<Incoming>,
    svc: StreamableHttpService<Server<D>, NeverSessionManager>,
    daemon: Arc<D>,
) -> Response<Body> {
    let mut req = match read_whole(req).await {
        Ok(r) => r,
        Err(refused) => return *refused,
    };
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
    // Open connections are capped, so peers that connect and say nothing
    // can't take every descriptor `vibed` has.
    let room = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut last_error: Option<String> = None;
    loop {
        let Ok(permit) = Arc::clone(&room).acquire_owned().await else {
            return;
        };
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                // Said once until it changes, as it repeats while it lasts
                // (out of descriptors, say).
                let why = e.to_string();
                if last_error.as_ref() != Some(&why) {
                    daemon.log(&format!("mcp: can't accept a connection: {why}"));
                    last_error = Some(why);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        last_error = None;
        let (svc, daemon) = (svc.clone(), Arc::clone(&daemon));
        tokio::spawn(async move {
            let conn = hyper::service::service_fn(move |req| {
                let (svc, daemon) = (svc.clone(), Arc::clone(&daemon));
                async move { Ok::<_, Infallible>(handle(req, svc, daemon).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(TokioIo::new(stream), conn)
                .await;
            drop(permit);
        });
    }
}

/// The addresses to listen on, such as [`crate::net::listen_on`] gives. Asked
/// again on every check, as `host.toml` can change at any time.
pub type Settings = dyn Fn() -> Vec<SocketAddr> + Send + Sync;

/// Keeps MCP served where the settings say, checking every `every`: on
/// loopback always, and on the Tailscale address while that's on and the
/// machine has one. A listener whose address is no longer wanted stops; one
/// that can't bind is tried again on the next check. `kick` has it check
/// at once, as after a change from Settings. After each check it tells the
/// daemon whether MCP is served on loopback ([`Daemon::serving`]), and that
/// it isn't once the supervisor ends.
pub fn spawn_listeners<D: Daemon>(
    daemon: Arc<D>,
    settings: Arc<Settings>,
    every: Duration,
    kick: Arc<tokio::sync::Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut running: HashMap<SocketAddr, JoinHandle<()>> = HashMap::new();
        let mut failing: HashMap<SocketAddr, String> = HashMap::new();
        let stopper = Stopper(&mut running, &*daemon);
        loop {
            let wanted = settings();
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
            daemon.serving(stopper.0.keys().any(|a| a.ip().is_loopback()));
            tokio::select! {
                _ = tokio::time::sleep(every) => {}
                _ = kick.notified() => {}
            }
        }
    })
}

/// Stops every listener when the supervisor ends, aborted or not, and
/// tells the daemon MCP is no longer served.
struct Stopper<'a, D: Daemon>(&'a mut HashMap<SocketAddr, JoinHandle<()>>, &'a D);

impl<D: Daemon> Drop for Stopper<'_, D> {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
        self.1.serving(false);
    }
}
