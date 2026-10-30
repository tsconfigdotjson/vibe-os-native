import Foundation

// The Swift side of the vibed JSON-RPC 2.0 contract. protocol/vibed-rpc.json
// and protocol/examples/ are the source of truth; VibeRPCTests checks these
// types against them, as the Rust contract tests check vibe_core::rpc.

public enum VibeRPC {
    /// Sent in `daemon.hello`; vibed refuses any other.
    public static let protocolVersion = 1
    public static let jsonrpc = "2.0"
    /// The longest message, newline excluded, either side accepts.
    public static let maxMessageBytes = 16 * 1024 * 1024
    /// Overrides the socket path.
    public static let socketEnv = "VIBE_SOCKET"
    /// The socket path relative to the home directory on macOS.
    public static let macOSSocket = "Library/Application Support/VibeOS/run/vibed.sock"
    /// Every method in the contract.
    public static let methods = [
        Hello.name,
        HostsList.name,
        ProjectsList.name,
        ProjectsAdd.name,
        ProjectsRemove.name,
        ProfilesList.name,
        ProfilesCreate.name,
        ProfilesUpdate.name,
        ProfilesDelete.name,
        LimitsGet.name,
        LimitsSet.name,
        KeysList.name,
        KeysCreate.name,
        KeysRevoke.name,
        KeysVerify.name,
        GhStatusGet.name,
        EventsSubscribe.name,
    ]
    /// Every notification vibed sends.
    public static let notifications = [EventNotice.name]
    /// What every vibed calls itself.
    public static let localHost = "local"
}

/// A request id. `.null` is only sent by vibed, for a request whose id it
/// couldn't read.
public enum RPCID: Codable, Hashable, Sendable {
    case number(Int64)
    case string(String)
    case null

    public init(from decoder: any Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let n = try? c.decode(Int64.self) {
            self = .number(n)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else {
            throw DecodingError.dataCorruptedError(
                in: c, debugDescription: "an id is an integer, a string or null")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .null: try c.encodeNil()
        }
    }
}

/// Any JSON value, for error data and untyped params.
public enum JSONValue: Codable, Hashable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: any Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let n = try? c.decode(Double.self) {
            self = .number(n)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let a = try? c.decode([JSONValue].self) {
            self = .array(a)
        } else {
            self = .object(try c.decode([String: JSONValue].self))
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }
}

/// A request, or a notification when `id` is nil.
public struct RPCRequest<Params: Codable & Sendable>: Codable, Sendable {
    public var id: RPCID?
    public var method: String
    public var params: Params?

    public init(id: RPCID?, method: String, params: Params?) {
        self.id = id
        self.method = method
        self.params = params
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case jsonrpc, id, method, params
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        guard try c.decode(String.self, forKey: .jsonrpc) == VibeRPC.jsonrpc else {
            throw DecodingError.dataCorruptedError(
                forKey: .jsonrpc, in: c, debugDescription: "jsonrpc must be \"2.0\"")
        }
        // An absent id makes a notification; a null one doesn't.
        if c.contains(.id) {
            id = try c.decode(RPCID.self, forKey: .id)
        } else {
            id = nil
        }
        method = try c.decode(String.self, forKey: .method)
        params = try c.decodeIfPresent(Params.self, forKey: .params)
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(VibeRPC.jsonrpc, forKey: .jsonrpc)
        try c.encodeIfPresent(id, forKey: .id)
        try c.encode(method, forKey: .method)
        try c.encodeIfPresent(params, forKey: .params)
    }
}

/// A response: exactly one of a result or an error.
public struct RPCResponse<Value: Codable & Sendable>: Codable, Sendable {
    public enum Outcome: Sendable {
        case result(Value)
        case error(RPCError)
    }

    public var id: RPCID
    public var outcome: Outcome

