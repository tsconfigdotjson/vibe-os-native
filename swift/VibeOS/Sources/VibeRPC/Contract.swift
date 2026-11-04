import Foundation

// The Swift side of the vibed JSON-RPC 2.0 contract. protocol/vibed-rpc.json
// and protocol/examples/ are the source of truth; VibeRPCTests checks these
// types against them, as the Rust contract tests check vibe_core::rpc.

public enum VibeRPC {
    /// Sent in `daemon.hello`; vibed refuses any other.
    public static let protocolVersion = 1
    public static let jsonrpc = "2.0"
    /// The first Linux base image whose guest serves shared MCP services
    /// (`vibe_env::images::SERVICES_BASE`). A template built on an older one
    /// keeps booting that one until it's rebuilt.
    public static let sharedServicesBase = 3
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
        HostsAdd.name,
        HostsUpgrade.name,
        HostsRemove.name,
        HostsExternal.name,
        TunnelPrepare.name,
        ProjectsList.name,
        ProjectsAdd.name,
        ProjectsRemove.name,
        ProfilesList.name,
        ProfilesCreate.name,
        ProfilesUpdate.name,
        ProfilesDelete.name,
        LimitsGet.name,
        LimitsSet.name,
        McpGet.name,
        McpSet.name,
        KeysList.name,
        KeysCreate.name,
        KeysRevoke.name,
        KeysVerify.name,
        KeysSync.name,
        GhStatusGet.name,
        EventsSubscribe.name,
        SessionsList.name,
        SessionsAttach.name,
        AgentsStart.name,
        AgentsList.name,
        AgentsDelete.name,
        AgentsInterrupt.name,
        AgentsSend.name,
        AgentsCheck.name,
        AgentsOutput.name,
        AgentsMessages.name,
        ToolboxStatusGet.name,
        ToolboxOpen.name,
        ToolboxUpdate.name,
        ToolboxRebuild.name,
        ToolboxLoginsGet.name,
        ToolboxClose.name,
        DoctorCheckGet.name,
        DoctorFix.name,
        ServicesList.name,
        ServicesConnect.name,
        ServicesComplete.name,
        ServicesConnectKey.name,
        ServicesDisconnect.name,
        ServicesSet.name,
        ServicesAudit.name,
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
    /// The call is for a Linux host whose link is down: its agents are
    /// unavailable until it's back.
    case hostUnavailable = -32005
    /// A call that came through the external address's tunnel carried no
    /// API key this Mac knows. Only the tunnel's socket says it.
    case unauthorized = -32006

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
        case .hostUnavailable: "host_unavailable"
        case .unauthorized: "unauthorized"
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
    case agentAdded = "agent.added"
    case agentDeleted = "agent.deleted"
    case agentState = "agent.state"
    /// What an agent's transcript says changed; `status` has it.
    case agentStatus = "agent.status"
    case agentLimit = "agent.limit"
    /// The subscriber fell behind and missed events: read everything again.
    case lagged
    /// The toolbox opened, or saved, updated or rebuilt the template, or
    /// failed to; `message` says which.
    case toolboxChanged = "toolbox.changed"
    /// Where MCP is served changed: `mcp.set` turned the Tailscale address
    /// on or off, or MCP started or stopped being served (`HostInfo.mcp`).
    case mcpChanged = "mcp.changed"
    /// A Linux host was added, under `host`'s name.
    case hostAdded = "host.added"
    /// A Linux host was forgotten; it runs on there, on its own.
    case hostRemoved = "host.removed"
    /// A Linux host's link came up or went down, or what it reports
    /// changed; `state` is its `HostLink`, `message` why it's down.
    case hostLink = "host.link"
    /// A shared service changed on `host`: `state` is its `ServiceState`,
    /// `message` its name.
    case serviceChanged = "service.changed"
}

/// Where a shared service stands, on the host that holds its login.
public enum ServiceState: String, Codable, CaseIterable, Hashable, Sendable {
    case disconnected, connected, refreshing
    /// A refresh was refused: only connecting again helps.
    case needsReconnect = "needs_reconnect"
    /// The service turned down a freshly refreshed token, or a key.
    case revoked
}

/// How a shared service was connected.
public enum ServiceAuth: String, Codable, CaseIterable, Hashable, Sendable {
    case oauth
    case apiKey = "api_key"
}

/// How vibed reaches a host: `local` for itself, else whether its link is up.
public enum HostLink: String, Codable, CaseIterable, Hashable, Sendable {
    case local, connected, down
}

/// The Mac's tunnel to the Linux host that's its external address: on the
/// Mac, the tunnel it keeps; in that host's own row, whether a Mac answers
/// through it.
public enum TunnelState: String, Codable, CaseIterable, Hashable, Sendable {
    /// ssh is starting, or the host hasn't come through it yet.
    case connecting
    /// The host reaches the Mac through it.
    case up
    /// It isn't up; `externalDetail` says why. It's tried again.
    case down
}

