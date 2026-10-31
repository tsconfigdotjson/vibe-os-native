import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// Settings… (PRD: Menu bar, Footer): API keys, hosts, projects and their
/// profiles, and host limits. Every value is vibed's; nothing is kept here
/// but the menu's own preferences.
struct SettingsView: View {
    @ObservedObject var model: MenuModel

    var body: some View {
        TabView {
            KeysTab(model: model).tabItem { Label("API Keys", systemImage: "key") }
            HostsTab(model: model).tabItem { Label("Hosts", systemImage: "server.rack") }
            ProjectsTab(model: model).tabItem { Label("Projects", systemImage: "folder") }
            LimitsTab(model: model).tabItem { Label("Host Limits", systemImage: "gauge.with.dots.needle.33percent") }
            MenuTab().tabItem { Label("Menu", systemImage: "menubar.rectangle") }
        }
        .padding()
        .frame(minWidth: 640, minHeight: 460)
    }
}

/// An error line, if there's one.
struct ErrorText: View {
    let error: String?

    var body: some View {
        if let error {
            Text(error)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

// MARK: API keys

struct KeysTab: View {
    @ObservedObject var model: MenuModel
    @State private var keys: [ApiKey] = []
    @State private var newName = ""
    @State private var created: KeyCreated?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("A key is equivalent to a shell on this Mac. Every key has full access; revoke one that leaks.")
                .font(.callout)
                .foregroundStyle(.secondary)
            Table(keys) {
                TableColumn("Name", value: \.name)
                TableColumn("Key") { key in Text("\(key.prefix)…").font(.system(.body, design: .monospaced)) }
                TableColumn("Created") { key in Text(day(key.createdAt)) }
                TableColumn("Last used") { key in Text(key.lastUsedAt.map(day) ?? "never") }
                TableColumn("") { key in
                    Button("Revoke") { Task { await revoke(key) } }
                }
                .width(70)
            }
            HStack {
                TextField("Name, such as laptop Claude Code", text: $newName)
                    .onSubmit { Task { await create() } }
                Button("New Key") { Task { await create() } }
                    .disabled(newName.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            if let created {
                GroupBox {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("\(created.key.name): copy it now, as it isn't shown again.").font(.callout)
                        Text(created.secret)
                            .font(.system(.body, design: .monospaced))
                            .textSelection(.enabled)
                        HStack {
                            Button("Copy") { WorktreeActions.copy(created.secret) }
                            Button("Copy Setup Command") {
                                WorktreeActions.copy(MCPSetup.command(secret: created.secret))
                            }
                            Spacer()
                            Button("Done") { self.created = nil }
                        }
                    }
                }
            }
            ErrorText(error: error)
        }
        .task(id: model.settingsRevision) { await load() }
    }

    private func load() async {
        do {
            keys = try await model.call(KeysList.self, NoParams()).keys
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func create() async {
        let name = newName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return }
        do {
            created = try await model.call(KeysCreate.self, KeyCreateParams(name: name))
            newName = ""
            await load()
        } catch {
            self.error = describe(error)
        }
    }

    private func revoke(_ key: ApiKey) async {
        guard
            confirm(
                "Revoke \(key.name)?", detail: "Anything using \(key.prefix)… stops working at once.",
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

extension ApiKey: @retroactive Identifiable {
    public var id: String { name }
}

/// The day of an RFC 3339 time, for a table.
func day(_ time: String) -> String {
    parseTime(time).map { $0.formatted(date: .abbreviated, time: .shortened) } ?? time
}

// MARK: Hosts

struct HostsTab: View {
    @ObservedObject var model: MenuModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Table(model.hosts) {
                TableColumn("Name", value: \.name)
                TableColumn("Machine", value: \.label)
                TableColumn("OS", value: \.os)
                TableColumn("Link") { host in Text(host.link.rawValue) }
                TableColumn("MCP") { host in Text(host.mcp ? "on" : "off") }
                TableColumn("External address") { host in
                    Toggle("", isOn: .constant(host.external))
                        .labelsHidden()
                        .disabled(true)
                }
            }
            Text("A Linux host with a public address can be the external address for outside MCP callers. "
                + "Adding Linux hosts and choosing the external address come with those features.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Button("Add Linux Host…") {}.disabled(true)
                Spacer()
                Button("Refresh") { Task { await model.refresh() } }
            }
        }
    }
}

extension HostInfo: @retroactive Identifiable {
    public var id: String { name }
}

// MARK: Projects and profiles

struct ProjectsTab: View {
    @ObservedObject var model: MenuModel
    @State private var host = VibeRPC.localHost
    @State private var projects: [Project] = []
    @State private var selected: String?
    @State private var source = ""
    @State private var adding = false
    @State private var error: String?

    private var project: Project? { projects.first { $0.name == selected } }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Picker("Host", selection: $host) {
                ForEach(model.hosts, id: \.name) { h in Text(h.name).tag(h.name) }
            }
            .frame(maxWidth: 260)
            HSplitView {
                List(projects, id: \.name, selection: $selected) { p in
                    VStack(alignment: .leading) {
                        Text(p.name)
                        Text(p.repo ?? p.path).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    .tag(p.name)
                }
                .frame(minWidth: 180, maxWidth: 240)
                Group {
                    if let project {
                        ProfilesEditor(model: model, host: host, project: project) { await load() }
                    } else {
                        Text("Pick a project to see its profiles.")
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
                .frame(minWidth: 360)
            }
            HStack {
                TextField("owner/name on GitHub, or a local path", text: $source)
                    .onSubmit { Task { await add() } }
                Button("Choose Folder…") { chooseFolder() }
                Button("Add") { Task { await add() } }
                    .disabled(adding || source.trimmingCharacters(in: .whitespaces).isEmpty)
                Button("Remove") { Task { await remove() } }
                    .disabled(project == nil)
            }
            if adding {
                HStack {
                    ProgressView().controlSize(.small)
                    Text("Adding \(source)…").foregroundStyle(.secondary)
                }
            }
            ErrorText(error: error)
        }
        .task(id: "\(host)#\(model.settingsRevision)") { await load() }
    }

    private func load() async {
        do {
            projects = try await model.call(ProjectsList.self, HostParams(host: host)).projects
            if selected == nil || project == nil { selected = projects.first?.name }
            error = nil
        } catch {
            self.error = describe(error)
        }
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
        let text = source.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return }
        // A path starts with / or ~; anything else is owner/name.
        let isPath = text.hasPrefix("/") || text.hasPrefix("~")
        let path = (text as NSString).expandingTildeInPath
        adding = true
        defer { adding = false }
        do {
            let added = try await model.call(
                ProjectsAdd.self,
                ProjectAddParams(host: host, repo: isPath ? nil : text, path: isPath ? path : nil))
            source = ""
            await load()
            selected = added.name
        } catch {
            self.error = describe(error)
        }
    }

    private func remove() async {
        guard let project else { return }
        guard
            confirm(
                "Remove \(project.name)?",
                detail: "VibeOS forgets it and its profiles. Its files stay at \(project.path).",
                action: "Remove")
        else { return }
        do {
            _ = try await model.call(ProjectsRemove.self, ProjectParams(host: host, project: project.name))
            selected = nil
            await load()
        } catch {
            self.error = describe(error)
        }
    }
}

/// A project's profiles, each editable in place.
struct ProfilesEditor: View {
    @ObservedObject var model: MenuModel
    let host: String
    let project: Project
    let changed: () async -> Void
    @State private var newName = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Profiles of \(project.name)").font(.headline)
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(project.profiles, id: \.self) { profile in
                        ProfileRow(model: model, host: host, project: project.name, profile: profile, changed: changed)
                    }
                }
            }
            HStack {
                TextField("New profile name", text: $newName)
                Button("New Profile") { Task { await create() } }
                    .disabled(newName.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            ErrorText(error: error)
        }
        .padding(.leading, 8)
    }

    private func create() async {
        do {
            _ = try await model.call(
                ProfilesCreate.self,
                ProfileCreateParams(
                    host: host, project: project.name, name: newName.trimmingCharacters(in: .whitespaces)))
            newName = ""
            error = nil
            await changed()
        } catch {
            self.error = describe(error)
        }
    }
}

/// One profile: environment, model, reasoning, mode and memory.
struct ProfileRow: View {
    @ObservedObject var model: MenuModel
    let host: String
    let project: String
    let profile: Profile
    let changed: () async -> Void
    @State private var name = ""
    @State private var environment = AgentEnvironment.linux
    @State private var claudeModel = ClaudeModel.opus
    @State private var reasoning = ReasoningLevel.high
    @State private var mode = PermissionMode.auto
    @State private var memory = ""
    @State private var error: String?

    private var memoryBytes: Int? { parseBytes(memory) }

    private var edited: Bool {
        name != profile.name || environment != profile.environment || claudeModel != profile.model
            || reasoning != profile.reasoning || mode != profile.mode || memoryBytes != profile.memory
    }

    var body: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    TextField("Name", text: $name)
                    TextField("Memory", text: $memory)
                        .frame(width: 80)
                        .help("1G to 1T, in whole MB, such as 4G or 6144M")
                }
                HStack {
                    Picker("", selection: $environment) {
                        ForEach(AgentEnvironment.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                    Picker("", selection: $claudeModel) {
                        ForEach(ClaudeModel.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                    Picker("", selection: $reasoning) {
                        ForEach(ReasoningLevel.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                    Picker("", selection: $mode) {
                        ForEach(PermissionMode.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                }
                .labelsHidden()
                HStack {
                    if mode == .bypass {
                        Text("bypass skips every permission prompt").font(.caption).foregroundStyle(.orange)
                    }
                    Spacer()
                    Button("Delete") { Task { await delete() } }
                    Button("Save") { Task { await save() } }
                        .disabled(!edited || memoryBytes == nil || name.isEmpty)
                }
                ErrorText(error: error)
            }
        }
        .onAppear(perform: reset)
        .onChange(of: profile) { reset() }
    }

    private func reset() {
        name = profile.name
        environment = profile.environment
        claudeModel = profile.model
        reasoning = profile.reasoning
        mode = profile.mode
        memory = formatSize(profile.memory)
    }

    private func save() async {
        guard let bytes = memoryBytes else { return }
        var params = ProfileUpdateParams(host: host, project: project, profile: profile.name)
        if name != profile.name { params.name = name }
        if environment != profile.environment { params.environment = environment }
        if claudeModel != profile.model { params.model = claudeModel }
        if reasoning != profile.reasoning { params.reasoning = reasoning }
        if mode != profile.mode { params.mode = mode }
        if bytes != profile.memory { params.memory = bytes }
        do {
            _ = try await model.call(ProfilesUpdate.self, params)
            error = nil
            await changed()
        } catch {
            self.error = describe(error)
        }
    }

    private func delete() async {
        guard
            confirm(
                "Delete the profile \(profile.name)?", detail: "Agents started from it keep their settings.",
                action: "Delete")
        else { return }
        do {
            _ = try await model.call(
                ProfilesDelete.self, ProfileParams(host: host, project: project, profile: profile.name))
            await changed()
        } catch {
            self.error = describe(error)
        }
    }
}

// MARK: Host limits

struct LimitsTab: View {
    @ObservedObject var model: MenuModel
    @State private var host = VibeRPC.localHost
    @State private var limits: HostLimits?
    /// What each row's field says, in `rows` order.
    @State private var fields: [String] = []
    @State private var error: String?
    /// The fields as last saved, to say so while they're unchanged.
    @State private var saved: [String]?

    /// Each limit: its key in `host.toml`, a label, its kind, and its value.
    private struct Field {
        enum Kind { case bytes, count, seconds }
        let key: String
        let label: String
        let kind: Kind
        let value: KeyPath<HostLimits, Int>
        let set: WritableKeyPath<LimitsSetParams, Int?>
    }

    private let rows: [Field] = [
        Field(key: "reserve_memory", label: "Memory kept for macOS and vibed", kind: .bytes,
              value: \.reserveMemory, set: \.reserveMemory),
        Field(key: "reserve_cores", label: "Cores kept for macOS", kind: .count,
              value: \.reserveCores, set: \.reserveCores),
        Field(key: "max_awake_agents", label: "Most agents awake at once", kind: .count,
              value: \.maxAwakeAgents, set: \.maxAwakeAgents),
        Field(key: "cpu_cores", label: "CPU cores per agent", kind: .count,
              value: \.cpuCores, set: \.cpuCores),
        Field(key: "max_processes", label: "Processes per macos agent", kind: .count,
              value: \.maxProcesses, set: \.maxProcesses),
        Field(key: "disk_soft", label: "Worktree disk: warn past", kind: .bytes,
              value: \.diskSoft, set: \.diskSoft),
        Field(key: "disk_hard", label: "Worktree disk: pause past", kind: .bytes,
              value: \.diskHard, set: \.diskHard),
        Field(key: "vm_disk", label: "A linux agent's own disk", kind: .bytes,
              value: \.vmDisk, set: \.vmDisk),
        Field(key: "idle_debounce", label: "Idle before sleeping", kind: .seconds,
              value: \.idleDebounce, set: \.idleDebounce),
        Field(key: "snapshot_disk", label: "Disk for all sleeping linux agents", kind: .bytes,
              value: \.snapshotDisk, set: \.snapshotDisk),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Picker("Host", selection: $host) {
                ForEach(model.hosts, id: \.name) { h in Text(h.name).tag(h.name) }
            }
            .frame(maxWidth: 260)
            Form {
                if fields.count == rows.count {
                    ForEach(rows.indices, id: \.self) { i in
                        LabeledContent(rows[i].label) {
                            TextField(rows[i].key, text: $fields[i])
                                .frame(width: 120)
                                .foregroundStyle(parse(i) == nil ? Color.red : Color.primary)
                        }
                    }
                }
            }
            .formStyle(.grouped)
            Text("Sizes such as 4G or 512M, durations such as 60s. Saved to ~/.config/vibeos/host.toml.")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack {
                if saved == fields { Text("Saved.").foregroundStyle(.secondary) }
                Spacer()
                Button("Revert") { display(limits) }
                Button("Save") { Task { await save() } }
                    .disabled(limits == nil || fields.count != rows.count || rows.indices.contains { parse($0) == nil })
            }
            ErrorText(error: error)
        }
        .task(id: "\(host)#\(model.settingsRevision)") { await load() }
        .onChange(of: host) { saved = nil }
    }

    private func text(_ row: Field, _ value: Int) -> String {
        switch row.kind {
        case .bytes: formatSize(value)
        case .count: "\(value)"
        case .seconds: "\(value)s"
        }
    }

    private func parse(_ i: Int) -> Int? {
        guard fields.indices.contains(i) else { return nil }
        let text = fields[i]
        switch rows[i].kind {
        case .bytes: return parseBytes(text)
        case .count: return Int(text.trimmingCharacters(in: .whitespaces)).flatMap { $0 >= 0 ? $0 : nil }
        case .seconds: return parseSeconds(text)
        }
    }

    private func display(_ limits: HostLimits?) {
        guard let limits else { return }
        fields = rows.map { text($0, limits[keyPath: $0.value]) }
    }

    private func load() async {
        do {
            let fresh = try await model.call(LimitsGet.self, HostParams(host: host))
            limits = fresh
            display(fresh)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func save() async {
        guard let limits else { return }
        var params = LimitsSetParams(host: host)
        for (i, row) in rows.enumerated() {
            guard let value = parse(i) else { return }
            if value != limits[keyPath: row.value] { params[keyPath: row.set] = value }
        }
        do {
            let fresh = try await model.call(LimitsSet.self, params)
            self.limits = fresh
            display(fresh)
            error = nil
            saved = fields
        } catch {
            self.error = describe(error)
        }
    }
}

// MARK: The menu's own preferences

struct MenuTab: View {
    @AppStorage(Preferences.notifyEndOfTurn) private var endOfTurn = false
    @AppStorage(Preferences.editor) private var editor = ""

    var body: some View {
        Form {
            Section("Notifications") {
                Text("A notification comes when an agent has a question, stops with an error or hits a limit. "
                    + "Clicking one opens its terminal.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Toggle("Also at the end of every turn", isOn: $endOfTurn)
            }
            Section("Open in Editor") {
                Picker("Editor", selection: $editor) {
                    Text("The first installed of VS Code, Cursor, Zed or Xcode").tag("")
                    ForEach(WorktreeActions.editors, id: \.self) { id in
                        if let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: id) {
                            Text(url.deletingPathExtension().lastPathComponent).tag(id)
                        }
                    }
                }
            }
        }
        .formStyle(.grouped)
    }
}
