import Foundation
import VibeRPC

/// What Settings → Services says of a service's one connection: there's one
/// login per install, on the host that owns it (PRD: Shared MCP services).
public struct ServiceConnection: Equatable, Sendable {
    /// The button that signs in, if one's wanted.
    public enum SignIn: Equatable, Sendable {
        case connect, reconnect
    }

    public var status: String
    public var tone: ServiceTone
    /// The one thing to do about it, if there is one, on the row itself.
    public var signIn: SignIn?
    /// Whether there's a login to disconnect.
    public var canDisconnect: Bool
    /// What went wrong last, and when, for people.
    public var detail: String?

    public init(_ overview: ServiceOverview) {
        switch overview.state {
        case .disconnected:
            status = "Not connected"
            tone = .off
            signIn = .connect
        case .connected:
            status = "Connected"
            tone = .good
            signIn = nil
        case .refreshing:
            status = "Refreshing…"
            tone = .working
            signIn = nil
        case .ownerUnreachable:
            status = "Can't reach the host that owns it"
            tone = .working
            signIn = nil
        case .needsReconnect:
            status = "Needs reconnecting"
            tone = .needsYou
            signIn = .reconnect
        case .revoked:
            status = "Revoked"
            tone = .needsYou
            signIn = .reconnect
        }
        canDisconnect = overview.signedIn
        detail = overview.detail.map { detail in
            guard let at = overview.detailAt else { return detail }
            return "\(detail) (\(serviceTime(at)))"
        }
    }
}

/// What a status dot means.
public enum ServiceTone: Equatable, Sendable {
    case good, working, needsYou, off
}

/// What Settings → Services says of one host's standing with a service.
public struct ServiceSummary: Equatable, Sendable {
    public var status: String
    public var tone: ServiceTone
    /// What went wrong last, and when, for people.
    public var detail: String?
    /// How many tools its agents see, and how they reach them.
    public var note: String?

    public init(_ service: ServiceInfo, now: Date = Date()) {
        let until = service.accessUntil.map(serviceTime)
        let ranOut = service.accessUntil
            .flatMap { ISO8601DateFormatter().date(from: $0) }
            .map { $0 <= now } ?? false
        switch (service.state, service.role) {
        case (.disconnected, _):
            status = "No access"
            tone = .off
        case (.connected, .owner):
            status = until.map { "Holds the login · access until \($0)" } ?? "Holds the login"
            tone = .good
        case (.connected, .follower) where ranOut:
            status = until.map { "Access expired \($0)" } ?? "Access expired"
            tone = .needsYou
        case (.connected, .follower):
            status = until.map { "Has access until \($0)" } ?? "Has access"
            tone = .good
        case (.refreshing, _):
            status = "Refreshing…"
            tone = .working
        case (.ownerUnreachable, _):
            status = "No current access: can't reach \(service.owner ?? "the host that owns it")"
            tone = .needsYou
        case (.needsReconnect, _):
            status = "Needs reconnecting"
            tone = .needsYou
        case (.revoked, _):
            status = "Revoked"
            tone = .needsYou
        }
        detail = service.detail.map { detail in
            guard let at = service.detailAt else { return detail }
            return "\(detail) (\(serviceTime(at)))"
        }
        var notes: [String] = []
        if !service.enabled {
            notes.append("Turned off: agents see no tools")
        } else if service.state != .disconnected {
            notes.append(service.tools == 1 ? "1 tool" : "\(service.tools) tools")
        }
        if service.readOnly { notes.append("Read-only") }
        if !service.deniedTools.isEmpty {
            notes.append(service.deniedTools.count == 1 ? "1 tool denied" : "\(service.deniedTools.count) tools denied")
        }
        if service.autoAllow { notes.append("Runs without asking") }
        note = notes.isEmpty ? nil : notes.joined(separator: " · ")
    }
}

/// A time from vibed, for people, or as it was if it can't be read.
public func serviceTime(_ iso: String) -> String {
    guard let date = ISO8601DateFormatter().date(from: iso) else { return iso }
    return date.formatted(date: .abbreviated, time: .shortened)
}

/// What Settings says when a host can't say how its services stand. One running
/// an older vibed doesn't know these calls, which isn't for an owner to decode.
public func serviceFailure(_ error: any Error, host: String) -> String {
    if let client = error as? VibedClientError, case .rpc(let rpc) = client,
        rpc.code == RPCErrorCode.methodNotFound.rawValue
    {
        return "\(host) needs updating for shared services, which VibeOS does by itself: Settings → Hosts says how that's going."
    }
    return describe(error)
}

/// Whether this Mac's Linux agents can't have shared services until Rebuild
/// Template: its template is on a base image from before them, and one that has
/// them is installed for it to move to. Until that's downloaded, a rebuild
/// wouldn't help.
public func servicesNeedRebuild(_ toolbox: ToolboxStatus?) -> Bool {
    guard let toolbox, toolbox.state != .unavailable, let base = toolbox.base,
        let newest = toolbox.newestBase
    else { return false }
    return base < VibeRPC.sharedServicesBase && newest >= VibeRPC.sharedServicesBase
}

/// The denied tools as they're typed: names, separated by commas or spaces.
public func parseToolNames(_ text: String) -> [String] {
    var seen = Set<String>()
    return text.split(whereSeparator: { $0 == "," || $0.isWhitespace })
        .map(String.init)
        .filter { seen.insert($0).inserted }
}
