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
    /// nil for a malformed `request_raw` line, or a notification example.
    var request: Any?
    /// NSNull for none.
    var response: Any
    /// Notifications vibed sends on the connection after the response.
    var events: [Any]
    /// A notification example: a message vibed sends unasked.
    var notification: Any?
}

func contractExamples() throws -> [Example] {
    let dir = protocolDir.appendingPathComponent("examples")
    let files = try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
        .filter { $0.pathExtension == "json" }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
    #expect(!files.isEmpty)
    return try files.map { url in
        let name = url.lastPathComponent
        let object = try #require(try loadJSON(url) as? [String: Any], "\(name)")
        if let notification = object["notification"] {
            return Example(name: name, request: nil, response: NSNull(), events: [], notification: notification)
        }
        return Example(
            name: name,
            request: object["request"],
            response: try #require(object["response"], "\(name): response"),
            events: object["events"] as? [Any] ?? [],
            notification: nil)
    }
}

func jsonData(_ object: Any) throws -> Data {
    try JSONSerialization.data(withJSONObject: object, options: [.fragmentsAllowed])
}

/// Whether two parsed JSON values are the same, compared as canonical JSON:
/// NSNumber's isEqual would take `true` for `1`.
func sameJSON(_ a: Any, _ b: Any) -> Bool {
    let canonical = { (v: Any) in
        try? JSONSerialization.data(withJSONObject: v, options: [.sortedKeys, .fragmentsAllowed])
    }
    guard let x = canonical(a), let y = canonical(b) else { return false }
    return x == y
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
    "NoParams": keys(NoParams.CodingKeys.self),
    "HostParams": keys(HostParams.CodingKeys.self),
    "HostInfo": keys(HostInfo.CodingKeys.self),
    "MemoryUse": keys(MemoryUse.CodingKeys.self),
    "HostList": keys(HostList.CodingKeys.self),
    "HostAddParams": keys(HostAddParams.CodingKeys.self),
    "HostNameParams": keys(HostNameParams.CodingKeys.self),
    "HostInstall": keys(HostInstall.CodingKeys.self),
    "HostExternalParams": keys(HostExternalParams.CodingKeys.self),
    "TunnelPrepared": keys(TunnelPrepared.CodingKeys.self),
    "Project": keys(Project.CodingKeys.self),
    "ProjectList": keys(ProjectList.CodingKeys.self),
    "ProjectAddParams": keys(ProjectAddParams.CodingKeys.self),
    "ProjectParams": keys(ProjectParams.CodingKeys.self),
    "ProjectRemoveParams": keys(ProjectRemoveParams.CodingKeys.self),
    "Profile": keys(Profile.CodingKeys.self),
    "ProfileList": keys(ProfileList.CodingKeys.self),
    "ProfileCreateParams": keys(ProfileCreateParams.CodingKeys.self),
    "ProfileUpdateParams": keys(ProfileUpdateParams.CodingKeys.self),
    "ProfileParams": keys(ProfileParams.CodingKeys.self),
    "HostLimits": keys(HostLimits.CodingKeys.self),
    "LimitsSetParams": keys(LimitsSetParams.CodingKeys.self),
    "McpSettings": keys(McpSettings.CodingKeys.self),
    "McpSetParams": keys(McpSetParams.CodingKeys.self),
    "ApiKey": keys(ApiKey.CodingKeys.self),
    "KeyList": keys(KeyList.CodingKeys.self),
    "KeyCreateParams": keys(KeyCreateParams.CodingKeys.self),
    "KeyCreated": keys(KeyCreated.CodingKeys.self),
    "KeyParams": keys(KeyParams.CodingKeys.self),
    "KeyVerifyParams": keys(KeyVerifyParams.CodingKeys.self),
    "KeyHash": keys(KeyHash.CodingKeys.self),
    "KeySyncParams": keys(KeySyncParams.CodingKeys.self),
    "GhStatusParams": keys(GhStatusParams.CodingKeys.self),
    "GhStatus": keys(GhStatus.CodingKeys.self),
    "Subscribed": keys(Subscribed.CodingKeys.self),
    "Event": keys(DaemonEvent.CodingKeys.self),
    "SessionParams": keys(SessionParams.CodingKeys.self),
    "SessionList": keys(SessionList.CodingKeys.self),
    "SessionInfo": keys(SessionInfo.CodingKeys.self),
    "SessionExit": keys(SessionExit.CodingKeys.self),
    "SessionAttach": keys(SessionAttach.CodingKeys.self),
    "AgentStartParams": keys(AgentStartParams.CodingKeys.self),
    "AgentDeleteParams": keys(AgentDeleteParams.CodingKeys.self),
    "AgentParams": keys(AgentParams.CodingKeys.self),
    "Agent": keys(Agent.CodingKeys.self),
    "AgentStatus": keys(AgentStatus.CodingKeys.self),
    "Wakeup": keys(Wakeup.CodingKeys.self),
    "AgentList": keys(AgentList.CodingKeys.self),
    "AgentSendParams": keys(AgentSendParams.CodingKeys.self),
    "AgentSent": keys(AgentSent.CodingKeys.self),
    "AgentCheckParams": keys(AgentCheckParams.CodingKeys.self),
    "AgentCheck": keys(AgentCheck.CodingKeys.self),
    "AgentOutputParams": keys(AgentOutputParams.CodingKeys.self),
    "AgentOutput": keys(AgentOutput.CodingKeys.self),
    "AgentMessagesParams": keys(AgentMessagesParams.CodingKeys.self),
    "Message": keys(Message.CodingKeys.self),
    "MessageList": keys(MessageList.CodingKeys.self),
    "ToolboxStatus": keys(ToolboxStatus.CodingKeys.self),
    "ToolboxRun": keys(ToolboxRun.CodingKeys.self),
    "ToolboxLogins": keys(ToolboxLogins.CodingKeys.self),
    "Login": keys(Login.CodingKeys.self),
    "DoctorReport": keys(DoctorReport.CodingKeys.self),
    "DoctorCheck": keys(DoctorCheck.CodingKeys.self),
    "DoctorFixParams": keys(DoctorFixParams.CodingKeys.self),
    "ServiceInfo": keys(ServiceInfo.CodingKeys.self),
    "ServiceList": keys(ServiceList.CodingKeys.self),
    "ServiceParams": keys(ServiceParams.CodingKeys.self),
    "ServiceAuthorization": keys(ServiceAuthorization.CodingKeys.self),
    "ServiceCompleteParams": keys(ServiceCompleteParams.CodingKeys.self),
    "ServiceKeyParams": keys(ServiceKeyParams.CodingKeys.self),
    "ServiceSetParams": keys(ServiceSetParams.CodingKeys.self),
    "ServiceAuditEntry": keys(ServiceAuditEntry.CodingKeys.self),
    "ServiceAuditList": keys(ServiceAuditList.CodingKeys.self),
]

