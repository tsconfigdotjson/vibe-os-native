import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

// Stand-ins for vibed, Terminal and Notification Center. vibed's answers are
// JSON, decoded into the contract's types, as the real client decodes them.

func decode<T: Decodable>(_ type: T.Type, _ object: Any) throws -> T {
    try JSONDecoder().decode(T.self, from: JSONSerialization.data(withJSONObject: object))
}

/// An agent as `agents.list` gives one.
func agentJSON(
    _ ref: String, host: String = "local", project: String = "app", profile: String = "software-eng-mac",
    state: AgentState = .busy, activity: AgentActivity = .thinking, title: String? = nil,
    line: String? = nil, memoryUsed: Int? = nil, createdAt: String = "2026-10-30T12:00:00Z"
) -> [String: Any] {
    var status: [String: Any] = ["activity": activity.rawValue, "certainty": "firm", "pending_wakeups": []]
    if let title { status["title"] = title }
    if let line { status["line"] = line }
    var agent: [String: Any] = [
        "host": host, "ref": ref, "project": project, "profile": profile, "environment": "macos",
        "state": state.rawValue, "worktree": "/Users/me/VibeOS/worktrees/\(project)/\(ref)",
        "branch": ref, "session_id": "40414243-4445-4647-8849-4a4b4c4d4e4f", "created_at": createdAt,
        "attach": host == "local" ? "vibe attach \(ref)" : "ssh -t \(host) vibe attach \(ref)",
        "memory": 4 << 30, "status": status,
    ]
    if let memoryUsed { agent["memory_used"] = memoryUsed }
    return agent
}

func agent(
    _ ref: String, host: String = "local", project: String = "app", state: AgentState = .busy,
    activity: AgentActivity = .thinking, title: String? = nil, memoryUsed: Int? = nil
) throws -> Agent {
    try decode(
        Agent.self,
        agentJSON(ref, host: host, project: project, state: state, activity: activity, title: title, memoryUsed: memoryUsed))
}

func hostJSON(
    _ name: String, link: HostLink = .local, external: Bool = false, mcp: Bool = false, missing: [String] = [],
    detail: String? = nil, tunnel: TunnelState? = nil, externalDetail: String? = nil
) -> [String: Any] {
    var host: [String: Any] = [
        "name": name, "os": name == "local" ? "macos" : "linux", "label": name == "local" ? "mac-mini" : name,
        "link": link.rawValue, "external": external, "mcp": mcp, "version": "0.1.0", "missing": missing,
    ]
    if name != "local" { host["address"] = "me@\(name).example.com" }
    if let detail { host["detail"] = detail }
    if external {
        host["url"] = "https://\(name).example.com/mcp"
        host["tunnel"] = (tunnel ?? .up).rawValue
    }
    if let externalDetail { host["external_detail"] = externalDetail }
    return host
}

func event(_ kind: EventKind, ref: String? = nil, host: String = "local", seq: Int = 1, extra: [String: Any] = [:])
    throws -> DaemonEvent
{
    var object: [String: Any] = ["seq": seq, "at": "2026-10-30T12:00:00Z", "host": host, "kind": kind.rawValue]
    if let ref {
        object["ref"] = ref
        object["project"] = "app"
    }
    object.merge(extra) { $1 }
    return try decode(DaemonEvent.self, object)
}

/// A value tests change while a fake reads it from another thread.
final class Box<Value>: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: Value

    init(_ value: Value) {
        stored = value
    }

    var value: Value {
        get { lock.withLock { stored } }
        set { lock.withLock { stored = newValue } }
    }
}

/// vibed: each method answers from a handler given the params as JSON.
final class FakeDaemon: DaemonConnection, @unchecked Sendable {
    /// Runs on the thread the call is made on, off the main actor.
    typealias Handler = @Sendable ([String: Any]) throws -> [String: Any]

    private let lock = NSLock()
    private var handlers: [String: Handler] = [:]
    private var log: [(method: String, params: [String: Any])] = []
    private var streams: [AsyncThrowingStream<EventMessage, any Error>.Continuation] = []
    private var timeouts: [String: TimeInterval] = [:]

    func on(_ method: String, _ handler: @escaping Handler) {
        lock.withLock { handlers[method] = handler }
    }

    /// The methods called, in order.
    var calls: [String] { lock.withLock { log.map(\.method) } }

