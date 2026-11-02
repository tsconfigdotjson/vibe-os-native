import Darwin
import Foundation
import VibeRPC

/// A vibed made of canned answers, so every surface can be drawn without a
/// daemon: `VibeOS --gallery` and `--render-surfaces` show the menu,
/// Settings and the sheets with it, in light and dark, and the tests check
/// each answer decodes. Answers are JSON, decoded into the contract's types
/// as the real client decodes vibed's.
public final class FixtureDaemon: DaemonConnection, @unchecked Sendable {
    public enum Scenario: String, CaseIterable, Sendable {
        /// Agents in every state, on this Mac and two Linux hosts, one down.
        case busy
        /// Set up, with nothing running.
        case quiet
        /// A fresh install: no projects, no keys, no Claude login.
        case firstRun = "first-run"
        /// vibed isn't there.
        case offline
    }

    public let scenario: Scenario
    /// The time agents' ages count from.
    public let now: Date

    public init(_ scenario: Scenario, now: Date = Date()) {
        self.scenario = scenario
        self.now = now
    }

    public func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params) throws -> M.Result {
        if scenario == .offline {
            throw VibedClientError.connect(path: "~/Library/Application Support/VibeOS/run/vibed.sock", errno: ECONNREFUSED)
        }
        let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(params)) as? [String: Any] ?? [:]
        let host = json["host"] as? String ?? VibeRPC.localHost
        guard let answer = answer(M.name, host: host, params: json) else {
            throw VibedClientError.rpc(RPCError(code: RPCErrorCode.methodNotFound.rawValue, message: "no \(M.name) here"))
        }
        return try JSONDecoder().decode(M.Result.self, from: JSONSerialization.data(withJSONObject: answer))
    }

    public func events(client: String) -> AsyncThrowingStream<EventMessage, any Error> {
        AsyncThrowingStream { _ in }
    }

    // MARK: Answers

    private func time(_ ago: TimeInterval) -> String {
        ISO8601DateFormatter().string(from: now.addingTimeInterval(-ago))
    }

    private var populated: Bool { scenario == .busy }
    private var setUp: Bool { scenario != .firstRun }

    func answer(_ method: String, host: String, params: [String: Any]) -> [String: Any]? {
        switch method {
        case HostsList.name:
            return ["hosts": hosts]
        case AgentsList.name:
            return ["agents": populated ? agents(on: host) : []]
        case GhStatusGet.name:
            return ["state": "logged_in", "account": "octocat", "checked_at": time(600)]
        case ToolboxStatusGet.name:
            return ["state": "closed", "template": 3, "home": 2, "base": 1, "newest_base": 2]
        case McpGet.name:
            return [
                "port": 7878, "tailscale": populated, "url": "http://127.0.0.1:7878/mcp",
                "tailscale_urls": populated ? ["http://100.101.102.103:7878/mcp"] : [String](),
            ]
        case ProjectsList.name:
            return ["projects": setUp ? projects(on: host) : []]
        case KeysList.name:
            return ["keys": setUp ? keys : []]
        case KeysCreate.name:
            let name = params["name"] as? String ?? "laptop Claude Code"
            return [
                "key": ["name": name, "prefix": "vk_q7m2xk4p", "created_at": time(0)],
                "secret": "vk_q7m2xk4pR8vN3cT6wY1zA5bE9hJ2kL4mP7sU0dF3gH6",
            ]
        case LimitsGet.name:
            return [
                "reserve_memory": 4 << 30, "reserve_cores": 2, "max_awake_agents": 16, "cpu_cores": 4,
                "max_processes": 512, "disk_soft": 20 << 30, "disk_hard": 30 << 30, "vm_disk": 20 << 30,
                "idle_debounce": 60, "snapshot_disk": 20 << 30,
            ]
        case ToolboxLoginsGet.name:
            guard setUp else { return ["claude": false] }
            return [
                "home": 2, "claude": true,
                "logins": [
                    ["tool": "gh", "account": "octocat on github.com"],
                    ["tool": "git", "account": "The Octocat <octocat@github.com>"],
                    ["tool": "ssh", "account": "ED25519 SHA256:3x8f…Q2k (octocat@vibe)"],
                ],
            ]
        case SessionsList.name:
            return ["sessions": [Any]()]
        case HostsAdd.name:
            return [
                "changed": true,
                "host": [
                    "name": params["name"] as? String ?? "vps", "os": "linux", "label": "vps-fra-1", "link": "connected",
                    "external": false, "mcp": true, "address": params["address"] as? String ?? "me@vps.example.com",
                    "version": "0.1.0", "missing": ["claude"],
                ] as [String: Any],
                "notes": [
                    "Lingering is on for you on vps.example.com, so vibed runs when you're logged out and starts "
                        + "again after a reboot."
                ],
            ]
        default:
            return nil
        }
    }

    var hosts: [[String: Any]] {
        var local: [String: Any] = [
            "name": "local", "os": "macos", "label": "studio", "link": "local", "external": false, "mcp": true,
            "version": "0.1.0", "missing": [String](),
        ]
        guard populated else {
            local["mcp"] = scenario != .firstRun
            return [local]
        }
        return [
            local,
            [
                "name": "vps", "os": "linux", "label": "vps-fra-1", "link": "connected", "external": true,
                "mcp": true, "address": "me@vps.example.com", "version": "0.1.0", "missing": [String](),
                "url": "https://vps.example.com/mcp", "tunnel": "up",
            ],
            [
                "name": "box", "os": "linux", "label": "box", "link": "down", "external": false, "mcp": false,
                "address": "me@box.example.com", "version": "0.1.0", "missing": ["claude"],
                "detail": "ssh: connect to host box.example.com port 22: Operation timed out",
            ],
        ]
    }

    private func agent(
        _ ref: String, host: String = "local", project: String, profile: String, state: AgentState,
        activity: AgentActivity, title: String?, line: String?, used: Int?, age: TimeInterval,
        extra: [String: Any] = [:]
    ) -> [String: Any] {
        var status: [String: Any] = [
            "activity": activity.rawValue, "certainty": "firm", "pending_wakeups": [Any](), "since": time(60),
        ]
        if let title { status["title"] = title }
        if let line { status["line"] = line }
        status.merge(extra) { $1 }
        let environment = profile.hasSuffix("mac") ? "macos" : "linux"
        var agent: [String: Any] = [
            "host": host, "ref": ref, "project": project, "profile": profile, "environment": environment,
            "state": state.rawValue, "worktree": "/Users/me/VibeOS/worktrees/\(project)/\(ref)", "branch": ref,
            "session_id": "40414243-4445-4647-8849-4a4b4c4d4e4f", "created_at": time(age),
            "attach": host == VibeRPC.localHost ? "vibe attach \(ref)" : "ssh -t \(host) vibe attach \(ref)",
            "memory": 4 << 30, "status": status,
        ]
        if let used { agent["memory_used"] = used }
        return agent
    }

    func agents(on host: String) -> [[String: Any]] {
        switch host {
        case VibeRPC.localHost:
            return [
                agent(
                    "quiet-amber-otter", project: "app", profile: "software-eng-mac", state: .busy,
                    activity: .runningTool, title: "Fix the login race in SessionStore",
                    line: "$ swift test --filter SessionStoreTests", used: 2_254_857_830, age: 2 * 3600 + 300),
                agent(
                    "brave-copper-lynx", project: "app", profile: "software-eng-linux", state: .question,
                    activity: .awaitingAnswer, title: "Dark mode for the settings window",
                    line: "Which accent should the toggles use?", used: 1_288_490_188, age: 47 * 60),
                agent(
                    "swift-ivory-crane", project: "app", profile: "software-eng-mac", state: .paused,
                    activity: .runningTool, title: "Profile the image pipeline",
                    line: "Paused at 100% of its 4G", used: 4 << 30, age: 3 * 3600),
                agent(
                    "calm-silver-heron", project: "web", profile: "software-eng-linux", state: .idle,
                    activity: .done, title: "Bump Node to 22 and fix the lockfile",
                    line: "Opened pull request #482", used: 644_245_094, age: 26 * 60),
                agent(
                    "gentle-teal-finch", project: "web", profile: "software-eng-linux", state: .asleep,
                    activity: .errored, title: "Draft the release notes", line: nil, used: nil, age: 26 * 3600,
                    extra: ["stop_reason": "usage_limit", "resets_at": time(-90 * 60)]),
            ]
        case "vps":
            return [
                agent(
                    "bold-rust-marten", host: "vps", project: "api", profile: "software-eng-linux", state: .busy,
                    activity: .thinking, title: "Move the jobs table to Postgres 17", line: "Thinking…",
                    used: 3_543_348_019, age: 12 * 60)
            ]
        case "box":
            // Its link is down: vibed lists it as it last saw it, and the
            // menu shows the row unavailable.
            return [
                agent(
                    "keen-olive-wren", host: "box", project: "api", profile: "software-eng-linux", state: .idle,
                    activity: .done, title: "Nightly dependency audit", line: "No new advisories", used: 412_316_860,
                    age: 9 * 3600)
            ]
        default:
            return []
        }
    }

    private func profile(_ name: String, _ environment: String, model: String = "opus", mode: String = "auto")
        -> [String: Any]
    {
        ["name": name, "environment": environment, "model": model, "reasoning": "high", "mode": mode, "memory": 4 << 30]
    }

    func projects(on host: String) -> [[String: Any]] {
        if host != VibeRPC.localHost {
            return [
                [
                    "host": host, "name": "api", "source": "github", "repo": "octocat/api",
                    "path": "/home/me/VibeOS/projects/api", "added_at": time(86400),
                    "profiles": [profile("software-eng-linux", "linux")],
                ]
            ]
        }
        return [
            [
                "host": host, "name": "app", "source": "github", "repo": "octocat/app",
                "path": "/Users/me/VibeOS/projects/app", "added_at": time(3 * 86400),
                "profiles": [
                    profile("software-eng-linux", "linux"), profile("software-eng-mac", "macos"),
                    profile("reviewer", "linux", model: "sonnet", mode: "bypass"),
                ],
            ],
            [
                "host": host, "name": "web", "source": "path", "path": "/Users/me/src/web",
                "added_at": time(2 * 86400),
                "profiles": [profile("software-eng-linux", "linux"), profile("software-eng-mac", "macos")],
            ],
        ]
    }

    var keys: [[String: Any]] {
        [
            ["name": "laptop Claude Code", "prefix": "vk_aaaqeaye", "created_at": time(3 * 86400), "last_used_at": time(240)],
            ["name": "cloud orchestrator", "prefix": "vk_b7d2kq9m", "created_at": time(86400)],
        ]
    }
}

/// A Terminal that does nothing, for drawing surfaces with no Terminal.
@MainActor
public final class FixtureTerminal: TerminalScripting {
    public init() {}
    public func raise(ttys: [String], busyOnly: [String]) throws -> Bool { false }
    public func run(_ command: String) throws -> String? { nil }
    public func runWithoutScripting(_ command: String) throws {}
}

extension MenuModel {
    /// A model on the fixture vibed, read once: what the gallery draws.
    public static func fixture(_ scenario: FixtureDaemon.Scenario) async -> MenuModel {
        let model = MenuModel(daemon: FixtureDaemon(scenario), terminal: FixtureTerminal(), endOfTurn: { false })
        await model.refresh()
        return model
    }
}
