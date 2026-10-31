import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// The menu (PRD: Menu bar and Terminal attach): a header, the agents
/// grouped by host and project, and a footer.
struct MenuView: View {
    @ObservedObject var model: MenuModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HeaderView(model: model)
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
            Divider()
            if let message = model.message {
                HStack(alignment: .top) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    Text(message).font(.callout).fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    Button {
                        model.message = nil
                    } label: {
                        Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                Divider()
            }
            AgentList(model: model)
            Divider()
            FooterView(model: model)
                .padding(.vertical, 4)
        }
        .frame(width: 380)
        .onAppear { model.menuIsOpen = true }
        .onDisappear { model.menuIsOpen = false }
    }
}

struct HeaderView: View {
    @ObservedObject var model: MenuModel

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("VibeOS").font(.headline)
                Spacer()
                Text(agentCount).font(.callout).foregroundStyle(.secondary)
            }
            switch model.connection {
            case .connecting:
                Text("Connecting to vibed…").font(.caption).foregroundStyle(.secondary)
            case .unavailable(let why):
                Text("vibed is unavailable: \(why)")
                    .font(.caption).foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            case .connected:
                HStack(spacing: 10) {
                    if let memory = model.memory {
                        Label("\(formatBytes(memory.used)) / \(formatBytes(memory.total))", systemImage: "memorychip")
                    }
                    Label(ghText, systemImage: "person.crop.circle")
                        .foregroundStyle(model.gh?.state == .loggedIn ? Color.secondary : Color.orange)
                    Label(model.localHost?.mcp == true ? "MCP on" : "MCP off", systemImage: "point.3.connected.trianglepath.dotted")
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                .labelStyle(.titleAndIcon)
                ForEach(model.otherHosts, id: \.name) { host in
                    HStack(spacing: 4) {
                        Circle()
                            .fill(host.link == .connected ? Color.green : Color.red)
                            .frame(width: 6, height: 6)
                        Text("\(host.name): \(host.link == .connected ? "connected" : "down")")
                        if host.external { Text("· external address") }
                    }
                    .font(.caption)
                    .foregroundStyle(.secondary)
                }
            }
        }
    }

    private var agentCount: String {
        let all = model.agents.count
        let awake = model.icon.awake
        return all == 1 ? "1 agent · \(awake) awake" : "\(all) agents · \(awake) awake"
    }

    private var ghText: String {
        guard let gh = model.gh else { return "gh: …" }
        switch gh.state {
        case .loggedIn: return gh.account.map { "gh: \($0)" } ?? "gh: logged in"
        case .loggedOut: return "gh: logged out"
        case .notInstalled: return "gh: not installed"
        case .error: return "gh: error"
        case .unknown: return "gh: …"
        }
    }
}

struct AgentList: View {
    @ObservedObject var model: MenuModel

    var body: some View {
        if model.agents.isEmpty {
            Text(model.connection == .connected ? "No agents yet. Start one with New Agent…, or over MCP." : "")
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(12)
        } else {
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(model.groups) { host in
                        ForEach(host.projects) { project in
                            Text(model.groups.count > 1 || host.host != VibeRPC.localHost
                                ? "\(host.host) · \(project.project)" : project.project)
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(.secondary)
                                .padding(.horizontal, 12)
                                .padding(.top, 8)
                                .padding(.bottom, 2)
                            ForEach(project.agents, id: \.key) { agent in
                                AgentRow(
                                    model: model, agent: agent,
                                    unavailable: model.unavailableHosts.contains(agent.host))
                            }
                        }
                    }
                }
                .padding(.bottom, 6)
            }
            .frame(maxHeight: 460)
            .fixedSize(horizontal: false, vertical: true)
        }
    }
}

struct AgentRow: View {
    @ObservedObject var model: MenuModel
    let agent: Agent
    let unavailable: Bool
    @State private var hovering = false
    @AppStorage(Preferences.editor) private var editor = ""