    public init(id: RPCID, outcome: Outcome) {
        self.id = id
        self.outcome = outcome
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case jsonrpc, id, result, error
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        guard try c.decode(String.self, forKey: .jsonrpc) == VibeRPC.jsonrpc else {
            throw DecodingError.dataCorruptedError(
                forKey: .jsonrpc, in: c, debugDescription: "jsonrpc must be \"2.0\"")
        }
        id = try c.decode(RPCID.self, forKey: .id)
        switch (c.contains(.result), c.contains(.error)) {
        case (true, false):
            outcome = .result(try c.decode(Value.self, forKey: .result))
        case (false, true):
            outcome = .error(try c.decode(RPCError.self, forKey: .error))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .result, in: c,
                debugDescription: "a response has exactly one of result and error")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(VibeRPC.jsonrpc, forKey: .jsonrpc)
        try c.encode(id, forKey: .id)
        switch outcome {
        case .result(let value): try c.encode(value, forKey: .result)
        case .error(let error): try c.encode(error, forKey: .error)
        }
    }
}

/// The error object of a response.
public struct RPCError: Codable, Hashable, Sendable, Error, CustomStringConvertible {
    public var code: Int
    public var message: String
    public var data: JSONValue?

    public init(code: Int, message: String, data: JSONValue? = nil) {
        self.code = code
        self.message = message
        self.data = data
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case code, message, data
    }

    public var description: String { "\(message) (\(code))" }
}

/// The error codes in the contract, in the order protocol/vibed-rpc.json
/// lists them.
public enum RPCErrorCode: Int, CaseIterable, Sendable {
    case parseError = -32700
    case invalidRequest = -32600
    case methodNotFound = -32601
    case invalidParams = -32602
    case internalError = -32603
    /// The client's protocol isn't vibed's; the data is a
    /// `ProtocolMismatchData`.
    case protocolMismatch = -32000
    /// No host, project, profile or key by that name.
    case notFound = -32001
    /// The name is taken, or the thing is in use.
    case conflict = -32002
    /// host.toml can't be read; the message says where and why.
    case hostConfigInvalid = -32003
    /// A command vibed ran, such as `gh repo clone`, failed.
    case commandFailed = -32004

    /// The name protocol/vibed-rpc.json uses.
    public var name: String {
        switch self {
        case .parseError: "parse_error"
        case .invalidRequest: "invalid_request"
        case .methodNotFound: "method_not_found"
        case .invalidParams: "invalid_params"
        case .internalError: "internal_error"
        case .protocolMismatch: "protocol_mismatch"
        case .notFound: "not_found"
        case .conflict: "conflict"
        case .hostConfigInvalid: "host_config_invalid"
        case .commandFailed: "command_failed"
        }
    }
}

/// A method in the contract, with its param and result types.
public protocol RPCMethod {
    associatedtype Params: Codable & Sendable
    associatedtype Result: Codable & Sendable
    static var name: String { get }
}

/// `daemon.hello`: the handshake. It names the client and checks both sides
/// speak the same protocol version.
public enum Hello: RPCMethod {
    public typealias Params = HelloParams
    public typealias Result = HelloResult
    public static let name = "daemon.hello"
}

public struct HelloParams: Codable, Hashable, Sendable {
    /// Who is calling, for vibed's log.
    public var client: String
    public var protocolVersion: Int

    public init(client: String, protocolVersion: Int = VibeRPC.protocolVersion) {
        self.client = client
        self.protocolVersion = protocolVersion
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case client
        case protocolVersion = "protocol"
    }
}

public struct HelloResult: Codable, Hashable, Sendable {
    /// Always "vibed".
    public var daemon: String
    public var version: String
    public var protocolVersion: Int
    /// "macos" or "linux".
    public var os: String
    /// "aarch64" or "x86_64".
    public var arch: String
    public var pid: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case daemon, version
        case protocolVersion = "protocol"
        case os, arch, pid
    }
}

/// The data of a `protocolMismatch` error.
public struct ProtocolMismatchData: Codable, Hashable, Sendable {
    /// vibed's protocol version.
    public var protocolVersion: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case protocolVersion = "protocol"
    }
}

/// A notification vibed sends, with its param type.
public protocol RPCNotification {
    associatedtype Params: Codable & Sendable
    static var name: String { get }
}

// MARK: - Names

// Swift names differ from the index's where those would clash with
// SwiftUI's `Environment`, Foundation's `Host` and the like; ContractTests
// maps them.

/// Where an agent runs (index: Environment).
public enum AgentEnvironment: String, Codable, CaseIterable, Hashable, Sendable {
    case linux, macos
}

/// Claude's `--model` (index: Model).
public enum ClaudeModel: String, Codable, CaseIterable, Hashable, Sendable {
    case opus, sonnet
}

/// Claude's `--effort` (index: Reasoning).
public enum ReasoningLevel: String, Codable, CaseIterable, Hashable, Sendable {
    case low, medium, high, xhigh, max
}

