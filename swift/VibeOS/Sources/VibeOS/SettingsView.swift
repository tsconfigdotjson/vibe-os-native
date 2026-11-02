import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// Settings… (PRD: Menu bar, Footer): projects and their profiles, API keys,
/// hosts and external access, host limits, and the menu's own preferences,
/// in a sidebar as System Settings has. Every value is vibed's; nothing is
/// kept here but the menu's own preferences. Fields save themselves.
enum SettingsPane: String, CaseIterable, Identifiable {
    case projects, keys, hosts, limits, general

    /// Where the last pane shown is kept, so Settings opens where it was
    /// and the menu can open it on a pane.
    static let storageKey = "settingsPane"

    var id: Self { self }

    var title: String {
        switch self {
        case .projects: "Projects"
        case .keys: "API Keys"
        case .hosts: "Hosts"
        case .limits: "Host Limits"
        case .general: "General"
        }
    }

    var symbol: String {
        switch self {
        case .projects: "folder.fill"
        case .keys: "key.fill"
        case .hosts: "server.rack"
        case .limits: "gauge.with.dots.needle.67percent"
        case .general: "gearshape.fill"
        }
    }

    var color: Color {
        switch self {
        case .projects: .blue
        case .keys: .orange
        case .hosts: .indigo
        case .limits: .green
        case .general: .gray
        }
    }
}

struct SettingsView: View {
    @ObservedObject var model: MenuModel
    /// A pane to show instead of the one last shown, for the gallery.
    var showing: SettingsPane?
    @AppStorage(SettingsPane.storageKey) private var stored = SettingsPane.projects.rawValue

    private var pane: Binding<SettingsPane?> {
        Binding(
            get: { showing ?? SettingsPane(rawValue: stored) ?? .projects },
            set: { if let pane = $0 { stored = pane.rawValue } })
    }

    var body: some View {
        NavigationSplitView {
            List(SettingsPane.allCases, selection: pane) { item in
                Label {
                    Text(item.title)
                } icon: {
                    IconTile(symbol: item.symbol, color: item.color)
                }
                .tag(item)
            }
            .navigationSplitViewColumnWidth(min: 180, ideal: 200, max: 240)
            .toolbar(removing: .sidebarToggle)
        } detail: {
            switch pane.wrappedValue ?? .projects {
            case .projects: NavigationStack { ProjectsPane(model: model) }
            case .keys: KeysPane(model: model)
            case .hosts: HostsPane(model: model)
            case .limits: LimitsPane(model: model)
            case .general: GeneralPane()
            }
        }
        .frame(minWidth: 720, minHeight: 480)
    }
}

/// Picks one of the hosts, where a pane is per host.
struct HostPicker: View {
    @ObservedObject var model: MenuModel
    @Binding var host: String

    var body: some View {
        Picker("Host", selection: $host) {
            ForEach(model.hosts, id: \.name) { h in
                Text(h.link == .local ? "\(h.label) (this Mac)" : h.name).tag(h.name)
            }
        }
    }
}

// MARK: Projects and profiles

struct ProjectsPane: View {
    @ObservedObject var model: MenuModel
    @State private var host = VibeRPC.localHost
    @State private var projects: [Project] = []
    @State private var loaded = false
    @State private var source = ""
    @State private var adding = false
    @State private var error: String?

    private var trimmed: String { source.trimmingCharacters(in: .whitespaces) }
    private var local: Bool { host == VibeRPC.localHost }