    func params(of method: String) -> [[String: Any]] {
        lock.withLock { log.filter { $0.method == method }.map(\.params) }
    }

    func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params) throws -> M.Result {
        let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(params)) as? [String: Any] ?? [:]
        let handler = lock.withLock { () -> Handler? in
            log.append((M.name, json))
            return handlers[M.name]
        }
        guard let handler else {
            throw VibedClientError.rpc(RPCError(code: RPCErrorCode.methodNotFound.rawValue, message: "no \(M.name)"))
        }
        return try decode(M.Result.self, try handler(json))
    }

    func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params, timeout: TimeInterval) throws -> M.Result {
        lock.withLock { timeouts[M.name] = timeout }
        return try call(method, params)
    }

    /// The timeout the last long call of `method` asked for.
    func timeout(of method: String) -> TimeInterval? {
        lock.withLock { timeouts[method] }
    }

    func events(client: String) -> AsyncThrowingStream<EventMessage, any Error> {
        AsyncThrowingStream { continuation in
            lock.withLock { streams.append(continuation) }
        }
    }

    /// Sends `message` to every open subscription.
    func send(_ message: EventMessage) {
        for stream in lock.withLock({ streams }) { stream.yield(message) }
    }

    /// Ends every open subscription, as vibed going away does.
    func drop() {
        let all = lock.withLock { () -> [AsyncThrowingStream<EventMessage, any Error>.Continuation] in
            defer { streams = [] }
            return streams
        }
        for stream in all { stream.finish(throwing: VibedClientError.closed) }
    }

    var subscriptions: Int { lock.withLock { streams.count } }
}

/// A vibed with one host and the agents given, by host.
func fakeDaemon(agents: [String: [[String: Any]]] = [:], hosts: [[String: Any]] = [hostJSON("local")])
    -> FakeDaemon
{
    let daemon = FakeDaemon()
    daemon.on(HostsList.name) { _ in ["hosts": hosts] }
    daemon.on(AgentsList.name) { params in
        ["agents": agents[params["host"] as? String ?? "local"] ?? []]
    }
    daemon.on(GhStatusGet.name) { _ in ["state": "logged_in", "account": "me"] }
    return daemon
}

/// Terminal: tabs by tty, each busy until its command ends or it's closed.
@MainActor
final class FakeTerminal: TerminalScripting {
    var tabs: [String: String] = [:]  // tty: command
    /// Tabs whose command has ended, left open.
    var ended: Set<String> = []
    var raised: [String] = []
    var ran: [String] = []
    var withoutScripting: [String] = []
    var permitted = true
    private var next = 1

    func raise(ttys: [String], busyOnly: [String]) throws -> Bool {
        guard permitted else { throw TerminalError.notPermitted }
        let found = ttys.first { tabs[$0] != nil } ?? busyOnly.first { tabs[$0] != nil && !ended.contains($0) }
        guard let tty = found else { return false }
        raised.append(tty)
        return true
    }

    func run(_ command: String) throws -> String? {
        guard permitted else { throw TerminalError.notPermitted }
        var tty: String
        repeat {
            tty = "/dev/ttys\(String(format: "%03d", next))"
            next += 1
        } while tabs[tty] != nil
        tabs[tty] = command
        ran.append(command)
        return tty
    }

    func runWithoutScripting(_ command: String) throws {
        withoutScripting.append(command)
    }

    func close(_ tty: String) {
        tabs[tty] = nil
        ended.remove(tty)
    }

    /// Its command ends, as when the attach detaches; the tab stays.
    func end(_ tty: String) {
        ended.insert(tty)
    }
}

@MainActor
final class FakeNotifier: Notifier {
    var onClick: ((String, String) -> Void)?
    var posted: [Notice] = []

    func post(_ notice: Notice) {
        posted.append(notice)
    }
}

/// Waits up to `seconds` for `condition`, checking as the main actor runs.
@MainActor
func eventually(
    _ what: String, within seconds: Double = 5, sourceLocation: SourceLocation = #_sourceLocation,
    _ condition: () -> Bool
) async {
    let deadline = Date().addingTimeInterval(seconds)
    while !condition() {
        if Date() > deadline {
            Issue.record("timed out waiting for \(what)", sourceLocation: sourceLocation)
            return
        }
        try? await Task.sleep(for: .milliseconds(10))
    }
}