/// `auto`, or `bypass` for `--dangerously-skip-permissions` (index: Mode).
public enum PermissionMode: String, Codable, CaseIterable, Hashable, Sendable {
    case auto, bypass
}

public enum ProjectSource: String, Codable, CaseIterable, Hashable, Sendable {
    case github, path
}

public enum GhState: String, Codable, CaseIterable, Hashable, Sendable {
    case unknown
    case loggedIn = "logged_in"
    case loggedOut = "logged_out"
    case notInstalled = "not_installed"
    case error
}

public enum EventKind: String, Codable, CaseIterable, Hashable, Sendable {
    case projectAdded = "project.added"
    case projectRemoved = "project.removed"
    case profilesChanged = "profiles.changed"
    case limitsChanged = "limits.changed"
    case keysChanged = "keys.changed"
    case ghStatus = "gh.status"
    case agentState = "agent.state"
    case agentLimit = "agent.limit"
    /// The subscriber fell behind and missed events: read everything again.
    case lagged
}

// MARK: - Methods

/// `hosts.list`: every host this vibed knows; for now, itself.
public enum HostsList: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = HostList
    public static let name = "hosts.list"
}

/// `projects.list`: a host's projects, with their profiles.
public enum ProjectsList: RPCMethod {
    public typealias Params = HostParams
    public typealias Result = ProjectList
    public static let name = "projects.list"
}

/// `projects.add`: clones a GitHub repo, or registers a local one.
public enum ProjectsAdd: RPCMethod {
    public typealias Params = ProjectAddParams
    public typealias Result = Project
    public static let name = "projects.add"
}

/// `projects.remove`: forgets a project; its files stay on disk.
public enum ProjectsRemove: RPCMethod {
    public typealias Params = ProjectParams
    public typealias Result = Project
    public static let name = "projects.remove"
}

public enum ProfilesList: RPCMethod {
    public typealias Params = ProjectParams
    public typealias Result = ProfileList
    public static let name = "profiles.list"
}

public enum ProfilesCreate: RPCMethod {
    public typealias Params = ProfileCreateParams
    public typealias Result = Profile
    public static let name = "profiles.create"
}

public enum ProfilesUpdate: RPCMethod {
    public typealias Params = ProfileUpdateParams
    public typealias Result = Profile
    public static let name = "profiles.update"
}

public enum ProfilesDelete: RPCMethod {
    public typealias Params = ProfileParams
    public typealias Result = Profile
    public static let name = "profiles.delete"
}

public enum LimitsGet: RPCMethod {
    public typealias Params = HostParams
    public typealias Result = HostLimits
    public static let name = "limits.get"
}

public enum LimitsSet: RPCMethod {
    public typealias Params = LimitsSetParams
    public typealias Result = HostLimits
    public static let name = "limits.set"
}

public enum KeysList: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = KeyList
    public static let name = "keys.list"
}

/// `keys.create`: the secret is in this response only.
public enum KeysCreate: RPCMethod {
    public typealias Params = KeyCreateParams
    public typealias Result = KeyCreated
    public static let name = "keys.create"
}

public enum KeysRevoke: RPCMethod {
    public typealias Params = KeyParams
    public typealias Result = ApiKey
    public static let name = "keys.revoke"
}

public enum KeysVerify: RPCMethod {
    public typealias Params = KeyVerifyParams
    public typealias Result = ApiKey
    public static let name = "keys.verify"
}

public enum GhStatusGet: RPCMethod {
    public typealias Params = GhStatusParams
    public typealias Result = GhStatus
    public static let name = "gh.status"
}

/// `events.subscribe`: the connection then also gets `events.event`
/// notifications, so use a connection of its own for it.
public enum EventsSubscribe: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = Subscribed
    public static let name = "events.subscribe"
}

/// `events.event`: one event, sent to subscribed connections.
public enum EventNotice: RPCNotification {
    public typealias Params = DaemonEvent
    public static let name = "events.event"
}

// MARK: - Types

/// The params of a method that takes none: `{}`.
public struct NoParams: Codable, Hashable, Sendable {
    public init() {}

    /// No keys. A struct, because an enum without cases can't have a raw
    /// type.
    public struct CodingKeys: CodingKey, CaseIterable, Sendable {
        public static var allCases: [CodingKeys] { [] }
        public var stringValue: String { "" }
        public var intValue: Int? { nil }
        public init?(stringValue: String) { return nil }
        public init?(intValue: Int) { return nil }
    }