    var body: some View {
        Form {
            if model.hosts.count > 1 {
                Section { HostPicker(model: model, host: $host) }
            }
            Section("Projects") {
                if projects.isEmpty {
                    Text(loaded ? "No projects here yet. Add one below." : "Loading…")
                        .foregroundStyle(.secondary)
                }
                ForEach(projects, id: \.name) { project in
                    NavigationLink {
                        ProjectDetail(model: model, host: host, name: project.name)
                    } label: {
                        ProjectLabel(project: project)
                    }
                }
            }
            Section {
                HStack {
                    TextField("Project", text: $source, prompt: Text("owner/name, or a folder"))
                        .labelsHidden()
                        .autocorrectionDisabled()
                        .onSubmit { Task { await add() } }
                    if local {
                        Button("Choose…") { chooseFolder() }
                    }
                    Button("Add") { Task { await add() } }
                        .disabled(adding || trimmed.isEmpty)
                }
                .disabled(adding)
                if adding { Working(text: "Adding \(trimmed)…") }
                ErrorText(error: error)
            } header: {
                Text("Add a Project")
            } footer: {
                Text(
                    local
                        ? "A GitHub repo is cloned with this Mac's gh login into ~/VibeOS/projects; a folder stays "
                            + "where it is. It starts with two profiles, one per environment."
                        : "A GitHub repo is cloned with \(host)'s own gh login; a path is a folder on \(host). "
                            + "It starts with one Linux profile."
                )
                .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Projects")
        .task(id: "\(host)#\(model.settingsRevision)") { await load() }
    }

    private func load() async {
        do {
            projects = try await model.call(ProjectsList.self, HostParams(host: host)).projects
            error = nil
        } catch {
            projects = []
            self.error = describe(error)
        }
        loaded = true
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add"
        NSApp.activate()
        if panel.runModal() == .OK, let url = panel.url { source = url.path }
    }

    private func add() async {
        let text = trimmed
        guard !text.isEmpty, !adding else { return }
        // A path starts with / or ~; anything else is owner/name. A path on
        // a Linux host is that host's, so ~ there isn't this Mac's home.
        let isPath = text.hasPrefix("/") || text.hasPrefix("~")
        let path = local ? (text as NSString).expandingTildeInPath : text
        adding = true
        defer { adding = false }
        do {
            _ = try await model.call(
                ProjectsAdd.self,
                ProjectAddParams(host: host, repo: isPath ? nil : text, path: isPath ? path : nil),
                timeout: MenuModel.patience(for: host))
            source = ""
            error = nil
            await load()
        } catch {
            self.error = describe(error)
        }
    }
}

struct ProjectLabel: View {
    let project: Project

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: projectSymbol(project))
                .foregroundStyle(.secondary)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 2) {
                Text(project.name)
                Text(project.repo ?? project.path)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer()
            Text(project.profiles.count == 1 ? "1 profile" : "\(project.profiles.count) profiles")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}

/// A project's kind, as a glyph: a GitHub repo's branch, or a folder.
func projectSymbol(_ project: Project) -> String {
    project.source == .github ? "arrow.triangle.branch" : "folder.fill"
}

/// A project's page header: its name, where it comes from, and where it is.
struct ProjectHeader: View {
    let project: Project

    var body: some View {
        HStack(spacing: 12) {
            IconTile(symbol: projectSymbol(project), color: .blue, size: 40)
            VStack(alignment: .leading, spacing: 3) {
                Text(project.name).font(.title2.weight(.semibold))
                Text(project.repo.map { "github.com/\($0)" } ?? "A folder")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Text(project.path)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 6)
        .accessibilityElement(children: .combine)
    }
}

/// One project: its profiles, each edited in place, and its removal.
struct ProjectDetail: View {
    @ObservedObject var model: MenuModel
    let host: String
    let name: String
    @State private var project: Project?
    /// The profiles open for editing.
    @State private var expanded: Set<String>
    @State private var newProfile = ""
    @State private var error: String?
    @Environment(\.dismiss) private var dismiss

    init(model: MenuModel, host: String, name: String, expanded: Set<String> = []) {
        self.model = model
        self.host = host
        self.name = name
        _expanded = State(initialValue: expanded)
    }

