import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// The menu (PRD: Menu bar and Terminal attach): a header, the agents
/// grouped by host and project, and a footer. With no agents it welcomes a
/// new owner through setup, or says all is quiet; without vibed, it says so
/// and that the agents keep running.
struct MenuView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HeaderView(model: model)
                .padding(.horizontal, Metrics.gutter)
                .padding(.top, 12)
                .padding(.bottom, 10)
            Divider()
            if let message = model.message {
                InlineMessage(text: message) { model.message = nil }
                    .padding(.horizontal, 10)
                    .padding(.top, 8)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
            content
            Divider()
            FooterView(model: model)
                .padding(.vertical, 5)
        }
        .frame(width: Metrics.menuWidth)
        .animation(motion(.smooth(duration: 0.25), reduce: reduceMotion), value: model.message)
        .animation(motion(.smooth(duration: 0.25), reduce: reduceMotion), value: model.connection)
        .onAppear { model.menuIsOpen = true }
        .onDisappear { model.menuIsOpen = false }
    }

    @ViewBuilder
    private var content: some View {
        switch model.connection {
        case .connecting:
            Working(text: "Connecting to vibed…")
                .frame(maxWidth: .infinity)
                .padding(.vertical, 28)
        case .unavailable(let why):
            if model.agents.isEmpty {
                OfflineView(model: model, why: why)
            } else {
                // The last rows vibed gave, all unavailable, under a note.
                InlineMessage(
                    text: "Can't reach vibed. Your agents keep running; the menu reconnects on its own.",
                    symbol: "bolt.horizontal.circle")
                    .padding(.horizontal, 10)
                    .padding(.top, 8)
                    .help(sentence(why))
                AgentList(model: model, offline: true)
            }
        case .connected:
            if model.agents.isEmpty {
                EmptyMenuView(model: model)
            } else {
                AgentList(model: model)
            }
        }
    }
}

// MARK: Header

/// The agent count, and how the host is doing: the Mac's memory, `gh`,
/// MCP, and each Linux host's link and external address.
struct HeaderView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text("VibeOS").font(.headline)
                Spacer()
                if model.connection == .connected {
                    Text(agentSummary(model.agents))
                        .font(.callout)
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .contentTransition(.numericText())
                        .animation(motion(.snappy, reduce: reduceMotion), value: agentSummary(model.agents))
                }
            }
            if model.connection == .connected {
                let parts = headerStatus(
                    memory: model.memory, gh: model.gh, mcp: model.localHost?.mcp == true, hosts: model.hosts)
                // It wraps rather than truncates: what's cut would be the
                // end, where a host that's down shows.
                statusLine(parts)
                    .font(.subheadline)
                    .fixedSize(horizontal: false, vertical: true)
                    .help(tooltip)
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(parts.map(\.spoken).joined(separator: ", "))
            }
        }
    }

    /// The parts, one after another: quiet, with colour only where
    /// something's wrong.
    private func statusLine(_ parts: [StatusSegment]) -> Text {
        var line = Text("")
        for (i, part) in parts.enumerated() {
            let piece = Text(part.text).foregroundStyle(color(part.tone))
            line = i == 0 ? piece : Text("\(line)\(Text("  ·  ").foregroundStyle(.tertiary))\(piece)")
        }
        return line
    }

    private func color(_ tone: StatusSegment.Tone) -> Color {
        switch tone {
        case .normal: .secondary
        case .attention: Palette.attention
        case .trouble: Palette.trouble
        }
    }

    /// The line's tooltip: where MCP is, `gh`'s own words, and each host's.
    private var tooltip: String {
        var lines: [String] = []
        if model.localHost?.mcp == true { lines.append("MCP at \(model.mcp?.url ?? MCPSetup.url)") }
        if let detail = model.gh?.detail { lines.append(detail) }
        for host in model.otherHosts {
            lines.append("\(host.name): " + hostHelp(host).replacingOccurrences(of: "\n", with: "; "))
        }
        return lines.joined(separator: "\n")
    }

    /// A host's tooltip: why its link is down, or its hostname; and while
    /// it's the external address, where and how that is.
    private func hostHelp(_ host: HostInfo) -> String {
        var lines = [host.link == .down ? sentence(host.detail ?? "its link is down; it reconnects on its own") : host.label]
        if let external = externalStatus(host) {
            lines.append(host.url.map { "External address: \($0)" } ?? "External address")
            lines.append(external.summary)
            if let detail = external.detail { lines.append(sentence(detail)) }
        }
        return lines.joined(separator: "\n")
    }
}