    public init(from decoder: any Decoder) throws {
        _ = try decoder.container(keyedBy: CodingKeys.self)
    }

    public func encode(to encoder: any Encoder) throws {
        _ = encoder.container(keyedBy: CodingKeys.self)
    }
}

/// For methods that act on one host; nil means this one.
public struct HostParams: Codable, Hashable, Sendable {
    public var host: String?

    public init(host: String? = nil) {
        self.host = host
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host
    }
}

public struct HostInfo: Codable, Hashable, Sendable {
    /// `local` for vibed's own host.
    public var name: String
    /// "macos" or "linux".
    public var os: String
    /// The hostname, for people.
    public var label: String

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, os, label
    }
}

public struct HostList: Codable, Hashable, Sendable {
    public var hosts: [HostInfo]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case hosts
    }
}

public struct Profile: Codable, Hashable, Sendable {
    public var name: String
    public var environment: AgentEnvironment
    public var model: ClaudeModel
    public var reasoning: ReasoningLevel
    public var mode: PermissionMode
    /// Bytes.
    public var memory: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, environment, model, reasoning, mode, memory
    }
}

public struct ProfileList: Codable, Hashable, Sendable {
    public var profiles: [Profile]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case profiles
    }
}

public struct Project: Codable, Hashable, Sendable {
    public var host: String
    public var name: String
    public var source: ProjectSource
    /// `owner/name`, for a project cloned from GitHub.
    public var repo: String?
    public var path: String
    public var addedAt: String
    public var profiles: [Profile]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, name, source, repo, path
        case addedAt = "added_at"
        case profiles
    }
}

public struct ProjectList: Codable, Hashable, Sendable {
    public var projects: [Project]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case projects
    }
}

/// Exactly one of `repo` and `path`.
public struct ProjectAddParams: Codable, Hashable, Sendable {
    public var host: String?
    public var repo: String?
    public var path: String?
    public var name: String?

    public init(host: String? = nil, repo: String? = nil, path: String? = nil, name: String? = nil) {
        self.host = host
        self.repo = repo
        self.path = path
        self.name = name
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, repo, path, name
    }
}

public struct ProjectParams: Codable, Hashable, Sendable {
    public var host: String?
    public var project: String

    public init(host: String? = nil, project: String) {
        self.host = host
        self.project = project
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project
    }
}

/// Fields left nil take the defaults: linux, opus, high, auto, 4 GB.
public struct ProfileCreateParams: Codable, Hashable, Sendable {
    public var host: String?
    public var project: String
    public var name: String
    public var environment: AgentEnvironment?
    public var model: ClaudeModel?
    public var reasoning: ReasoningLevel?
    public var mode: PermissionMode?
    public var memory: Int?

    public init(
        host: String? = nil, project: String, name: String, environment: AgentEnvironment? = nil,
        model: ClaudeModel? = nil, reasoning: ReasoningLevel? = nil, mode: PermissionMode? = nil,
        memory: Int? = nil
    ) {
        self.host = host
        self.project = project
        self.name = name
        self.environment = environment
        self.model = model
        self.reasoning = reasoning
        self.mode = mode
        self.memory = memory
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project, name, environment, model, reasoning, mode, memory
    }
}

/// `profile` names the profile to change; `name` renames it.
public struct ProfileUpdateParams: Codable, Hashable, Sendable {
    public var host: String?
    public var project: String
    public var profile: String
    public var name: String?
    public var environment: AgentEnvironment?
    public var model: ClaudeModel?
    public var reasoning: ReasoningLevel?
    public var mode: PermissionMode?
    public var memory: Int?

    public init(
        host: String? = nil, project: String, profile: String, name: String? = nil,
        environment: AgentEnvironment? = nil, model: ClaudeModel? = nil,
        reasoning: ReasoningLevel? = nil, mode: PermissionMode? = nil, memory: Int? = nil
    ) {
        self.host = host
        self.project = project
        self.profile = profile
        self.name = name
        self.environment = environment
        self.model = model
        self.reasoning = reasoning
        self.mode = mode
        self.memory = memory
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project, profile, name, environment, model, reasoning, mode, memory
    }
}

public struct ProfileParams: Codable, Hashable, Sendable {
    public var host: String?
    public var project: String
    public var profile: String

