//! The JSON-RPC 2.0 contract for the `vibed` socket.
//!
//! The CLI, the menu bar app and MCP all speak this one protocol to the same
//! core handlers. `protocol/vibed-rpc.json` and `protocol/examples/` are the
//! shared source of truth: contract tests on the Rust side (here and in
//! `vibed`) and on the Swift side (`swift/VibeOS`) check against them, so the
//! two can't drift.
//!
//! Framing is one JSON value per line (UTF-8, `\n`-terminated), in both
//! directions, on the Unix socket and through `vibe rpc` over SSH alike.

use std::fmt;

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Bumped on any incompatible change to the contract. Clients send theirs in
/// `daemon.hello`, and the daemon refuses a mismatch.
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest message, newline excluded, either side accepts.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// The `"jsonrpc": "2.0"` member, which must be exactly that string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct V2;

impl Serialize for V2 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for V2 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "2.0" {
            Ok(V2)
        } else {
            Err(D::Error::invalid_value(
                serde::de::Unexpected::Str(&s),
                &"\"2.0\"",
            ))
        }
    }
}

/// A request id. `Null` is only sent by the daemon, when it can't read the id
/// of a request it's rejecting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
    Null,
}

/// Deserializes a member that is present, so `null` becomes `Some(null)`
/// rather than `None`, which stays reserved for an absent member.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// A request, or a notification when `id` is absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: V2,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub id: Option<Id>,
    pub method: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub params: Option<Value>,
}

impl Request {
    /// A call to `M` that expects a response.
    pub fn call<M: Method>(id: Id, params: &M::Params) -> Self {
        Request {
            jsonrpc: V2,
            id: Some(id),
            method: M::NAME.to_owned(),
            params: Some(serde_json::to_value(params).expect("params serialize to JSON")),
        }
    }
}

/// A response: exactly one of `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawResponse", into = "RawResponse")]
pub struct Response {
    pub jsonrpc: V2,
    pub id: Id,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Result(Value),
    Error(ErrorObject),
}

#[derive(Clone, Serialize, Deserialize)]
struct RawResponse {
    jsonrpc: V2,
    id: Id,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ErrorObject>,
}

impl TryFrom<RawResponse> for Response {
    type Error = &'static str;

    fn try_from(raw: RawResponse) -> Result<Self, Self::Error> {
        let outcome = match (raw.result, raw.error) {
            (Some(result), None) => Outcome::Result(result),
            (None, Some(error)) => Outcome::Error(error),
            _ => return Err("a response has exactly one of `result` and `error`"),
        };
        Ok(Response {
            jsonrpc: raw.jsonrpc,
            id: raw.id,
            outcome,
        })
    }
}

impl From<Response> for RawResponse {
    fn from(r: Response) -> Self {
        let (result, error) = match r.outcome {
            Outcome::Result(v) => (Some(v), None),
            Outcome::Error(e) => (None, Some(e)),
        };
        RawResponse {
            jsonrpc: r.jsonrpc,
            id: r.id,
            result,
            error,
        }
    }
}

impl Response {
    pub fn result(id: Id, result: Value) -> Self {
        Response {
            jsonrpc: V2,
            id,
            outcome: Outcome::Result(result),
        }
    }

    pub fn error(id: Id, error: ErrorObject) -> Self {
        Response {
            jsonrpc: V2,
            id,
            outcome: Outcome::Error(error),
        }
    }
}

/// The error codes in the contract: the JSON-RPC 2.0 standard ones, then
/// ours, which count down from -32000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    /// The client's `protocol` in `daemon.hello` isn't the daemon's. The
    /// error's data is a [`ProtocolMismatchData`].
    ProtocolMismatch,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 6] = [
        ErrorCode::ParseError,
        ErrorCode::InvalidRequest,
        ErrorCode::MethodNotFound,
        ErrorCode::InvalidParams,
        ErrorCode::InternalError,
        ErrorCode::ProtocolMismatch,
    ];

    pub fn code(self) -> i64 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            ErrorCode::ProtocolMismatch => -32000,
        }
    }

    /// The name used for this code in `protocol/vibed-rpc.json`.
    pub fn name(self) -> &'static str {
        match self {
            ErrorCode::ParseError => "parse_error",
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::MethodNotFound => "method_not_found",
            ErrorCode::InvalidParams => "invalid_params",
            ErrorCode::InternalError => "internal_error",
            ErrorCode::ProtocolMismatch => "protocol_mismatch",
        }
    }

    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ErrorObject {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: impl Serialize) -> Self {
        self.data = Some(serde_json::to_value(data).expect("error data serializes to JSON"));
        self
    }
}

impl fmt::Display for ErrorObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ErrorObject {}

/// A method in the contract, with its parameter and result types.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned;
    type Result: Serialize + DeserializeOwned;
}

/// Every method in the contract.
pub const METHODS: &[&str] = &[Hello::NAME];

/// `daemon.hello`: the handshake. It names the client and checks that both
/// sides speak the same protocol version.
#[derive(Debug)]
pub enum Hello {}