/// Whether a session's shim still holds its terminal.
public enum SessionState: String, Codable, CaseIterable, Hashable, Sendable {
    case running, exited
}

/// What an agent is doing: `question` is waiting on an answer, `asleep`
/// wakes on a send, `paused` hit a limit, `exited` resumes on a send.
public enum AgentState: String, Codable, CaseIterable, Hashable, Sendable {
    case busy, idle, question, asleep, paused, exited
}

/// What an agent's transcript says it's doing: agent-tail's finer activity,
/// which `AgentState` folds (index: Activity).
public enum AgentActivity: String, Codable, CaseIterable, Hashable, Sendable {
    case awaitingAnswer = "awaiting_answer"
    case awaitingApproval = "awaiting_approval"
    case runningTool = "running_tool"
    case thinking
    case backgroundWork = "background_work"
    case done, interrupted, errored, unknown
}

/// Whether the transcript says so, or it's a guess from timing.
public enum Certainty: String, Codable, CaseIterable, Hashable, Sendable {
    case firm, inferred
}

/// What armed a scheduled wakeup: a `/loop`, or a cron job.
public enum WakeupKind: String, Codable, CaseIterable, Hashable, Sendable {
    case loop, cron
}

/// Why a turn stopped beyond an error: `usageLimit` shows as "limited until …".
public enum StopReason: String, Codable, CaseIterable, Hashable, Sendable {
    case usageLimit = "usage_limit"
}

/// What the Linux toolbox is doing; `unavailable` on a host with none.
public enum ToolboxState: String, Codable, CaseIterable, Hashable, Sendable {
    case unavailable, closed, open, saving, updating, rebuilding
}

/// What a toolbox run was: a shell session, the `[update]`, or Rebuild template.
public enum ToolboxAction: String, Codable, CaseIterable, Hashable, Sendable {
    case shell, update, rebuild
}

/// What a login kept in the Linux home is for: `gh`, `gcloud`, git's
/// identity, or an SSH key.
public enum LoginTool: String, Codable, CaseIterable, Hashable, Sendable {
    case gh, gcloud, git, ssh
}

/// What `vibe doctor` checks, in the order it reports them: git; `gh`'s
/// login, and git using `gh` for GitHub; the `claude` macos agents run, and
/// its login; system sleep off; Tailscale (optional); and on a Mac, the
/// Linux images.
public enum CheckId: String, Codable, CaseIterable, Hashable, Sendable {
    case git, gh
    case ghGit = "gh_git"
    case claude
    case claudeLogin = "claude_login"
    case power, tailscale, linux
}

/// How a doctor check went: `pass`; `fail`, which needs the owner;
/// `optional`, missing but not needed; or `working`, under way.
public enum CheckState: String, Codable, CaseIterable, Hashable, Sendable {
    case pass, fail, optional, working
}

/// Who said a message of an agent's conversation.
public enum MessageRole: String, Codable, CaseIterable, Hashable, Sendable {
    case user, assistant
    /// A tool call, as one short line.
    case tool
}

// MARK: - Methods

/// `hosts.list`: every host this vibed knows: itself first, then the Linux
/// hosts added to it, each with its link.
public enum HostsList: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = HostList
    public static let name = "hosts.list"
}

/// `hosts.add`: Add Linux Host. Over SSH, installs or upgrades vibed on the
/// server, checks git, gh and claude there, and starts the link. It can take
/// minutes: call it with a long timeout.
public enum HostsAdd: RPCMethod {
    public typealias Params = HostAddParams
    public typealias Result = HostInstall
    public static let name = "hosts.add"
}

/// `hosts.upgrade`: installs this build's vibed on a host added already, if
/// it runs another, and checks its tools again.
public enum HostsUpgrade: RPCMethod {
    public typealias Params = HostNameParams
    public typealias Result = HostInstall
    public static let name = "hosts.upgrade"
}

/// `hosts.remove`: forgets a Linux host and ends its link; nothing on the
/// server changes.
public enum HostsRemove: RPCMethod {
    public typealias Params = HostNameParams
    public typealias Result = HostInfo
    public static let name = "hosts.remove"
}

/// `hosts.external`: Settings → Hosts → Use as External Address. The Linux
/// host serves `https://<domain>/mcp` with a certificate from Let's
/// Encrypt, and this Mac keeps an SSH tunnel to it. One host at most.
public enum HostsExternal: RPCMethod {
    public typealias Params = HostExternalParams
    public typealias Result = HostInfo
    public static let name = "hosts.external"
}

/// `tunnel.prepare`: on a Linux host, where the Mac's tunnel ends. Only
/// vibed calls it, over the link.
public enum TunnelPrepare: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = TunnelPrepared
    public static let name = "tunnel.prepare"
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

