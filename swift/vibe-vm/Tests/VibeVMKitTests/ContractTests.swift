import Foundation
import Testing

@testable import VibeVMKit

// Checks the Swift types against the vibe-vm contract in protocol/vibe-vm/,
// the same files the Rust side reads (crates/vibe-env/tests/contract.rs).

/// The repo's protocol/vibe-vm/ directory: this file is
/// swift/vibe-vm/Tests/VibeVMKitTests/ContractTests.swift.
let contractDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()  // VibeVMKitTests
    .deletingLastPathComponent()  // Tests
    .deletingLastPathComponent()  // vibe-vm
    .deletingLastPathComponent()  // swift
    .deletingLastPathComponent()  // the repo
    .appendingPathComponent("protocol/vibe-vm")

func jsonObject(_ data: Data) throws -> NSDictionary {
    try #require(try JSONSerialization.jsonObject(with: data) as? NSDictionary)
}

/// Decodes `value` as `T` and encodes it again, which must give the same
/// JSON.
func roundTrip<T: Codable>(_ type: T.Type, _ value: Any, _ name: String) throws -> T {
    let data = try JSONSerialization.data(withJSONObject: value)
    let decoded = try JSONDecoder().decode(T.self, from: data)
    let again = try jsonObject(Wire.line(decoded))
    #expect(again.isEqual(to: try jsonObject(data)), "\(name): \(again)")
    return decoded
}

/// A reply: a failure if `ok` is false, `T` otherwise.
func roundTripReply<T: Codable>(_ type: T.Type, _ reply: Any, _ name: String) throws -> Failure? {
    let ok = try #require((reply as? NSDictionary)?["ok"] as? Bool, "\(name): ok")
    if ok {
        _ = try roundTrip(T.self, reply, name)
        return nil
    }
    return try roundTrip(Failure.self, reply, name)
}

@Suite struct ContractTests {
    @Test func theConfigRoundTrips() throws {
        let data = try Data(contentsOf: contractDir.appendingPathComponent("config.json"))
        let config = try roundTrip(VMConfig.self, try jsonObject(data), "config.json")
        try config.validate()
    }

    @Test func exactlyTheOptionalConfigFieldsCanBeDropped() throws {
        let data = try Data(contentsOf: contractDir.appendingPathComponent("config.json"))
        let object = try jsonObject(data)
        let optional: Set<String> = ["initrd", "machine_id", "mac", "console_log"]
        for case let key as String in object.allKeys {
            let without = object.mutableCopy() as! NSMutableDictionary
            without.removeObject(forKey: key)
            let decoded = try? JSONDecoder().decode(
                VMConfig.self, from: JSONSerialization.data(withJSONObject: without))
            #expect((decoded != nil) == optional.contains(key), "dropping \(key)")
        }
    }

    @Test func everyExampleRoundTrips() throws {
        let dir = contractDir.appendingPathComponent("examples")
        let files = try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "json" }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
        #expect(!files.isEmpty)
        var codes = Set<ErrorCode>()
        var ops = Set<String>()
        var started = Set<Started>()
        for file in files {
            let name = file.lastPathComponent
            let example = try jsonObject(Data(contentsOf: file))
            var failure: Failure?
            if let ready = example["ready"] {
                let command = try #require((example["command"] as? [String])?.first, "\(name)")
                switch command {
                case "create":
                    failure = try roundTripReply(Created.self, ready, name)
                case "boot", "restore":
                    failure = try roundTripReply(Ready.self, ready, name)
                    if failure == nil {
                        let value = try roundTrip(Ready.self, ready, name)
                        #expect(value.started.rawValue == command, "\(name)")
                        started.insert(value.started)
                    }
                default:
                    Issue.record("\(name): unknown command \(command)")
                }
            } else if let raw = example["request_raw"] as? String {
                guard case .failure(let refused) = Request.parse(Data(raw.utf8)) else {
                    Issue.record("\(name): \(raw) should be refused")
                    continue
                }
                failure = try roundTripReply(Done.self, try #require(example["reply"]), name)
                #expect(refused == failure, "\(name): the parser's own refusal")
            } else {
                let requestObject = try #require(example["request"], "\(name)")
                let request = try roundTrip(Request.self, requestObject, name)
                guard case .success(let parsed) = Request.parse(try JSONSerialization.data(withJSONObject: requestObject))
                else {
                    Issue.record("\(name): the parser refused it")
                    continue
                }
                #expect(parsed == request)
                ops.insert(try #require((requestObject as? NSDictionary)?["op"] as? String))
                let reply = try #require(example["reply"], "\(name)")
                switch request {
                case .status: failure = try roundTripReply(Status.self, reply, name)
                case .sleep: failure = try roundTripReply(Slept.self, reply, name)
                case .stop, .connect: failure = try roundTripReply(Done.self, reply, name)
                }
            }
            if let failure { codes.insert(failure.code) }
        }
        #expect(codes == Set(ErrorCode.allCases), "error codes without an example: \(Set(ErrorCode.allCases).subtracting(codes))")
        #expect(ops == ["status", "sleep", "stop", "connect"])
        #expect(started == [.boot, .restore])
    }

    @Test func requestsAreParsedStrictly() {
        let cases: [(String, String)] = [
            ("", "one JSON object"),
            ("[]", "one JSON object"),
            (#"{"port":1}"#, "one JSON object"),
            (#"{"op":"dance"}"#, "unknown op `dance`"),
            (#"{"op":"connect"}"#, "bad `connect` request"),
            (#"{"op":"connect","port":-1}"#, "bad `connect` request"),
            (#"{"op":"sleep","balloon_ms":"x"}"#, "bad `sleep` request"),
        ]
        for (line, message) in cases {
            guard case .failure(let failure) = Request.parse(Data(line.utf8)) else {
                Issue.record("\(line) was accepted")
                continue
            }
            #expect(failure.code == .badRequest)
            #expect(failure.error.contains(message), "\(line): \(failure.error)")
        }
        #expect(Request.parse(Data(#"{"op":"sleep"}"#.utf8)) == .success(.sleep(balloonMS: nil)))
    }
}
