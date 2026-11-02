import SwiftUI
import VibeMenu
import VibeRPC

/// New agent…: the prompt first, as it's the point, then a host, a project
/// and one of its profiles, and an optional name (PRD: Key user flows,
/// Start an agent). Return starts it, or ⌘↩ from the prompt.
struct NewAgentView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.dismissWindow) private var dismissWindow
    @Environment(\.openWindow) private var openWindow
    @State private var host = VibeRPC.localHost
    @State private var projects: [Project] = []
    @State private var loaded = false
    @State private var project = ""
    @State private var profile = ""
    @State private var prompt = ""
    @State private var name = ""
    @State private var starting = false
    @State private var error: String?
    @FocusState private var promptFocused: Bool

    private var profiles: [Profile] {
        projects.first { $0.name == project }?.profiles ?? []
    }

    private var chosen: Profile? { profiles.first { $0.name == profile } }

    private var canStart: Bool {
        !starting && !project.isEmpty && !profile.isEmpty
            && !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if loaded && projects.isEmpty && error == nil {
                noProjects
            } else {
                composer
            }
        }
        .frame(width: 540)
        .task(id: host) { await loadProjects() }
        .onChange(of: model.settingsRevision) { Task { await loadProjects() } }
        .onChange(of: project) { pickProfile() }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 16) {
            ZStack(alignment: .topLeading) {
                TextEditor(text: $prompt)
                    .font(.body)
                    .scrollContentBackground(.hidden)
                    .focused($promptFocused)
                    .padding(8)
                    .accessibilityLabel("Prompt")
                    // Its scroller shows only while it scrolls.
                    .background(OverlayScrollers(target: .nearbyTextView))
                if prompt.isEmpty {
                    Text("What should it work on?")
                        .foregroundStyle(.tertiary)
                        .padding(.horizontal, 13)
                        .padding(.vertical, 8)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
            }
            .frame(minHeight: 150)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(Color(nsColor: .textBackgroundColor))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .strokeBorder(promptFocused ? Color.accentColor.opacity(0.6) : Color.primary.opacity(0.12))
            )
            .disabled(starting)

            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 10) {
                if model.hosts.count > 1 {
                    GridRow {
                        label("Host")
                        Picker("Host", selection: $host) {
                            ForEach(model.hosts, id: \.name) { h in
                                Text(h.link == .local ? "\(h.label) (this Mac)" : h.name).tag(h.name)
                            }
                        }
                        .labelsHidden()
                        .frame(width: Self.controlWidth)
                    }
                }
                GridRow {
                    label("Project")
                    Picker("Project", selection: $project) {
                        if projects.isEmpty { Text("—").tag("") }
                        ForEach(projects, id: \.name) { p in Text(p.name).tag(p.name) }
                    }
                    .labelsHidden()
                    .frame(width: Self.controlWidth)
                }
                // The label sits by the pop-up, not by the summary under it.
                GridRow(alignment: .firstTextBaseline) {
                    label("Profile")
                    VStack(alignment: .leading, spacing: 4) {
                        Picker("Profile", selection: $profile) {
                            if profiles.isEmpty { Text("—").tag("") }
                            ForEach(profiles, id: \.name) { p in
                                Label(p.name, systemImage: environmentSymbol(p.environment)).tag(p.name)
                            }
                        }
                        .labelsHidden()
                        .frame(width: Self.controlWidth)
                        if let chosen {
                            ProfileSummary(profile: chosen)
                        }
                    }
                }
                GridRow {
                    label("Name")
                    TextField("Name", text: $name, prompt: Text("Three random words"))
                        .labelsHidden()
                        .autocorrectionDisabled()
                        .frame(width: Self.controlWidth)
                        .help("The agent's ref and branch. Left empty, it gets three random words, such as quiet-amber-otter.")
                }
            }
            .disabled(starting)

            ErrorText(error: error)

            HStack {
                if starting {
                    Working(text: "Cutting a worktree, then starting Claude…")
                }
                Spacer()
                Button("Cancel") { dismissWindow(id: WindowID.newAgent) }
                    .keyboardShortcut(.cancelAction)
                // In the accent colour, live once there's a prompt. Return
                // starts it from the other fields; in the prompt, Return is
                // a new line, so it isn't the default button there, and ⌘↩
                // starts it instead.
                Button("Start Agent") { Task { await start() } }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(promptFocused ? nil : .defaultAction)
                    .disabled(!canStart)
                    .help("Return, or ⌘↩ from the prompt")
                    .background {
                        Button("Start Agent") { Task { await start() } }
                            .keyboardShortcut(.return, modifiers: .command)
                            .disabled(!canStart)
                            .opacity(0)
                            .accessibilityHidden(true)
                    }
            }
        }
        .padding(20)
        .onAppear { promptFocused = true }
    }

    /// Every control in the form is this wide, so they line up.
    private static let controlWidth: CGFloat = 300

    private func label(_ text: String) -> some View {
        Text(text)
            .foregroundStyle(.secondary)
            .gridColumnAlignment(.trailing)
    }

    /// No projects on this host: say how to add one.
    private var noProjects: some View {
        VStack(spacing: 14) {
            if model.hosts.count > 1 {
                Picker("Host", selection: $host) {
                    ForEach(model.hosts, id: \.name) { h in
                        Text(h.link == .local ? "\(h.label) (this Mac)" : h.name).tag(h.name)
                    }
                }
                .fixedSize()
            }
            ContentUnavailableView {
                Label(
                    host == VibeRPC.localHost ? "No projects yet" : "No projects on \(host) yet",
                    systemImage: "folder.badge.plus")
            } description: {
                Text("An agent works in a project: a GitHub repo or a folder. Add one in Settings and it's ready.")
            } actions: {
                Button("Open Settings") {
                    dismissWindow(id: WindowID.newAgent)
                    showSettings(.projects, with: openWindow)
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(24)
        .frame(minHeight: 260)
    }

    private func loadProjects() async {
        do {
            projects = try await model.call(ProjectsList.self, HostParams(host: host)).projects
            if !projects.contains(where: { $0.name == project }) { project = projects.first?.name ?? "" }
            pickProfile()
            error = nil
        } catch {
            projects = []
            self.error = describe(error)
        }
        loaded = true
    }

    private func pickProfile() {
        if !profiles.contains(where: { $0.name == profile }) { profile = profiles.first?.name ?? "" }
    }

    private func start() async {
        guard canStart else { return }
        starting = true
        defer { starting = false }
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        do {
            _ = try await model.start(
                AgentStartParams(
                    host: host, project: project, profile: profile, prompt: prompt,
                    name: trimmed.isEmpty ? nil : trimmed))
            prompt = ""
            name = ""
            error = nil
            dismissWindow(id: WindowID.newAgent)
        } catch {
            self.error = describe(error)
        }
    }
}

/// A profile in a line: `Linux · Opus · High · Auto · 4G`, warning of
/// bypass on macOS.
struct ProfileSummary: View {
    let profile: Profile

    var body: some View {
        HStack(spacing: 6) {
            Text(profileSummary(profile))
                .font(.subheadline)
                .foregroundStyle(.secondary)
            if profile.mode == .bypass && profile.environment == .macos {
                Label("acts as you", systemImage: "exclamationmark.shield")
                    .font(.subheadline)
                    .foregroundStyle(Palette.attention)
                    .help("Bypass skips every permission prompt, and a macOS agent acts as you.")
            }
        }
        .accessibilityElement(children: .combine)
    }
}