/// `projects.remove`: forgets a project. On a Linux host, with `delete`, its
/// clone is deleted too; the answer's `deleted` says whether it was.
public enum ProjectsRemove: RPCMethod {
    public typealias Params = ProjectRemoveParams
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

/// Where this host serves MCP, for Settings and its Copy setup command.
public enum McpGet: RPCMethod {
    public typealias Params = HostParams
    public typealias Result = McpSettings
    public static let name = "mcp.get"
}

/// Turns MCP on the Tailscale address on or off: written to `host.toml`
/// and applied at once.
public enum McpSet: RPCMethod {
    public typealias Params = McpSetParams
    public typealias Result = McpSettings
    public static let name = "mcp.set"
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

/// `keys.sync`: what a Mac's vibed sends its Linux hosts: every key it
/// has, by its hash. The app never calls it.
public enum KeysSync: RPCMethod {
    public typealias Params = KeySyncParams
    public typealias Result = KeyList
    public static let name = "keys.sync"
}

/// `gh.status`.
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

/// `sessions.list`: a host's sessions: running, and exited with a record.
public enum SessionsList: RPCMethod {
    public typealias Params = HostParams
    public typealias Result = SessionList
    public static let name = "sessions.list"
}

/// `sessions.attach`: where to attach to a running session. Terminal runs
/// `vibe attach <ref>`, which calls this itself; the menu uses `ttys` to
/// raise a tab that's already attached.
public enum SessionsAttach: RPCMethod {
    public typealias Params = SessionParams
    public typealias Result = SessionAttach
    public static let name = "sessions.attach"
}

/// `agents.start`: a worktree on a new branch, a compartment, the repo's
/// `.vibe/setup.sh`, and Claude on the prompt, in one call.
public enum AgentsStart: RPCMethod {
    public typealias Params = AgentStartParams
    public typealias Result = Agent
    public static let name = "agents.start"
}

/// `agents.list`: a host's agents, oldest first.
public enum AgentsList: RPCMethod {
    public typealias Params = HostParams
    public typealias Result = AgentList
    public static let name = "agents.list"
}

/// `agents.delete`: removes everything VibeOS made for the agent but its
/// branch. Refused on uncommitted or unpushed work unless `force`.
public enum AgentsDelete: RPCMethod {
    public typealias Params = AgentDeleteParams
    public typealias Result = Agent
    public static let name = "agents.delete"
}

/// `agents.interrupt`: presses Escape in the agent's terminal, which stops
/// Claude's turn. Refused unless its session runs; it never wakes one.
public enum AgentsInterrupt: RPCMethod {
    public typealias Params = AgentParams
    public typealias Result = Agent
    public static let name = "agents.interrupt"
}

/// `agents.send`: types text, or presses keys, in the agent's terminal, as
/// `vibe_send` does. The Mac reaches a Linux host's agents with it.
public enum AgentsSend: RPCMethod {
    public typealias Params = AgentSendParams
    public typealias Result = AgentSent
    public static let name = "agents.send"
}

/// `agents.check`: where the agent is, and whether it has answered its last
/// send. Never wakes it.
public enum AgentsCheck: RPCMethod {
    public typealias Params = AgentCheckParams
    public typealias Result = AgentCheck
    public static let name = "agents.check"
}

/// `agents.output`: its screen, and what scrolled off since `after`.
public enum AgentsOutput: RPCMethod {
    public typealias Params = AgentOutputParams
    public typealias Result = AgentOutput
    public static let name = "agents.output"
}

/// `agents.messages`: the last messages of its conversation.
public enum AgentsMessages: RPCMethod {
    public typealias Params = AgentMessagesParams
    public typealias Result = MessageList
    public static let name = "agents.messages"
}

/// `toolbox.status`: the versions new agents boot, and the last toolbox run.
public enum ToolboxStatusGet: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = ToolboxStatus
    public static let name = "toolbox.status"
}

/// `toolbox.open`: opens the Linux toolbox shell, and where to attach to it,
/// as `sessions.attach` says. Exiting the shell saves a new template and
/// Linux home version.
public enum ToolboxOpen: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = SessionAttach
    public static let name = "toolbox.open"
}

/// `toolbox.update`: runs toolbox.toml's `[update]` on the template now.
public enum ToolboxUpdate: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = ToolboxRun
    public static let name = "toolbox.update"
}

/// `toolbox.rebuild`: Rebuild template, on the newest base image.
public enum ToolboxRebuild: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = ToolboxRun
    public static let name = "toolbox.rebuild"
}

/// `toolbox.logins`: See logins. What the Linux home new agents boot holds,
/// as the toolbox found when it saved it, and whether linux agents have a
/// Claude login. Boots nothing.
public enum ToolboxLoginsGet: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = ToolboxLogins
    public static let name = "toolbox.logins"
}

/// `toolbox.close`: Save & Close. Hangs up on the open shell, as closing its
/// Terminal window does, saves as exiting it does, and gives the status
/// after. A closed toolbox is left as it is; one updating or rebuilding is
/// refused.
public enum ToolboxClose: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = ToolboxStatus
    public static let name = "toolbox.close"
}

/// `doctor.check`: `vibe doctor`, and the first run's checklist. Runs every
/// check now, as this host's vibed sees it. Changes nothing.
public enum DoctorCheckGet: RPCMethod {
    public typealias Params = NoParams
    public typealias Result = DoctorReport
    public static let name = "doctor.check"
}

