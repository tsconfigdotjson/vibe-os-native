import Foundation
import Testing

@testable import VibeRPC

// Checks the Swift types against the shared contract in protocol/, the same
// files the Rust contract tests read (crates/vibe-core/tests/contract.rs), so
// neither side can drift without a failing test.

/// The repo's protocol/ directory: this file is
/// swift/VibeOS/Tests/VibeRPCTests/ContractTests.swift.
let protocolDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()  // VibeRPCTests
    .deletingLastPathComponent()  // Tests
    .deletingLastPathComponent()  // VibeOS
    .deletingLastPathComponent()  // swift
    .deletingLastPathComponent()  // the repo
    .appendingPathComponent("protocol")

func loadJSON(_ url: URL) throws -> Any {
    try JSONSerialization.jsonObject(with: Data(contentsOf: url), options: [.fragmentsAllowed])
}

func contractIndex() throws -> [String: Any] {
    try #require(try loadJSON(protocolDir.appendingPathComponent("vibed-rpc.json")) as? [String: Any])
}

struct Example {
    var name: String
    /// nil for a malformed `request_raw` line.
    var request: Any?
    /// NSNull for none.
    var response: Any
}

func contractExamples() throws -> [Example] {
    let dir = protocolDir.appendingPathComponent("examples")
    let files = try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
        .filter { $0.pathExtension == "json" }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
    #expect(!files.isEmpty)
    return try files.map { url in
        let object = try #require(try loadJSON(url) as? [String: Any], "\(url.lastPathComponent)")
        return Example(
            name: url.lastPathComponent,
            request: object["request"],
            response: try #require(object["response"], "\(url.lastPathComponent): response"))
    }
}

func jsonData(_ object: Any) throws -> Data {
    try JSONSerialization.data(withJSONObject: object, options: [.fragmentsAllowed])
}

func sameJSON(_ a: Any, _ b: Any) -> Bool {
    (a as AnyObject).isEqual(b)
}

/// The fields of `name` the index says may be absent; nil for Response,
/// whose rule is exactly one of result and error.
func optionalFields(_ name: String) throws -> Set<String>? {
    let optional = try #require(try contractIndex()["optional"] as? [String: [String]])
    return optional[name].map { Set($0) }
}

/// Decodes `object` as `T` (named `name` in the index), checks it re-encodes
/// to the same JSON, and that each field can be left out exactly when the
/// index says it's optional.
func roundTrip<T: Codable>(_ type: T.Type, _ name: String, _ object: Any, _ what: String) throws {
    let decoded = try JSONDecoder().decode(T.self, from: jsonData(object))
    let reencoded = try JSONSerialization.jsonObject(
        with: JSONEncoder().encode(decoded), options: [.fragmentsAllowed])
    #expect(sameJSON(reencoded, object), "\(what) doesn't round-trip through \(T.self)")
    guard let optional = try optionalFields(name), let fields = object as? [String: Any] else {
        return
    }
    for field in fields.keys {
        var without = fields
        without.removeValue(forKey: field)
        let decodes = (try? JSONDecoder().decode(T.self, from: jsonData(without))) != nil
        #expect(
            decodes == optional.contains(field),
            "\(what): \(name) without `\(field)` should \(optional.contains(field) ? "" : "not ")decode")
    }
}

func keys<K: CodingKey & CaseIterable>(_: K.Type) -> Set<String> {
    Set(K.allCases.map(\.stringValue))
}

/// The field names of every type the index declares, from the Swift types.
let swiftTypes: [String: Set<String>] = [
    "Request": keys(RPCRequest<JSONValue>.CodingKeys.self),
    "Response": keys(RPCResponse<JSONValue>.CodingKeys.self),
    "ErrorObject": keys(RPCError.CodingKeys.self),
    "HelloParams": keys(HelloParams.CodingKeys.self),
    "HelloResult": keys(HelloResult.CodingKeys.self),
    "ProtocolMismatchData": keys(ProtocolMismatchData.CodingKeys.self),
]

/// The type of each error code's data, if it has one.
let swiftErrorData: [RPCErrorCode: String] = [.protocolMismatch: "ProtocolMismatchData"]

