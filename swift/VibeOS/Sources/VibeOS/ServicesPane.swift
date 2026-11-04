import AppKit
import AuthenticationServices
import SwiftUI
import VibeMenu
import VibeRPC

/// Settings → Services (PRD: Shared MCP services): one row per host for each
/// service, as each host holds a login of its own, so its agents keep working
/// whatever's happening to any other host, this Mac included. This app only ever
/// does the browser's part: the host makes the verifier and the exchange, and
/// no token comes here, even for this Mac's own.
struct ServicesPane: View {
    @ObservedObject var model: MenuModel
    /// What each host last said, by host; `failures` why one didn't answer.
    @State private var lists: [String: [ServiceInfo]] = [:]
    @State private var failures: [String: String] = [:]

    /// The services known, from whichever host answered first: they're the
    /// same set on all.
    private var services: [ServiceInfo] {
        for host in model.hosts {
            if let list = lists[host.name], !list.isEmpty { return list }
        }
        return []
    }

    var body: some View {
        Form {
            if services.isEmpty {
                Section {
                    if failures.isEmpty {
                        Working(text: "Asking each host…")
                    } else {
                        ForEach(failures.sorted(by: { $0.key < $1.key }), id: \.key) { host, why in
                            ErrorText(error: "\(host): \(why)")
                        }
                    }
                }
            }
            if servicesNeedRebuild(model.toolbox) {
                Section {
                    Label(
                        "Linux agents on this Mac have no shared services yet: the Linux template is on an older "
                            + "base image, and they need base \(VibeRPC.sharedServicesBase). macOS agents have them now.",
                        systemImage: "exclamationmark.triangle"
                    )
                    .foregroundStyle(Palette.attention)
                    .fixedSize(horizontal: false, vertical: true)
                    Button("Rebuild Template") { model.runInTerminal(["toolbox", "rebuild"], exec: false) }
                        .disabled(model.toolbox?.state != .closed)
                        .help("Replays toolbox.toml onto the newest base image, in Terminal")
                }
            }
            ForEach(services, id: \.name) { service in
                Section {
                    ForEach(model.hosts, id: \.name) { host in
                        ServiceHostRow(
                            model: model, host: host, service: service.name,
                            info: lists[host.name]?.first { $0.name == service.name },
                            failure: failures[host.name],
                            changed: { await load() })
                    }
                } header: {
                    Text(service.title)
                } footer: {
                    Text(footer(service)).foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Services")
        // What each host says now: a refresh or a lost login happens out of
        // sight, and a host's link comes and goes.
        .task(id: model.hosts.map(\.name)) {
            repeat {
                await model.refreshToolbox()
                await load()
                try? await Task.sleep(for: .seconds(3))
            } while !Task.isCancelled
        }
    }

    private func footer(_ service: ServiceInfo) -> String {
        "Every agent can use \(service.title), unless its profile has shared services off. Each host signs in "
            + "for itself, so its agents go on working while this Mac is away; its token stays on that host. Calls "
            + "that write go through each agent's own permission prompts, and any agent in bypass mode, or that "
            + "doesn't prompt, can act in \(service.title) as you."
    }

    /// Asks every host whose link is up, all at once: one that's slow to answer
    /// doesn't hold up what the rest say.
    private func load() async {
        let model = self.model
        await withTaskGroup(of: (String, [ServiceInfo]?, String?).self) { group in
            for host in model.hosts where host.link != .down {
                let name = host.name
                group.addTask { @MainActor in
                    do {
                        let list = try await model.call(ServicesList.self, HostParams(host: name), timeout: 20)
                        return (name, list.services, nil)
                    } catch {
                        return (name, nil, serviceFailure(error, host: name))
                    }
                }
            }
            for await (name, services, failure) in group {
                if let services { lists[name] = services }
                failures[name] = failure
            }
        }
        for host in model.hosts where host.link == .down {
            failures[host.name] = sentence(host.detail ?? "its link is down; it reconnects on its own")
        }
    }
}

/// One host's standing with one service, and what can be done about it.
private struct ServiceHostRow: View {
    @ObservedObject var model: MenuModel
    let host: HostInfo
    let service: String
    let info: ServiceInfo?
    let failure: String?
    let changed: () async -> Void

    @State private var working: String?
    @State private var error: String?
    @State private var expanded = false
    @State private var askingForKey = false
    @State private var showingCalls = false
    @StateObject private var signIn = WebSignIn()

    private var local: Bool { host.link == .local }
    private var reachable: Bool { host.link != .down }
    private var hostLabel: String { local ? "\(host.label) (this Mac)" : host.name }

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            if let info { settings(info).padding(.leading, 36) }
        } label: {
            summary
        }
        .sheet(isPresented: $askingForKey) {
            ServiceKeySheet(model: model, service: service, title: info?.title ?? service) {
                await changed()
            }
        }
    }

    private var summary: some View {
        let status = info.map(ServiceSummary.init)
        return HStack(spacing: 10) {
            IconTile(symbol: local ? "laptopcomputer" : "server.rack", color: local ? .blue : .indigo, size: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(hostLabel)
                HStack(spacing: 4) {
                    if let status {
                        Circle().fill(color(status.tone)).frame(width: 6, height: 6)
                        Text(status.status)
                        if let note = status.note { Text("· \(note)") }
                    } else if let failure, !reachable || info == nil {
                        Text(failure).lineLimit(2)
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                if let detail = status?.detail {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(status?.tone == .needsYou ? Palette.trouble : Color.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if !reachable && info != nil, let failure {
                    Text("Unavailable: \(failure)").font(.caption).foregroundStyle(.secondary)
                }
                ErrorText(error: error)
            }
            Spacer(minLength: 0)
            if let working {
                Working(text: working)
            } else if reachable, let status {
                if let wanted = status.signIn {
                    Button(wanted == .reconnect ? "Reconnect" : "Connect") { Task { await connect() } }
                        .accessibilityLabel(
                            "\(wanted == .reconnect ? "Reconnect" : "Connect") \(info?.title ?? service) on \(hostLabel)")
                }
                actions(status)
            }
        }
        .accessibilityElement(children: .contain)
    }

    private func actions(_ status: ServiceSummary) -> some View {
        Menu {
            if status.signIn == nil {
                Button("Reconnect") { Task { await connect() } }
            }
            if local {
                Button("Use a Personal API Key…") { askingForKey = true }
            }
            if status.canDisconnect {
                Divider()
                Button("Disconnect…", role: .destructive) { Task { await disconnect() } }
            }
        } label: {
            Image(systemName: "ellipsis.circle")
        }
        .menuStyle(.button)
        .buttonStyle(.borderless)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("More for \(hostLabel)")
        .accessibilityLabel("Actions for \(hostLabel)")
    }

    private func settings(_ info: ServiceInfo) -> some View {
        Group {
            Toggle(
                "On for agents here",
                isOn: Binding(get: { info.enabled }, set: { on in Task { await change { $0.enabled = on } } }))
            .help("Off, no agent on \(hostLabel) sees \(info.title)'s tools or can call them")
            Toggle(
                "Read-only",
                isOn: Binding(get: { info.readOnly }, set: { on in Task { await change { $0.readOnly = on } } }))
            .help("Agents reach \(info.title)'s read-only endpoint")
            Toggle(
                "Run tools without asking",
                isOn: Binding(get: { info.autoAllow }, set: { on in Task { await change { $0.autoAllow = on } } }))
            .help("Claude Code agents started from here on run \(info.title)'s tools, writes included, with no "
                + "permission prompt. Off, its prompts govern them.")
            LabeledContent("Denied tools") {
                CommitField(
                    title: "Denied tools", value: info.deniedTools.joined(separator: ", "), width: 260,
                    help: "Tool names, by commas: no agent here may list or call them", trailing: false,
                    isValid: { _ in true }
                ) { text in
                    await change { $0.deniedTools = parseToolNames(text) }
                }
            }
            LabeledContent("Recent calls") {
                Button("Show…") { showingCalls = true }
                    .help("The last calls agents on \(hostLabel) made, with no arguments or tokens")
            }
            .sheet(isPresented: $showingCalls) {
                ServiceCallsSheet(model: model, host: host.name, hostLabel: hostLabel)
            }
            if !local {
                LabeledContent("With this Mac away") {
                    HStack(spacing: 8) {
                        Text(setKeyCommand(service: service)).font(.callout.monospaced()).textSelection(.enabled)
                        CopyButton(title: "Copy", text: setKeyCommand(service: service))
                            .labelStyle(.iconOnly)
                            .buttonStyle(.borderless)
                    }
                }
                .help("In a shell on \(host.name): connects it with a personal API key, which stays there")
            }
        }
    }

    private func color(_ tone: ServiceSummary.Tone) -> Color {
        switch tone {
        case .good: Palette.signal
        case .working: Palette.attention
        case .needsYou: Palette.trouble
        case .off: Palette.rest
        }
    }

    /// Connect or Reconnect: the host says where to sign in, this app shows it,
    /// and what the page ends on goes back to the host. Whatever's connected
    /// now stays until that works.
    private func connect() async {
        working = "Signing in…"
        error = nil
        defer { working = nil }
        do {
            let page = try await model.call(
                ServicesConnect.self, ServiceParams(host: host.name, service: service), timeout: 90)
            guard let url = URL(string: page.url) else {
                error = "\(hostLabel) gave a page that can't be opened"
                return
            }
            let back = try await signIn.run(url: url, scheme: page.callbackScheme)
            _ = try await model.call(
                ServicesComplete.self,
                ServiceCompleteParams(host: host.name, service: service, callbackUrl: back.absoluteString),
                timeout: 90)
            await changed()
        } catch let e as ASWebAuthenticationSessionError where e.code == .canceledLogin {
            // Given up on: nothing's changed, and that's nothing to report.
        } catch {
            self.error = describe(error)
        }
    }

    private func disconnect() async {
        guard
            confirm(
                "Disconnect \(info?.title ?? service) on \(hostLabel)?",
                detail: "Agents there lose its tools until it's connected again. Other hosts keep theirs.",
                action: "Disconnect")
        else { return }
        working = "Disconnecting…"
        defer { working = nil }
        do {
            _ = try await model.call(
                ServicesDisconnect.self, ServiceParams(host: host.name, service: service), timeout: 60)
            error = nil
            await changed()
        } catch {
            self.error = describe(error)
        }
    }

    @discardableResult
    private func change(_ edit: (inout ServiceSetParams) -> Void) async -> Bool {
        var params = ServiceSetParams(host: host.name, service: service)
        edit(&params)
        do {
            _ = try await model.call(ServicesSet.self, params, timeout: 20)
            error = nil
            await changed()
            return true
        } catch {
            self.error = describe(error)
            return false
        }
    }
}

/// The last calls agents on a host made to its services, newest first. They're
/// kept on that host, as its calls are its own, and hold no argument or token.
private struct ServiceCallsSheet: View {
    @ObservedObject var model: MenuModel
    let host: String
    let hostLabel: String
    @Environment(\.dismiss) private var dismiss
    @State private var entries: [ServiceAuditEntry]?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Recent Calls on \(hostLabel)").font(.headline)
            if let entries, !entries.isEmpty {
                List(Array(entries.reversed().enumerated()), id: \.offset) { _, entry in
                    VStack(alignment: .leading, spacing: 2) {
                        Text("\(entry.service) · \(entry.tool)")
                        Text("\(entry.agent) · \(entry.outcome) · \(entry.ms) ms · \(entry.at)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                .frame(minWidth: 460, minHeight: 280)
            } else if entries != nil {
                Text("No agent here has called a service yet.").foregroundStyle(.secondary)
            } else if error == nil {
                Working(text: "Asking \(hostLabel)…")
            }
            ErrorText(error: error)
            HStack {
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .task {
            do {
                entries = try await model.call(ServicesAudit.self, HostParams(host: host), timeout: 20).entries
            } catch {
                self.error = serviceFailure(error, host: host)
            }
        }
    }
}

/// A personal API key for this Mac's own login. There's no such thing for a
/// Linux host from here: its key goes in on that host, so it never crosses the
/// link.
private struct ServiceKeySheet: View {
    @ObservedObject var model: MenuModel
    let service: String
    let title: String
    let done: () async -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var key = ""
    @State private var working = false
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Connect \(title) with an API Key").font(.headline)
            Text("It's checked with \(title), then kept in this Mac's Keychain. It's used instead of signing in.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            SecureField("Personal API key", text: $key)
                .textFieldStyle(.roundedBorder)
                .frame(minWidth: 340)
            ErrorText(error: error)
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Connect") { Task { await connect() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || working)
            }
        }
        .padding(20)
    }

    private func connect() async {
        working = true
        defer { working = false }
        do {
            _ = try await model.call(ServicesConnectKey.self, ServiceKeyParams(service: service, key: key), timeout: 90)
            key = ""
            await done()
            dismiss()
        } catch {
            self.error = describe(error)
        }
    }
}

/// The browser's part of signing in: a web authentication session, ending on
/// vibed's custom scheme. It's kept alive by the row that started it.
@MainActor
private final class WebSignIn: NSObject, ObservableObject, ASWebAuthenticationPresentationContextProviding {
    private var session: ASWebAuthenticationSession?

    /// Shows `url`, and gives the URL it ends on.
    func run(url: URL, scheme: String) async throws -> URL {
        defer { session = nil }
        return try await withCheckedThrowingContinuation { continuation in
            let answer = OnceOnly(continuation)
            let session = ASWebAuthenticationSession(url: url, callbackURLScheme: scheme) { back, error in
                if let back {
                    answer.resume(.success(back))
                } else {
                    answer.resume(.failure(error ?? ASWebAuthenticationSessionError(.canceledLogin)))
                }
            }
            session.presentationContextProvider = self
            self.session = session
            if !session.start() {
                answer.resume(.failure(ASWebAuthenticationSessionError(.presentationContextInvalid)))
            }
        }
    }

    nonisolated func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            NSApp.keyWindow ?? NSApp.windows.first(where: { $0.isVisible }) ?? ASPresentationAnchor()
        }
    }
}

/// A continuation that can't be resumed twice, should a session that wouldn't
/// start call its handler too.
private final class OnceOnly: @unchecked Sendable {
    private var continuation: CheckedContinuation<URL, any Error>?
    private let lock = NSLock()

    init(_ continuation: CheckedContinuation<URL, any Error>) {
        self.continuation = continuation
    }

    func resume(_ result: Result<URL, any Error>) {
        lock.lock()
        let waiting = continuation
        continuation = nil
        lock.unlock()
        waiting?.resume(with: result)
    }
}
