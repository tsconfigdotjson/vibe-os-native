//! Checks `vibe_core::rpc` against the shared contract in `protocol/`. The
//! Swift side runs the same checks (swift/VibeOS/Tests/VibeRPCTests), so a
//! change on either side that isn't made to the contract fails a test.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use serde::{Serialize, forward_to_deserialize_any};
use serde_json::Value;
use vibe_core::model::{
    Activity, AgentState, Certainty, Environment, EventKind, GhState, Mode, Model, ProjectSource,
    Reasoning, SessionState, StopReason, WakeupKind,
};
use vibe_core::paths;
use vibe_core::rpc::*;

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
    /// `None` for a malformed `request_raw` line, or a notification example.
    request: Option<Value>,
    /// `Null` for none.
    response: Value,
    /// Notifications the daemon sends on the connection after the response.
    events: Vec<Value>,
    /// A notification example: a message the daemon sends unasked.
    notification: Option<Value>,
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
            let mut keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
            if keys == BTreeSet::from(["description", "notification"]) {
                return Example {
                    name,
                    request: None,
                    response: Value::Null,
                    events: vec![],
                    notification: Some(obj["notification"].clone()),
                };
            }
            let has_raw = keys.contains("request_raw");
            let has_events = keys.remove("events");
            assert!(
                keys == BTreeSet::from(["description", "request", "response"])
                    || keys == BTreeSet::from(["description", "request_raw", "response"]),
                "{name}: an example has `description`, `request` or `request_raw`, `response` \
                 and maybe `events`; or `description` and `notification`"
            );
            if has_raw {
                assert!(
                    obj["request_raw"].is_string(),
                    "{name}: request_raw is a string"
                );
            }
            let events = if has_events {
                let events = obj["events"].as_array().cloned().unwrap_or_default();
                assert!(!events.is_empty(), "{name}: `events` is a non-empty array");
                events
            } else {
                vec![]
            };
            Example {
                name,
                request: (!has_raw).then(|| obj["request"].clone()),
                response: obj["response"].clone(),
                events,
                notification: None,
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

/// The last segment of a type's path, as the index names types.
fn short_name<T>() -> &'static str {
    let full = std::any::type_name::<T>();
    full.rsplit("::").next().unwrap()
}

/// The serde field names of every type named in the index.
fn rust_types() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    fn entry<T: DeserializeOwned>() -> (&'static str, BTreeSet<&'static str>) {
        (short_name::<T>(), fields_of::<T>())
    }
    BTreeMap::from([
        entry::<Request>(),
        entry::<Response>(),
        entry::<ErrorObject>(),
        entry::<HelloParams>(),
        entry::<HelloResult>(),
        entry::<ProtocolMismatchData>(),
        entry::<NoParams>(),
        entry::<HostParams>(),
        entry::<HostInfo>(),
        entry::<HostList>(),
        entry::<Project>(),
        entry::<ProjectList>(),
        entry::<ProjectAddParams>(),
        entry::<ProjectParams>(),
        entry::<Profile>(),
        entry::<ProfileList>(),
        entry::<ProfileCreateParams>(),
        entry::<ProfileUpdateParams>(),
        entry::<ProfileParams>(),
        entry::<HostLimits>(),
        entry::<LimitsSetParams>(),
        entry::<ApiKey>(),
        entry::<KeyList>(),
        entry::<KeyCreateParams>(),
        entry::<KeyCreated>(),
        entry::<KeyParams>(),
        entry::<KeyVerifyParams>(),
        entry::<GhStatusParams>(),
        entry::<GhStatus>(),
        entry::<Subscribed>(),
        entry::<Event>(),
        entry::<SessionParams>(),
        entry::<SessionList>(),
        entry::<SessionInfo>(),
        entry::<SessionExit>(),
        entry::<SessionAttach>(),
        entry::<AgentStartParams>(),
        entry::<AgentDeleteParams>(),
        entry::<Agent>(),
        entry::<AgentStatus>(),
        entry::<Wakeup>(),
        entry::<AgentList>(),
    ])
}

/// A method's Rust types: their index names, and a check that decodes an
/// example's params and result into them.
struct MethodTypes {
    params: &'static str,
    result: &'static str,
    check: fn(&Value, &Value, &str),
}

fn method<M: Method>() -> (&'static str, MethodTypes) {
    fn check<M: Method>(params: &Value, result: &Value, what: &str) {
        round_trip::<M::Params>(
            params,
            short_name::<M::Params>(),
            &format!("{what}: params"),
        );
        round_trip::<M::Result>(
            result,
            short_name::<M::Result>(),
            &format!("{what}: result"),
        );
    }
    (
        M::NAME,
        MethodTypes {
            params: short_name::<M::Params>(),
            result: short_name::<M::Result>(),
            check: check::<M>,
        },
    )
}

/// Every method's types; `METHODS` must list exactly these.
fn rust_methods() -> BTreeMap<&'static str, MethodTypes> {
    BTreeMap::from([
        method::<Hello>(),
        method::<HostsList>(),
        method::<ProjectsList>(),
        method::<ProjectsAdd>(),
        method::<ProjectsRemove>(),
        method::<ProfilesList>(),
        method::<ProfilesCreate>(),
        method::<ProfilesUpdate>(),
        method::<ProfilesDelete>(),
        method::<LimitsGet>(),
        method::<LimitsSet>(),
        method::<KeysList>(),
        method::<KeysCreate>(),
        method::<KeysRevoke>(),
        method::<KeysVerify>(),
        method::<GhStatusGet>(),
        method::<EventsSubscribe>(),
        method::<SessionsList>(),
        method::<SessionsAttach>(),
        method::<AgentsStart>(),
        method::<AgentsList>(),
        method::<AgentsDelete>(),
    ])
}

