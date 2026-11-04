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
    /// This host's Linux toolbox: its versions and what it's doing.
    @Published public private(set) var toolbox: ToolboxStatus? {
        // What Save & Close last failed at is past once the toolbox moves on.
        didSet { if toolbox?.state != oldValue?.state { set(\.toolboxCloseError, nil) } }
    }
    /// While Save & Close runs: from the click until vibed answers. Kept
    /// here, not in the pane, so it outlasts a look at another pane.
    @Published public private(set) var toolboxClosing = false
    /// Why the last Save & Close didn't save, unless the last run says so.
    @Published public private(set) var toolboxCloseError: String?
    /// Where this host serves MCP, for the setup command and Settings.
    @Published public private(set) var mcp: McpSettings?
    @Published public private(set) var memory: HostMemory?
    /// Whether events are coming in; without them only the poll updates
    /// the menu.
    @Published public private(set) var live = false
    /// A note for the menu: the last action that failed, and why.
    @Published public var message: String?
    /// Bumped by every event about settings, so open settings re-read.
    @Published public private(set) var settingsRevision = 0
    /// Refs being attached, so a second click waits for the first.
    @Published public private(set) var attaching: Set<String> = []
    /// How this copy of the app is installed (Install.swift).
    @Published public private(set) var install: InstallStatus = .notManaged
    /// While adding a Linux host waits for Local Network access: why, and
    /// how to turn it on (LocalNetwork.swift).
    @Published public private(set) var localNetworkOff: String?

    /// While the menu is open it polls more often, for memory bars.
    public var menuIsOpen = false {
        didSet { if menuIsOpen && !oldValue { pollNow() } }
    }

    /// While the menu is said to be closed, how often a poll still publishes
    /// the memory readings. Often enough that if it was never told the menu
    /// opened (it goes by SwiftUI's `onAppear`), they go slow rather than
    /// stop; rarely enough that they don't keep rebuilding Settings (#54).
    public var closedReadingsEvery: Duration = .seconds(30)
    private var readingsPublished: ContinuousClock.Instant?

    public let daemon: any DaemonConnection
    public let attacher: Attacher
    public let installer: any Installer
    /// Asks for Local Network access before a host is added; none, in
    /// tests that don't, asks nothing.
    public let localNetwork: (any LocalNetworkGate)?
    /// The add whose wait for Local Network access `localNetworkOff` is.
    private var askingForLocalNetwork: UUID?
    /// How the last register went, which SMAppService doesn't keep.
    private var registered: ServiceStatus?
    /// Whether vibed's LaunchAgent was restarted for an updated app, which
    /// happens once a launch at most.
    private var restartedForUpdate = false
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
    /// While a refresh is reading, the rows' changes from events and calls
    /// meanwhile, to apply again over what it read: the listing can be
    /// older than they are.
    private var changedDuringRefresh: [(host: String, ref: String, change: (inout Agent) -> Void)]?

    public init(
        daemon: any DaemonConnection, terminal: TerminalScripting, notifier: Notifier? = nil,
        installer: (any Installer)? = nil, localNetwork: (any LocalNetworkGate)? = nil,
        vibePath: String = defaultVibePath(), socket: String? = attachSocket(),
        openPoll: Duration = .seconds(3), closedPoll: Duration = .seconds(30),
        endOfTurn: @escaping () -> Bool = { UserDefaults.standard.bool(forKey: Preferences.notifyEndOfTurn) }
    ) {
        self.daemon = daemon
        self.attacher = Attacher(terminal: terminal, vibePath: vibePath, socket: socket)
        // Made here, not as a default: a default argument is evaluated off
        // the main actor.
        self.installer = installer ?? FixedInstaller()
        self.localNetwork = localNetwork
        self.notifier = notifier
        self.openPoll = openPoll
        self.closedPoll = closedPoll
        self.wantsEndOfTurn = endOfTurn
        wireNotifier()
    }

    /// Starts following vibed: its events, and the poll. Once.
    public func start() {
        guard tasks.isEmpty else { return }
        tasks.append(Task { [weak self] in await self?.firstLaunch() })
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

    /// The same, for a call that may take minutes: one that goes to a Linux
    /// host over SSH, or that clones, cuts a worktree or runs setup there.
    public func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params, timeout: TimeInterval) async throws
        -> M.Result
    {
        let daemon = self.daemon
        return try await Task.detached { try daemon.call(method, params, timeout: timeout) }.value
    }

    /// How long a call that does real work, on `host`, may take: a Linux
    /// host's starts, adds and deletes run over its link, and dial it first.
    public nonisolated static func patience(for host: String?) -> TimeInterval {
        (host ?? VibeRPC.localHost) == VibeRPC.localHost ? 5 * 60 : 60 * 60
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
        changedDuringRefresh = []
        defer { changedDuringRefresh = nil }
        // The memory readings change on every poll, and only the menu shows
        // them: on every poll while it's open, slowly while it's not.
        let clock = ContinuousClock()
        let readingsDue = menuIsOpen || readingsPublished.map { $0.duration(to: clock.now) >= closedReadingsEvery } ?? true
        if readingsDue {
            readingsPublished = clock.now
            set(\.memory, HostMemory.current())
        }
        let hosts: [HostInfo]
        do {
            hosts = try await call(HostsList.self, NoParams()).hosts
        } catch {
            set(\.connection, .unavailable(describe(error)))
            return
        }
        var agents: [Agent] = []
        var unavailable = Set<String>()
        for host in hosts {
            // A host whose link is down is listed as vibed last saw it, so
            // its rows show, unavailable, even after the app restarts.
            if host.link == .down { unavailable.insert(host.name) }
            do {
                agents += try await call(AgentsList.self, HostParams(host: host.name)).agents
                continue
            } catch {
                if host.link == .local {
                    set(\.connection, .unavailable(describe(error)))
                    return
                }
            }
            // One vibed can't list keeps the rows the menu had, unavailable.
            unavailable.insert(host.name)
            agents += self.agents.filter { $0.host == host.name }
        }
        if readingsDue || hosts.map(\.withoutReadings) != self.hosts.map(\.withoutReadings) {
            set(\.hosts, hosts)
        }
        if readingsDue || agents.map(\.withoutReadings) != self.agents.map(\.withoutReadings) {
            set(\.agents, agents)
        }
        for changed in changedDuringRefresh ?? [] {
            apply(host: changed.host, ref: changed.ref, changed.change)
        }
        changedDuringRefresh = []
        set(\.unavailableHosts, unavailable)
        set(\.connection, .connected)
        takeOverIfUpdated()
        set(\.gh, (try? await call(GhStatusGet.self, GhStatusParams())) ?? gh)
        set(\.toolbox, (try? await call(ToolboxStatusGet.self, NoParams())) ?? toolbox)
        set(\.mcp, (try? await call(McpGet.self, HostParams())) ?? mcp)
    }

    /// Sets `keyPath` to `value` only if that changes it, so a poll that
    /// finds nothing new publishes nothing, and the views watching the
    /// model aren't rebuilt for it (#54).
    private func set<T: Equatable>(_ keyPath: ReferenceWritableKeyPath<MenuModel, T>, _ value: T) {
        if self[keyPath: keyPath] != value { self[keyPath: keyPath] = value }
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
            let session = try await call(
                SessionsAttach.self, SessionParams(host: agent.host, ref: agent.ref),
                timeout: agent.isLocal ? 30 : 5 * 60)
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
                AgentsDelete.self, AgentDeleteParams(host: agent.host, ref: agent.ref, force: force ? true : nil),
                timeout: MenuModel.patience(for: agent.host))
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

    /// Opens the Linux toolbox shell in Terminal, as Settings → Linux
    /// Toolbox → Open Shell does: raises its tab if one is attached, or
    /// opens one running `vibe toolbox`, which boots it and attaches. Twice
    /// gives one tab.
    public func openToolbox() async {
        // The terminals attached to its shell, if it runs, from the
        // session list, which changes nothing: toolbox.open could boot it.
        let sessions = (try? await call(SessionsList.self, HostParams()))?.sessions ?? []
        let attached = sessions.first { $0.ref == Attacher.toolboxRef && $0.state == .running }?.ttys ?? []
        do {
            _ = try attacher.openToolbox(attached: attached)
            message = nil
        } catch {
            message = "Can't open the toolbox: \(describe(error))"
        }
    }

    /// Whether Open Shell can open the toolbox now: it's closed, or open
    /// already; an update, a rebuild or a save holds it.
    public var toolboxCanOpen: Bool {
        !toolboxClosing && (toolbox?.state == .closed || toolbox?.state == .open)
    }

    /// Reads the toolbox's status again, as Settings does while it shows
    /// it, so what it says is what vibed says now.
    public func refreshToolbox() async {
        set(\.toolbox, (try? await call(ToolboxStatusGet.self, NoParams())) ?? toolbox)
    }

    /// Settings → Linux Toolbox → Save & Close: hangs up on the open shell,
    /// as closing its Terminal window does, and saves the template and the
    /// Linux home, as exiting does. It waits for the save, which can take
    /// minutes after a big install. A click while one runs does nothing.
    public func closeToolbox() async {
        guard !toolboxClosing else { return }
        toolboxClosing = true
        defer { toolboxClosing = false }
        set(\.toolboxCloseError, nil)
        do {
            set(\.toolbox, try await call(ToolboxClose.self, NoParams(), timeout: MenuModel.saveTimeout))
        } catch {
            await refreshToolbox()
            // A failed save is the last run, which the pane shows already.
            let why = describe(error)
            set(\.toolboxCloseError, why == toolbox?.last?.error ? nil : why)
        }
    }

    /// How long Save & Close may take: as long as `vibe toolbox` waits for
    /// a save after a big install.
    public nonisolated static let saveTimeout: TimeInterval = 15 * 60

    /// What the Linux home holds, and whether Linux agents are signed in.
    /// It boots nothing, so it's fine to ask whenever Settings shows it.
    public func toolboxLogins() async throws -> ToolboxLogins {
        try await call(ToolboxLoginsGet.self, NoParams())
    }

    /// Runs `vibe <words>` in a new Terminal window, where it can take as
    /// long as it needs and show how it goes, as Rebuild Template and Log
    /// In to Claude do. With `exec`, the window's shell becomes it. `words`
    /// are shell words, as `Attacher.vibeCommand` takes them.
    public func runInTerminal(_ words: [String], exec: Bool) {
        runCommandInTerminal((exec ? "exec " : "") + attacher.vibeCommand(words))
    }

    /// Runs a shell command line in a new Terminal window, in the owner's
    /// own shell: `gh auth login`, say.
    public func runCommandInTerminal(_ command: String) {
        do {
            do {
                _ = try attacher.terminal.run(command)
            } catch TerminalError.notPermitted {
                try attacher.terminal.runWithoutScripting(command)
            }
            message = nil
        } catch {
            message = "Can't open Terminal: \(describe(error))"
        }
    }

    /// Turns MCP on this Mac's Tailscale address on or off, as Settings
    /// does.
    public func setMCPTailscale(_ on: Bool) async throws {
        mcp = try await call(McpSet.self, McpSetParams(tailscale: on))
    }

    // MARK: Linux hosts

    /// How long Add Linux Host may take: it uploads vibed over SSH, which
    /// on a slow link takes minutes.
    public nonisolated static let installTimeout: TimeInterval = 30 * 60

    /// Add Linux Host…: vibed installs itself on the server over SSH and
    /// links to it. The answer says what the server still needs.
    ///
    /// A server on the local network needs the app's Local Network access,
    /// which only the app can ask for: it asks first, and while access is
    /// off says so in `localNetworkOff` and waits for it. Then vibed adds
    /// the host either way; if access is still off, its answer says so.
    public func addHost(name: String, address: String) async throws -> HostInstall {
        if let localNetwork {
            let asking = UUID()
            askingForLocalNetwork = asking
            _ = await localNetwork.prepare(address) { [weak self] in
                Task { @MainActor [weak self] in
                    guard let self, self.askingForLocalNetwork == asking else { return }
                    self.localNetworkOff = LocalNetwork.offMessage(address)
                }
            }
            if askingForLocalNetwork == asking {
                askingForLocalNetwork = nil
                localNetworkOff = nil
            }
            try Task.checkCancellation()
        }
        let installed = try await call(
            HostsAdd.self, HostAddParams(name: name, address: address), timeout: MenuModel.installTimeout)
        settingsRevision += 1
        await refresh()
        return installed
    }

    /// Installs this Mac's vibed on a Linux host again, if it runs another.
    public func upgradeHost(_ name: String) async throws -> HostInstall {
        let installed = try await call(
            HostsUpgrade.self, HostNameParams(host: name), timeout: MenuModel.installTimeout)
        await refresh()
        return installed
    }

    /// The Linux host that's the external address, if one is.
    public var externalHost: HostInfo? { hosts.first { $0.external } }

    /// Settings → Hosts → Use as External Address: `host` serves
    /// `https://<domain>/mcp` and this Mac keeps a tunnel to it; with no
    /// domain, it stops. Turning it on for one host turns it off for any
    /// other.
    public func setExternal(_ host: String, domain: String?) async throws -> HostInfo {
        let info = try await call(
            HostsExternal.self, HostExternalParams(host: host, on: domain != nil, domain: domain), timeout: 60)
        await refresh()
        return info
    }

    /// Forgets a Linux host; it runs on there, on its own.
    public func removeHost(_ name: String) async throws {
        _ = try await call(HostsRemove.self, HostNameParams(host: name))
        agents.removeAll { $0.host == name }
        settingsRevision += 1
        await refresh()
    }

    /// Starts an agent, as New agent… does.
    public func start(_ params: AgentStartParams) async throws -> Agent {
        let agent = try await call(AgentsStart.self, params, timeout: MenuModel.patience(for: params.host))
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
                        set(\.live, true)
                        await refresh()
                    case .event(let event):
                        await handle(event)
                    }
                }
            } catch {}
            set(\.live, false)
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
            set(\.gh, (try? await call(GhStatusGet.self, GhStatusParams())) ?? gh)
        case .lagged:
            settingsRevision += 1
            await refresh()
        case .projectAdded, .projectRemoved, .profilesChanged, .limitsChanged, .keysChanged:
            settingsRevision += 1
        case .toolboxChanged:
            set(\.toolbox, (try? await call(ToolboxStatusGet.self, NoParams())) ?? toolbox)
        case .mcpChanged:
            // Where it's served, and whether it is, for the header.
            settingsRevision += 1
            await refresh()
        case .hostAdded, .hostRemoved, .hostLink:
            // A host's link came or went: its rows, and the header.
            settingsRevision += 1
            await refresh()
        }
    }

    private func update(host: String, ref: String, _ change: @escaping (inout Agent) -> Void) {
        apply(host: host, ref: ref, change)
        changedDuringRefresh?.append((host: host, ref: ref, change: change))
    }

    private func apply(host: String, ref: String, _ change: (inout Agent) -> Void) {
        guard let i = agents.firstIndex(where: { $0.host == host && $0.ref == ref }) else { return }
        var changed = agents[i]
        change(&changed)
        if changed != agents[i] { agents[i] = changed }
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
/// What a poll compares of a host when its memory readings aren't due:
/// they change every time.
extension HostInfo {
    fileprivate var withoutReadings: HostInfo {
        var host = self
        host.memory?.used = 0
        host.memory?.swapUsed = 0
        return host
    }
}

extension Agent {
    fileprivate var withoutReadings: Agent {
        var agent = self
        agent.memoryUsed = nil
        return agent
    }
}

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

// MARK: Install and setup

extension MenuModel {
    /// The first launch, and every launch after: where this copy is
    /// installed, registers vibed's LaunchAgent if it isn't, which starts
    /// it, and links vibe if that needs no password. A copy moved, or
    /// turned off in Login Items, is put right the same way.
    public func firstLaunch() async {
        if installer.location == .applications {
            if installer.service == .notRegistered {
                registered = installer.registerService()
            }
            if installer.command == .missing {
                try? installer.linkCommand(askingForPassword: false)
            }
        }
        await refreshInstall()
    }

    /// Reads how this copy is installed again.
    public func refreshInstall() async {
        var status = await installer.currentStatus()
        if status.service == .notRegistered, case .failed = registered {
            status.service = registered ?? status.service
        }
        if install != status { install = status }
    }

    /// Registers vibed's LaunchAgent again, as Try Again does.
    public func registerService() async {
        registered = installer.registerService()
        await refreshInstall()
        await refresh()
    }

    public func openLoginItems() {
        installer.openLoginItems()
    }

    /// Does what an install note's button says.
    public func act(on note: InstallNote) async {
        switch note.action {
        case .reveal: WorktreeActions.reveal(Bundle.main.bundlePath)
        case .loginItems: installer.openLoginItems()
        case .register: await registerService()
        }
    }

    /// Adds the vibe command, asking for a password if macOS needs one.
    public func linkCommand() async {
        do {
            try installer.linkCommand(askingForPassword: true)
            message = nil
        } catch let e as InstallError where e.description == "cancelled" {
            // The owner said no to the password prompt.
        } catch {
            message = "Can't add the vibe command: \(describe(error))"
        }
        await refreshInstall()
        settingsRevision += 1
    }

    /// Asks macOS for permission to control Terminal, which it does once;
    /// after a no, opens the settings where it's turned back on.
    public func allowTerminal() async {
        if install.terminal == .denied {
            installer.openAutomationSettings()
            return
        }
        _ = await installer.askTerminalPermission()
        await refreshInstall()
        settingsRevision += 1
    }

    /// Has vibed fix a doctor check, and reads the setup again.
    public func fix(_ check: CheckId) async {
        do {
            _ = try await call(DoctorFix.self, DoctorFixParams(check: check), timeout: 120)
            message = nil
        } catch {
            message = "Can't fix that: \(describe(error))"
        }
        settingsRevision += 1
    }

    /// Notes that something setup lists may have changed, so the welcome
    /// reads again: a step done in Terminal or Settings, say.
    public func setupMayHaveChanged() {
        settingsRevision += 1
    }

    /// After an update the running vibed is the old one, until its
    /// LaunchAgent restarts it from the new app. Agents keep running
    /// through that (PRD: Restarts). Only an older vibed is restarted, once
    /// a launch: never one that's newer, as a development build may be.
    func takeOverIfUpdated(appVersion mine: String? = appVersion()) {
        guard !restartedForUpdate, install.service == .enabled,
            let running = localHost?.version, let mine, versionIsOlder(running, than: mine)
        else { return }
        restartedForUpdate = true
        do {
            try installer.restartService()
        } catch {
            message = "This VibeOS is \(mine), but vibed is still \(running): \(describe(error))"
        }
    }
}

/// This app's version, from its Info.plist; none for a bare executable.
public func appVersion(_ bundle: Bundle = .main) -> String? {
    bundle.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
}
