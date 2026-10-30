//! Turns one JSON-RPC message into its response. Pure, so the contract
//! examples in `protocol/examples/` replay through it exactly.

use serde_json::Value;
use vibe_core::rpc::{
    ErrorCode, ErrorObject, Hello, HelloParams, HelloResult, Id, Method, PROTOCOL_VERSION,
    ProtocolMismatchData, Request, Response,
};

/// What `daemon.hello` reports about this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInfo {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub pid: u32,
}

impl DaemonInfo {
    pub fn current() -> Self {
        DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            pid: std::process::id(),
        }
    }
}

/// Handles one message, a line without its newline, and returns the line to
/// send back, if any. Notifications, and batches of only notifications, get
/// nothing.
pub fn handle_message(info: &DaemonInfo, message: &[u8]) -> Option<String> {
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
            let responses: Vec<Response> = items
                .into_iter()
                .filter_map(|v| handle_value(info, v))
                .collect();
            (!responses.is_empty()).then(|| encode(&responses))
        }
        value => handle_value(info, value).map(|r| encode(&r)),
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

fn handle_value(info: &DaemonInfo, value: Value) -> Option<Response> {
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
    let outcome = call(info, &request.method, request.params);
    let id = request.id?;
    Some(match outcome {
        Ok(result) => Response::result(id, result),
        Err(error) => Response::error(id, error),
    })
}

fn call(info: &DaemonInfo, method: &str, params: Option<Value>) -> Result<Value, ErrorObject> {
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
            handle::<Hello>(params, |p| hello(info, p))
        }
        _ => Err(ErrorObject::new(
            ErrorCode::MethodNotFound,
            format!("Method not found: {method}"),
        )),
    }
}

/// Decodes `M`'s params (by name only; absent means `{}`), runs the handler
/// and encodes its result.
fn handle<M: Method>(
    params: Option<Value>,
    handler: impl FnOnce(M::Params) -> Result<M::Result, ErrorObject>,
) -> Result<Value, ErrorObject> {
    let params = match params {
        None => Value::Object(Default::default()),
        Some(p @ Value::Object(_)) => p,
        Some(_) => return Err(invalid_params("params must be an object")),
    };
    let params = serde_json::from_value(params).map_err(|e| invalid_params(&e.to_string()))?;
    let result = handler(params)?;
    serde_json::to_value(result)
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

fn hello(info: &DaemonInfo, params: HelloParams) -> Result<HelloResult, ErrorObject> {
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
    use serde_json::json;

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: "9.9.9".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            pid: 1,
        }
    }

    fn reply(message: &str) -> Option<Value> {
        handle_message(&info(), message.as_bytes()).map(|s| {
            assert!(!s.contains('\n'), "a response is one line");
            serde_json::from_str(&s).unwrap()
        })
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
        let r = handle_message(
            &info(),
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"\xff\"}",
        )
        .unwrap();
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
    fn scalars_are_invalid_requests() {
        for m in ["1", "\"x\"", "null", "true"] {
            assert_eq!(reply(m).unwrap()["error"]["code"], -32600, "{m}");
        }
    }
}
