import Combine
import Foundation
import VibeRPC

/// What the menu shows, all of it from vibed: the app keeps no state of its
/// own (PRD: Architecture). vibed's events keep it current as they happen;
/// a slower poll refreshes what events don't carry, such as memory use.
@MainActor
public final class MenuModel: ObservableObject {
    public enum Connection: Equatable, Sendable {
        case connecting
        case connected
        case unavailable(String)
    }

    /// How a delete went.
    public enum DeleteOutcome: Equatable, Sendable {
        case deleted
        /// Refused for unsaved work; `force` would delete it anyway.
        case refused(String)
        case failed(String)
    }

    @Published public private(set) var connection: Connection = .connecting
    @Published public private(set) var hosts: [HostInfo] = []
    /// Every host's agents, as each host lists them.
    @Published public private(set) var agents: [Agent] = []
    /// Hosts whose agents couldn't be listed: their rows are the last
    /// known, shown as unavailable.
    @Published public private(set) var unavailableHosts: Set<String> = []
    @Published public private(set) var gh: GhStatus?
    @Published public private(set) var memory: HostMemory?
    /// Whether events are coming in; without them only the poll updates
    /// the menu.
    @Published public private(set) var live = false
    /// A note for the menu: the last action that failed, and why.
    @Published public var message: String?
    /// Bumped by every event about settings, so open settings re-read.
    @Published public private(set) var settingsRevision = 0
    /// The time rows count elapsed time to; moves on with each refresh.
    @Published public private(set) var now = Date()
    /// Refs being attached, so a second click waits for the first.
    @Published public private(set) var attaching: Set<String> = []

    /// While the menu is open it polls more often, for memory bars.
    public var menuIsOpen = false {
        didSet { if menuIsOpen && !oldValue { pollNow() } }
    }

    public let daemon: any DaemonConnection
    public let attacher: Attacher
    public var notifier: Notifier? {
        didSet { wireNotifier() }
    }
    public var openPoll: Duration
    public var closedPoll: Duration
    private let wantsEndOfTurn: () -> Bool
    private var tasks: [Task<Void, Never>] = []
    private var pollWake: CheckedContinuation<Void, Never>?
    /// Counts the poll's waits, so a timer only ends its own.
    private var pollWait = 0
    private var refreshTask: Task<Void, Never>?
    private var refreshAgain = false

    public init(
        daemon: any DaemonConnection, terminal: TerminalScripting, notifier: Notifier? = nil,
        vibePath: String = defaultVibePath(), socket: String? = attachSocket(),
        openPoll: Duration = .seconds(3), closedPoll: Duration = .seconds(30),
        endOfTurn: @escaping () -> Bool = { UserDefaults.standard.bool(forKey: Preferences.notifyEndOfTurn) }
    ) {
        self.daemon = daemon
        self.attacher = Attacher(terminal: terminal, vibePath: vibePath, socket: socket)
        self.notifier = notifier
        self.openPoll = openPoll
        self.closedPoll = closedPoll
        self.wantsEndOfTurn = endOfTurn
        wireNotifier()
    }

    /// Starts following vibed: its events, and the poll. Once.
    public func start() {
        guard tasks.isEmpty else { return }
        tasks.append(Task { [weak self] in await self?.followEvents() })
        tasks.append(Task { [weak self] in await self?.poll() })
    }

    public func stop() {
        tasks.forEach { $0.cancel() }
        tasks = []
        pollWake?.resume()
        pollWake = nil
    }

    // MARK: What the menu shows

    public var icon: IconSummary { IconSummary(agents: agents) }

    public var groups: [HostGroup] { groupAgents(agents, hosts: hosts.map(\.name)) }

    public var localHost: HostInfo? { hosts.first { $0.link == .local } }

    /// The hosts other than this one, for the header's link states.
    public var otherHosts: [HostInfo] { hosts.filter { $0.link != .local } }

    public func agent(host: String, ref: String) -> Agent? {
        agents.first { $0.host == host && $0.ref == ref }
    }

    // MARK: Calls