// MARK: Agents

/// The agents, grouped by host and project. ↑ and ↓ pick a row, and ↩
/// opens it, as in a menu.
struct AgentList: View {
    @ObservedObject var model: MenuModel
    /// vibed can't be reached: every row shows, unavailable.
    var offline = false
    @State private var selection: String?
    @FocusState private var focused: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(model.groups) { host in
                        ForEach(host.projects) { project in
                            GroupHeader(
                                model: model, group: project,
                                showHost: model.groups.count > 1 || host.host != VibeRPC.localHost,
                                unavailable: isUnavailable(project.host))
                            ForEach(project.agents, id: \.key) { agent in
                                AgentRow(
                                    model: model, agent: agent,
                                    unavailable: isUnavailable(agent.host),
                                    selected: selection == agent.key
                                )
                                .id(agent.key)
                                .transition(.opacity.combined(with: .move(edge: .top)))
                            }
                        }
                    }
                }
                .padding(.top, 2)
                .padding(.bottom, 6)
                .background(OverlayScrollers())
            }
            .frame(maxHeight: 440)
            .fixedSize(horizontal: false, vertical: true)
            .focusable()
            .focused($focused)
            .focusEffectDisabled()
            .onKeyPress(.downArrow) {
                move(by: 1, proxy)
                return .handled
            }
            .onKeyPress(.upArrow) {
                move(by: -1, proxy)
                return .handled
            }
            .onKeyPress(.return) {
                guard let key = selection, let agent = openable.first(where: { $0.key == key }) else { return .ignored }
                Task { await model.attach(agent) }
                return .handled
            }
            .onAppear { focused = true }
            .onDisappear { selection = nil }
            .animation(motion(.smooth(duration: 0.3), reduce: reduceMotion), value: model.agents.map(\.key))
        }
    }

    private func isUnavailable(_ host: String) -> Bool {
        offline || model.unavailableHosts.contains(host)
    }

    /// The rows a click can open, top to bottom.
    private var openable: [Agent] {
        model.groups.flatMap { $0.projects.flatMap(\.agents) }.filter { !isUnavailable($0.host) }
    }

    private func move(by step: Int, _ proxy: ScrollViewProxy) {
        let rows = openable.map(\.key)
        guard !rows.isEmpty else { return }
        let next: Int
        if let current = selection, let index = rows.firstIndex(of: current) {
            next = min(max(index + step, 0), rows.count - 1)
        } else {
            next = step > 0 ? 0 : rows.count - 1
        }
        selection = rows[next]
        withAnimation(motion(.snappy(duration: 0.2), reduce: reduceMotion)) { proxy.scrollTo(rows[next]) }
    }
}

/// A project's heading, with its host when there's more than this Mac.
struct GroupHeader: View {
    @ObservedObject var model: MenuModel
    let group: ProjectGroup
    let showHost: Bool
    let unavailable: Bool

    var body: some View {
        HStack(spacing: 5) {
            if showHost {
                Image(systemName: group.host == VibeRPC.localHost ? "laptopcomputer" : "server.rack")
                    .imageScale(.small)
                Text(hostName)
                Text("·").foregroundStyle(.tertiary)
            }
            Text(group.project)
            if unavailable {
                Text("· unavailable").foregroundStyle(Palette.trouble)
            }
        }
        .font(.subheadline.weight(.semibold))
        .foregroundStyle(.secondary)
        .padding(.horizontal, Metrics.gutter)
        .padding(.top, 10)
        .padding(.bottom, 3)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }

    private var hostName: String {
        group.host == VibeRPC.localHost ? (model.localHost?.label ?? "This Mac") : group.host
    }
}

