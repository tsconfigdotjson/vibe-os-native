//! A small HTTP client for services and their OAuth servers. It never follows
//! a redirect, and speaks plain HTTP only to loopback, where tests' stand-ins
//! are: a token never crosses the network in the clear.

use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) type Connector = HttpsConnector<HttpConnector>;

/// The client.
#[derive(Debug, Clone)]
pub(crate) struct Http {
    client: Client<Connector, Full<Bytes>>,
}

/// An answer whose body hasn't been read.
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Incoming,
}

impl Reply {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The next piece of the body; `None` at its end.
    pub(crate) async fn next_chunk(&mut self, within: Duration) -> Result<Option<Bytes>, String> {
        loop {
            let frame = tokio::time::timeout(within, self.body.frame())
                .await
                .map_err(|_| format!("nothing came for {}s", within.as_secs()))?;
            match frame {
                None => return Ok(None),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        return Ok(Some(data));
                    }
                }
            }
        }
    }

    /// The whole body, up to `max` bytes.
    pub(crate) async fn read(mut self, max: usize, within: Duration) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk(within).await? {
            out.extend_from_slice(&chunk);
            if out.len() > max {
                return Err(format!("an answer longer than {max} bytes"));
            }
        }
        Ok(out)
    }
}

/// Whether a URL may be spoken to: HTTPS, or plain HTTP to this machine.
pub(crate) fn check_url(url: &str) -> Result<Uri, String> {
    let uri: Uri = url.parse().map_err(|_| format!("`{url}` isn't a URL"))?;
    let host = uri.host().unwrap_or_default();
    match uri.scheme_str() {
        Some("https") if !host.is_empty() => Ok(uri),
        Some("http") if matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") => Ok(uri),
        _ => Err(format!("`{url}` isn't an HTTPS URL")),
    }
}

impl Http {
    /// A client that trusts this machine's roots.
    pub(crate) fn new() -> Http {
        let mut roots = rustls::RootCertStore::empty();
        // Whatever can't be read just isn't trusted: an HTTPS call then fails
        // and says so, and loopback needs none.
        let _ = roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        let provider = rustls::crypto::ring::default_provider();
        let tls = rustls::ClientConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Http {
            client: Client::builder(TokioExecutor::new()).build(connector),
        }
    }

    /// Sends a request, and waits up to `within` for its answer's head.
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<(&str, Vec<u8>)>,
        within: Duration,
    ) -> Result<Reply, String> {
        let uri = check_url(url)?;
        let mut request = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let body = match body {
            Some((content_type, bytes)) => {
                request = request.header("Content-Type", content_type);
                Full::new(Bytes::from(bytes))
            }
            None => Full::new(Bytes::new()),
        };
        let request = request
            .body(body)
            .map_err(|_| "a request that can't be sent".to_owned())?;
        let reply = tokio::time::timeout(within, self.client.request(request))
            .await
            .map_err(|_| format!("no answer in {}s", within.as_secs()))?
            .map_err(|e| describe(&e))?;
        let (parts, body) = reply.into_parts();
        Ok(Reply {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body,
        })
    }
}

/// Says why a request failed, with the cause: hyper's own error is only
/// "client error (Connect)".
fn describe(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text = format!("{text}: {s}");
        source = s.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_only_cross_the_network_encrypted() {
        for ok in [
            "https://mcp.example.com/mcp",
            "http://127.0.0.1:8080/mcp",
            "http://localhost:1/x",
            "http://[::1]:1/x",
        ] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://mcp.example.com/mcp",
            "http://127.0.0.1.example.com/",
            "ftp://127.0.0.1/",
            "vibeos://oauth/callback",
            "https:///nohost",
            "not a url",
            "",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }
}
