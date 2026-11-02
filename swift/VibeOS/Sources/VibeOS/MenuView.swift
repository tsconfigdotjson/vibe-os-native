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
    /// The host facts open under the header, for the gallery.
    var showFacts = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HeaderView(model: model, showFacts: showFacts)
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
            if model.agents.isEmpty, let note = InstallNote(model.install) {
                InstallNoteView(model: model, note: note)
            } else if model.agents.isEmpty {
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

/// The count of agents, then only what needs the owner, each on a row of
/// its own (PRD: Menu bar, Header). The steady facts (the Mac's memory,
/// `gh`, MCP, each Linux host's link and external address) sit behind one
/// quiet button beside the count, and open in place.
struct HeaderView: View {
    @ObservedObject var model: MenuModel
    @State private var showFacts: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    init(model: MenuModel, showFacts: Bool = false) {
        self.model = model
        _showFacts = State(initialValue: showFacts)
    }

    var body: some View {
        let connected = model.connection == .connected
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text("VibeOS").font(.headline)
                Spacer()
                if connected {
                    Text(agentSummary(model.agents))
                        .font(.callout)
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .contentTransition(.numericText())
                        .animation(motion(.snappy, reduce: reduceMotion), value: agentSummary(model.agents))
                    Button {
                        withAnimation(motion(.snappy(duration: 0.22), reduce: reduceMotion)) { showFacts.toggle() }
                    } label: {
                        Image(systemName: "info.circle")
                            .symbolVariant(showFacts ? .fill : .none)
                            .foregroundStyle(.secondary)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help(showFacts ? "Hide host status" : "Memory, GitHub, MCP and hosts")
                    .accessibilityLabel("Host status")
                    .accessibilityValue(showFacts ? "Shown" : "Hidden")
                    .accessibilityAddTraits(.isToggle)
                }
            }
            if connected {
                ForEach(headerAlerts(gh: model.gh, hosts: model.hosts)) { alert in
                    AlertRow(alert: alert)
                }
                if showFacts {
                    FactsGrid(
                        facts: headerFacts(
                            memory: model.memory, gh: model.gh, mcp: model.localHost?.mcp == true, hosts: model.hosts),
                        tooltip: tooltip
                    )
                    .transition(.opacity.combined(with: .move(edge: .top)))
                }
            }
        }
    }

    /// The facts' tooltip: where MCP is, `gh`'s own words, and each host's.
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

/// One thing that needs the owner: a glyph, what's wrong, and what it
/// means, on a tint of its colour.
struct AlertRow: View {
    let alert: HeaderAlert

    var body: some View {
        let tint = alert.tone == .trouble ? Palette.trouble : Palette.attention
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: alert.symbol)
                .foregroundStyle(tint)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                headline.font(.callout.weight(.semibold))
                Text(alert.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: Metrics.cardRadius, style: .continuous).fill(tint.opacity(0.06)))
        .overlay(
            RoundedRectangle(cornerRadius: Metrics.cardRadius, style: .continuous)
                .strokeBorder(tint.opacity(0.45), lineWidth: 0.5)
        )
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(alert.title). \(alert.detail)")
        .accessibilityHint(alert.reason ?? "")
        .help(alert.reason ?? "")
    }

    /// The host's name in monospace, as agents' refs are, then the rest.
    private var headline: Text {
        guard let name = alert.name else { return Text(alert.headline) }
        return Text("\(Text(name).font(.system(.callout, design: .monospaced).weight(.semibold))) \(alert.headline)")
    }
}