    /// Calls vibed off the main actor, as the client blocks.
    public func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params) async throws -> M.Result {
        let daemon = self.daemon
        return try await Task.detached { try daemon.call(method, params) }.value
    }

    /// Reads everything the menu shows again, and returns once a read that
    /// began after the call is done. Calls that come while one runs are
    /// folded into one more after it.
    public func refresh() async {
        if let running = refreshTask {
            refreshAgain = true
            await running.value
            return
        }
        let task = Task { [weak self] in
            guard let self else { return }
            repeat {
                self.refreshAgain = false
                await self.refreshOnce()
            } while self.refreshAgain
            self.refreshTask = nil
        }
        refreshTask = task
        await task.value
    }

    private func refreshOnce() async {
        memory = HostMemory.current()
        now = Date()
        let hosts: [HostInfo]
        do {
            hosts = try await call(HostsList.self, NoParams()).hosts
        } catch {
            connection = .unavailable(describe(error))
            return
        }
        var agents: [Agent] = []
        var unavailable = Set<String>()
        for host in hosts {
            if host.link != .down {
                do {
                    agents += try await call(AgentsList.self, HostParams(host: host.name)).agents
                    continue
                } catch {
                    if host.link == .local {
                        connection = .unavailable(describe(error))
                        return
                    }
                }
            }
            // A host out of reach keeps its last known rows, unavailable.
            unavailable.insert(host.name)
            agents += self.agents.filter { $0.host == host.name }
        }
        self.hosts = hosts
        self.agents = agents
        unavailableHosts = unavailable
        connection = .connected
        gh = (try? await call(GhStatusGet.self, GhStatusParams())) ?? gh
    }

    /// Opens Terminal on `agent`: raises its tab if it has one, or opens
    /// one running `vibe attach`. A second click while the first is still
    /// going does nothing, so two clicks give one tab.
    public func attach(_ agent: Agent) async {
        let key = agent.key
        guard !attaching.contains(key) else { return }
        attaching.insert(key)
        defer { attaching.remove(key) }
        do {
            let session = try await call(SessionsAttach.self, SessionParams(host: agent.host, ref: agent.ref))
            _ = try attacher.attach(agent, attached: session.ttys)
            message = nil
        } catch {
            message = "Can't attach \(agent.ref): \(describe(error))"
        }
    }

    /// Attaches to an agent named by host and ref, as a notification does.
    public func attach(host: String, ref: String) async {
        if agent(host: host, ref: ref) == nil { await refresh() }
        guard let found = agent(host: host, ref: ref) else {
            message = "\(ref) is gone"
            return
        }
        await attach(found)
    }

    public func interrupt(_ agent: Agent) async {
        do {
            let updated = try await call(AgentsInterrupt.self, AgentParams(host: agent.host, ref: agent.ref))
            replace(updated)
            message = nil
        } catch {
            message = "Can't interrupt \(agent.ref): \(describe(error))"
        }
    }

    public func delete(_ agent: Agent, force: Bool) async -> DeleteOutcome {
        do {
            _ = try await call(
                AgentsDelete.self, AgentDeleteParams(host: agent.host, ref: agent.ref, force: force ? true : nil))
            attacher.forget(agent.key)
            agents.removeAll { $0.key == agent.key }
            message = nil
            // A read already under way may have listed it before it went.
            await refresh()
            return .deleted
        } catch VibedClientError.rpc(let error) where error.code == RPCErrorCode.conflict.rawValue && !force {
            return .refused(error.message)
        } catch {
            return .failed(describe(error))
        }
    }

    /// Starts an agent, as New agent… does.
    public func start(_ params: AgentStartParams) async throws -> Agent {
        let agent = try await call(AgentsStart.self, params)
        if self.agent(host: agent.host, ref: agent.ref) == nil { agents.append(agent) }
        return agent
    }

    // MARK: Events

    private func followEvents() async {
        var backoff = 1.0
        while !Task.isCancelled {
            do {
                for try await message in daemon.events(client: "VibeOS.app") {
                    switch message {
                    case .subscribed:
                        backoff = 1
                        live = true
                        await refresh()
                    case .event(let event):
                        await handle(event)
                    }
                }
            } catch {}
            live = false
            if Task.isCancelled { return }
            // vibed went away, or isn't up yet: say so, and try again.
            await refresh()
            try? await Task.sleep(for: .seconds(backoff))
            backoff = min(backoff * 2, 10)
        }
    }

    /// Applies one event: an agent's state or status at once, from the
    /// event itself, and anything else by reading again.
    public func handle(_ event: DaemonEvent) async {
        let before = event.ref.flatMap { agent(host: event.host, ref: $0) }
        if let note = notice(for: event, agent: before, endOfTurn: wantsEndOfTurn()) {
            notifier?.post(note)
        }
        switch event.kind {
        case .agentState:
            if let state = event.state.flatMap(AgentState.init(rawValue:)), let ref = event.ref {
                update(host: event.host, ref: ref) { $0.state = state }
            }
            if before == nil { await refresh() }
        case .agentStatus:
            if let status = event.status, let ref = event.ref {
                update(host: event.host, ref: ref) { $0.status = status }
            }
            if before == nil { await refresh() }
        case .agentAdded, .agentDeleted, .agentLimit:
            await refresh()
        case .ghStatus:
            gh = (try? await call(GhStatusGet.self, GhStatusParams())) ?? gh
        case .lagged:
            settingsRevision += 1
            await refresh()
        case .projectAdded, .projectRemoved, .profilesChanged, .limitsChanged, .keysChanged:
            settingsRevision += 1
        }
    }

    private func update(host: String, ref: String, _ change: (inout Agent) -> Void) {
        guard let i = agents.firstIndex(where: { $0.host == host && $0.ref == ref }) else { return }
        change(&agents[i])
    }

    private func replace(_ agent: Agent) {
        update(host: agent.host, ref: agent.ref) { $0 = agent }
    }

    // MARK: Polling

    private func poll() async {
        while !Task.isCancelled {
            await sleepUntilNextPoll()
            if Task.isCancelled { return }
            await refresh()
        }
    }

    private func sleepUntilNextPoll() async {
        let interval = menuIsOpen ? openPoll : closedPoll
        pollWait += 1
        let wait = pollWait
        await withCheckedContinuation { (wake: CheckedContinuation<Void, Never>) in
            pollWake = wake
            Task { [weak self] in
                try? await Task.sleep(for: interval)
                if self?.pollWait == wait { self?.pollNow() }
            }
        }
    }

    /// Ends the poll's wait, so it refreshes now.
    private func pollNow() {
        pollWake?.resume()
        pollWake = nil
    }

    private func wireNotifier() {
        notifier?.onClick = { [weak self] host, ref in
            Task { await self?.attach(host: host, ref: ref) }
        }
    }
}

/// An error in a sentence for the menu.
public func describe(_ error: any Error) -> String {
    switch error {
    case let error as VibedClientError:
        if case .rpc(let rpc) = error { return rpc.message }
        return error.description
    case let error as TerminalError:
        return error.description
    default:
        return error.localizedDescription
    }
}
