import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// New agent…: the prompt first, as it's the point, then a host, a project
/// and one of its profiles, and an optional name (PRD: Key user flows,
/// Start an agent). Return starts it; Shift-Return is a new line.
struct NewAgentView: View {
    /// A pop-up's item: hosts, projects and profiles go by name.
    typealias Choice = PopUp<String>.Item

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
    /// What was refused for want of a Pi login, to offer that sign-in.
    @State private var piSignIn: PiSignInRequest?
    @State private var signingIn: PiSignInRequest?
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
        // Escape closes it, whichever state it's in.
        .onExitCommand { dismissWindow(id: WindowID.newAgent) }
        .sheet(item: $signingIn) { request in
            // Signed in, what was refused can simply be started again.
            PiSignInSheet(model: model, request: request) {
                error = nil
                piSignIn = nil
            }
        }
        .task(id: host) { await loadProjects() }
        .onChange(of: model.settingsRevision) { Task { await loadProjects() } }
        .onChange(of: project) { pickProfile() }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 18) {
            PromptField(text: $prompt, focused: $promptFocused) { Task { await start() } }
                .disabled(starting)
            // One column of right-aligned labels, every control at the
            // same x and width, the profile's summary under its pop-up.
            VStack(alignment: .leading, spacing: 10) {
                if model.hosts.count > 1 {
                    FormRow(label: "Host") {
                        PopUp(
                            label: "Host",
                            items: model.hosts.map {
                                Choice(title: $0.link == .local ? "\($0.label) (this Mac)" : $0.name, value: $0.name)
                            },
                            selection: $host)
                    }
                }
                FormRow(label: "Project") {
                    PopUp(
                        label: "Project",
                        items: projects.isEmpty
                            ? [Choice(title: "—", value: "")] : projects.map { Choice(title: $0.name, value: $0.name) },
                        selection: $project)
                }
                FormRow(label: "Profile") {
                    PopUp(
                        label: "Profile",
                        items: profiles.isEmpty
                            ? [Choice(title: "—", value: "")]
                            : profiles.map {
                                Choice(title: $0.name, value: $0.name, symbol: environmentSymbol($0.environment))
                            },
                        selection: $profile)
                }
                if let chosen {
                    FormRow(label: "") {
                        ProfileSummary(profile: chosen)
                    }
                }
                FormRow(label: "Name") {
                    TextField("Name", text: $name, prompt: Text("Optional, picked for you"))
                        .textFieldStyle(.roundedBorder)
                        .labelsHidden()
                        .autocorrectionDisabled()
                        .help("The agent's ref and branch. Left empty, it gets three random words, such as quiet-amber-otter.")
                }
            }
            .disabled(starting)

            ErrorText(error: error)
            if let piSignIn {
                Button(piSignIn.provider.map { "Sign In to \($0)…" } ?? "Sign In to a Provider…") {
                    signingIn = piSignIn
                }
            }

            HStack {
                if starting {
                    Working(text: "Cutting a worktree, then starting \(chosen?.kind.title ?? "the agent")…")
                }
                Spacer()
                Button("Cancel") { dismissWindow(id: WindowID.newAgent) }
                    .keyboardShortcut(.cancelAction)
                // The default button: Return starts it, from the prompt too,
                // where Shift-Return is the new line. ⌘↩ works as well.
                Button("Start Agent") { Task { await start() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canStart)
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
                .keyboardShortcut(.defaultAction)
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
            piSignIn = nil
            dismissWindow(id: WindowID.newAgent)
        } catch {
            self.error = describe(error)
            piSignIn = piSignInOffer(from: error).map {
                PiSignInRequest(host: host, target: $0.target, provider: $0.provider)
            }
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
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
            if profile.mode == .bypass && profile.environment == .macos {
                Label("acts as you", systemImage: "exclamationmark.shield")
                    .font(.subheadline)
                    .foregroundStyle(Palette.attention)
                    .help(
                        profile.kind == .pi
                            ? "Pi never asks before running a tool, and a macOS agent acts as you."
                            : "Bypass skips every permission prompt, and a macOS agent acts as you.")
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// The prompt, as the sheet's hero: large type, room to write, a clear focus
/// ring and a calm placeholder. Return starts the agent and Shift-Return is
/// a new line, as in Raycast and Claude.
struct PromptField: View {
    @Binding var text: String
    var focused: FocusState<Bool>.Binding
    let submit: () -> Void

    var body: some View {
        let isFocused = focused.wrappedValue
        VStack(alignment: .trailing, spacing: 6) {
            ZStack(alignment: .topLeading) {
                TextEditor(text: $text)
                    .font(.title3)
                    .scrollContentBackground(.hidden)
                    .focused(focused)
                    .padding(10)
                    .accessibilityLabel("Prompt")
                    .accessibilityHint("Return starts the agent; Shift-Return adds a new line")
                    .background(OverlayScrollers(target: .nearbyTextView))
                    .onKeyPress(.return, phases: .down) { press in
                        if press.modifiers.contains(.shift) || press.modifiers.contains(.option) { return .ignored }
                        // Mid-composition (Japanese, Chinese, Korean input),
                        // Return confirms the text: it's the input method's.
                        if let editor = NSApp.keyWindow?.firstResponder as? NSTextView, editor.hasMarkedText() {
                            return .ignored
                        }
                        submit()
                        return .handled
                    }
                if text.isEmpty {
                    Text("What should it work on?")
                        .font(.title3)
                        .foregroundStyle(Color(nsColor: .placeholderTextColor))
                        .padding(.horizontal, 15)
                        .padding(.vertical, 10)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
            }
            .frame(minHeight: 140)
            .background(
                RoundedRectangle(cornerRadius: Metrics.cardRadius, style: .continuous)
                    .fill(Color(nsColor: .textBackgroundColor))
            )
            .overlay(
                RoundedRectangle(cornerRadius: Metrics.cardRadius, style: .continuous)
                    .strokeBorder(
                        isFocused ? Color.accentColor : Color.primary.opacity(0.14), lineWidth: isFocused ? 2 : 1)
            )
            .shadow(color: isFocused ? Color.accentColor.opacity(0.22) : .clear, radius: 6)
            Text("Return to start · Shift-Return for a new line")
                .font(.caption)
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
        }
    }
}