/// The steady facts, in two aligned columns.
struct FactsGrid: View {
    let facts: [HeaderFact]
    let tooltip: String

    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 5) {
            ForEach(facts) { fact in
                GridRow {
                    // One element per fact for VoiceOver: a GridRow's
                    // modifiers go to each cell, so the label cell speaks
                    // for both.
                    Label(fact.label, systemImage: fact.symbol)
                        .foregroundStyle(.secondary)
                        .accessibilityElement(children: .ignore)
                        .accessibilityLabel("\(fact.label), \(fact.value)")
                    Text(fact.value)
                        .foregroundStyle(color(fact.tone))
                        .monospacedDigit()
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .accessibilityHidden(true)
                }
            }
        }
        .font(.subheadline)
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: Metrics.cardRadius, style: .continuous).fill(Color.primary.opacity(0.05))
        )
        .help(tooltip)
    }

    private func color(_ tone: StatusTone) -> Color {
        switch tone {
        case .normal: .primary
        case .attention: Palette.attention
        case .trouble: Palette.trouble
        }
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
    /// The rows' height, to know when they run past the list's.
    @State private var contentHeight: CGFloat = 0
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
                                unavailable: isUnavailable(project.host),
                                first: project.id == model.groups.first?.projects.first?.id)
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
                // The rows alone, before the padding that depends on it.
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { contentHeight = $0 }
                .padding(.top, 2)
                // Room for the last row to clear the fade at the very end.
                .padding(.bottom, overflows ? Self.fade : 6)
                .background(OverlayScrollers())
            }
            .frame(maxHeight: Self.maxHeight)
            // A soft fade at the bottom edge while more rows are below, so
            // a cut-off group reads as more to scroll to, not a hard edge.
            .mask {
                VStack(spacing: 0) {
                    Color.black
                    LinearGradient(colors: [.black, .black.opacity(0)], startPoint: .top, endPoint: .bottom)
                        .frame(height: overflows ? Self.fade : 0)
                }
            }
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

    private static let maxHeight: CGFloat = 440
    private static let fade: CGFloat = 24

    /// Whether the rows (with their usual padding) run past the list's
    /// height, so it scrolls.
    private var overflows: Bool { contentHeight + 2 + 6 > Self.maxHeight + 1 }

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
    /// The first heading sits close under the header; each after it has
    /// room above, so groups read as groups.
    var first = false

    var body: some View {
        HStack(spacing: 5) {
            Text(group.project)
                .foregroundStyle(.primary)
            if showHost {
                Text("on \(hostName)")
            }
            if unavailable {
                Text("· unavailable").foregroundStyle(Palette.trouble)
            }
        }
        .font(.subheadline.weight(.semibold))
        .foregroundStyle(.secondary)
        .padding(.horizontal, Metrics.gutter)
        .padding(.top, first ? 8 : 16)
        .padding(.bottom, 2)
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
                            .foregroundStyle(.secondary)
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
                        Text(title)
                            .font(.body.weight(.medium))
                            .lineLimit(1)
                    }
                    // One quiet status line: what it's doing, and its
                    // memory as a thin bar that only speaks up (in words and
                    // colour) near or at its limit.
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
                                .foregroundStyle(.secondary)
                            }
                            Spacer(minLength: 6)
                            if let gauge = MemoryGauge(agent) {
                                MemoryBar(gauge: gauge, note: memoryNote(agent))
                            }
                        }
                        .font(.subheadline)
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
                WelcomeView(setup: setup) { act($0) }
            } else {
                QuietView { show(WindowID.newAgent, with: openWindow) }
            }
        }
        // Again on each open and each settings change, so a step just done
        // in Settings or Terminal shows as done; and every few seconds
        // while one is under way, as the Linux download is.
        .task(id: model.settingsRevision) {
            repeat {
                setup = await loadSetup()
                loaded = true
                guard setup?.steps.contains(where: \.isWorking) == true else { break }
                try? await Task.sleep(for: .seconds(3))
            } while !Task.isCancelled
        }
    }

    private func loadSetup() async -> SetupProgress? {
        do {
            let projects = try await model.call(ProjectsList.self, HostParams()).projects
            let keys = try await model.call(KeysList.self, NoParams()).keys
            var claude: Bool?
            var github: Bool?
            if let toolbox = model.toolbox, toolbox.state != .unavailable, let logins = try? await model.toolboxLogins() {
                claude = logins.claude
                github = logins.logins.map { found in found.contains { $0.tool == .gh } }
            }
            // A vibed from before vibe doctor has none to give.
            let doctor = (try? await model.call(DoctorCheckGet.self, NoParams(), timeout: 90))?.checks ?? []
            await model.refreshInstall()
            return SetupProgress(
                hasProject: !projects.isEmpty, hasKey: !keys.isEmpty, linuxClaude: claude, linuxGitHub: github,
                doctor: doctor, install: model.install, ghInstalled: model.gh?.state != .notInstalled)
        } catch {
            return nil
        }
    }

    private func act(_ action: SetupAction) {
        switch action {
        case .linkCommand: Task { await model.linkCommand() }
        case .allowTerminal: Task { await model.allowTerminal() }
        case .fix(let check): Task { await model.fix(check) }
        case .terminal(let line): model.runCommandInTerminal(line)
        case .vibe(let words): model.runInTerminal(words, exec: true)
        case .open(let link):
            if let url = URL(string: link) { NSWorkspace.shared.open(url) }
        case .installCommandLineTools: SetupActions.installCommandLineTools()
        case .openToolbox: Task { await model.openToolbox() }
        case .projects: showSettings(.projects, with: openWindow)
        case .keys: showSettings(.keys, with: openWindow)
        }
    }
}

/// The first run: one list, in order, of what this Mac needs and what's
/// left to set up. The checks that passed fold into one row; the next step
/// says what it does; the mark lights a window as the steps get done.
struct WelcomeView: View {
    let setup: SetupProgress
    let act: (SetupAction) -> Void

    var body: some View {
        VStack(spacing: 14) {
            BrandMark(lit: setup.lit, size: 64)
            VStack(spacing: 4) {
                Text("Welcome to VibeOS").font(.title3.weight(.semibold))
                Text("A few steps and this Mac is ready to host agents.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            ScrollView {
                VStack(spacing: 0) {
                    ForEach(setup.rows) { row in
                        switch row {
                        case .passed(let steps):
                            PassedRow(title: setup.passedTitle, steps: steps)
                        case .step(let step):
                            SetupStepRow(step: step, current: step.id == setup.next, act: act)
                        }
                    }
                }
            }
            .frame(maxHeight: 400)
            .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.top, 20)
        .padding(.bottom, 10)
        .frame(maxWidth: .infinity)
    }
}

/// This Mac's checks that passed, as one row.
struct PassedRow: View {
    let title: String
    let steps: [SetupProgress.Step]

