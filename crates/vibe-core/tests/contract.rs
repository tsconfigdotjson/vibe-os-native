//! Checks `vibe_core::rpc` against the shared contract in `protocol/`. The
//! Swift side runs the same checks (swift/VibeOS/Tests/VibeRPCTests), so a
//! change on either side that isn't made to the contract fails a test.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use serde::{Serialize, forward_to_deserialize_any};
use serde_json::Value;
use vibe_core::paths;
use vibe_core::rpc::{
    ErrorCode, ErrorObject, Hello, HelloParams, HelloResult, MAX_MESSAGE_BYTES, METHODS, Method,
    Outcome, PROTOCOL_VERSION, ProtocolMismatchData, Request, Response,
};

fn protocol_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn index() -> Value {
    read_json(&protocol_dir().join("vibed-rpc.json"))
}

struct Example {
    name: String,
    /// `None` for a malformed `request_raw` line.
    request: Option<Value>,
    response: Value,
}

fn examples() -> Vec<Example> {
    let dir = protocol_dir().join("examples");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no examples in {}", dir.display());
    paths
        .iter()
        .map(|p| {
            let v = read_json(p);
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let obj = v
                .as_object()
                .unwrap_or_else(|| panic!("{name}: not an object"));
            let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
            let has_raw = keys.contains("request_raw");
            assert!(
                keys == BTreeSet::from(["description", "request", "response"])
                    || keys == BTreeSet::from(["description", "request_raw", "response"]),
                "{name}: an example has `description`, `request` or `request_raw`, and `response`"
            );
            if has_raw {
                assert!(
                    obj["request_raw"].is_string(),
                    "{name}: request_raw is a string"
                );
            }
            Example {
                name,
                request: (!has_raw).then(|| obj["request"].clone()),
                response: obj["response"].clone(),
            }
        })
        .collect()
}

/// Reads the field names serde uses for a struct, without a value.
fn fields_of<T: DeserializeOwned>() -> BTreeSet<&'static str> {
    struct Probe<'a>(&'a mut Option<&'static [&'static str]>);

    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not a struct"))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = Some(fields);
            Err(de::Error::custom("probed"))
        }

        forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map
            enum identifier ignored_any
        }
    }

    let mut fields = None;
    let _ = T::deserialize(Probe(&mut fields));
    fields
        .expect("type deserializes as a struct")
        .iter()
        .copied()
        .collect()
}

/// The serde field names of every type named in the index.
fn rust_types() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    BTreeMap::from([
        ("Request", fields_of::<Request>()),
        ("Response", fields_of::<Response>()),
        ("ErrorObject", fields_of::<ErrorObject>()),
        ("HelloParams", fields_of::<HelloParams>()),
        ("HelloResult", fields_of::<HelloResult>()),
        ("ProtocolMismatchData", fields_of::<ProtocolMismatchData>()),
    ])
}

/// The (params, result) type names of each method, as the index names them.
fn rust_method_types(method: &str) -> (&'static str, &'static str) {
    match method {
        Hello::NAME => ("HelloParams", "HelloResult"),
        other => panic!("{other} has no Rust types; add it to rust_method_types"),
    }
}

/// The type of an error code's `data`, if it has one.
fn rust_error_data(code: ErrorCode) -> Option<&'static str> {
    match code {
        ErrorCode::ProtocolMismatch => Some("ProtocolMismatchData"),
        _ => None,
    }
}

/// The fields of `ty` the index says may be absent. `None` for `Response`,
/// whose rule is exactly one of `result` and `error`.
fn optional_fields(ty: &str) -> Option<BTreeSet<String>> {
    index()["optional"].get(ty).map(|fields| {
        fields
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_owned())
            .collect()
    })
}