@Suite struct ContractTests {
    @Test func indexMatchesSwift() throws {
        let index = try contractIndex()
        #expect(index["protocol_version"] as? Int == VibeRPC.protocolVersion)
        #expect(index["jsonrpc"] as? String == VibeRPC.jsonrpc)
        #expect(index["max_message_bytes"] as? Int == VibeRPC.maxMessageBytes)
        let socket = try #require(index["socket"] as? [String: String])
        #expect(socket["env"] == VibeRPC.socketEnv)
        #expect(socket["macos"] == VibeRPC.macOSSocket)

        let methods = try #require(index["methods"] as? [[String: String]])
        #expect(methods.map { $0["name"] } == VibeRPC.methods)
        for method in methods {
            switch method["name"] {
            case Hello.name:
                #expect(method["params"] == String(describing: Hello.Params.self))
                #expect(method["result"] == String(describing: Hello.Result.self))
            default:
                Issue.record("\(method["name"] ?? "?") has no Swift types; add them")
            }
        }

        let errors = try #require(index["errors"] as? [[String: Any]])
        #expect(errors.count == RPCErrorCode.allCases.count)
        for (error, code) in zip(errors, RPCErrorCode.allCases) {
            #expect(error["name"] as? String == code.name)
            #expect(error["code"] as? Int == code.rawValue, "\(code.name)")
            #expect(error["data"] as? String == swiftErrorData[code], "\(code.name)")
        }
    }

    @Test func typesMatchCodingKeys() throws {
        let declared = try #require(try contractIndex()["types"] as? [String: [String]])
        #expect(Set(declared.keys) == Set(swiftTypes.keys))
        for (name, fields) in declared {
            #expect(Set(fields).count == fields.count, "\(name) lists a field twice")
            #expect(Set(fields) == swiftTypes[name], "\(name): fields differ from the CodingKeys")
        }
        // Optional fields are declared for every type but Response.
        let optional = try #require(try contractIndex()["optional"] as? [String: [String]])
        #expect(Set(optional.keys) == Set(declared.keys).subtracting(["Response"]))
        for (name, fields) in optional {
            #expect(Set(fields).isSubset(of: swiftTypes[name] ?? []), "\(name): optional fields")
        }
    }