    var body: some View {
        HStack(alignment: .center, spacing: 10) {
            Image(systemName: "checkmark.circle.fill")
                .font(.title3)
                .foregroundStyle(Palette.signal)
                .frame(width: 20, height: 20)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).foregroundStyle(.secondary)
                Text(steps.map(\.title).joined(separator: ", "))
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, Metrics.gutter)
        .padding(.vertical, 7)
        .help(steps.map { "\($0.title): \($0.detail)" }.joined(separator: "\n"))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(title): \(steps.map(\.title).joined(separator: ", "))")
    }
}

/// One step: a mark for how it stands, its title, what it does when it's
/// the next one (or under way), and its button.
struct SetupStepRow: View {
    let step: SetupProgress.Step
    /// The next step to do, which says what it does.
    let current: Bool
    let act: (SetupAction) -> Void

    var body: some View {
        HStack(alignment: .center, spacing: 10) {
            HStack(alignment: .center, spacing: 10) {
                StepMark(status: step.status)
                    .frame(width: 20, height: 20)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text(step.title)
                            .foregroundStyle(step.done ? Color.secondary : Color.primary)
                        if step.status == .optional {
                            Text("Optional")
                                .font(.caption2.weight(.medium))
                                .foregroundStyle(.secondary)
                                .padding(.horizontal, 5)
                                .padding(.vertical, 1)
                                .background(Capsule().strokeBorder(Color.secondary.opacity(0.4)))
                        }
                    }
                    if current || step.isWorking {
                        Text(step.detail)
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if let progress = step.progress {
                        ProgressView(value: Double(progress), total: 100)
                            .controlSize(.small)
                            .frame(maxWidth: 180)
                    }
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("\(step.title), \(step.statusWords)")
            .accessibilityHint(step.detail)
            Spacer(minLength: 6)
            if let action = step.action, let title = step.actionTitle {
                StepButton(title: title, prominent: current) { act(action) }
                    .accessibilityHint(step.detail)
            }
        }
        .padding(.horizontal, Metrics.gutter)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .help(step.detail)
    }
}

/// A step's button: the system's small bordered button, prominent for the
/// next step.
struct StepButton: View {
    let title: String
    let prominent: Bool
    let action: () -> Void

    var body: some View {
        if prominent {
            Button(title, action: action).buttonStyle(.borderedProminent).controlSize(.small)
        } else {
            Button(title, action: action).buttonStyle(.bordered).controlSize(.small)
        }
    }
}

/// How a step stands: a checkmark when done, a spinner while under way, a
/// dashed ring when optional, a ring when it's still to do.
struct StepMark: View {
    let status: SetupProgress.Step.Status

    var body: some View {
        switch status {
        case .done:
            Image(systemName: "checkmark.circle.fill").font(.title3).foregroundStyle(Palette.signal)
        case .working:
            ProgressView().controlSize(.small)
        case .optional:
            Image(systemName: "circle.dashed").font(.title3).foregroundStyle(.tertiary)
        case .todo:
            Image(systemName: "circle").font(.title3).foregroundStyle(.secondary)
        }
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

/// What stands between this copy and a running vibed, when it's how it's
/// installed: run from the disk image, turned off in Login Items, or a
/// LaunchAgent macOS wouldn't take.
struct InstallNote: Equatable {
    enum Action: Equatable { case loginItems, register }

    var title: String
    var detail: String
    var action: Action?
    var actionTitle: String?

    init?(_ install: InstallStatus) {
        if install.location == .notMoved {
            title = "Move VibeOS to Applications"
            detail = "VibeOS runs from your Applications folder. Drag it there from the disk image, then open it again."
        } else if install.service == .requiresApproval {
            title = "Let VibeOS run in the background"
            detail = "vibed, which runs your agents, is turned off in Login Items. Turn on VibeOS there."
            action = .loginItems
            actionTitle = "Open Login Items"
        } else if let why = install.registerFailure {
            title = "VibeOS couldn't start vibed"
            detail = sentence(why)
            action = .register
            actionTitle = "Try Again"
        } else {
            return nil
        }
    }
}

struct InstallNoteView: View {
    @ObservedObject var model: MenuModel
    let note: InstallNote

    var body: some View {
        VStack(spacing: 10) {
            BrandMark(lit: 0, size: 44)
            Text(note.title).font(.headline)
            Text(note.detail)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            if let action = note.action, let title = note.actionTitle {
                Button(title) {
                    switch action {
                    case .loginItems: model.openLoginItems()
                    case .register: Task { await model.registerService() }
                    }
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(.vertical, 22)
        .padding(.horizontal, 28)
        .frame(maxWidth: .infinity)
        .accessibilityElement(children: .contain)
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
            .help("Closes this menu. vibed and every agent keep running.")
        }
    }
}