    var body: some View {
        let lines = rowLines(agent)
        Button {
            Task { await model.attach(agent) }
        } label: {
            HStack(alignment: .top, spacing: 8) {
                StateDotView(dot: StateDot(agent.state))
                    .padding(.top, 5)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(agent.ref)
                            .font(.system(.body, design: .monospaced))
                            .lineLimit(1)
                        Text(agent.profile)
                            .font(.caption2)
                            .padding(.horizontal, 5)
                            .padding(.vertical, 1)
                            .background(Capsule().fill(Color.secondary.opacity(0.15)))
                            .lineLimit(1)
                        Spacer(minLength: 4)
                        if model.attaching.contains(agent.key) {
                            ProgressView().controlSize(.mini)
                        }
                        if let age = elapsed(agent, now: model.now) {
                            Text(age)
                                .font(.caption)
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                    }
                    if let title = lines.title {
                        Text(title).font(.callout).lineLimit(1)
                    }
                    if let line = lines.line {
                        Text(line).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    if let gauge = MemoryGauge(agent) {
                        HStack(spacing: 6) {
                            ProgressView(value: gauge.fraction)
                                .progressViewStyle(.linear)
                                .tint(gauge.fraction > 0.8 ? Color.orange : Color.accentColor)
                            Text(gauge.label)
                                .font(.caption2)
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                    }
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 6).fill(hovering ? Color.primary.opacity(0.08) : .clear))
            .padding(.horizontal, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .opacity(unavailable ? 0.5 : 1)
        .disabled(unavailable)
        .onHover { hovering = $0 }
        .help(unavailable ? "\(agent.host) is unavailable" : "Open in Terminal")
        .contextMenu {
            // Not while paused: its tree is stopped, so the Escape would
            // wait, and cancel the turn it resumes into.
            Button("Interrupt") { Task { await model.interrupt(agent) } }
                .disabled(!agent.state.isAwake || agent.state == .paused || unavailable)
            Button("Reveal Worktree in Finder") { WorktreeActions.reveal(agent.worktree) }
                .disabled(!agent.isLocal)
            Button("Open in Editor") {
                WorktreeActions.openInEditor(agent.worktree, preferred: editor.isEmpty ? nil : editor)
            }
            .disabled(!agent.isLocal)
            Button("Copy Attach Command") { WorktreeActions.copy(agent.attach) }
            Divider()
            Button("Delete Agent…", role: .destructive) { delete() }
                .disabled(unavailable)
        }
    }

    private func delete() {
        guard
            confirm(
                "Delete \(agent.ref)?",
                detail: "This ends the agent and removes its worktree, transcripts and compartment. "
                    + "Its branch \(agent.branch) is kept.",
                action: "Delete")
        else { return }
        Task {
            switch await model.delete(agent, force: false) {
            case .deleted:
                break
            case .refused(let why):
                if confirm("Delete \(agent.ref) anyway?", detail: why, action: "Delete Anyway") {
                    if case .failed(let why) = await model.delete(agent, force: true) {
                        model.message = "Can't delete \(agent.ref): \(why)"
                    }
                }
            case .failed(let why):
                model.message = "Can't delete \(agent.ref): \(why)"
            }
        }
    }
}

/// busy (pulsing), question (amber), idle (grey), asleep (hollow), paused
/// or exited (red).
struct StateDotView: View {
    let dot: StateDot
    @State private var dim = false

    var body: some View {
        Group {
            switch dot {
            case .busy:
                Circle().fill(Color.green)
                    .opacity(dim ? 0.3 : 1)
                    .animation(.easeInOut(duration: 0.8).repeatForever(autoreverses: true), value: dim)
                    .onAppear { dim = true }
            case .question: Circle().fill(Color.orange)
            case .idle: Circle().fill(Color.gray)
            case .asleep: Circle().strokeBorder(Color.secondary, lineWidth: 1.5)
            case .stopped: Circle().fill(Color.red)
            }
        }
        .frame(width: 9, height: 9)
        .accessibilityLabel(label)
    }

    private var label: String {
        switch dot {
        case .busy: "busy"
        case .question: "has a question"
        case .idle: "idle"
        case .asleep: "asleep"
        case .stopped: "paused or exited"
        }
    }
}

struct FooterView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            FooterItem(title: "New Agent…", symbol: "plus") {
                show(WindowID.newAgent, with: openWindow)
            }
            .disabled(model.connection != .connected)
            Menu {
                Button("Open Shell") { Task { await model.openToolbox() } }
                    .disabled(!canOpenToolbox)
                // In Terminal, as it can take many minutes and says how it goes.
                Button("Rebuild Template") { model.runInTerminal(["toolbox", "rebuild"], exec: false) }
                    .disabled(model.toolbox?.state != .closed)
                Button("See Logins") {}.disabled(true)
                let lines = toolboxLines(model.toolbox)
                if !lines.isEmpty {
                    Divider()
                    ForEach(lines, id: \.self) { Text($0) }
                }
                Divider()
                Button("Log In to Claude for Linux Agents…") { model.runInTerminal(["setup-token"], exec: true) }
            } label: {
                Label("Linux Toolbox", systemImage: "shippingbox")
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            .padding(.horizontal, 16)
            .padding(.vertical, 5)
            FooterItem(title: "Add Linux Host…", symbol: "server.rack") {}
                .disabled(true)
                .help("Adding Linux hosts comes with Linux host support")
            FooterItem(title: "Settings…", symbol: "gearshape") {
                show(WindowID.settings, with: openWindow)
            }
            Divider().padding(.vertical, 4)
            // Quitting the menu leaves vibed and every agent running.
            FooterItem(title: "Quit Menu", symbol: "power") {
                NSApplication.shared.terminate(nil)
            }
        }
    }

    /// The shell opens while the toolbox is closed or already open; an
    /// update, a rebuild or a save holds it.
    private var canOpenToolbox: Bool {
        guard let state = model.toolbox?.state else { return false }
        return state == .closed || state == .open
    }
}

/// A footer row that looks like a menu item.
struct FooterItem: View {
    let title: String
    let symbol: String
    let action: () -> Void
    @State private var hovering = false
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: action) {
            Label(title, systemImage: symbol)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12)
                .padding(.vertical, 5)
                .background(
                    RoundedRectangle(cornerRadius: 5)
                        .fill(hovering && isEnabled ? Color.primary.opacity(0.08) : .clear)
                )
                .padding(.horizontal, 4)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(isEnabled ? Color.primary : Color.secondary)
        .onHover { hovering = $0 }
    }
}