/// `doctor.fix`: does what VibeOS can about one check (`gh_git` runs `gh
/// auth setup-git`; `linux` downloads now), then checks again. Any other is
/// refused, with what to do.
public enum DoctorFix: RPCMethod {
    public typealias Params = DoctorFixParams
    public typealias Result = DoctorReport
    public static let name = "doctor.fix"
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
    /// `local` for vibed's own host; another's link is `connected` or `down`.
    public var link: HostLink
    /// Whether it's the external address for outside MCP callers.
    public var external: Bool
    /// Whether it serves MCP now: on loopback, at least.
    public var mcp: Bool
    /// A Linux host's SSH destination, such as `me@vps.example.com`.
    public var address: String?
    /// The vibed it runs, when that's known.
    public var version: String?
    /// What a Linux host lacks of git, gh and claude, which its agents need.
    public var missing: [String]
    /// For a host whose link is down, why.
    public var detail: String?
    /// While it's the external address, where outside MCP callers reach it.
    public var url: String?
    /// While it's the external address, the Mac's tunnel to it.
    public var tunnel: TunnelState?
    /// While it's the external address and something keeps it from
    /// working, or holds it up, what, and how to fix it.
    public var externalDetail: String?
    /// What its memory is doing now: this host's own, and a Linux host's
    /// from its last ping while its link is up.
    public var memory: MemoryUse?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, os, label, link, external, mcp, address, version, missing, detail, url, tunnel
        case externalDetail = "external_detail"
        case memory
    }
}

/// A host's memory as its kernel measures it. A report, never a limit: each
/// agent's memory is a ceiling it's held to, and nothing is refused for
/// memory; the host swaps if its agents use more than it has.
public struct MemoryUse: Codable, Hashable, Sendable {
    /// Physical memory, in bytes.
    public var total: Int
    /// In use, in bytes: on macOS as Activity Monitor counts it.
    public var used: Int
    /// Swap, in bytes: 0 with none.
    public var swapTotal: Int
    public var swapUsed: Int
    /// While the host is short of memory, as its kernel says, why. A
    /// warning only.
    public var pressure: String?

    public init(total: Int, used: Int, swapTotal: Int = 0, swapUsed: Int = 0, pressure: String? = nil) {
        self.total = total
        self.used = used
        self.swapTotal = swapTotal
        self.swapUsed = swapUsed
        self.pressure = pressure
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case total, used
        case swapTotal = "swap_total"
        case swapUsed = "swap_used"
        case pressure
    }
}

/// What Use as External Address takes.
public struct HostExternalParams: Codable, Hashable, Sendable {
    /// A Linux host this Mac added.
    public var host: String?
    public var on: Bool
    /// The public DNS name it's reached at, pointing at the host. Needed to
    /// turn it on.
    public var domain: String?

    public init(host: String?, on: Bool, domain: String? = nil) {
        self.host = host
        self.on = on
        self.domain = domain
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, on, domain
    }
}

/// Where the Mac's tunnel ends on a Linux host.
public struct TunnelPrepared: Codable, Hashable, Sendable {
    /// The socket's absolute path on the host.
    public var socket: String

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case socket
    }
}

/// What Add Linux Host takes.
public struct HostAddParams: Codable, Hashable, Sendable {
    /// What the host goes by, such as `vps`; its agents read `vps:<ref>`.
    public var name: String
    /// Its SSH destination, as after `ssh`.
    public var address: String

    public init(name: String, address: String) {
        self.name = name
        self.address = address
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, address
    }
}

/// Names a Linux host.
public struct HostNameParams: Codable, Hashable, Sendable {
    public var host: String

    public init(host: String) {
        self.host = host
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host
    }
}

/// How an install over SSH went.
public struct HostInstall: Codable, Hashable, Sendable {
    public var host: HostInfo
    /// Whether it changed what the server runs.
    public var changed: Bool
    /// What the owner should know, in sentences.
    public var notes: [String]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, changed, notes
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
    /// `false` if its agents get no shared MCP services. Absent means on, as
    /// it is for every profile by default; `sharedServicesOn` says.
    public var sharedServices: Bool?

    public var sharedServicesOn: Bool { sharedServices ?? true }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, environment, model, reasoning, mode, memory
        case sharedServices = "shared_services"
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
    /// Only in `projects.remove`'s answer: its clone was deleted too.
    public var deleted: Bool?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, name, source, repo, path
        case addedAt = "added_at"
        case profiles, deleted
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

/// `projects.remove`. On a Linux host, `delete` deletes the clone VibeOS
/// made too; a removal there that would delete one is refused without it.
public struct ProjectRemoveParams: Codable, Hashable, Sendable {
    public var host: String?
    public var project: String
    public var delete: Bool?

