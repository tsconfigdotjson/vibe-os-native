//! What OAuth needs from a service: discovery from its MCP endpoint, dynamic
//! client registration (a public client, with PKCE), and the token endpoint.

use std::time::Duration;

use http::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http::{Http, check_url};
use crate::util::{base64url, form};

/// Where a web authentication session ends.
pub const REDIRECT_URI: &str = "vibeos://oauth/callback";

/// The scheme of [`REDIRECT_URI`], which the app's session waits for.
pub const CALLBACK_SCHEME: &str = "vibeos";

/// How long an OAuth server gets to answer.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The most an OAuth server may say.
const MAX_BODY: usize = 1 << 20;

/// Where a service's authorization server does what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    /// It adds `iss` to its redirects, so one without it isn't its.
    pub iss_in_redirect: bool,
}

async fn get_json(http: &Http, url: &str) -> Result<Value, String> {
    let reply = http
        .send(
            Method::GET,
            url,
            &[("Accept", "application/json")],
            None,
            TIMEOUT,
        )
        .await
        .map_err(|e| format!("can't reach {url}: {e}"))?;
    let status = reply.status;
    if status != 200 {
        return Err(format!("{url} answered {status}"));
    }
    let body = reply.read(MAX_BODY, TIMEOUT).await?;
    serde_json::from_slice(&body).map_err(|_| format!("{url} didn't answer with JSON"))
}

/// `https://host/path` as (`https://host`, `/path`).
fn split_url(url: &str) -> Result<(String, String), String> {
    let uri = check_url(url)?;
    let scheme = uri.scheme_str().unwrap_or("https");
    let authority = uri.authority().map(|a| a.as_str()).unwrap_or_default();
    let path = uri.path().trim_end_matches('/');
    Ok((format!("{scheme}://{authority}"), path.to_owned()))
}

/// Finds a service's authorization server from its MCP endpoint (RFC 9728,
/// then RFC 8414), and holds it to what it says of itself.
pub(crate) async fn discover(http: &Http, endpoint: &str) -> Result<Metadata, String> {
    let (origin, path) = split_url(endpoint)?;
    let resource = get_json(
        http,
        &format!("{origin}/.well-known/oauth-protected-resource{path}"),
    )
    .await?;
    let server = resource["authorization_servers"][0]
        .as_str()
        .ok_or("its resource metadata names no authorization server")?
        .trim_end_matches('/')
        .to_owned();
    let (as_origin, as_path) = split_url(&server)?;
    let md = get_json(
        http,
        &format!("{as_origin}/.well-known/oauth-authorization-server{as_path}"),
    )
    .await?;
    let issuer = md["issuer"]
        .as_str()
        .ok_or("its authorization server names no issuer")?;
    if issuer.trim_end_matches('/') != server {
        return Err(format!(
            "its authorization server calls itself {issuer}, not {server}"
        ));
    }
    if let Some(methods) = md["code_challenge_methods_supported"].as_array()
        && !methods.iter().any(|m| m == "S256")
    {
        return Err("its authorization server doesn't take PKCE with S256".into());
    }
    let endpoint_of = |name: &str| -> Result<Option<String>, String> {
        match md[name].as_str() {
            Some(url) => {
                check_url(url)?;
                Ok(Some(url.to_owned()))
            }
            None => Ok(None),
        }
    };
    Ok(Metadata {
        issuer: issuer.to_owned(),
        authorization_endpoint: endpoint_of("authorization_endpoint")?
            .ok_or("its authorization server has no authorization endpoint")?,
        token_endpoint: endpoint_of("token_endpoint")?
            .ok_or("its authorization server has no token endpoint")?,
        registration_endpoint: endpoint_of("registration_endpoint")?,
        revocation_endpoint: endpoint_of("revocation_endpoint")?,
        iss_in_redirect: md["authorization_response_iss_parameter_supported"] == true,
    })
}