/// The (params, result) type names of each method, as the index names them.
fn rust_method_types(method: &str) -> (&'static str, &'static str) {
    let methods = rust_methods();
    let m = methods
        .get(method)
        .unwrap_or_else(|| panic!("{method} has no Rust types; add it to rust_methods"));
    (m.params, m.result)
}

/// The variants of every enum the index lists.
fn rust_enums() -> BTreeMap<&'static str, Vec<&'static str>> {
    fn names<T: Copy>(all: &[T], name: fn(T) -> &'static str) -> Vec<&'static str> {
        all.iter().map(|v| name(*v)).collect()
    }
    BTreeMap::from([
        ("Environment", names(Environment::ALL, Environment::as_str)),
        ("Model", names(Model::ALL, Model::as_str)),
        ("Reasoning", names(Reasoning::ALL, Reasoning::as_str)),
        ("Mode", names(Mode::ALL, Mode::as_str)),
        (
            "ProjectSource",
            names(ProjectSource::ALL, ProjectSource::as_str),
        ),
        ("GhState", names(GhState::ALL, GhState::as_str)),
        ("EventKind", names(EventKind::ALL, EventKind::as_str)),
        (
            "SessionState",
            names(SessionState::ALL, SessionState::as_str),
        ),
        ("AgentState", names(AgentState::ALL, AgentState::as_str)),
        ("Activity", names(Activity::ALL, Activity::as_str)),
        ("Certainty", names(Certainty::ALL, Certainty::as_str)),
        ("WakeupKind", names(WakeupKind::ALL, WakeupKind::as_str)),
        ("StopReason", names(StopReason::ALL, StopReason::as_str)),
    ])
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

    let notifications: Vec<&str> = index["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(notifications, NOTIFICATIONS);
    assert_eq!(
        index["notifications"][0]["params"],
        short_name::<<EventNotice as Notification>::Params>()
    );

    let enums: BTreeMap<&str, Vec<&str>> = index["enums"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            let names = v
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap())
                .collect();
            (k.as_str(), names)
        })
        .collect();
    assert_eq!(enums, rust_enums(), "the index's enums differ from Rust's");

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
    // Nested fields name declared types, and fields those types have.
    for (outer, fields) in index["nested"].as_object().unwrap() {
        for (field, inner) in fields.as_object().unwrap() {
            assert!(
                rust[outer.as_str()].contains(field.as_str()),
                "{outer}.{field}"
            );
            assert!(
                declared.contains_key(inner.as_str().unwrap()),
                "{outer}.{field}"
            );
        }
    }
    for n in index["notifications"].as_array().unwrap() {
        assert!(declared.contains_key(n["params"].as_str().unwrap()));
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
    keys: BTreeMap<String, BTreeSet<String>>,
    succeeded: BTreeSet<String>,
    notified: BTreeSet<String>,
    codes: BTreeSet<i64>,
}

impl Seen {
    /// Records the keys of `value`, a `ty`, and of the types nested in it.
    fn keys(&mut self, ty: &str, value: &Value) {
        let obj = value
            .as_object()
            .unwrap_or_else(|| panic!("{ty}: not an object: {value}"));
        self.keys
            .entry(ty.to_owned())
            .or_default()
            .extend(obj.keys().cloned());
        if let Some(nested) = index()["nested"].get(ty) {
            for (field, inner) in nested.as_object().unwrap() {
                let inner = inner.as_str().unwrap();
                match obj.get(field) {
                    Some(Value::Array(items)) => items.iter().for_each(|i| self.keys(inner, i)),
                    Some(v @ Value::Object(_)) => self.keys(inner, v),
                    _ => {}
                }
            }
        }
    }

    /// Checks a notification the daemon sends.
    fn notification(&mut self, n: &Value, what: &str) {
        let typed: Request = serde_json::from_value(n.clone())
            .unwrap_or_else(|e| panic!("{what}: not a notification: {e}"));
        assert_eq!(typed.id, None, "{what}: a notification has no id");
        round_trip::<Request>(n, "Request", what);
        assert_eq!(
            typed.method,
            EventNotice::NAME,
            "{what}: unknown notification"
        );
        let params = typed.params.unwrap_or(Value::Null);
        round_trip::<Event>(&params, "Event", &format!("{what}: params"));
        self.keys("Event", &params);
        self.notified.insert(typed.method);
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
        if let Some(n) = &ex.notification {
            seen.notification(n, &ex.name);
            continue;
        }
        for (i, e) in ex.events.iter().enumerate() {
            seen.notification(e, &format!("{}: event {i}", ex.name));
        }
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
                    // Params are always given in the examples, so both
                    // sides can check them; `{}` when there are none.
                    let params = req
                        .params
                        .clone()
                        .unwrap_or_else(|| panic!("{what}: give the request's params"));
                    let methods = rust_methods();
                    let types = methods
                        .get(req.method.as_str())
                        .unwrap_or_else(|| panic!("{what}: no typed check for {}", req.method));
                    (types.check)(&params, &result, &what);
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
    for n in NOTIFICATIONS {
        assert!(
            seen.notified.contains(*n),
            "no example of the notification {n}"
        );
    }
    for code in ErrorCode::ALL {
        // An internal error can't be provoked on purpose, and an invalid
        // host.toml needs a broken file; vibed's own tests cover that one.
        if !matches!(
            code,
            ErrorCode::InternalError | ErrorCode::HostConfigInvalid
        ) {
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