    var body: some View {
        Form {
            if let project {
                Section {
                    ProjectHeader(project: project)
                }
                Section {
                    ForEach(project.profiles, id: \.name) { profile in
                        ProfileRow(
                            model: model, host: host, project: name, profile: profile,
                            expanded: Binding(
                                get: { expanded.contains(profile.name) },
                                set: { open in
                                    if open { expanded.insert(profile.name) } else { expanded.remove(profile.name) }
                                }),
                            renamed: { new in
                                // The old name is kept too, so the row stays
                                // open while the rename is on its way.
                                if expanded.contains(profile.name) { expanded.insert(new) }
                            }
                        ) { await load() }
                    }
                } header: {
                    Text("Profiles")
                } footer: {
                    Text("Click a profile to change it. Memory is a Linux VM's RAM, or a macOS agent's hard limit.")
                        .foregroundStyle(.secondary)
                }
                Section {
                    HStack {
                        TextField("New profile", text: $newProfile, prompt: Text("Name, such as reviewer"))
                            .labelsHidden()
                            .autocorrectionDisabled()
                            .onSubmit { Task { await create() } }
                        Button("Add Profile") { Task { await create() } }
                            .disabled(newProfile.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                } header: {
                    Text("Add a Profile")
                } footer: {
                    Text("It starts as Linux, Opus, High, Auto and 4G. Change any of it above.")
                        .foregroundStyle(.secondary)
                }
                Section {
                    Button("Remove Project…", role: .destructive) { Task { await remove(project) } }
                    ErrorText(error: error)
                } footer: {
                    Text("VibeOS forgets the project and its profiles; its files stay at \(project.path). "
                        + "A project with agents can't be removed.")
                        .foregroundStyle(.secondary)
                }
            } else if let error {
                ErrorText(error: error)
            } else {
                ProgressView().frame(maxWidth: .infinity)
            }
        }
        .formStyle(.grouped)
        .navigationTitle(name)
        .task(id: model.settingsRevision) { await load() }
    }

    private func load() async {
        do {
            project = try await model.call(ProjectsList.self, HostParams(host: host)).projects.first { $0.name == name }
            if project == nil { dismiss() }
        } catch {
            self.error = describe(error)
        }
    }

    private func create() async {
        let profile = newProfile.trimmingCharacters(in: .whitespaces)
        guard !profile.isEmpty else { return }
        do {
            _ = try await model.call(ProfilesCreate.self, ProfileCreateParams(host: host, project: name, name: profile))
            newProfile = ""
            error = nil
            await load()
        } catch {
            self.error = describe(error)
        }
    }

    private func remove(_ project: Project) async {
        guard
            confirm(
                "Remove \(project.name)?",
                detail: "VibeOS forgets it and its profiles. Its files stay at \(project.path), and adding it "
                    + "again picks them up.",
                action: "Remove")
        else { return }
        do {
            _ = try await model.call(ProjectsRemove.self, ProjectParams(host: host, project: project.name))
            dismiss()
        } catch {
            self.error = describe(error)
        }
    }
}

/// One profile: environment, model, reasoning, mode and memory (PRD:
/// Profiles). Each change is sent as it's made.
struct ProfileRow: View {
    @ObservedObject var model: MenuModel
    let host: String
    let project: String
    let profile: Profile
    @Binding var expanded: Bool
    /// Called with the new name once a rename took, so the row stays open.
    var renamed: (String) -> Void = { _ in }
    let changed: () async -> Void
    /// What a change just made shows until vibed's answer comes back.
    @State private var pending: Profile?
    @State private var error: String?

    private var shown: Profile { pending ?? profile }

    /// `macos` is the Mac's alone (PRD: Core concepts, Environment).
    private var environments: [AgentEnvironment] {
        host == VibeRPC.localHost ? AgentEnvironment.allCases : [.linux]
    }

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            LabeledContent("Name") {
                CommitField(
                    title: "Name", value: profile.name, width: 220,
                    isValid: { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
                ) { text in
                    let name = text.trimmingCharacters(in: .whitespaces)
                    // Before the reload the update makes, so the row is
                    // open under its new name when it comes back.
                    renamed(name)
                    return await update { $0.name = name }
                }
            }
            Picker("Environment", selection: binding(\.environment, \.environment)) {
                ForEach(environments, id: \.self) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            Picker("Model", selection: binding(\.model, \.model)) {
                ForEach(ClaudeModel.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            Picker("Reasoning", selection: binding(\.reasoning, \.reasoning)) {
                ForEach(ReasoningLevel.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            Picker("Mode", selection: binding(\.mode, \.mode)) {
                ForEach(PermissionMode.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            LabeledContent("Memory") {
                CommitField(
                    title: "Memory", value: formatSize(profile.memory), width: 90,
                    help: "1G to 1T, in whole MB, such as 4G or 6144M", isValid: validMemory
                ) { text in
                    await update { $0.memory = parseBytes(text) }
                }
            }
            if shown.mode == .bypass {
                Label(
                    shown.environment == .macos
                        ? "Bypass skips every permission prompt, and a macOS agent acts as you."
                        : "Bypass skips every permission prompt; the VM is its sandbox.",
                    systemImage: "exclamationmark.shield"
                )
                .font(.callout)
                .foregroundStyle(shown.environment == .macos ? Palette.attention : Color.secondary)
            }
            ErrorText(error: error)
            HStack {
                Spacer()
                Button("Delete Profile…", role: .destructive) { Task { await delete() } }
                    .accessibilityLabel("Delete profile \(profile.name)")
            }
        } label: {
            // A click anywhere on the row opens it, as a button, so VoiceOver
            // has it too.
            Button { expanded.toggle() } label: { summary }
                .buttonStyle(.plain)
                .accessibilityHint(expanded ? "Closes its settings" : "Opens its settings")
        }
    }

    private var summary: some View {
        HStack(spacing: 10) {
            Image(systemName: environmentSymbol(shown.environment))
                .foregroundStyle(.secondary)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 2) {
                Text(profile.name)
                Text(profileSummary(shown))
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
            if shown.mode == .bypass && shown.environment == .macos {
                Image(systemName: "exclamationmark.shield")
                    .foregroundStyle(Palette.attention)
                    .help("Bypass on macOS: this agent acts as you")
                    .accessibilityLabel("Bypass on macOS")
            }
        }
        .contentShape(Rectangle())
    }

    private func validMemory(_ text: String) -> Bool {
        guard let bytes = parseBytes(text) else { return false }
        return bytes >= 1 << 30 && bytes <= 1 << 40 && bytes % (1 << 20) == 0
    }

    /// A picker's binding: it shows the change at once, then sends it.
    private func binding<T: Equatable>(
        _ key: WritableKeyPath<Profile, T>, _ param: WritableKeyPath<ProfileUpdateParams, T?>
    ) -> Binding<T> {
        Binding(
            get: { shown[keyPath: key] },
            set: { value in
                guard value != shown[keyPath: key] else { return }
                var next = shown
                next[keyPath: key] = value
                pending = next
                Task { await update { $0[keyPath: param] = value } }
            })
    }

    @discardableResult
    private func update(_ edit: (inout ProfileUpdateParams) -> Void) async -> Bool {
        var params = ProfileUpdateParams(host: host, project: project, profile: profile.name)
        edit(&params)
        do {
            _ = try await model.call(ProfilesUpdate.self, params)
            error = nil
            await changed()
            pending = nil
            return true
        } catch {
            pending = nil
            self.error = describe(error)
            return false
        }
    }

    private func delete() async {
        guard
            confirm(
                "Delete the profile \(profile.name)?", detail: "Agents started from it keep their settings.",
                action: "Delete")
        else { return }
        do {
            _ = try await model.call(ProfilesDelete.self, ProfileParams(host: host, project: project, profile: profile.name))
            await changed()
        } catch {
            self.error = describe(error)
        }
    }
}

// MARK: API keys

struct KeysPane: View {
    @ObservedObject var model: MenuModel
    @State private var keys: [ApiKey] = []
    @State private var loaded = false
    @State private var creating = false
    @State private var error: String?

    var body: some View {
        Form {
            Section {
                if keys.isEmpty {
                    Text(loaded ? "No keys yet. An MCP client needs one to reach your agents." : "Loading…")
                        .foregroundStyle(.secondary)
                }
                ForEach(keys, id: \.name) { key in
                    KeyRow(key: key) { Task { await revoke(key) } }
                }
            } header: {
                Text("Keys")
            } footer: {
                Text(keysNote).foregroundStyle(.secondary)
            }
            Section {
                Button("New Key…") { creating = true }
                ErrorText(error: error)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("API Keys")
        .sheet(isPresented: $creating, onDismiss: { Task { await load() } }) {
            NewKeySheet(model: model)
        }
        .task(id: model.settingsRevision) { await load() }
    }

    private var keysNote: String {
        let reach = model.externalHost.map { "this Mac, and through \($0.name)" } ?? "this Mac"
        return "A key is as good as a shell on \(reach). Revoke one you no longer trust, and it stops at once."
    }

    private func load() async {
        do {
            keys = try await model.call(KeysList.self, NoParams()).keys
            error = nil
        } catch {
            self.error = describe(error)
        }
        loaded = true
    }

    private func revoke(_ key: ApiKey) async {
        guard
            confirm(
                "Revoke \(key.name)?",
                detail: "Anything using \(key.prefix)… stops working at once, on this Mac and every Linux host.",
                action: "Revoke")
        else { return }
        do {
            _ = try await model.call(KeysRevoke.self, KeyParams(name: key.name))
            await load()
        } catch {
            self.error = describe(error)
        }
    }
}

struct KeyRow: View {
    let key: ApiKey
    let revoke: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "key.horizontal.fill")
                .foregroundStyle(.secondary)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 2) {
                Text(key.name)
                Text("\(Text(key.prefix + "…").font(.caption.monospaced()))\(Text(" · " + details).font(.caption))")
                    .foregroundStyle(.secondary)
            }
            // The name and details read as one; Revoke stays its own button.
            .accessibilityElement(children: .combine)
            Spacer()
            // Red, as it can't be undone; it asks first.
            Button("Revoke…", role: .destructive, action: revoke)
                .buttonStyle(.bordered)
                .tint(Palette.trouble)
                .accessibilityLabel("Revoke \(key.name)")
        }
    }

    private var details: String {
        let made = parseTime(key.createdAt).map { "made \($0.formatted(.dateTime.month(.abbreviated).day()))" }
        let used = key.lastUsedAt.flatMap(parseTime).map { "used \($0.formatted(.relative(presentation: .named)))" }
        return [made, used ?? "never used"].compactMap { $0 }.joined(separator: " · ")
    }
}

/// New Key…: a name, then the key, once, with lines to set clients up.
struct NewKeySheet: View {
    @ObservedObject var model: MenuModel
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var created: KeyCreated?
    @State private var working = false
    @State private var error: String?
    @State private var appeared = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    init(model: MenuModel, preview: KeyCreated? = nil) {
        self.model = model
        _created = State(initialValue: preview)
    }

    private var trimmed: String { name.trimmingCharacters(in: .whitespaces) }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if let created {
                made(created)
            } else {
                form
            }
        }
        .padding(20)
        .frame(width: 520)
        // Escape closes it, before or after the key is made.
        .onExitCommand { dismiss() }
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 12) {
                IconTile(symbol: "key.fill", color: .orange, size: 36)
                VStack(alignment: .leading, spacing: 3) {
                    Text("New API Key").font(.headline)
                    Text("Name it for where it will live. A key is as good as a shell on this Mac.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            TextField("Name", text: $name, prompt: Text("laptop Claude Code"))
                .textFieldStyle(.roundedBorder)
                .onSubmit { Task { await create() } }
            ErrorText(error: error)
            HStack {
                if working { Working(text: "Making the key…") }
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Create Key") { Task { await create() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(trimmed.isEmpty || working)
            }
        }
    }

    private func made(_ created: KeyCreated) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 12) {
                // A small success: it springs in, then bounces once.
                // Under Reduce Motion it's simply there.
                Image(systemName: "checkmark.seal.fill")
                    .font(.system(size: 30))
                    .foregroundStyle(Palette.signal)
                    .scaleEffect(appeared || reduceMotion ? 1 : 0.3)
                    .opacity(appeared || reduceMotion ? 1 : 0)
                    .symbolEffect(.bounce, value: reduceMotion ? false : appeared)
                VStack(alignment: .leading, spacing: 3) {
                    Text("\(created.key.name) is ready").font(.headline)
                    Text("Copy it now: it isn't shown again.").font(.callout).foregroundStyle(.secondary)
                }
            }
            Text(created.secret)
                .font(.system(.callout, design: .monospaced))
                .textSelection(.enabled)
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: Metrics.cardRadius, style: .continuous).fill(Color.primary.opacity(0.05)))
                .accessibilityLabel("The key")
            HStack {
                CopyButton(title: "Copy Key", text: created.secret)
                CopyButton(
                    title: "Copy Setup Command",
                    text: MCPSetup.command(secret: created.secret, url: model.mcp?.url ?? MCPSetup.url), prominent: true)
            }
            let others = otherSetups(created.secret)
            if !others.isEmpty {
                HStack {
                    Text("Also for").font(.callout).foregroundStyle(.secondary)
                    ForEach(others, id: \.title) { setup in
                        CopyButton(title: setup.title, text: setup.command)
                            .help(setup.help)
                    }
                }
            }
            Text("A setup command is a claude mcp add line: paste it where Claude Code runs.")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .task {
            try? await Task.sleep(for: .milliseconds(120))
            withAnimation(motion(.spring(response: 0.42, dampingFraction: 0.55), reduce: reduceMotion)) {
                appeared = true
            }
        }
    }

    private struct Setup {
        let title: String
        let command: String
        let help: String
    }

    /// Setup lines for clients elsewhere: on the tailnet, while MCP is
    /// served there, and anywhere, through the external address.
    private func otherSetups(_ secret: String) -> [Setup] {
        var setups: [Setup] = []
        if let tailscale = model.mcp?.tailscaleUrls.first {
            setups.append(
                Setup(
                    title: "Tailscale", command: MCPSetup.command(secret: secret, url: tailscale),
                    help: "Sets up a client on your tailnet, at \(tailscale)"))
        }
        if let host = model.externalHost, let url = host.url {
            setups.append(
                Setup(
                    title: "The Internet", command: MCPSetup.command(secret: secret, url: url),
                    help: "Sets up a client anywhere, through \(host.name), at \(url)"))
        }
        return setups
    }

    private func create() async {
        guard !trimmed.isEmpty, !working else { return }
        working = true
        defer { working = false }
        do {
            created = try await model.call(KeysCreate.self, KeyCreateParams(name: trimmed))
            error = nil
        } catch {
            self.error = describe(error)
        }
    }
}

// MARK: Hosts

struct HostsPane: View {
    @ObservedObject var model: MenuModel
    @Environment(\.openWindow) private var openWindow
    @State private var error: String?
    @State private var mcpError: String?
    /// The host whose vibed is being updated.
    @State private var updating: String?
    /// The host Use as External Address is being turned on for.
    @State private var externalFor: HostInfo?