    public init(host: String? = nil, project: String, profile: String) {
        self.host = host
        self.project = project
        self.profile = profile
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project, profile
    }
}

/// Host-wide limits. Sizes are bytes; `idleDebounce` is seconds.
public struct HostLimits: Codable, Hashable, Sendable {
    public var reserveMemory: Int
    public var reserveCores: Int
    public var maxAwakeAgents: Int
    public var cpuCores: Int
    public var maxProcesses: Int
    public var diskSoft: Int
    public var diskHard: Int
    public var vmDisk: Int
    public var idleDebounce: Int
    public var snapshotDisk: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case reserveMemory = "reserve_memory"
        case reserveCores = "reserve_cores"
        case maxAwakeAgents = "max_awake_agents"
        case cpuCores = "cpu_cores"
        case maxProcesses = "max_processes"
        case diskSoft = "disk_soft"
        case diskHard = "disk_hard"
        case vmDisk = "vm_disk"
        case idleDebounce = "idle_debounce"
        case snapshotDisk = "snapshot_disk"
    }
}

/// The settings to change; nil ones keep their values.
public struct LimitsSetParams: Codable, Hashable, Sendable {
    public var host: String?
    public var reserveMemory: Int?
    public var reserveCores: Int?
    public var maxAwakeAgents: Int?
    public var cpuCores: Int?
    public var maxProcesses: Int?
    public var diskSoft: Int?
    public var diskHard: Int?
    public var vmDisk: Int?
    public var idleDebounce: Int?
    public var snapshotDisk: Int?

    public init(host: String? = nil) {
        self.host = host
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host
        case reserveMemory = "reserve_memory"
        case reserveCores = "reserve_cores"
        case maxAwakeAgents = "max_awake_agents"
        case cpuCores = "cpu_cores"
        case maxProcesses = "max_processes"
        case diskSoft = "disk_soft"
        case diskHard = "disk_hard"
        case vmDisk = "vm_disk"
        case idleDebounce = "idle_debounce"
        case snapshotDisk = "snapshot_disk"
    }
}

/// An API key as it's shown: never the key itself.
public struct ApiKey: Codable, Hashable, Sendable {
    public var name: String
    /// The start of the key, like `vk_abcdefgh`.
    public var prefix: String
    public var createdAt: String
    public var lastUsedAt: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, prefix
        case createdAt = "created_at"
        case lastUsedAt = "last_used_at"
    }
}

public struct KeyList: Codable, Hashable, Sendable {
    public var keys: [ApiKey]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case keys
    }
}

public struct KeyCreateParams: Codable, Hashable, Sendable {
    public var name: String

    public init(name: String) {
        self.name = name
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name
    }
}

public struct KeyCreated: Codable, Hashable, Sendable {
    public var key: ApiKey
    /// The key itself, `vk_…`. It can't be shown again.
    public var secret: String

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case key, secret
    }
}

public struct KeyParams: Codable, Hashable, Sendable {
    public var name: String

    public init(name: String) {
        self.name = name
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name
    }
}

public struct KeyVerifyParams: Codable, Hashable, Sendable {
    public var secret: String

    public init(secret: String) {
        self.secret = secret
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case secret
    }
}

public struct GhStatusParams: Codable, Hashable, Sendable {
    /// Run `gh auth status` now instead of returning the last result.
    public var refresh: Bool?

    public init(refresh: Bool? = nil) {
        self.refresh = refresh
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case refresh
    }
}

public struct GhStatus: Codable, Hashable, Sendable {
    public var state: GhState
    public var account: String?
    public var detail: String?
    public var checkedAt: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case state, account, detail
        case checkedAt = "checked_at"
    }
}

public struct Subscribed: Codable, Hashable, Sendable {
    /// The seq of the last event before the subscription.
    public var seq: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case seq
    }
}

/// Something that happened on a host (index: Event).
public struct DaemonEvent: Codable, Hashable, Sendable {
    public var seq: Int
    public var at: String
    public var host: String
    public var kind: EventKind
    public var project: String?
    /// The agent's ref.
    public var ref: String?
    /// An agent's new state, or a `GhState` for `gh.status`.
    public var state: String?
    /// Which limit an `agent.limit` is about.
    public var limit: String?
    public var message: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case seq, at, host, kind, project, ref, state, limit, message
    }
}