    @Test func examplesRoundTripThroughTheTypes() throws {
        var succeeded = Set<String>()
        for example in try contractExamples() {
            var requests: [Any] = []
            if let list = example.request as? [Any] {
                requests = list
            } else if let one = example.request {
                requests = [one]
            }
            // Requests that are valid envelopes, by id.
            var byID: [RPCID: (method: String, params: Any?)] = [:]
            for request in requests {
                guard
                    let typed = try? JSONDecoder().decode(
                        RPCRequest<JSONValue>.self, from: jsonData(request))
                else { continue }
                try roundTrip(RPCRequest<JSONValue>.self, "Request", request, "\(example.name): request")
                if let id = typed.id {
                    byID[id] = (typed.method, (request as? [String: Any])?["params"])
                }
            }

            var responses: [Any] = []
            if let list = example.response as? [Any] {
                responses = list
            } else if !(example.response is NSNull) {
                responses = [example.response]
            }
            for response in responses {
                let what = "\(example.name): response"
                try roundTrip(RPCResponse<JSONValue>.self, "Response", response, what)
                let typed = try JSONDecoder().decode(RPCResponse<JSONValue>.self, from: jsonData(response))
                let fields = try #require(response as? [String: Any])
                switch typed.outcome {
                case .result:
                    let request = try #require(byID[typed.id], "\(what): no request with its id")
                    let params = try #require(request.params, "\(what): params")
                    let result = try #require(fields["result"])
                    switch request.method {
                    case Hello.name:
                        try roundTrip(HelloParams.self, "HelloParams", params, "\(what): params")
                        try roundTrip(HelloResult.self, "HelloResult", result, "\(what): result")
                        try roundTrip(RPCResponse<HelloResult>.self, "Response", response, what)
                    default:
                        Issue.record("\(what): no typed check for \(request.method); add one")
                    }
                    succeeded.insert(request.method)
                case .error(let error):
                    let code = try #require(RPCErrorCode(rawValue: error.code), "\(what): unknown code")
                    try roundTrip(RPCError.self, "ErrorObject", try #require(fields["error"]), "\(what): error")
                    let data = (fields["error"] as? [String: Any])?["data"]
                    switch (swiftErrorData[code], data) {
                    case ("ProtocolMismatchData"?, let data?):
                        try roundTrip(ProtocolMismatchData.self, "ProtocolMismatchData", data, "\(what): data")
                    case (nil, nil):
                        break
                    default:
                        Issue.record("\(what): data doesn't match its code")
                    }
                }
            }
        }
        #expect(succeeded == Set(VibeRPC.methods), "a method has no successful example")
    }

    @Test func notificationsAndNullIDsDiffer() throws {
        let notification = try JSONDecoder().decode(
            RPCRequest<JSONValue>.self, from: Data(#"{"jsonrpc":"2.0","method":"m"}"#.utf8))
        #expect(notification.id == nil)
        let nullID = try JSONDecoder().decode(
            RPCRequest<JSONValue>.self, from: Data(#"{"jsonrpc":"2.0","id":null,"method":"m"}"#.utf8))
        #expect(nullID.id == .null)
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(
                RPCRequest<JSONValue>.self, from: Data(#"{"jsonrpc":"1.0","id":1,"method":"m"}"#.utf8))
        }
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(
                RPCResponse<JSONValue>.self, from: Data(#"{"jsonrpc":"2.0","id":1}"#.utf8))
        }
    }
}

@Suite struct ClientTests {
    @Test func socketPathComesFromTheEnvironmentFirst() {
        #expect(
            VibedClient.defaultSocketPath(environment: ["VIBE_SOCKET": "/tmp/x.sock"], home: "/Users/u")
                == "/tmp/x.sock")
        #expect(
            VibedClient.defaultSocketPath(environment: ["VIBE_SOCKET": ""], home: "/Users/u")
                == "/Users/u/Library/Application Support/VibeOS/run/vibed.sock")
    }

    @Test func noDaemonIsAClearError() {
        let client = VibedClient(socketPath: "/tmp/vibe-no-such-\(UUID()).sock")
        do {
            _ = try client.hello(client: "test")
            Issue.record("connected to a socket that doesn't exist")
        } catch let error as VibedClientError {
            #expect(error == .connect(path: client.socketPath, errno: ENOENT))
            #expect(error.description.contains("is vibed running?"))
        } catch {
            Issue.record("unexpected error: \(error)")
        }
    }

    @Test func overlongSocketPathIsRefused() {
        let client = VibedClient(socketPath: "/" + String(repeating: "a", count: 200))
        #expect(throws: VibedClientError.socketPathTooLong(client.socketPath)) {
            try client.hello(client: "test")
        }
    }
}

/// Calls a real vibed. scripts/check-full.sh starts one and sets
/// VIBE_TEST_SOCKET; the test writes VIBE_TEST_MARKER so the script knows it
/// ran rather than being skipped.
@Suite(.enabled(if: ProcessInfo.processInfo.environment["VIBE_TEST_SOCKET"] != nil))
struct LiveDaemonTests {
    let client = VibedClient(socketPath: ProcessInfo.processInfo.environment["VIBE_TEST_SOCKET"] ?? "")

    @Test func helloAgainstALiveDaemon() throws {
        let hello = try client.hello(client: "VibeRPCTests")
        #expect(hello.daemon == "vibed")
        #expect(hello.protocolVersion == VibeRPC.protocolVersion)
        #expect(hello.os == "macos")
        #expect(hello.pid > 0)

        // A client on another protocol is refused with the daemon's version.
        do {
            _ = try client.call(Hello.self, HelloParams(client: "VibeRPCTests", protocolVersion: 999))
            Issue.record("vibed accepted protocol 999")
        } catch VibedClientError.rpc(let rpc) {
            #expect(rpc.code == RPCErrorCode.protocolMismatch.rawValue)
            #expect(rpc.data == .object(["protocol": .number(Double(VibeRPC.protocolVersion))]))
        } catch {
            Issue.record("unexpected error: \(error)")
        }

        if let marker = ProcessInfo.processInfo.environment["VIBE_TEST_MARKER"] {
            try Data("ok\n".utf8).write(to: URL(fileURLWithPath: marker))
        }
    }
}