/// Decodes `value` as `T` (named `ty` in the index), checks it re-encodes to
/// the same JSON, and that each field can be left out exactly when the index
/// says it's optional.
fn round_trip<T: DeserializeOwned + Serialize>(value: &Value, ty: &str, what: &str) {
    let typed: T = serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("{what} doesn't decode: {e}\n{value}"));
    assert_eq!(
        serde_json::to_value(&typed).unwrap(),
        *value,
        "{what} doesn't re-encode to the same JSON"
    );
    if let Some(optional) = optional_fields(ty) {
        for field in value.as_object().unwrap().keys() {
            let mut without = value.clone();
            without.as_object_mut().unwrap().remove(field);
            let decodes = serde_json::from_value::<T>(without).is_ok();
            assert_eq!(
                decodes,
                optional.contains(field),
                "{what}: {ty} without `{field}` should {}decode",
                if optional.contains(field) { "" } else { "not " }
            );
        }
    }
}

#[test]
fn index_matches_rust() {
    let index = index();
    assert_eq!(index["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(index["jsonrpc"], "2.0");
    assert_eq!(index["max_message_bytes"], MAX_MESSAGE_BYTES);
    assert_eq!(index["socket"]["env"], paths::SOCKET_ENV);
    assert_eq!(index["socket"]["macos"], paths::MACOS_SOCKET);
    assert_eq!(index["socket"]["linux"], paths::LINUX_SOCKET);

    let methods = index["methods"].as_array().unwrap();
    let names: Vec<&str> = methods
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names, METHODS,
        "methods differ from vibe_core::rpc::METHODS"
    );
    for m in methods {
        let (params, result) = rust_method_types(m["name"].as_str().unwrap());
        assert_eq!(m["params"], params, "{}: params type", m["name"]);
        assert_eq!(m["result"], result, "{}: result type", m["name"]);
    }

    let errors = index["errors"].as_array().unwrap();
    assert_eq!(errors.len(), ErrorCode::ALL.len(), "error codes differ");
    for (e, code) in errors.iter().zip(ErrorCode::ALL) {
        assert_eq!(e["name"], code.name());
        assert_eq!(e["code"], code.code(), "{}", code.name());
        assert_eq!(
            e.get("data").and_then(Value::as_str),
            rust_error_data(code),
            "{}",
            code.name()
        );
    }
}

#[test]
fn types_match_serde_fields() {
    let index = index();
    let declared = index["types"].as_object().unwrap();
    let rust = rust_types();
    assert_eq!(
        declared.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        rust.keys().copied().collect::<BTreeSet<_>>(),
        "the index's types differ from the Rust types"
    );
    for (name, fields) in declared {
        let list: Vec<&str> = fields
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        let set: BTreeSet<&str> = list.iter().copied().collect();
        assert_eq!(set.len(), list.len(), "{name} lists a field twice");
        assert_eq!(
            set,
            rust[name.as_str()],
            "{name}: fields differ from serde's"
        );
    }
    // Optional fields are declared for every type but Response, whose rule is
    // exactly one of `result` and `error`.
    let optional = index["optional"].as_object().unwrap();
    let mut expected: BTreeSet<&str> = declared.keys().map(String::as_str).collect();
    expected.remove("Response");
    assert_eq!(
        optional.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        expected
    );
    for (name, fields) in optional {
        for f in fields.as_array().unwrap() {
            let f = f.as_str().unwrap();
            assert!(
                rust[name.as_str()].contains(f),
                "{name}: optional `{f}` isn't a field"
            );
        }
    }
    // Every type an index entry refers to is declared.
    for m in index["methods"].as_array().unwrap() {
        for k in ["params", "result"] {
            assert!(
                declared.contains_key(m[k].as_str().unwrap()),
                "{}: {k}",
                m["name"]
            );
        }
    }
}

/// What walking every example saw.
#[derive(Default)]
struct Seen {
    /// Keys seen per type, for coverage.
    keys: BTreeMap<&'static str, BTreeSet<String>>,
    succeeded: BTreeSet<String>,
    codes: BTreeSet<i64>,
}

impl Seen {
    fn keys(&mut self, ty: &'static str, value: &Value) {
        let obj = value
            .as_object()
            .unwrap_or_else(|| panic!("{ty}: not an object: {value}"));
        self.keys.entry(ty).or_default().extend(obj.keys().cloned());
    }
}

fn as_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items.clone(),
        Value::Null => vec![],
        other => vec![other.clone()],
    }
}