/// The index's enums, from the Swift ones.
let swiftEnums: [String: [String]] = [
    "Environment": AgentEnvironment.allCases.map(\.rawValue),
    "Model": ClaudeModel.allCases.map(\.rawValue),
    "Reasoning": ReasoningLevel.allCases.map(\.rawValue),
    "Mode": PermissionMode.allCases.map(\.rawValue),
    "ProjectSource": ProjectSource.allCases.map(\.rawValue),
    "GhState": GhState.allCases.map(\.rawValue),
    "EventKind": EventKind.allCases.map(\.rawValue),
    "HostLink": HostLink.allCases.map(\.rawValue),
    "TunnelState": TunnelState.allCases.map(\.rawValue),
    "SessionState": SessionState.allCases.map(\.rawValue),
    "AgentState": AgentState.allCases.map(\.rawValue),
    "Activity": AgentActivity.allCases.map(\.rawValue),
    "Certainty": Certainty.allCases.map(\.rawValue),
    "WakeupKind": WakeupKind.allCases.map(\.rawValue),
    "StopReason": StopReason.allCases.map(\.rawValue),
    "ToolboxState": ToolboxState.allCases.map(\.rawValue),
    "ToolboxAction": ToolboxAction.allCases.map(\.rawValue),
    "LoginTool": LoginTool.allCases.map(\.rawValue),
    "CheckId": CheckId.allCases.map(\.rawValue),
    "CheckState": CheckState.allCases.map(\.rawValue),
    "ServiceState": ServiceState.allCases.map(\.rawValue),
    "ServiceAuth": ServiceAuth.allCases.map(\.rawValue),
    "MessageRole": MessageRole.allCases.map(\.rawValue),
]

/// A method's Swift types, and a typed round trip of an example of it.
struct MethodCheck {
    var params: String
    var result: String
    var roundTrip: (_ params: Any, _ result: Any, _ response: Any, _ what: String) throws -> Void
}

func check<M: RPCMethod>(_: M.Type) -> (String, MethodCheck) {
    let params = String(describing: M.Params.self)
    let result = String(describing: M.Result.self)
    let check = MethodCheck(params: params, result: result) { p, r, response, what in
        try roundTrip(M.Params.self, params, p, "\(what): params")
        try roundTrip(M.Result.self, result, r, "\(what): result")
        try roundTrip(RPCResponse<M.Result>.self, "Response", response, what)
    }
    return (M.name, check)
}

