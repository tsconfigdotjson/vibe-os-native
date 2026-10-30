//! Checks `vibe_env::vm` against the vibe-vm contract in
//! `protocol/vibe-vm/`, the files the Swift side is tested against too
//! (`swift/vibe-vm/Tests/VibeVMKitTests/ContractTests.swift`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use vibe_env::vm::{
    Created, Done, ErrorCode, Failure, Ready, Request, Slept, Started, Status, VmConfig,
    decode_reply, encode_reply,
};

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/vibe-vm")
}

fn load(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

struct Example {
    name: String,
    value: Value,
}

fn examples() -> Vec<Example> {
    let mut paths: Vec<PathBuf> = fs::read_dir(contract_dir().join("examples"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    paths
        .into_iter()
        .map(|p| Example {
            name: p.file_name().unwrap().to_string_lossy().into_owned(),
            value: load(&p),
        })
        .collect()
}

/// Decodes a reply line into `T` (or a failure) and encodes it again,
/// which must give the same JSON. Returns the failure, if it was one.
fn round_trip<T: DeserializeOwned + Serialize>(name: &str, reply: &Value) -> Option<Failure> {
    let decoded =
        decode_reply::<T>(&reply.to_string()).unwrap_or_else(|e| panic!("{name}: {e}: {reply}"));
    let again: Value = serde_json::from_str(&encode_reply(&decoded)).unwrap();
    assert_eq!(&again, reply, "{name}");
    decoded.err()
}

#[test]
fn the_config_round_trips_and_is_valid() {
    let value = load(&contract_dir().join("config.json"));
    let config: VmConfig = serde_json::from_value(value.clone()).unwrap();
    config.validate().unwrap();
    assert_eq!(serde_json::to_value(&config).unwrap(), value);
    assert!(config.disks.iter().any(|d| d.read_only));
    assert!(config.disks.iter().any(|d| !d.read_only));
}

#[test]
fn exactly_the_optional_config_fields_can_be_dropped() {
    let value = load(&contract_dir().join("config.json"));
    let Value::Object(fields) = &value else {
        panic!("not an object")
    };
    let optional = ["initrd", "machine_id", "mac", "console_log"];
    for key in fields.keys() {
        let mut without = fields.clone();
        without.remove(key);
        let decoded = serde_json::from_value::<VmConfig>(Value::Object(without));
        assert_eq!(
            decoded.is_ok(),
            optional.contains(&key.as_str()),
            "dropping {key}"
        );
    }
}

#[test]
fn every_example_round_trips() {
    let mut codes = BTreeSet::new();
    let mut ops = BTreeSet::new();
    let mut started = BTreeSet::new();
    for Example { name, value } in examples() {
        let name = name.as_str();
        let failure = if let Some(ready) = value.get("ready") {
            let command = value["command"][0].as_str().unwrap();
            match command {
                "create" => round_trip::<Created>(name, ready),
                "boot" | "restore" => {
                    let failure = round_trip::<Ready>(name, ready);
                    if failure.is_none() {
                        let r: Ready = serde_json::from_value(ready.clone()).unwrap();
                        assert_eq!(serde_json::to_value(r.started).unwrap(), command, "{name}");
                        started.insert(command.to_owned());
                    }
                    failure
                }
                other => panic!("{name}: unknown command {other}"),
            }
        } else if let Some(raw) = value.get("request_raw") {
            let raw = raw.as_str().unwrap();
            assert!(
                serde_json::from_str::<Request>(raw).is_err(),
                "{name}: {raw} should be refused"
            );
            round_trip::<Done>(name, &value["reply"])
        } else {
            let request: Request = serde_json::from_value(value["request"].clone())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                serde_json::to_value(&request).unwrap(),
                value["request"],
                "{name}"
            );
            let reply = &value["reply"];
            ops.insert(value["request"]["op"].as_str().unwrap().to_owned());
            match request {
                Request::Status => round_trip::<Status>(name, reply),
                Request::Sleep { .. } => round_trip::<Slept>(name, reply),
                Request::Stop | Request::Connect { .. } => round_trip::<Done>(name, reply),
            }
        };
        if let Some(f) = failure {
            codes.insert(serde_json::to_value(f.code).unwrap().to_string());
        }
    }
    for code in ErrorCode::ALL {
        let code = serde_json::to_value(code).unwrap().to_string();
        assert!(codes.contains(&code), "no example of error {code}");
    }
    assert_eq!(
        ops.into_iter().collect::<Vec<_>>(),
        ["connect", "sleep", "status", "stop"]
    );
    for s in [Started::Boot, Started::Restore] {
        let s = serde_json::to_value(s).unwrap();
        assert!(
            started.contains(s.as_str().unwrap()),
            "no ready line for {s}"
        );
    }
}

#[test]
fn sleep_takes_an_optional_balloon_time_and_floor() {
    let r: Request = serde_json::from_str(r#"{"op":"sleep"}"#).unwrap();
    assert_eq!(
        r,
        Request::Sleep {
            balloon_ms: None,
            balloon_floor: None
        }
    );
    let r: Request =
        serde_json::from_str(r#"{"op":"sleep","balloon_ms":0,"balloon_floor":1024}"#).unwrap();
    assert_eq!(
        r,
        Request::Sleep {
            balloon_ms: Some(0),
            balloon_floor: Some(1024)
        }
    );
    assert!(serde_json::from_str::<Request>(r#"{"op":"connect"}"#).is_err());
    assert!(serde_json::from_str::<Request>(r#"{"port":1}"#).is_err());
}