/// Decodes every example into the typed structs and checks it re-encodes to
/// the same JSON.
fn walk_examples() -> Seen {
    let mut seen = Seen::default();
    for ex in examples() {
        let requests: Vec<Value> = ex.request.as_ref().map(as_list).unwrap_or_default();
        // Requests that are valid envelopes, by id.
        let mut by_id = BTreeMap::new();
        for r in &requests {
            if let Ok(req) = serde_json::from_value::<Request>(r.clone()) {
                round_trip::<Request>(r, "Request", &format!("{}: request", ex.name));
                seen.keys("Request", r);
                if let Some(id) = &req.id {
                    by_id.insert(serde_json::to_string(id).unwrap(), req);
                }
            }
        }
        if let Value::Array(rs) = &ex.response {
            assert!(
                !rs.is_empty(),
                "{}: an empty response batch is never sent",
                ex.name
            );
        }
        for resp in as_list(&ex.response) {
            let what = format!("{}: response", ex.name);
            round_trip::<Response>(&resp, "Response", &what);
            seen.keys("Response", &resp);
            let typed: Response = serde_json::from_value(resp.clone()).unwrap();
            let request = by_id.get(&serde_json::to_string(&typed.id).unwrap());
            match typed.outcome {
                Outcome::Result(result) => {
                    let req = request.unwrap_or_else(|| panic!("{what}: no request with its id"));
                    let params = req.params.clone().unwrap_or(Value::Null);
                    match req.method.as_str() {
                        Hello::NAME => {
                            round_trip::<HelloParams>(
                                &params,
                                "HelloParams",
                                &format!("{what}: params"),
                            );
                            round_trip::<HelloResult>(
                                &result,
                                "HelloResult",
                                &format!("{what}: result"),
                            );
                        }
                        other => panic!("{what}: no typed check for {other}; add one"),
                    }
                    let (pty, rty) = rust_method_types(&req.method);
                    seen.keys(pty, &params);
                    seen.keys(rty, &result);
                    seen.succeeded.insert(req.method.clone());
                }
                Outcome::Error(error) => {
                    let ev = &resp["error"];
                    round_trip::<ErrorObject>(ev, "ErrorObject", &format!("{what}: error"));
                    seen.keys("ErrorObject", ev);
                    let code = ErrorCode::from_code(error.code)
                        .unwrap_or_else(|| panic!("{what}: unknown error code {}", error.code));
                    seen.codes.insert(error.code);
                    match (rust_error_data(code), &error.data) {
                        (Some("ProtocolMismatchData"), Some(data)) => {
                            round_trip::<ProtocolMismatchData>(
                                data,
                                "ProtocolMismatchData",
                                &format!("{what}: data"),
                            );
                            seen.keys("ProtocolMismatchData", data);
                        }
                        (None, None) => {}
                        (ty, data) => panic!("{what}: expected data {ty:?}, got {data:?}"),
                    }
                }
            }
        }
    }
    seen
}

#[test]
fn examples_round_trip_through_the_types() {
    walk_examples();
}

#[test]
fn examples_cover_the_contract() {
    let seen = walk_examples();
    for m in METHODS {
        assert!(seen.succeeded.contains(*m), "no successful example for {m}");
    }
    for code in ErrorCode::ALL {
        // An internal error can't be provoked on purpose.
        if code != ErrorCode::InternalError {
            assert!(
                seen.codes.contains(&code.code()),
                "no example of {}",
                code.name()
            );
        }
    }
    let declared = &index()["types"];
    for (ty, fields) in rust_types() {
        let seen_keys: BTreeSet<&str> = seen
            .keys
            .get(ty)
            .map(|k| k.iter().map(String::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            seen_keys, fields,
            "the examples don't use exactly the fields of {ty}"
        );
        assert!(declared.get(ty).is_some());
    }
}