    public init(host: String? = nil, project: String, delete: Bool? = nil) {
        self.host = host
        self.project = project
        self.delete = delete
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project, delete
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
    public var sharedServices: Bool?

    public init(
        host: String? = nil, project: String, name: String, environment: AgentEnvironment? = nil,
        model: ClaudeModel? = nil, reasoning: ReasoningLevel? = nil, mode: PermissionMode? = nil,
        memory: Int? = nil, sharedServices: Bool? = nil
    ) {
        self.host = host
        self.project = project
        self.name = name
        self.environment = environment
        self.model = model
        self.reasoning = reasoning
        self.mode = mode
        self.memory = memory
        self.sharedServices = sharedServices
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project, name, environment, model, reasoning, mode, memory
        case sharedServices = "shared_services"
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
    public var sharedServices: Bool?

    public init(
        host: String? = nil, project: String, profile: String, name: String? = nil,
        environment: AgentEnvironment? = nil, model: ClaudeModel? = nil,
        reasoning: ReasoningLevel? = nil, mode: PermissionMode? = nil, memory: Int? = nil, sharedServices: Bool? = nil
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
        self.sharedServices = sharedServices
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project, profile, name, environment, model, reasoning, mode, memory
        case sharedServices = "shared_services"
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
    /// Retired: memory is never kept back from agents. Always 0, for an
    /// older vibed; nothing reads it.
    public var reserveMemory: Int?
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

/// Where a host serves MCP.
public struct McpSettings: Codable, Hashable, Sendable {
    /// The port, on loopback and Tailscale alike (`mcp_port` in
    /// `host.toml`, which is set there only).
    public var port: Int
    /// Whether it's served on this host's Tailscale address too.
    public var tailscale: Bool
    /// Its loopback URL, `http://127.0.0.1:<port>/mcp`.
    public var url: String
    /// Its URL on each of this host's Tailscale addresses while `tailscale`
    /// is on; none while it's off, or the host has no Tailscale address.
    public var tailscaleUrls: [String]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case port
        case tailscale
        case url
        case tailscaleUrls = "tailscale_urls"
    }
}

/// The MCP settings to change; nil ones keep their values.
public struct McpSetParams: Codable, Hashable, Sendable {
    public var host: String?
    public var tailscale: Bool?

    public init(host: String? = nil, tailscale: Bool? = nil) {
        self.host = host
        self.tailscale = tailscale
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host
        case tailscale
    }
}

/// An API key as it's shown: never the key itself.
public struct ApiKey: Codable, Hashable, Sendable {
    public var name: String
    /// The start of the key, like `vk_abcdefgh`.
    public var prefix: String
    public var createdAt: String
    public var lastUsedAt: String?
    /// On a Linux host: made on the Mac it's linked to, and pushed there.
    public var pushed: Bool?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, prefix
        case createdAt = "created_at"
        case lastUsedAt = "last_used_at"
        case pushed
    }
}

/// A key as a Mac pushes it to its hosts: its hash, never the key.
public struct KeyHash: Codable, Hashable, Sendable {
    public var name: String
    public var prefix: String
    /// SHA-256 of the key, in lowercase hex.
    public var hash: String
    public var createdAt: String

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, prefix, hash
        case createdAt = "created_at"
    }
}

public struct KeySyncParams: Codable, Hashable, Sendable {
    public var keys: [KeyHash]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case keys
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
    /// An agent's new status, for `agent.status`.
    public var status: AgentStatus?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case seq, at, host, kind, project, ref, state, limit, message, status
    }
}

public struct SessionParams: Codable, Hashable, Sendable {
    public var host: String?
    public var ref: String

    public init(host: String? = nil, ref: String) {
        self.host = host
        self.ref = ref
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref
    }
}

public struct SessionList: Codable, Hashable, Sendable {
    public var sessions: [SessionInfo]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case sessions
    }
}

/// A session: the terminal an agent runs in, held by its shim.
public struct SessionInfo: Codable, Hashable, Sendable {
    public var host: String
    public var ref: String
    public var state: SessionState
    /// `<ref> · <profile>`, or the ref alone for a session with no agent.
    public var title: String
    /// How many `vibe attach` clients are attached.
    public var clients: Int
    /// Their terminals, such as `/dev/ttys003`.
    public var ttys: [String]
    /// How the command ended, once it has.
    public var exit: SessionExit?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, state, title, clients, ttys, exit
    }
}

/// How a session's command ended.
public struct SessionExit: Codable, Hashable, Sendable {
    public var code: Int?
    /// Such as `SIGKILL`.
    public var signal: String?
    public var at: String

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case code, signal, at
    }
}

public struct SessionAttach: Codable, Hashable, Sendable {
    public var host: String
    public var ref: String
    public var title: String
    /// The shim's socket.
    public var socket: String
    /// Terminals already attached, to raise instead of opening another.
    public var ttys: [String]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, title, socket, ttys
    }
}

public struct AgentStartParams: Codable, Hashable, Sendable {
    public var host: String?
    public var project: String
    public var profile: String
    /// What Claude starts on, given as an argument.
    public var prompt: String
    /// The branch or commit to cut from; by default origin's default branch.
    public var from: String?
    /// The ref, which names the worktree and branch; by default three
    /// random words.
    public var name: String?