    private var linuxHosts: [HostInfo] { model.hosts.filter { $0.link != .local } }

    var body: some View {
        Form {
            Section {
                if let mcp = model.mcp {
                    ForEach([mcp.url] + mcp.tailscaleUrls, id: \.self) { url in
                        LabeledContent(url == mcp.url ? "This Mac" : "Tailscale") {
                            HStack(spacing: 8) {
                                Text(url).font(.callout.monospaced()).textSelection(.enabled)
                                CopyButton(title: "Copy", text: url).labelStyle(.iconOnly).buttonStyle(.borderless)
                            }
                        }
                    }
                    Toggle(
                        "Also serve MCP on this Mac's Tailscale address",
                        isOn: Binding(get: { mcp.tailscale }, set: { on in Task { await setTailscale(on) } }))
                } else {
                    Text("Where MCP is served isn't known yet.").foregroundStyle(.secondary)
                }
                ErrorText(error: mcpError)
            } header: {
                Text("MCP on This Mac")
            } footer: {
                Text(mcpFooter).foregroundStyle(.secondary)
            }
            Section {
                ForEach(model.hosts, id: \.name) { host in
                    HostRow(
                        host: host, updating: updating == host.name,
                        upgrade: { Task { await upgrade(host.name) } },
                        remove: { Task { await remove(host.name) } })
                }
                Button("Add Linux Host…") { show(WindowID.addHost, with: openWindow) }
            } header: {
                Text("Hosts")
            } footer: {
                Text("A Linux host runs its agents itself, under its own logins, and this Mac's menu and MCP show "
                    + "them too. Removing one only forgets it here: its vibed and agents run on, on their own.")
                    .foregroundStyle(.secondary)
            }
            if !linuxHosts.isEmpty {
                Section {
                    Picker("Use as external address", selection: externalSelection) {
                        Text("None").tag("")
                        ForEach(linuxHosts, id: \.name) { Text($0.name).tag($0.name) }
                    }
                    if let host = model.externalHost {
                        ExternalAddressRows(host: host)
                    }
                } header: {
                    Text("External Address")
                } footer: {
                    Text(externalFooter).foregroundStyle(.secondary)
                }
            }
            if let error {
                Section { ErrorText(error: error) }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Hosts")
        .sheet(item: $externalFor) { host in
            ExternalAddressSheet(model: model, host: host)
        }
    }

    private var mcpFooter: String {
        var lines: [String] = []
        if let mcp = model.mcp, mcp.tailscale, mcp.tailscaleUrls.isEmpty {
            lines.append("This Mac has no Tailscale address now; MCP is served there once it has one.")
        }
        lines.append("Every call needs an API key. The port is mcp_port in ~/.config/vibeos/host.toml.")
        return lines.joined(separator: " ")
    }

    private var externalFooter: String {
        let name = model.externalHost?.name ?? "the host"
        return "One Linux host with a public address can be the way in for MCP clients anywhere, to its agents "
            + "and this Mac's. Clients send an API key with every call: \(name) checks it, and this Mac checks it "
            + "again for its own agents. Nothing listens publicly on this Mac; it reaches \(name) through an SSH "
            + "tunnel of its own."
    }

    /// The picker shows the external address; choosing a host asks for its
    /// public name, and None asks before it stops.
    private var externalSelection: Binding<String> {
        Binding(
            get: { model.externalHost?.name ?? "" },
            set: { name in
                if name.isEmpty {
                    if let host = model.externalHost { Task { await turnOffExternal(host) } }
                } else if let host = model.hosts.first(where: { $0.name == name }) {
                    externalFor = host
                }
            })
    }

    private func turnOffExternal(_ host: HostInfo) async {
        guard
            confirm(
                "Stop using \(host.name) as the external address?",
                detail: "MCP clients outside lose their way in, to every agent on \(host.name) and on this Mac. "
                    + "The tunnel closes and \(host.name) stops listening publicly. Keys stay as they are.",
                action: "Stop Using")
        else { return }
        do {
            _ = try await model.setExternal(host.name, domain: nil)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func upgrade(_ name: String) async {
        updating = name
        defer { updating = nil }
        do {
            _ = try await model.upgradeHost(name)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func remove(_ name: String) async {
        guard
            confirm(
                "Remove \(name)?",
                detail: "This Mac forgets it and its agents. Nothing changes on the server: vibed and its agents "
                    + "run on there, with their own MCP, and you can add it again later.",
                action: "Remove")
        else { return }
        do {
            try await model.removeHost(name)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func setTailscale(_ on: Bool) async {
        do {
            try await model.setMCPTailscale(on)
            mcpError = nil
        } catch {
            mcpError = describe(error)
        }
    }
}

extension HostInfo: @retroactive Identifiable {
    public var id: String { name }
}

/// A host: what it is, its link, its vibed, and what it still needs.
struct HostRow: View {
    let host: HostInfo
    let updating: Bool
    let upgrade: () -> Void
    let remove: () -> Void

    var body: some View {
        let local = host.link == .local
        HStack(spacing: 10) {
            IconTile(symbol: local ? "laptopcomputer" : "server.rack", color: local ? .blue : .indigo, size: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(local ? "\(host.label) (this Mac)" : host.name)
                HStack(spacing: 6) {
                    if !local {
                        HStack(spacing: 4) {
                            Circle()
                                .fill(host.link == .connected ? Palette.signal : Palette.trouble)
                                .frame(width: 6, height: 6)
                            Text(host.link == .connected ? "Connected" : "Down")
                        }
                    }
                    if !local { Text(host.mcp ? "MCP on" : "MCP off") }
                    if let version = host.version { Text("vibed \(version)") }
                    if let address = host.address { Text(address).lineLimit(1).truncationMode(.middle) }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                if !host.missing.isEmpty {
                    Label("Still needs \(host.missing.joined(separator: ", ")) on the server", systemImage: "wrench.and.screwdriver")
                        .font(.caption)
                        .foregroundStyle(Palette.attention)
                        .help("Install it on \(host.name), then log in there: gh auth login, and claude with /login")
                }
            }
            Spacer()
            if updating {
                Working(text: "Updating…")
            } else if !local {
                Menu {
                    Button("Update vibed", action: upgrade)
                    Divider()
                    Button("Remove \(host.name)…", role: .destructive, action: remove)
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .menuStyle(.button)
                .buttonStyle(.borderless)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("Update or remove \(host.name)")
                .accessibilityLabel("Actions for \(host.name)")
            }
        }
        .help(host.link == .down ? sentence(host.detail ?? "its link is down; it reconnects on its own") : "")
        .accessibilityElement(children: .contain)
    }
}

/// The external address, under its picker: where outside clients reach it,
/// and how it's doing.
struct ExternalAddressRows: View {
    let host: HostInfo

    var body: some View {
        if let url = host.url {
            LabeledContent("Address") {
                HStack(spacing: 8) {
                    Text(url).font(.callout.monospaced()).textSelection(.enabled)
                    CopyButton(title: "Copy", text: url).labelStyle(.iconOnly).buttonStyle(.borderless)
                }
            }
        }
        if let status = externalStatus(host) {
            VStack(alignment: .leading, spacing: 4) {
                Label {
                    Text(status.summary).foregroundStyle(.primary)
                } icon: {
                    Image(systemName: status.symbol).foregroundStyle(status.tone.color)
                }
                if let detail = status.detail {
                    Text(sentence(detail))
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .accessibilityElement(children: .combine)
        }
    }
}

/// Use as External Address: the public name the host is reached at. vibed
/// checks it too, and the host gets its certificate for it.
struct ExternalAddressSheet: View {
    @ObservedObject var model: MenuModel
    let host: HostInfo
    @Environment(\.dismiss) private var dismiss
    @State private var domain: String
    @State private var working = false
    @State private var error: String?

    init(model: MenuModel, host: HostInfo) {
        self.model = model
        self.host = host
        _domain = State(initialValue: suggestedDomain(fromSSH: host.address ?? ""))
    }

    private var name: String? { normalizedDomain(domain) }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 12) {
                IconTile(symbol: "globe", color: .indigo, size: 36)
                VStack(alignment: .leading, spacing: 3) {
                    Text("Use \(host.name) as the External Address").font(.headline)
                    Text("MCP clients anywhere, such as an orchestrator in the cloud, reach agents on \(host.name) "
                        + "and on this Mac at one address, with an API key.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            VStack(alignment: .leading, spacing: 6) {
                TextField("Public name", text: $domain, prompt: Text("vps.example.com"))
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                    .onSubmit { Task { await turnOn() } }
                Text("https://\(name ?? "public name")/mcp")
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                    .accessibilityLabel("Clients use https://\(name ?? "the public name")/mcp")
            }
            .disabled(working)
            Text("\(host.name) gets its certificate from Let's Encrypt, so the name must point at it and port 443 "
                + "must be open there. Nothing opens on this Mac.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let other = model.externalHost, other.name != host.name {
                Label("\(other.name) stops being the external address.", systemImage: "info.circle")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            ErrorText(error: error)
            HStack {
                if working { Working(text: "Telling \(host.name)…") }
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Turn On") { Task { await turnOn() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(working || name == nil)
            }
        }
        .padding(20)
        .frame(width: 500)
    }

    private func turnOn() async {
        guard let name, !working else { return }
        working = true
        defer { working = false }
        do {
            _ = try await model.setExternal(host.name, domain: name)
            dismiss()
        } catch {
            self.error = describe(error)
        }
    }
}

// MARK: Host limits

struct LimitsPane: View {
    @ObservedObject var model: MenuModel
    @State private var host = VibeRPC.localHost
    @State private var limits: HostLimits?
    @State private var error: String?

    /// Each limit: its key in `host.toml`, a label, its kind, and its value.
    struct Field {
        enum Kind { case bytes, count, seconds }
        let key: String
        let label: String
        let kind: Kind
        let value: KeyPath<HostLimits, Int>
        let set: WritableKeyPath<LimitsSetParams, Int?>
    }

    struct FieldGroup {
        let title: String
        let fields: [Field]
    }

    static let groups: [FieldGroup] = [
        FieldGroup(
            title: "Memory and CPU",
            fields: [
                Field(key: "reserve_memory", label: "Memory kept for macOS and vibed", kind: .bytes,
                      value: \.reserveMemory, set: \.reserveMemory),
                Field(key: "reserve_cores", label: "Cores kept for macOS", kind: .count,
                      value: \.reserveCores, set: \.reserveCores),
                Field(key: "cpu_cores", label: "CPU cores per agent", kind: .count,
                      value: \.cpuCores, set: \.cpuCores),
                Field(key: "max_awake_agents", label: "Most agents awake at once", kind: .count,
                      value: \.maxAwakeAgents, set: \.maxAwakeAgents),
                Field(key: "max_processes", label: "Processes per macOS agent", kind: .count,
                      value: \.maxProcesses, set: \.maxProcesses),
            ]),
        FieldGroup(
            title: "Disk",
            fields: [
                Field(key: "disk_soft", label: "Worktree disk: warn past", kind: .bytes,
                      value: \.diskSoft, set: \.diskSoft),
                Field(key: "disk_hard", label: "Worktree disk: pause past", kind: .bytes,
                      value: \.diskHard, set: \.diskHard),
                Field(key: "vm_disk", label: "A Linux agent's own disk", kind: .bytes,
                      value: \.vmDisk, set: \.vmDisk),
                Field(key: "snapshot_disk", label: "Disk for all sleeping Linux agents", kind: .bytes,
                      value: \.snapshotDisk, set: \.snapshotDisk),
            ]),
        FieldGroup(
            title: "Sleep",
            fields: [
                Field(key: "idle_debounce", label: "Idle before sleeping", kind: .seconds,
                      value: \.idleDebounce, set: \.idleDebounce)
            ]),
    ]

    var body: some View {
        Form {
            if model.hosts.count > 1 {
                Section { HostPicker(model: model, host: $host) }
            }
            if let limits {
                ForEach(Self.groups, id: \.title) { group in
                    Section(group.title) {
                        ForEach(group.fields, id: \.key) { field in
                            LabeledContent {
                                CommitField(
                                    title: field.label, value: text(field, limits[keyPath: field.value]), width: 90,
                                    help: "\(field.key) in host.toml", isValid: { parse(field.kind, $0) != nil }
                                ) { text in
                                    await save(field, text)
                                }
                            } label: {
                                Text(field.label)
                                Text(field.key).font(.caption.monospaced())
                            }
                        }
                    }
                }
            } else if error == nil {
                ProgressView().frame(maxWidth: .infinity)
            }
            Section {
                ErrorText(error: error)
            } footer: {
                Text("Sizes such as 4G or 512M, durations such as 60s. Saved to ~/.config/vibeos/host.toml, where "
                    + "edits by hand take effect at once too.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Host Limits")
        .task(id: "\(host)#\(model.settingsRevision)") { await load() }
    }

    private func text(_ field: Field, _ value: Int) -> String {
        switch field.kind {
        case .bytes: formatSize(value)
        case .count: "\(value)"
        case .seconds: "\(value)s"
        }
    }

    private func parse(_ kind: Field.Kind, _ text: String) -> Int? {
        switch kind {
        case .bytes: return parseBytes(text)
        case .count: return Int(text.trimmingCharacters(in: .whitespaces)).flatMap { $0 >= 0 ? $0 : nil }
        case .seconds: return parseSeconds(text)
        }
    }

    private func load() async {
        do {
            limits = try await model.call(LimitsGet.self, HostParams(host: host))
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func save(_ field: Field, _ text: String) async -> Bool {
        guard let value = parse(field.kind, text) else { return false }
        var params = LimitsSetParams(host: host)
        params[keyPath: field.set] = value
        do {
            limits = try await model.call(LimitsSet.self, params)
            error = nil
            return true
        } catch {
            self.error = describe(error)
            return false
        }
    }
}

// MARK: The menu's own preferences

struct GeneralPane: View {
    @AppStorage(Preferences.notifyEndOfTurn) private var endOfTurn = false
    @AppStorage(Preferences.editor) private var editor = ""

    var body: some View {
        Form {
            Section {
                Toggle(isOn: $endOfTurn) {
                    Text("Also when an agent finishes its turn")
                    Text("Quietly, without a sound.")
                }
            } header: {
                Text("Notifications")
            } footer: {
                Text("A notification comes when an agent has a question, stops with an error or hits a limit. "
                    + "Clicking one opens its terminal.")
                    .foregroundStyle(.secondary)
            }
            Section("Worktrees") {
                Picker("Open in Editor", selection: $editor) {
                    Text("The first of VS Code, Cursor, Zed or Xcode").tag("")
                    ForEach(WorktreeActions.editors, id: \.self) { id in
                        if let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: id) {
                            Text(url.deletingPathExtension().lastPathComponent).tag(id)
                        }
                    }
                }
            }
            Section("About") {
                HStack(spacing: 14) {
                    BrandMark(lit: 3, size: 44)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("VibeOS").font(.headline)
                        Text("Version \(version)").font(.caption).foregroundStyle(.secondary)
                        Text("Quitting the menu leaves vibed and every agent running.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                .padding(.vertical, 4)
                .accessibilityElement(children: .combine)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("General")
    }

    private var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "development build"
    }
}