/// Every method's Swift types, by method name.
func methodChecks() -> [String: MethodCheck] {
    Dictionary(uniqueKeysWithValues: [
        check(Hello.self),
        check(HostsList.self),
        check(HostsAdd.self),
        check(HostsUpgrade.self),
        check(HostsRemove.self),
        check(HostsExternal.self),
        check(TunnelPrepare.self),
        check(ProjectsList.self),
        check(ProjectsAdd.self),
        check(ProjectsRemove.self),
        check(ProfilesList.self),
        check(ProfilesCreate.self),
        check(ProfilesUpdate.self),
        check(ProfilesDelete.self),
        check(LimitsGet.self),
        check(LimitsSet.self),
        check(McpGet.self),
        check(McpSet.self),
        check(KeysList.self),
        check(KeysCreate.self),
        check(KeysRevoke.self),
        check(KeysVerify.self),
        check(KeysSync.self),
        check(GhStatusGet.self),
        check(EventsSubscribe.self),
        check(SessionsList.self),
        check(SessionsAttach.self),
        check(AgentsStart.self),
        check(AgentsList.self),
        check(AgentsDelete.self),
        check(AgentsInterrupt.self),
        check(AgentsSend.self),
        check(AgentsCheck.self),
        check(AgentsOutput.self),
        check(AgentsMessages.self),
        check(ToolboxStatusGet.self),
        check(ToolboxOpen.self),
        check(ToolboxUpdate.self),
        check(ToolboxRebuild.self),
        check(ToolboxLoginsGet.self),
        check(ToolboxClose.self),
        check(DoctorCheckGet.self),
        check(DoctorFix.self),
        check(ServicesList.self),
        check(ServicesConnect.self),
        check(ServicesComplete.self),
        check(ServicesConnectKey.self),
        check(ServicesDisconnect.self),
        check(ServicesSet.self),
        check(ServicesAudit.self),
    ])
}

/// Checks a notification vibed sends: an `events.event` with an Event.
func checkNotification(_ object: Any, _ what: String) throws {
    let typed = try JSONDecoder().decode(RPCRequest<DaemonEvent>.self, from: jsonData(object))
    #expect(typed.id == nil, "\(what): a notification has no id")
    #expect(typed.method == EventNotice.name, "\(what): unknown notification \(typed.method)")
    try roundTrip(RPCRequest<DaemonEvent>.self, "Request", object, what)
    let params = try #require((object as? [String: Any])?["params"], "\(what): params")
    try roundTrip(DaemonEvent.self, "Event", params, "\(what): params")
}

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
        let checks = methodChecks()
        #expect(Set(checks.keys) == Set(VibeRPC.methods))
        for method in methods {
            let name = method["name"] ?? "?"
            guard let check = checks[name] else {
                Issue.record("\(name) has no Swift types; add them")
                continue
            }
            #expect(method["params"] == check.params, "\(name): params")
            #expect(method["result"] == check.result, "\(name): result")
        }

        let notifications = try #require(index["notifications"] as? [[String: String]])
        #expect(notifications.map { $0["name"] } == VibeRPC.notifications)
        #expect(notifications.first?["params"] == "Event")

        let enums = try #require(index["enums"] as? [String: [String]])
        #expect(enums == swiftEnums)

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
            if let notification = example.notification {
                try checkNotification(notification, example.name)
                continue
            }
            for (i, event) in example.events.enumerated() {
                try checkNotification(event, "\(example.name): event \(i)")
            }
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
                    if let check = methodChecks()[request.method] {
                        try check.roundTrip(params, result, response, what)
                    } else {
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

    /// The menu's event stream: subscribed, then an event a call causes.
    @Test(.timeLimit(.minutes(1))) func eventsFromALiveDaemon() async throws {
        var subscribed = false
        for try await message in client.events(client: "VibeRPCTests events") {
            switch message {
            case .subscribed:
                subscribed = true
                // The default, so the other live test still finds it.
                var params = LimitsSetParams()
                params.maxAwakeAgents = 16
                _ = try client.call(LimitsSet.self, params)
            case .event(let event):
                #expect(subscribed)
                if event.kind == .limitsChanged { return }
            }
        }
        Issue.record("the event stream ended before the limits.changed event")
    }

    @Test func helloAgainstALiveDaemon() throws {
        let hello = try client.hello(client: "VibeRPCTests")
        #expect(hello.daemon == "vibed")
        #expect(hello.protocolVersion == VibeRPC.protocolVersion)
        #expect(hello.os == "macos")
        #expect(hello.pid > 0)

        // The registry, through the typed methods. check-full.sh starts this
        // vibed with an empty home, so everything has its default.
        let hosts = try client.call(HostsList.self, NoParams())
        #expect(hosts.hosts.map(\.name) == [VibeRPC.localHost])
        #expect(hosts.hosts.first?.os == "macos")
        let limits = try client.call(LimitsGet.self, HostParams())
        #expect(limits.maxAwakeAgents == 16)
        #expect(limits.snapshotDisk == 20 << 30)
        let mcp = try client.call(McpGet.self, HostParams())
        #expect(mcp.url == "http://127.0.0.1:\(mcp.port)/mcp")
        #expect(!mcp.tailscale)
        #expect(mcp.tailscaleUrls.isEmpty)
        let projects = try client.call(ProjectsList.self, HostParams(host: VibeRPC.localHost))
        #expect(projects.projects.isEmpty)
        do {
            _ = try client.call(ProfilesList.self, ProjectParams(project: "nope"))
            Issue.record("vibed listed the profiles of a project that doesn't exist")
        } catch VibedClientError.rpc(let rpc) {
            #expect(rpc.code == RPCErrorCode.notFound.rawValue)
        }

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
