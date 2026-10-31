import SwiftUI
import VibeMenu
import VibeRPC

/// New agent…: a host, a project, one of its profiles, and the prompt
/// (PRD: Key user flows, Start an agent).
struct NewAgentView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.dismissWindow) private var dismissWindow
    @State private var host = VibeRPC.localHost
    @State private var projects: [Project] = []
    @State private var project = ""
    @State private var profile = ""
    @State private var prompt = ""
    @State private var name = ""
    @State private var starting = false
    @State private var error: String?

    private var profiles: [Profile] {
        projects.first { $0.name == project }?.profiles ?? []
    }

    private var canStart: Bool {
        !starting && !project.isEmpty && !profile.isEmpty
            && !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Form {
                Picker("Host", selection: $host) {
                    ForEach(model.hosts, id: \.name) { h in
                        Text(h.name == VibeRPC.localHost ? "\(h.label) (this Mac)" : h.name).tag(h.name)
                    }
                }
                Picker("Project", selection: $project) {
                    if projects.isEmpty { Text("No projects: add one in Settings").tag("") }
                    ForEach(projects, id: \.name) { p in Text(p.name).tag(p.name) }
                }
                Picker("Profile", selection: $profile) {
                    if profiles.isEmpty { Text("—").tag("") }
                    ForEach(profiles, id: \.name) { p in
                        Text("\(p.name) · \(p.environment.rawValue) · \(formatBytes(p.memory))").tag(p.name)
                    }
                }
                TextField("Name", text: $name, prompt: Text("three random words"))
                LabeledContent("Prompt") {
                    TextEditor(text: $prompt)
                        .font(.body)
                        .frame(minHeight: 120)
                }
            }
            .formStyle(.grouped)
            if let error {
                Text(error)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal)
            }
            HStack {
                if starting {
                    ProgressView().controlSize(.small)
                    Text("Starting: a worktree, its compartment, then Claude…")
                        .font(.callout).foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { dismissWindow(id: WindowID.newAgent) }
                    .keyboardShortcut(.cancelAction)
                Button("Start") { Task { await start() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canStart)
            }
            .padding([.horizontal, .bottom])
        }
        .frame(width: 520)
        .task(id: host) { await loadProjects() }
        .onChange(of: model.settingsRevision) { Task { await loadProjects() } }
        .onChange(of: project) { pickProfile() }
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
    }

    private func pickProfile() {
        if !profiles.contains(where: { $0.name == profile }) { profile = profiles.first?.name ?? "" }
    }

    private func start() async {
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
