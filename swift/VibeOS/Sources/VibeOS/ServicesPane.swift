import AppKit
import AuthenticationServices
import SwiftUI
import VibeMenu
import VibeRPC

/// Settings → Services (PRD: Shared MCP services): one login per install for each
/// service, on the host that owns it, with a row saying how every host stands.
/// This app only ever does the browser's part: the owner makes the verifier and
/// the exchange, and its refresh token never comes here, even when the owner's a
/// Linux host and this Mac uses the access token it's given.
struct ServicesPane: View {
    @ObservedObject var model: MenuModel
    @State private var services: [ServiceOverview] = []
    @State private var failure: String?

    var body: some View {
        Form {
            if services.isEmpty {
                Section {
                    if let failure {
                        ErrorText(error: failure)
                    } else {
                        Working(text: "Asking each host…")
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
                    ServiceConnectionRow(model: model, service: service, changed: { await load() })
                    ForEach(service.hosts, id: \.host) { host in
                        ServiceHostRow(
                            model: model, host: host, service: service.name, title: service.title,
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
        .task {
            repeat {
                await model.refreshToolbox()
                await load()
                try? await Task.sleep(for: .seconds(3))
            } while !Task.isCancelled
        }
    }

    private func footer(_ service: ServiceOverview) -> String {
        "Every agent can use \(service.title), unless its profile has shared services off. There's one login, on the "
            + "host that owns it, and only that host can renew it. Other hosts use a short-lived access token it gives "
            + "them by way of this Mac, so with this Mac away they go on until theirs runs out. Calls that write go "
            + "through each agent's own permission prompts, and any agent in bypass mode, or that doesn't prompt, can "
            + "act in \(service.title) as you."
    }

    private func load() async {
        do {
            services = try await model.call(ServicesOverview.self, NoParams(), timeout: 20).services
            failure = nil
        } catch {
            failure = describe(error)
        }
    }
}

/// The name a host goes by here.
@MainActor
private func hostLabel(_ host: String, model: MenuModel) -> String {
    guard host == "local" else { return host }
    let label = model.hosts.first { $0.link == .local }?.label ?? "This Mac"
    return "\(label) (this Mac)"
}

/// The one connection: how it stands, who owns it, and signing in.
private struct ServiceConnectionRow: View {
    @ObservedObject var model: MenuModel
    let service: ServiceOverview
    let changed: () async -> Void

    @State private var working: String?
    @State private var error: String?
    @StateObject private var signIn = WebSignIn()

    private var status: ServiceConnection { ServiceConnection(service) }
    private var ownerLabel: String { hostLabel(service.owner, model: model) }
    /// Where the login could go instead: any host that's answering.
    private var elsewhere: [String] {
        service.hosts.filter { $0.reachable && $0.host != service.owner }.map(\.host)
    }

    var body: some View {
        HStack(spacing: 10) {
            IconTile(symbol: "person.badge.key.fill", color: .purple, size: 26)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 4) {
                    Circle().fill(toneColor(status.tone)).frame(width: 6, height: 6)
                    Text(status.status)
                }
                Text(service.signedIn ? "Login on \(ownerLabel)" : "Signing in keeps the login on \(ownerLabel)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if let detail = status.detail {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(status.tone == .needsYou ? Palette.trouble : Color.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                ErrorText(error: error)
            }
            Spacer(minLength: 0)
            if let working {
                Working(text: working)
            } else {
                if let wanted = status.signIn {
                    Button(wanted == .reconnect ? "Reconnect" : "Connect") {
                        Task { await connect(on: service.owner) }
                    }
                    .accessibilityLabel("\(wanted == .reconnect ? "Reconnect" : "Connect") \(service.title)")
                }
                actions
            }
        }
        .accessibilityElement(children: .contain)
    }

    private var actions: some View {
        Menu {
            if status.signIn == nil {
                Button("Reconnect") { Task { await connect(on: service.owner) } }
            }
            if !elsewhere.isEmpty {
                Menu(service.signedIn ? "Move Login To" : "Keep Login On") {
                    ForEach(elsewhere, id: \.self) { host in
                        Button(hostLabel(host, model: model)) { Task { await move(to: host) } }
                    }
                }
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
        .help("More for \(service.title)")
        .accessibilityLabel("Actions for \(service.title)")
    }

    private func move(to host: String) async {
        if service.signedIn {
            guard
                confirm(
                    "Move \(service.title)'s login to \(hostLabel(host, model: model))?",
                    detail: "You'll sign in again in your browser. The login on \(ownerLabel) is then forgotten, and "
                        + "every other host uses the new one. That works even if \(ownerLabel) is away: it's told "
                        + "when it's next reachable.",
                    action: "Sign In")
            else { return }
        }
        await connect(on: host)
    }

    /// Connect or Reconnect: `host` says where to sign in, this app shows it, and
    /// what the page ends on goes back to `host`, which is then the owner.
    /// Whatever's connected now stays until that works.
    private func connect(on host: String) async {
        working = "Signing in…"
        error = nil
        defer { working = nil }
        do {
            let page = try await model.call(
                ServicesConnect.self, ServiceParams(host: host, service: service.name), timeout: 90)
            guard let url = URL(string: page.url) else {
                error = "\(hostLabel(host, model: model)) gave a page that can't be opened"
                return
            }
            let back = try await signIn.run(url: url, scheme: page.callbackScheme)
            _ = try await model.call(
                ServicesComplete.self,
                ServiceCompleteParams(host: host, service: service.name, callbackUrl: back.absoluteString),
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
                "Disconnect \(service.title)?",
                detail: "Every host's agents lose its tools until it's connected again.",
                action: "Disconnect")
        else { return }
        working = "Disconnecting…"
        defer { working = nil }
        do {
            _ = try await model.call(ServicesDisconnect.self, ServiceParams(service: service.name), timeout: 60)
            error = nil
            await changed()
        } catch {
            self.error = describe(error)
        }
    }
}

/// One host's standing with a service, and its own settings for it.
private struct ServiceHostRow: View {
    @ObservedObject var model: MenuModel
    let host: ServiceHost
    let service: String
    let title: String
    let changed: () async -> Void

    @State private var error: String?
    @State private var expanded = false
    @State private var showingCalls = false

    private var local: Bool { host.host == "local" }
    private var label: String { hostLabel(host.host, model: model) }

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            if let info = host.service { settings(info).padding(.leading, 36) }
        } label: {
            summary
        }
    }

    private var summary: some View {
        let status = host.service.map(ServiceSummary.init)
        return HStack(spacing: 10) {
            IconTile(symbol: local ? "laptopcomputer" : "server.rack", color: local ? .blue : .indigo, size: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(label)
                HStack(spacing: 4) {
                    if let status {
                        Circle().fill(toneColor(status.tone)).frame(width: 6, height: 6)
                        Text(status.status)
                        if let note = status.note { Text("· \(note)") }
                    } else {
                        Text("Not answering. If its link is up, it may need updating: Settings → Hosts → Update vibed.")
                            .lineLimit(2)
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                if let detail = status?.detail, host.service?.role == .follower {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(status?.tone == .needsYou ? Palette.trouble : Color.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                ErrorText(error: error)
            }
            Spacer(minLength: 0)
        }
        .accessibilityElement(children: .contain)
    }

    private func settings(_ info: ServiceInfo) -> some View {
        Group {
            Toggle(
                "On for agents here",
                isOn: Binding(get: { info.enabled }, set: { on in Task { await change { $0.enabled = on } } }))
            .help("Off, no agent on \(label) sees \(info.title)'s tools or can call them")
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
                    .help("The last calls agents on \(label) made, with no arguments or tokens")
            }
            .sheet(isPresented: $showingCalls) {
                ServiceCallsSheet(model: model, host: host.host, hostLabel: label)
            }
        }
    }

    @discardableResult
    private func change(_ edit: (inout ServiceSetParams) -> Void) async -> Bool {
        var params = ServiceSetParams(host: host.host, service: service)
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

private func toneColor(_ tone: ServiceTone) -> Color {
    switch tone {
    case .good: Palette.signal
    case .working: Palette.attention
    case .needsYou: Palette.trouble
    case .off: Palette.rest
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