/// Registers a public client for VibeOS, and gives its id.
pub(crate) async fn register(http: &Http, md: &Metadata) -> Result<String, String> {
    let url = md
        .registration_endpoint
        .as_deref()
        .ok_or("its authorization server doesn't register clients")?;
    let body = json!({
        "client_name": "VibeOS",
        "redirect_uris": [REDIRECT_URI],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let reply = http
        .send(
            Method::POST,
            url,
            &[("Accept", "application/json")],
            Some(("application/json", body.to_string().into_bytes())),
            TIMEOUT,
        )
        .await
        .map_err(|e| format!("can't reach {url}: {e}"))?;
    let status = reply.status;
    let text = reply.read(MAX_BODY, TIMEOUT).await?;
    if !(200..300).contains(&status) {
        return Err(format!("registering a client was refused ({status})"));
    }
    let v: Value = serde_json::from_slice(&text).map_err(|_| "a registration that isn't JSON")?;
    v["client_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "a registration without a client id".to_owned())
}

/// PKCE's challenge for a verifier.
pub fn challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// What's asked of the authorization endpoint.
#[derive(Debug, Clone, Copy)]
pub struct Authorize<'a> {
    pub md: &'a Metadata,
    pub client_id: &'a str,
    pub verifier: &'a str,
    pub state: &'a str,
    pub resource: &'a str,
    pub scopes: &'a [String],
}

/// The page a web authentication session opens.
pub fn authorize_url(a: Authorize<'_>) -> String {
    let scope = a.scopes.join(" ");
    let challenge = challenge(a.verifier);
    let query = form(&[
        ("response_type", "code"),
        ("client_id", a.client_id),
        ("redirect_uri", REDIRECT_URI),
        ("scope", &scope),
        ("state", a.state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", a.resource),
    ]);
    let joiner = if a.md.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{joiner}{query}", a.md.authorization_endpoint)
}

/// What a token endpoint gave.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Why a token endpoint gave nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// It said no: an OAuth error code such as `invalid_grant`, and what it
    /// said of it. Asking again won't help.
    Refused { code: String, detail: String },
    /// It couldn't be asked, or it failed: asking again later may help.
    Unavailable(String),
}

/// Asks a token endpoint.
pub(crate) async fn token(
    http: &Http,
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<TokenSet, TokenError> {
    let reply = http
        .send(
            Method::POST,
            endpoint,
            &[("Accept", "application/json")],
            Some((
                "application/x-www-form-urlencoded",
                form(params).into_bytes(),
            )),
            TIMEOUT,
        )
        .await
        .map_err(TokenError::Unavailable)?;
    let status = reply.status;
    let text = reply
        .read(MAX_BODY, TIMEOUT)
        .await
        .map_err(TokenError::Unavailable)?;
    let v: Value = serde_json::from_slice(&text).unwrap_or(Value::Null);
    if let (400 | 401, Some(code)) = (status, v["error"].as_str()) {
        return Err(TokenError::Refused {
            code: code.to_owned(),
            detail: v["error_description"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
        });
    }
    if status != 200 {
        return Err(TokenError::Unavailable(format!("it answered {status}")));
    }
    let access = v["access_token"]
        .as_str()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| TokenError::Unavailable("an answer without a token".into()))?;
    Ok(TokenSet {
        access: access.to_owned(),
        refresh: v["refresh_token"].as_str().map(str::to_owned),
        expires_in: v["expires_in"].as_i64(),
        scope: v["scope"].as_str().map(str::to_owned),
    })
}

/// Tells a service a token's no longer wanted. Only ever a courtesy: what
/// counts is that it's gone from here.
pub(crate) async fn revoke(http: &Http, endpoint: &str, token: &str, client_id: Option<&str>) {
    let mut params = vec![("token", token)];
    if let Some(id) = client_id {
        params.push(("client_id", id));
    }
    let _ = http
        .send(
            Method::POST,
            endpoint,
            &[],
            Some((
                "application/x-www-form-urlencoded",
                form(&params).into_bytes(),
            )),
            Duration::from_secs(10),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc_7636() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn urls_split_for_well_known_paths() {
        assert_eq!(
            split_url("https://mcp.example.com/mcp/").unwrap(),
            ("https://mcp.example.com".to_owned(), "/mcp".to_owned())
        );
        assert_eq!(
            split_url("http://127.0.0.1:9/").unwrap(),
            ("http://127.0.0.1:9".to_owned(), String::new())
        );
    }

    #[test]
    fn an_authorize_url_says_everything_a_public_client_must() {
        let md = Metadata {
            issuer: "https://a.example".into(),
            authorization_endpoint: "https://a.example/authorize".into(),
            token_endpoint: "https://a.example/token".into(),
            registration_endpoint: None,
            revocation_endpoint: None,
            iss_in_redirect: true,
        };
        let url = authorize_url(Authorize {
            md: &md,
            client_id: "client 1",
            verifier: "v",
            state: "st",
            resource: "https://mcp.example/mcp",
            scopes: &["read".into(), "write".into()],
        });
        let query = crate::util::query_of(&url);
        let get = |k: &str| {
            query
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert!(url.starts_with("https://a.example/authorize?"));
        assert_eq!(get("response_type"), "code");
        assert_eq!(get("client_id"), "client 1");
        assert_eq!(get("redirect_uri"), REDIRECT_URI);
        assert_eq!(get("scope"), "read write");
        assert_eq!(get("state"), "st");
        assert_eq!(get("code_challenge"), challenge("v"));
        assert_eq!(get("code_challenge_method"), "S256");
        assert_eq!(get("resource"), "https://mcp.example/mcp");
    }
}
