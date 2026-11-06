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
        /// A fresh install on a Mac where gh and claude are already logged
        /// in: no projects, no keys, no Claude login for linux agents, git
        /// not yet using gh, a Mac that sleeps, and Linux downloading.
        case firstRun = "first-run"
        /// A fresh install on a Mac with nothing on it yet: no Command Line
        /// Tools, no gh, no Claude Code.
        case freshMac = "fresh-mac"
        /// vibed isn't there.
        case offline
        /// A fictional team's working day, on this Mac and two Linux hosts,
        /// for the README's screenshots: `--gallery --demo`. Larkfield Labs,
        /// its people and its hosts are made up.
        case demo
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

    private var populated: Bool { scenario == .busy || scenario == .demo }
    private var demo: Bool { scenario == .demo }
    private var account: String { demo ? "larkfield-dev" : "octocat" }
    private var macHome: String { demo ? "/Users/dev" : "/Users/me" }
    private var tailscaleAddress: String { demo ? "100.96.14.27" : "100.101.102.103" }
    private var limit: Int { demo ? 8 << 30 : 4 << 30 }
    private var setUp: Bool { scenario != .firstRun && scenario != .freshMac }

    func answer(_ method: String, host: String, params: [String: Any]) -> [String: Any]? {
        switch method {
        case HostsList.name:
            return ["hosts": hosts]
        case AgentsList.name:
            return ["agents": populated ? agents(on: host) : []]
        case GhStatusGet.name:
            if scenario == .freshMac { return ["state": "not_installed", "detail": "gh isn't installed"] }
            return ["state": "logged_in", "account": account, "checked_at": time(600)]
        case DoctorCheckGet.name, DoctorFix.name:
            return ["checks": doctor]
        case ToolboxStatusGet.name, ToolboxClose.name:
            return ["state": "closed", "template": 3, "home": 2, "base": 1, "newest_base": 2]
        case PiModelsGet.name:
            return [
                "host": host, "target": params["target"] as? String ?? "native",
                "providers": [
                    [
                        "id": "anthropic", "name": "Anthropic", "signed_in": true, "ambient": false, "oauth": true,
                        "api_key": true,
                        "models": [["id": "claude-sonnet-4", "name": "Claude Sonnet 4"]],
                    ],
                    [
                        "id": "opencode", "name": "OpenCode Zen", "signed_in": false, "ambient": false, "oauth": false,
                        "api_key": true,
                        "models": [["id": "deepseek-v4.1-flash", "name": "DeepSeek V4.1 Flash"]],
                    ],
                ],
            ]
        case PiStatusGet.name, PiInstall.name, PiForget.name:
            // As the doctor's Pi check says of this Mac.
            if scenario == .firstRun || scenario == .freshMac {
                return ["host": host, "vms": [String: Any]()]
            }
            return [
                "host": host,
                "native": ["path": "\(macHome)/.local/bin/pi", "version": "1.5.0"],
                "logins": ["openai"],
                "vms": ["version": "1.5.0", "logins": ["anthropic"]],
            ]
        case McpGet.name:
            return [
                "port": 7878, "tailscale": populated, "url": "http://127.0.0.1:7878/mcp",
                "tailscale_urls": populated ? ["http://\(tailscaleAddress):7878/mcp"] : [String](),
            ]
        case McpServeGet.name, McpServeSet.name:
            // A Linux host's MCP on its tailnet name; a Mac has its own toggle.
            guard host != VibeRPC.localHost else {
                return ["on": false, "available": false, "note": "This is for a Linux host."]
            }
            let on = params["on"] as? Bool ?? populated
            var serve: [String: Any] = ["on": on, "available": true]
            if on { serve["url"] = "https://\(host).tailnet-example.ts.net/mcp" }
            return serve
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
                "reserve_cores": 2, "max_awake_agents": 16, "cpu_cores": 4,
                "max_processes": 512, "disk_soft": 20 << 30, "disk_hard": 30 << 30, "vm_disk": 20 << 30,
                "idle_debounce": 60, "snapshot_disk": 20 << 30,
            ]
        case ToolboxLoginsGet.name:
            // No Linux home yet: no logins in it.
            guard setUp else { return ["claude": false, "logins": [Any]()] }
            return [
                "home": 2, "claude": true,
                "logins": [
                    ["tool": "gh", "account": "\(account) on github.com"],
                    [
                        "tool": "git",
                        "account": demo ? "Larkfield Dev <dev@larkfield.example>" : "The Octocat <octocat@github.com>",
                    ],
                    ["tool": "ssh", "account": "ED25519 SHA256:3x8f…Q2k (\(account)@vibe)"],
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
                    "VibeOS keeps running on vps.example.com when you're logged out, and starts "
                        + "again after a reboot."
                ],
            ]
        default:
            return nil
        }
    }

    var hosts: [[String: Any]] {
        if demo { return demoHosts }
        var local: [String: Any] = [
            "name": "local", "os": "macos", "label": "studio", "link": "local", "external": false, "mcp": true,
            "version": "0.1.0", "missing": [String](),
            "memory": ["total": 64 << 30, "used": 23 << 30, "swap_total": 0, "swap_used": 0],
        ]
        guard populated else {
            local["mcp"] = setUp
            return [local]
        }
        return [
            local,
            [
                "name": "vps", "os": "linux", "label": "vps-fra-1", "link": "connected", "external": false,
                "mcp": true, "address": "me@vps.example.com", "version": "0.1.0", "missing": [String](),
                "memory": ["total": 16 << 30, "used": 5 << 30, "swap_total": 4 << 30, "swap_used": 0],
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
            "state": state.rawValue, "worktree": "\(macHome)/VibeOS/worktrees/\(project)/\(ref)", "branch": ref,
            "session_id": "40414243-4445-4647-8849-4a4b4c4d4e4f", "created_at": time(age),
            "attach": host == VibeRPC.localHost ? "vibe attach \(ref)" : "ssh -t \(host) vibe attach \(ref)",
            "memory": limit, "status": status,
        ]
        if let used { agent["memory_used"] = used }
        return agent
    }

    func agents(on host: String) -> [[String: Any]] {
        if demo { return demoAgents(on: host) }
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
        ["name": name, "environment": environment, "model": model, "reasoning": "high", "mode": mode, "memory": limit]
    }

    func projects(on host: String) -> [[String: Any]] {
        if demo { return demoProjects(on: host) }
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

    /// What vibe doctor finds on this scenario's Mac.
    var doctor: [[String: Any]] {
        func check(_ id: CheckId, _ state: CheckState, _ detail: String, hint: String? = nil, fixable: Bool = false)
            -> [String: Any]
        {
            var c: [String: Any] = ["id": id.rawValue, "state": state.rawValue, "detail": detail, "fixable": fixable]
            if let hint { c["hint"] = hint }
            return c
        }
        let git = check(.git, .pass, "git 2.50.1 is installed.")
        let gh = check(.gh, .pass, "GitHub's gh is logged in to github.com as \(account).")
        let claude = check(.claude, .pass, "Claude Code 2.1.314 is at \(macHome)/.local/bin/claude.")
        let login = check(.claudeLogin, .pass, "Claude Code is signed in, with its login in your Keychain.")
        // Only a Linux host's: no Mac's vibed says it. It's here so the canned report
        // has every check the contract does, and the welcome leaves it out.
        let linger = check(.linger, .pass, "vibed runs while you're logged out, and starts again after a reboot.")
        let tailscale = check(.tailscale, .pass, "Tailscale is up, at \(tailscaleAddress); Settings can serve MCP there too.")
        let linux = check(.linux, .pass, "Linux base image 2 and Claude Code 2.1.314 are ready for linux agents.")
        // The doctor's last check, as `vibed` says it. Only agents whose profile
        // runs Pi need it, so a Mac without it has nothing to do about it.
        let piInstalled = check(.pi, .pass, "Pi 1.5.0 is at \(macHome)/.local/bin/pi.")
        let noPi = check(
            .pi, .optional, "Pi isn't installed, or your login shell can't find it.",
            hint: "Only agents whose profile runs Pi need it: install it in Settings → Agents.")
        switch scenario {
        case .busy, .quiet, .offline, .demo:
            return [
                git, gh, check(.ghGit, .pass, "git asks gh for GitHub's credentials."), claude, login,
                check(.power, .pass, "This Mac never sleeps on its own."), linger, tailscale, linux, piInstalled,
            ]
        case .firstRun:
            var downloading = check(.linux, .working, "Downloading the Linux base image.")
            downloading["progress"] = 40
            return [
                git, gh, check(.ghGit, .pass, "git asks gh for GitHub's credentials."), claude, login,
                check(
                    .power, .fail, "This Mac sleeps after 10 minutes idle, and agents stop answering while it sleeps.",
                    hint: "In System Settings → Energy, turn on Prevent automatic sleeping when the display is off."),
                linger,
                tailscale,
                downloading,
                noPi,
            ]
        case .freshMac:
            return [
                check(
                    .git, .fail, "git needs Apple's Command Line Tools, which aren't installed.",
                    hint: "It takes Apple's Command Line Tools, and VibeOS's setup has a button for them."),
                check(
                    .gh, .fail, "GitHub's gh isn't installed.",
                    hint: "VibeOS installs it by itself."),
                check(.ghGit, .optional, "VibeOS has git use gh for GitHub as soon as gh is logged in."),
                check(
                    .claude, .fail, "Claude Code isn't installed, or your login shell can't find it.",
                    hint: "VibeOS installs it by itself."),
                check(
                    .claudeLogin, .fail, "Claude Code can sign in once it's installed.",
                    hint: "Install Claude Code first."),
                check(.power, .pass, "This Mac never sleeps on its own."),
                linger,
                check(
                    .tailscale, .optional,
                    "Tailscale isn't running. It's optional: it lets your other machines reach MCP here.",
                    hint: "Install Tailscale from tailscale.com and log in, if you want it."),
                check(
                    .linux, .fail, "Can't download the Linux base image: the network is unreachable.",
                    hint: "Check this Mac is online, then try again.", fixable: true),
                noPi,
            ]
        }
    }

    var keys: [[String: Any]] {
        if demo { return demoKeys }
        return [
            ["name": "laptop Claude Code", "prefix": "vk_aaaqeaye", "created_at": time(3 * 86400), "last_used_at": time(240)],
            ["name": "cloud orchestrator", "prefix": "vk_b7d2kq9m", "created_at": time(86400)],
        ]
    }
}

// MARK: The demo: fictional, and all that the README's screenshots show

extension FixtureDaemon {
    fileprivate var demoHosts: [[String: Any]] {
        [
            [
                "name": "local", "os": "macos", "label": "mac-studio", "link": "local", "external": false, "mcp": true,
                "version": "0.1.7", "missing": [String](),
                "memory": ["total": 64 << 30, "used": 21 << 30, "swap_total": 0, "swap_used": 0],
            ],
            [
                "name": "atlas", "os": "linux", "label": "atlas-fra1", "link": "connected", "external": false,
                "mcp": true, "address": "deploy@atlas.larkfield.example", "version": "0.1.7", "missing": [String](),
                "memory": ["total": 32 << 30, "used": 9 << 30, "swap_total": 4 << 30, "swap_used": 0],
            ],
            [
                // A Tailscale name, as an SSH destination can be.
                "name": "forge", "os": "linux", "label": "forge", "link": "connected", "external": false,
                "mcp": true, "address": "deploy@forge", "version": "0.1.7", "missing": [String](),
                "memory": ["total": 128 << 30, "used": 41 << 30, "swap_total": 0, "swap_used": 0],
            ],
        ]
    }

    fileprivate func demoAgents(on host: String) -> [[String: Any]] {
        switch host {
        case VibeRPC.localHost:
            return [
                agent(
                    "tidy-cedar-plover", project: "larkfield-ios", profile: "software-eng-mac", state: .busy,
                    activity: .runningTool, title: "Fix the offline sync race in JobStore",
                    line: "$ xcodebuild test -scheme Larkfield -only-testing:JobStoreTests",
                    used: 3_543_348_019, age: 38 * 60),
                agent(
                    "warm-slate-ibis", project: "larkfield-ios", profile: "software-eng-linux", state: .question,
                    activity: .awaitingAnswer, title: "Add quiet hours to push notifications",
                    line: "Should quiet hours follow the device or the account time zone?",
                    used: 1_288_490_188, age: 21 * 60),
                agent(
                    "sunny-moss-vole", project: "dispatch-web", profile: "software-eng-linux", state: .idle,
                    activity: .done, title: "Upgrade Vite and fix the failing route tests",
                    line: "Opened pull request #318", used: 944_245_094, age: 52 * 60),
                agent(
                    "lucky-fern-tern", project: "dispatch-web", profile: "reviewer", state: .asleep,
                    activity: .done, title: "Review the route planner pull request",
                    line: "Left 3 comments on #316", used: nil, age: 5 * 3600),
            ]
        case "atlas":
            return [
                agent(
                    "merry-basalt-shrew", host: "atlas", project: "dispatch-api", profile: "software-eng-linux",
                    state: .busy, activity: .thinking, title: "Partition job_events by month", line: "Thinking…",
                    used: 2_854_348_019, age: 2 * 3600 + 14 * 60),
                agent(
                    "plain-indigo-stoat", host: "atlas", project: "dispatch-api", profile: "software-eng-linux",
                    state: .busy, activity: .runningTool, title: "Find the slow query behind /v2/routes",
                    line: "$ pytest tests/test_routes.py -q", used: 1_754_857_830, age: 26 * 60),
            ]
        case "forge":
            return [
                agent(
                    "round-clay-pika", host: "forge", project: "eta-model", profile: "software-eng-linux",
                    state: .busy, activity: .runningTool, title: "Retrain the ETA model on October data",
                    line: "$ python train.py --config configs/eta-oct.yaml", used: 6_442_450_944,
                    age: 3 * 3600 + 40 * 60)
            ]
        default:
            return []
        }
    }

    fileprivate func demoProjects(on host: String) -> [[String: Any]] {
        if host != VibeRPC.localHost {
            let name = host == "forge" ? "eta-model" : "dispatch-api"
            return [
                [
                    "host": host, "name": name, "source": "github", "repo": "larkfield-labs/\(name)",
                    "path": "/home/deploy/VibeOS/projects/\(name)", "added_at": time(86400),
                    "profiles": [profile("software-eng-linux", "linux")],
                ]
            ]
        }
        return [
            [
                "host": host, "name": "larkfield-ios", "source": "github", "repo": "larkfield-labs/larkfield-ios",
                "path": "/Users/dev/VibeOS/projects/larkfield-ios", "added_at": time(3 * 86400),
                "profiles": [profile("software-eng-linux", "linux"), profile("software-eng-mac", "macos")],
            ],
            [
                "host": host, "name": "dispatch-web", "source": "github", "repo": "larkfield-labs/dispatch-web",
                "path": "/Users/dev/VibeOS/projects/dispatch-web", "added_at": time(2 * 86400),
                "profiles": [
                    profile("software-eng-linux", "linux"), profile("software-eng-mac", "macos"),
                    profile("reviewer", "linux", model: "sonnet", mode: "bypass"),
                ],
            ],
        ]
    }

    fileprivate var demoKeys: [[String: Any]] {
        [
            [
                "name": "laptop Claude Code", "prefix": "vk_demo4f2a", "created_at": time(3 * 86400),
                "last_used_at": time(240),
            ],
            [
                "name": "release orchestrator", "prefix": "vk_demo91c7", "created_at": time(86400),
                "last_used_at": time(35 * 60),
            ],
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

extension InstallStatus {
    /// Installed from the disk image just now: vibed's LaunchAgent on, vibe
    /// not linked yet, and macOS yet to ask about Terminal.
    public static let justInstalled = InstallStatus(
        location: .applications, service: .enabled, command: .missing, terminal: .notAsked)
    /// Opened from the disk image, not moved to Applications.
    public static let fromDiskImage = InstallStatus(
        location: .notMoved, service: .notManaged, command: .notManaged, terminal: .unknown)
    /// Installed, with vibed turned off in Login Items.
    public static let turnedOff = InstallStatus(
        location: .applications, service: .requiresApproval, command: .linked, terminal: .granted)
}

extension MenuModel {
    /// A model on the fixture vibed, read once: what the gallery draws. A
    /// first run is a copy just installed, unless `install` says otherwise.
    public static func fixture(_ scenario: FixtureDaemon.Scenario, install: InstallStatus? = nil) async -> MenuModel {
        let firstRun = scenario == .firstRun || scenario == .freshMac
        let installer = FixedInstaller(install ?? (firstRun ? .justInstalled : .notManaged))
        let model = MenuModel(
            daemon: FixtureDaemon(scenario), terminal: FixtureTerminal(), installer: installer, endOfTurn: { false })
        await model.refreshInstall()
        await model.refresh()
        return model
    }
}