struct AgentRow: View {
    @ObservedObject var model: MenuModel
    let agent: Agent
    let unavailable: Bool
    var selected = false
    @AppStorage(Preferences.editor) private var editor = ""

    var body: some View {
        let lines = rowLines(agent)
        Button {
            Task { await model.attach(agent) }
        } label: {
            HStack(alignment: .top, spacing: 10) {
                StatusDot(dot: StateDot(agent.state))
                    .padding(.top, 4)
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 6) {
                        Text(agent.ref)
                            .font(.system(.callout, design: .monospaced).weight(.medium))
                            .lineLimit(1)
                            .truncationMode(.middle)
                        // The environment, as a symbol: the profile is in the
                        // tooltip and in what VoiceOver says.
                        Image(systemName: environmentSymbol(agent.environment))
                            .imageScale(.small)
                            .foregroundStyle(.tertiary)
                        Spacer(minLength: 6)
                        if model.attaching.contains(agent.key) {
                            ProgressView().controlSize(.mini)
                        } else if let age = elapsed(agent, now: model.now) {
                            Text(age)
                                .font(.subheadline)
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                    }
                    if let title = lines.title {
                        Text(title).lineLimit(1)
                    }
                    if lines.line != nil || MemoryGauge(agent) != nil {
                        HStack(spacing: 6) {
                            if let line = lines.line {
                                HStack(spacing: 4) {
                                    if let symbol = activitySymbol(agent) {
                                        Image(systemName: symbol)
                                            .imageScale(.small)
                                            .foregroundStyle(agent.state == .question ? Palette.attention : .secondary)
                                    }
                                    Text(line).lineLimit(1)
                                }
                            }
                            Spacer(minLength: 6)
                            if let gauge = MemoryGauge(agent) {
                                MemoryBar(gauge: gauge)
                            }
                        }
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                    }
                }
            }
            .padding(.horizontal, Metrics.gutter)
            .padding(.vertical, 9)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .buttonStyle(MenuRowStyle(selected: selected))
        .opacity(unavailable ? 0.45 : 1)
        .disabled(unavailable)
        .help(
            "\(agent.profile) · \(agent.environment.title)\n"
                + (unavailable ? "\(agent.host) can't be reached. Its agents keep running there." : "Click to open it in Terminal."))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(rowAccessibilityLabel(agent, now: model.now, unavailable: unavailable))
        .accessibilityHint(unavailable ? "" : "Opens it in Terminal")
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
        // The context menu's actions, for VoiceOver, on the same terms.
        .accessibilityAction {
            if !unavailable { Task { await model.attach(agent) } }
        }
        .accessibilityAction(named: "Interrupt") {
            if canInterrupt { Task { await model.interrupt(agent) } }
        }
        .accessibilityAction(named: "Copy Attach Command") { WorktreeActions.copy(agent.attach) }
        .accessibilityAction(named: "Delete Agent") {
            if !unavailable { delete() }
        }
        .contextMenu {
            Button("Interrupt", systemImage: "stop.circle") { Task { await model.interrupt(agent) } }
                .disabled(!canInterrupt)
            Divider()
            Button("Reveal Worktree in Finder", systemImage: "folder") { WorktreeActions.reveal(agent.worktree) }
                .disabled(!agent.isLocal)
            Button("Open in Editor", systemImage: "chevron.left.forwardslash.chevron.right") {
                WorktreeActions.openInEditor(agent.worktree, preferred: editor.isEmpty ? nil : editor)
            }
            .disabled(!agent.isLocal)
            Button("Copy Attach Command", systemImage: "doc.on.doc") { WorktreeActions.copy(agent.attach) }
            Divider()
            Button("Delete Agent…", systemImage: "trash", role: .destructive) { delete() }
                .disabled(unavailable)
        }
    }

    /// Not while paused: its tree is stopped, so the Escape would wait, and
    /// cancel the turn it resumes into.
    private var canInterrupt: Bool {
        agent.state.isAwake && agent.state != .paused && !unavailable
    }