    public init(
        host: String? = nil, project: String, profile: String, prompt: String,
        from: String? = nil, name: String? = nil
    ) {
        self.host = host
        self.project = project
        self.profile = profile
        self.prompt = prompt
        self.from = from
        self.name = name
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, project, profile, prompt, from, name
    }
}

public struct AgentDeleteParams: Codable, Hashable, Sendable {
    public var host: String?
    public var ref: String
    /// Delete even with uncommitted changes or unpushed commits.
    public var force: Bool?

    public init(host: String? = nil, ref: String, force: Bool? = nil) {
        self.host = host
        self.ref = ref
        self.force = force
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, force
    }
}

/// Names one agent.
public struct AgentParams: Codable, Hashable, Sendable {
    public var host: String?
    public var ref: String

    public init(host: String? = nil, ref: String) {
        self.host = host
        self.ref = ref
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref
    }
}

/// `agents.send`: text, or keys by name, for one agent.
public struct AgentSendParams: Codable, Hashable, Sendable {
    public var host: String?
    public var ref: String
    public var text: String?
    /// Whether Enter follows the text; true unless false.
    public var submit: Bool?
    /// Paste even while the agent is on a question.
    public var force: Bool?
    /// Keys by name: "2", "Enter", "Down", "Ctrl+C".
    public var keys: [String]?

    public init(
        host: String? = nil, ref: String, text: String? = nil, submit: Bool? = nil,
        force: Bool? = nil, keys: [String]? = nil
    ) {
        self.host = host
        self.ref = ref
        self.text = text
        self.submit = submit
        self.force = force
        self.keys = keys
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, text, submit, force, keys
    }
}

public struct AgentSent: Codable, Hashable, Sendable {
    /// It was asleep, paused or exited, and was woken or resumed first.
    public var woke: Bool
    public var state: AgentState

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case woke, state
    }
}

public struct AgentCheckParams: Codable, Hashable, Sendable {
    public var host: String?
    public var ref: String
    public var background: Bool?

    public init(host: String? = nil, ref: String, background: Bool? = nil) {
        self.host = host
        self.ref = ref
        self.background = background
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, background
    }
}

public struct AgentCheck: Codable, Hashable, Sendable {
    public var state: AgentState
    public var activity: AgentActivity
    public var line: String?
    /// It has answered the last send to it, or its start's prompt.
    public var answered: Bool

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case state, activity, line, answered
    }
}

public struct AgentOutputParams: Codable, Hashable, Sendable {
    public var host: String?
    public var ref: String
    /// A cursor from an earlier read.
    public var after: Int?

    public init(host: String? = nil, ref: String, after: Int? = nil) {
        self.host = host
        self.ref = ref
        self.after = after
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, after
    }
}

/// What an agent's terminal shows.
public struct AgentOutput: Codable, Hashable, Sendable {
    public var screen: String
    public var history: [String]
    public var cursor: Int
    public var dropped: Int
    public var more: Bool
    /// The live terminal, or the last one saved.
    public var live: Bool

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case screen, history, cursor, dropped, more, live
    }
}

public struct AgentMessagesParams: Codable, Hashable, Sendable {
    public var host: String?
    public var ref: String
    /// How many of the last messages; 10 if nil.
    public var last: Int?

    public init(host: String? = nil, ref: String, last: Int? = nil) {
        self.host = host
        self.ref = ref
        self.last = last
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, last
    }
}

/// One message of an agent's conversation, from its transcript.
public struct Message: Codable, Hashable, Sendable {
    public var role: MessageRole
    public var text: String
    public var at: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case role, text, at
    }
}

public struct MessageList: Codable, Hashable, Sendable {
    public var messages: [Message]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case messages
    }
}

/// One Claude Code in its own worktree and compartment.
public struct Agent: Codable, Hashable, Sendable {
    public var host: String
    public var ref: String
    public var project: String
    public var profile: String
    public var environment: AgentEnvironment
    public var state: AgentState
    public var worktree: String
    public var branch: String
    /// Claude Code's `--session-id`.
    public var sessionId: String
    public var createdAt: String
    /// The command that attaches a terminal, such as `vibe attach <ref>`.
    public var attach: String
    /// Its memory limit in bytes: its profile's when it started.
    public var memory: Int
    /// What it uses now, in bytes, while its session runs; only
    /// `agents.list` and `agents.interrupt` measure it.
    public var memoryUsed: Int?
    /// What its transcript says it's doing.
    public var status: AgentStatus

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, ref, project, profile, environment, state, worktree, branch
        case sessionId = "session_id"
        case createdAt = "created_at"
        case attach, memory
        case memoryUsed = "memory_used"
        case status
    }
}

