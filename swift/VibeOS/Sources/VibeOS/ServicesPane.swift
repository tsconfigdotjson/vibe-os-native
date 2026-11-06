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
                        "Linux agents on this Mac have no shared services until their Linux is updated. "
                            + "Settings → Linux Toolbox says how that's going. macOS agents have them now.",
                        systemImage: "exclamationmark.triangle"
                    )
                    .foregroundStyle(Palette.attention)
                    .fixedSize(horizontal: false, vertical: true)
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
                    HStack(spacing: 6) {
                        if service.name == "linear" {
                            LinearLogo()
                                .frame(width: 14, height: 14)
                                .accessibilityHidden(true)
                        }
                        Text(service.title)
                    }
                } footer: {
                    Text(warning(service)).foregroundStyle(.secondary)
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

    private func warning(_ service: ServiceOverview) -> String {
        "Agents can use \(service.title) by default. Those that don't ask for permission, such as agents in Bypass "
            + "mode, can act in \(service.title) as you."
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

/// Linear's logomark, unaltered, to say which service this is. The path is
/// logo-light.svg's, from Linear-Brand-Assets.zip on https://linear.app/brand
/// (viewBox 0 0 100 100). It takes the foreground colour, as Linear prefers it
/// monochrome.
private struct LinearLogo: Shape {
    func path(in rect: CGRect) -> Path {
        let scale = min(rect.width, rect.height) / 100
        func at(_ x: CGFloat, _ y: CGFloat) -> CGPoint {
            CGPoint(x: rect.minX + x * scale, y: rect.minY + y * scale)
        }
        var p = Path()
        p.move(to: at(1.2254, 61.5228))
        p.addCurve(to: at(2.8218, 60.6658), control1: at(1.0029, 60.5743), control2: at(2.1329, 59.9769))
        p.addLine(to: at(39.3342, 97.1782))
        p.addCurve(to: at(38.4772, 98.7746), control1: at(40.0231, 97.8671), control2: at(39.4257, 98.9971))
        p.addCurve(to: at(1.2254, 61.5228), control1: at(20.0515, 94.4522), control2: at(5.5478, 79.9485))
        p.closeSubpath()
        p.move(to: at(0.0019, 46.8891))
        p.addCurve(to: at(0.2915, 47.6497), control1: at(-0.0158, 47.1724), control2: at(0.0908, 47.449))
        p.addLine(to: at(52.3503, 99.7085))
        p.addCurve(to: at(53.1109, 99.9981), control1: at(52.551, 99.9092), control2: at(52.8276, 100.016))
        p.addCurve(to: at(60.0733, 99.0722), control1: at(55.4801, 99.8505), control2: at(57.8047, 99.5381))
        p.addCurve(to: at(60.5515, 97.4241), control1: at(60.8378, 98.9152), control2: at(61.1034, 97.9759))
        p.addLine(to: at(2.576, 39.4485))
        p.addCurve(to: at(0.9278, 39.9267), control1: at(2.0241, 38.8966), control2: at(1.0848, 39.1622))
        p.addCurve(to: at(0.0019, 46.8891), control1: at(0.4619, 42.1953), control2: at(0.1495, 44.5199))
        p.closeSubpath()
        p.move(to: at(4.2109, 29.7054))
        p.addCurve(to: at(4.4186, 30.8054), control1: at(4.0444, 30.0792), control2: at(4.1292, 30.516))
        p.addLine(to: at(69.1946, 95.5814))
        p.addCurve(to: at(70.2946, 95.7891), control1: at(69.484, 95.8708), control2: at(69.9208, 95.9556))
        p.addCurve(to: at(75.4801, 93.1051), control1: at(72.0807, 94.9935), control2: at(73.8117, 94.0964))
        p.addCurve(to: at(75.6633, 91.5644), control1: at(76.0322, 92.7771), control2: at(76.1174, 92.0184))
        p.addLine(to: at(8.4357, 24.3367))
        p.addCurve(to: at(6.8949, 24.5199), control1: at(7.9816, 23.8826), control2: at(7.223, 23.9678))
        p.addCurve(to: at(4.2109, 29.7054), control1: at(5.9036, 26.1883), control2: at(5.0065, 27.9193))
        p.closeSubpath()
        p.move(to: at(12.6587, 18.074))
        p.addCurve(to: at(12.6144, 16.7199), control1: at(12.2886, 17.7039), control2: at(12.2657, 17.1103))
        p.addCurve(to: at(49.9519, 0), control1: at(21.7795, 6.4593), control2: at(35.1114, 0))
        p.addCurve(to: at(100, 50.0481), control1: at(77.5927, 0), control2: at(100, 22.4073))
        p.addCurve(to: at(83.2801, 87.3856), control1: at(100, 64.8886), control2: at(93.5407, 78.2205))
        p.addCurve(to: at(81.9259, 87.3413), control1: at(82.8898, 87.7343), control2: at(82.2961, 87.7114))
        p.addLine(to: at(12.6587, 18.074))
        p.closeSubpath()
        return p
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
        let status = host.service.map { ServiceSummary($0) }
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
                        Text("Not answering. If it's behind this Mac, VibeOS is updating it: Settings → Hosts says how that's going.")
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