impl Method for Hello {
    const NAME: &'static str = "daemon.hello";
    type Params = HelloParams;
    type Result = HelloResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    /// Who is calling, for the daemon's log, e.g. `vibe-cli` or `VibeOS.app`.
    pub client: String,
    /// The client's [`PROTOCOL_VERSION`].
    pub protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    /// Always `vibed`.
    pub daemon: String,
    /// The daemon's version.
    pub version: String,
    /// The daemon's [`PROTOCOL_VERSION`].
    pub protocol: u32,
    /// `std::env::consts::OS`: `macos` or `linux`.
    pub os: String,
    /// `std::env::consts::ARCH`: `aarch64` or `x86_64`.
    pub arch: String,
    pub pid: u32,
}

/// The data of a [`ErrorCode::ProtocolMismatch`] error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolMismatchData {
    /// The daemon's [`PROTOCOL_VERSION`].
    pub protocol: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_marker_must_be_2_0() {
        assert!(serde_json::from_value::<V2>(json!("2.0")).is_ok());
        assert!(serde_json::from_value::<V2>(json!("1.0")).is_err());
        assert!(serde_json::from_value::<V2>(json!(2.0)).is_err());
        assert_eq!(serde_json::to_value(V2).unwrap(), json!("2.0"));
    }

    #[test]
    fn absent_id_is_a_notification_but_null_id_is_not() {
        let note: Request =
            serde_json::from_value(json!({"jsonrpc": "2.0", "method": "m"})).unwrap();
        assert_eq!(note.id, None);
        assert_eq!(note.params, None);
        let null: Request =
            serde_json::from_value(json!({"jsonrpc": "2.0", "id": null, "method": "m"})).unwrap();
        assert_eq!(null.id, Some(Id::Null));
        assert_eq!(
            serde_json::to_value(&note).unwrap(),
            json!({"jsonrpc": "2.0", "method": "m"})
        );
        assert_eq!(
            serde_json::to_value(&null).unwrap(),
            json!({"jsonrpc": "2.0", "id": null, "method": "m"})
        );
    }

    #[test]
    fn null_params_are_kept_so_they_can_be_rejected() {
        let r: Request = serde_json::from_value(
            json!({"jsonrpc": "2.0", "id": 1, "method": "m", "params": null}),
        )
        .unwrap();
        assert_eq!(r.params, Some(Value::Null));
    }

    #[test]
    fn ids() {
        for (v, id) in [
            (json!(7), Id::Number(7)),
            (json!(-1), Id::Number(-1)),
            (json!("abc"), Id::String("abc".into())),
            (json!(null), Id::Null),
        ] {
            assert_eq!(serde_json::from_value::<Id>(v.clone()).unwrap(), id);
            assert_eq!(serde_json::to_value(&id).unwrap(), v);
        }
        assert!(serde_json::from_value::<Id>(json!(1.5)).is_err());
        assert!(serde_json::from_value::<Id>(json!([1])).is_err());
    }

    #[test]
    fn request_rejects_unknown_members() {
        let r = serde_json::from_value::<Request>(
            json!({"jsonrpc": "2.0", "id": 1, "method": "m", "param": {}}),
        );
        assert!(r.is_err());
    }

    #[test]
    fn response_has_exactly_one_outcome() {
        let ok: Response =
            serde_json::from_value(json!({"jsonrpc": "2.0", "id": 1, "result": null})).unwrap();
        assert_eq!(ok.outcome, Outcome::Result(Value::Null));
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "result": null})
        );
        let err: Response = serde_json::from_value(
            json!({"jsonrpc": "2.0", "id": "x", "error": {"code": -32601, "message": "no"}}),
        )
        .unwrap();
        assert_eq!(
            err.outcome,
            Outcome::Error(ErrorObject::new(ErrorCode::MethodNotFound, "no"))
        );
        for bad in [
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "id": 1, "result": 1, "error": {"code": 1, "message": ""}}),
            json!({"jsonrpc": "1.0", "id": 1, "result": 1}),
        ] {
            assert!(serde_json::from_value::<Response>(bad).is_err());
        }
    }

    #[test]
    fn error_codes_are_unique_and_round_trip() {
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(c.code()), Some(c));
            assert_eq!(
                ErrorCode::ALL
                    .iter()
                    .filter(|o| o.name() == c.name())
                    .count(),
                1
            );
        }
        assert_eq!(ErrorCode::from_code(1), None);
    }

    #[test]
    fn call_builds_a_typed_request() {
        let r = Request::call::<Hello>(
            Id::Number(3),
            &HelloParams {
                client: "t".into(),
                protocol: PROTOCOL_VERSION,
            },
        );
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"jsonrpc": "2.0", "id": 3, "method": "daemon.hello",
                   "params": {"client": "t", "protocol": PROTOCOL_VERSION}})
        );
    }

    #[test]
    fn hello_params_reject_unknown_fields() {
        let r = serde_json::from_value::<HelloParams>(
            json!({"client": "t", "protocol": 1, "extra": true}),
        );
        assert!(r.is_err());
    }
}