/// What an agent's transcript says it's doing, from agent-tail.
public struct AgentStatus: Codable, Hashable, Sendable {
    public var activity: AgentActivity
    public var certainty: Certainty
    /// When the activity began.
    public var since: String?
    /// The session's title, for the menu row.
    public var title: String?
    /// The latest activity line, for the menu row.
    public var line: String?
    public var pendingWakeups: [Wakeup]
    public var stopReason: StopReason?
    /// When a usage limit resets, if Claude recorded it.
    public var resetsAt: String?
    public var error: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case activity, certainty, since, title, line
        case pendingWakeups = "pending_wakeups"
        case stopReason = "stop_reason"
        case resetsAt = "resets_at"
        case error
    }
}

/// A wakeup an agent has armed: a `/loop` or a cron job.
public struct Wakeup: Codable, Hashable, Sendable {
    public var kind: WakeupKind
    public var dueAt: String?
    public var certainty: Certainty
    /// A recurring cron job stays pending until it's deleted.
    public var recurring: Bool
    public var reason: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case kind
        case dueAt = "due_at"
        case certainty, recurring, reason
    }
}

public struct AgentList: Codable, Hashable, Sendable {
    public var agents: [Agent]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case agents
    }
}

/// The Linux toolbox, for `toolbox.status`.
public struct ToolboxStatus: Codable, Hashable, Sendable {
    public var state: ToolboxState
    /// The newest template version, which new agents boot.
    public var template: Int?
    /// The newest Linux home version.
    public var home: Int?
    /// The base release new agents boot: the template's.
    public var base: Int?
    /// The newest base release installed; newer than `base`, offer Rebuild template.
    public var newestBase: Int?
    /// The last shell, update or rebuild to finish.
    public var last: ToolboxRun?
    /// How many terminals are attached to the open shell: 0 when it's open
    /// in the background. Nil when that isn't known: it isn't open, its shim
    /// isn't up yet or didn't answer, or a vibed from before this field.
    public var clients: Int?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case state, template, home, base
        case newestBase = "newest_base"
        case last, clients
    }
}

/// How a toolbox run went.
public struct ToolboxRun: Codable, Hashable, Sendable {
    public var action: ToolboxAction
    /// When it finished.
    public var at: String
    /// The template version it saved.
    public var template: Int?
    /// After a rebuild: what the old template has that the recipe doesn't cover.
    public var uncovered: [String]
    /// Why it failed; then nothing was kept.
    public var error: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case action, at, template, uncovered, error
    }
}

/// The logins linux agents start with, for `toolbox.logins`.
public struct ToolboxLogins: Codable, Hashable, Sendable {
    /// The Linux home version new agents boot; none before the toolbox has
    /// saved one.
    public var home: Int?
    /// The gh, gcloud, git and SSH logins that home holds, found as the
    /// toolbox saved it; nil if it couldn't tell. Without a home, empty.
    public var logins: [Login]?
    /// Whether linux agents have a Claude login (`vibe setup-token`'s token,
    /// kept on the Mac).
    public var claude: Bool
    /// Without one, why, as a sentence: none kept yet, or it can't be read.
    public var claudeProblem: String?

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case home, logins, claude
        case claudeProblem = "claude_problem"
    }
}

/// One login the Linux home holds. Never its secret.
public struct Login: Codable, Hashable, Sendable {
    public var tool: LoginTool
    /// Whose it is: `octocat on github.com`, an account, `Name <email>`, or
    /// an SSH key's file and comment.
    public var account: String

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case tool, account
    }
}

/// What `doctor.check` found, check by check.
public struct DoctorReport: Codable, Hashable, Sendable {
    public var checks: [DoctorCheck]

    public init(checks: [DoctorCheck]) {
        self.checks = checks
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case checks
    }
}

/// One doctor check.
public struct DoctorCheck: Codable, Hashable, Sendable {
    public var id: CheckId
    public var state: CheckState
    /// What it found, as a sentence.
    public var detail: String
    /// Unless it passed, what to do about it, as a sentence.
    public var hint: String?
    /// Whether `doctor.fix` can do that.
    public var fixable: Bool
    /// While it's `working`, how far along, in percent, when that's known.
    public var progress: Int?

    public init(
        id: CheckId, state: CheckState, detail: String, hint: String? = nil, fixable: Bool = false,
        progress: Int? = nil
    ) {
        self.id = id
        self.state = state
        self.detail = detail
        self.hint = hint
        self.fixable = fixable
        self.progress = progress
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case id, state, detail, hint, fixable, progress
    }
}

public struct DoctorFixParams: Codable, Hashable, Sendable {
    public var check: CheckId

    public init(check: CheckId) {
        self.check = check
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case check
    }
}

// MARK: Shared MCP services

/// The supported services, and how each stands on the host named. A host holds
/// its own logins, so these go to it over its link, and only ever say how it
/// stands; no token crosses.
public enum ServicesList: RPCMethod {
    public typealias Params = HostParams
    public typealias Result = ServiceList
    public static let name = "services.list"
}

/// The first half of connecting or reconnecting a host: it makes the PKCE
/// verifier, which it keeps, and gives the page to open in a web authentication
/// session. What's connected stays until `services.complete`.
public enum ServicesConnect: RPCMethod {
    public typealias Params = ServiceParams
    public typealias Result = ServiceAuthorization
    public static let name = "services.connect"
}