    private func delete() {
        guard
            confirm(
                "Delete \(agent.ref)?",
                detail: "This ends the agent and removes its worktree, transcripts and compartment. "
                    + "Its branch, \(agent.branch), is kept.",
                action: "Delete")
        else { return }
        Task {
            switch await model.delete(agent, force: false) {
            case .deleted:
                break
            case .refused(let why):
                if confirm(
                    "\(agent.ref) has work that isn't pushed", detail: sentence(why) + "\n\nDelete it anyway?",
                    action: "Delete Anyway")
                {
                    if case .failed(let why) = await model.delete(agent, force: true) {
                        model.message = "Couldn't delete \(agent.ref): \(why)"
                    }
                }
            case .failed(let why):
                model.message = "Couldn't delete \(agent.ref): \(why)"
            }
        }
    }
}

// MARK: Empty, first run and offline

/// No agents: a welcome through setup until it's done, then a quiet night.
struct EmptyMenuView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.openWindow) private var openWindow
    @State private var setup: SetupProgress?
    @State private var loaded = false

    var body: some View {
        Group {
            if !loaded {
                ProgressView().controlSize(.small).frame(maxWidth: .infinity).padding(.vertical, 40)
            } else if let setup, !setup.complete {
                WelcomeView(setup: setup) { act(on: $0) }
            } else {
                QuietView { show(WindowID.newAgent, with: openWindow) }
            }
        }
        // Again on each open and each settings change, so a step just done
        // in Settings or Terminal shows as done.
        .task(id: model.settingsRevision) {
            setup = await loadSetup()
            loaded = true
        }
    }

    private func loadSetup() async -> SetupProgress? {
        do {
            let projects = try await model.call(ProjectsList.self, HostParams()).projects
            let keys = try await model.call(KeysList.self, NoParams()).keys
            var claude: Bool?
            if let toolbox = model.toolbox, toolbox.state != .unavailable {
                claude = (try? await model.toolboxLogins())?.claude
            }
            return SetupProgress(hasProject: !projects.isEmpty, hasKey: !keys.isEmpty, linuxClaude: claude)
        } catch {
            return nil
        }
    }

    private func act(on step: SetupProgress.Step.ID) {
        switch step {
        case .project: showSettings(.projects, with: openWindow)
        case .claude: model.runInTerminal(["setup-token"], exec: true)
        case .key: showSettings(.keys, with: openWindow)
        }
    }
}

/// The first run: the mark lights a window for each step done.
struct WelcomeView: View {
    let setup: SetupProgress
    let act: (SetupProgress.Step.ID) -> Void

    var body: some View {
        VStack(spacing: 14) {
            BrandMark(lit: setup.done, size: 64)
            VStack(spacing: 4) {
                Text("Welcome to VibeOS").font(.title3.weight(.semibold))
                Text("A few steps and this Mac is ready to host agents.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            VStack(spacing: 0) {
                ForEach(Array(setup.steps.enumerated()), id: \.element.id) { index, step in
                    Button { act(step.id) } label: {
                        SetupStepRow(step: step, number: index + 1)
                    }
                    .buttonStyle(MenuRowStyle())
                    .disabled(step.done)
                }
            }
        }
        .padding(.top, 20)
        .padding(.bottom, 10)
        .frame(maxWidth: .infinity)
    }
}

struct SetupStepRow: View {
    let step: SetupProgress.Step
    let number: Int

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            ZStack {
                if step.done {
                    Image(systemName: "checkmark.circle.fill")
                        .font(.title3)
                        .foregroundStyle(Palette.signal)
                        .transition(.scale.combined(with: .opacity))
                } else {
                    Circle().strokeBorder(Color.secondary.opacity(0.5), lineWidth: 1.25)
                    Text("\(number)").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                }
            }
            .frame(width: 20, height: 20)
            VStack(alignment: .leading, spacing: 2) {
                Text(step.title)
                    .foregroundStyle(step.done ? Color.secondary : Color.primary)
                Text(step.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 4)
            if !step.done {
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .padding(.top, 3)
            }
        }
        .padding(.horizontal, Metrics.gutter)
        .padding(.vertical, 7)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Step \(number), \(step.title)\(step.done ? ", done" : ""). \(step.detail)")
    }
}

/// Set up, and nothing running.
struct QuietView: View {
    let newAgent: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            BrandMark(lit: 0, size: 56)
            VStack(spacing: 4) {
                Text("All quiet").font(.title3.weight(.semibold))
                Text("No agents running. Start one here, or from any MCP client.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Button("New Agent…", action: newAgent)
                .buttonStyle(.borderedProminent)
        }
        .padding(.vertical, 22)
        .padding(.horizontal, 28)
        .frame(maxWidth: .infinity)
    }
}

/// vibed can't be reached. The agents don't depend on the menu, so it says
/// that first.
struct OfflineView: View {
    @ObservedObject var model: MenuModel
    let why: String
    @State private var trying = false

    var body: some View {
        VStack(spacing: 10) {
            BrandMark(lit: 0, size: 44)
                .saturation(0)
                .opacity(0.6)
            Text("Can't reach vibed").font(.headline)
            Text("Your agents keep running. The menu reconnects on its own.")
                .font(.callout)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            Button {
                Task {
                    trying = true
                    await model.refresh()
                    trying = false
                }
            } label: {
                if trying { ProgressView().controlSize(.small) } else { Text("Try Again") }
            }
            .disabled(trying)
            DisclosureGroup("Details") {
                Text(sentence(why))
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 4)
            }
            .font(.subheadline)
            .foregroundStyle(.secondary)
            .frame(maxWidth: 280)
            .help(sentence(why))
        }
        .padding(.vertical, 22)
        .padding(.horizontal, 28)
        .frame(maxWidth: .infinity)
    }
}

// MARK: Footer

struct FooterView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let connected = model.connection == .connected
        VStack(alignment: .leading, spacing: 0) {
            Button { show(WindowID.newAgent, with: openWindow) } label: {
                MenuItemLabel(title: "New Agent…", symbol: "plus", trailing: "⌘N")
            }
            .buttonStyle(MenuRowStyle())
            .keyboardShortcut("n")
            .disabled(!connected)
            Menu {
                Button("Open Shell") { Task { await model.openToolbox() } }
                    .disabled(!model.toolboxCanOpen)
                // In Terminal, as it can take many minutes and says how it goes.
                Button("Rebuild Template") { model.runInTerminal(["toolbox", "rebuild"], exec: false) }
                    .disabled(model.toolbox?.state != .closed)
                Button("See Logins…") { show(WindowID.logins, with: openWindow) }
                    .disabled(model.toolbox == nil || model.toolbox?.state == .unavailable)
                let lines = toolboxLines(model.toolbox)
                if !lines.isEmpty {
                    Divider()
                    ForEach(lines, id: \.self) { Text($0) }
                }
                Divider()
                Button("Log In to Claude for Linux Agents…") { model.runInTerminal(["setup-token"], exec: true) }
            } label: {
                MenuItemLabel(title: "Linux Toolbox", symbol: "shippingbox", trailingSymbol: "chevron.right")
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            .menuIndicator(.hidden)
            .modifier(HoverRow())
            Button { show(WindowID.addHost, with: openWindow) } label: {
                MenuItemLabel(title: "Add Linux Host…", symbol: "server.rack")
            }
            .buttonStyle(MenuRowStyle())
            .disabled(!connected)
            Button { show(WindowID.settings, with: openWindow) } label: {
                MenuItemLabel(title: "Settings…", symbol: "gearshape", trailing: "⌘,")
            }
            .buttonStyle(MenuRowStyle())
            .keyboardShortcut(",")
            Divider()
                .padding(.horizontal, Metrics.gutter)
                .padding(.vertical, 4)
            // Quitting the menu leaves vibed and every agent running.
            Button { NSApplication.shared.terminate(nil) } label: {
                MenuItemLabel(title: "Quit Menu", symbol: "power", trailing: "⌘Q")
            }
            .buttonStyle(MenuRowStyle())
            .keyboardShortcut("q")
            .help("vibed and your agents keep running")
        }
    }
}