/// The second half: the URL the session ended on. The host makes the exchange
/// and keeps the tokens.
public enum ServicesComplete: RPCMethod {
    public typealias Params = ServiceCompleteParams
    public typealias Result = ServiceInfo
    public static let name = "services.complete"
}

/// Connects this host with a personal API key. It takes no `host`: a key never
/// crosses a link.
public enum ServicesConnectKey: RPCMethod {
    public typealias Params = ServiceKeyParams
    public typealias Result = ServiceInfo
    public static let name = "services.connect_key"
}

public enum ServicesDisconnect: RPCMethod {
    public typealias Params = ServiceParams
    public typealias Result = ServiceInfo
    public static let name = "services.disconnect"
}

public enum ServicesSet: RPCMethod {
    public typealias Params = ServiceSetParams
    public typealias Result = ServiceInfo
    public static let name = "services.set"
}

/// The last calls agents on the host named made to its services, oldest first.
public enum ServicesAudit: RPCMethod {
    public typealias Params = HostParams
    public typealias Result = ServiceAuditList
    public static let name = "services.audit"
}

/// One shared service, on one host. Nothing here is secret.
public struct ServiceInfo: Codable, Hashable, Sendable {
    public var name: String
    public var title: String
    public var state: ServiceState
    public var auth: ServiceAuth?
    /// Off, every agent's calls are refused: the kill switch.
    public var enabled: Bool
    public var readOnly: Bool
    public var deniedTools: [String]
    /// Claude Code runs its tools without asking, for agents started from here
    /// on. Off, its own permission prompts govern them.
    public var autoAllow: Bool
    /// How many tools agents see.
    public var tools: Int
    /// The last thing that went wrong, for people, and when.
    public var detail: String?
    public var detailAt: String?

    public init(
        name: String, title: String, state: ServiceState, auth: ServiceAuth? = nil, enabled: Bool = true,
        readOnly: Bool = false, deniedTools: [String] = [], autoAllow: Bool = false, tools: Int = 0,
        detail: String? = nil, detailAt: String? = nil
    ) {
        self.name = name
        self.title = title
        self.state = state
        self.auth = auth
        self.enabled = enabled
        self.readOnly = readOnly
        self.deniedTools = deniedTools
        self.autoAllow = autoAllow
        self.tools = tools
        self.detail = detail
        self.detailAt = detailAt
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case name, title, state, auth, enabled
        case readOnly = "read_only"
        case deniedTools = "denied_tools"
        case autoAllow = "auto_allow"
        case tools, detail
        case detailAt = "detail_at"
    }
}

public struct ServiceList: Codable, Hashable, Sendable {
    public var services: [ServiceInfo]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case services
    }
}

public struct ServiceParams: Codable, Hashable, Sendable {
    public var host: String?
    public var service: String

    public init(host: String? = nil, service: String) {
        self.host = host
        self.service = service
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, service
    }
}

public struct ServiceAuthorization: Codable, Hashable, Sendable {
    public var service: String
    /// The page to open.
    public var url: String
    /// The scheme of the URL it ends on.
    public var callbackScheme: String

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case service, url
        case callbackScheme = "callback_scheme"
    }
}

public struct ServiceCompleteParams: Codable, Hashable, Sendable {
    public var host: String?
    public var service: String
    public var callbackUrl: String

    public init(host: String? = nil, service: String, callbackUrl: String) {
        self.host = host
        self.service = service
        self.callbackUrl = callbackUrl
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, service
        case callbackUrl = "callback_url"
    }
}

public struct ServiceKeyParams: Codable, Hashable, Sendable {
    public var service: String
    public var key: String

    public init(service: String, key: String) {
        self.service = service
        self.key = key
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case service, key
    }
}

public struct ServiceSetParams: Codable, Hashable, Sendable {
    public var host: String?
    public var service: String
    public var enabled: Bool?
    public var readOnly: Bool?
    public var deniedTools: [String]?
    public var autoAllow: Bool?

    public init(
        host: String? = nil, service: String, enabled: Bool? = nil, readOnly: Bool? = nil,
        deniedTools: [String]? = nil, autoAllow: Bool? = nil
    ) {
        self.host = host
        self.service = service
        self.enabled = enabled
        self.readOnly = readOnly
        self.deniedTools = deniedTools
        self.autoAllow = autoAllow
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case host, service, enabled
        case readOnly = "read_only"
        case deniedTools = "denied_tools"
        case autoAllow = "auto_allow"
    }
}

/// One call an agent made to a service. Never its arguments or a token.
public struct ServiceAuditEntry: Codable, Hashable, Sendable {
    public var at: String
    public var agent: String
    public var service: String
    public var tool: String
    public var outcome: String
    /// How long it took, in milliseconds.
    public var ms: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case at, agent, service, tool, outcome, ms
    }
}

public struct ServiceAuditList: Codable, Hashable, Sendable {
    public var entries: [ServiceAuditEntry]

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case entries
    }
}
