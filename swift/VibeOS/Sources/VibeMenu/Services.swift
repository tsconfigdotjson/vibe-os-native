import Foundation
import VibeRPC

/// What Settings → Services says of one service on one host. A host holds its
/// own login to each service, so every host has its own status and its own
/// Connect or Reconnect (PRD: Shared MCP services).
public struct ServiceSummary: Equatable, Sendable {
    /// What the status dot means.
    public enum Tone: Equatable, Sendable {
        case good, working, needsYou, off
    }

    /// The button that signs in again, if one's wanted.
    public enum SignIn: Equatable, Sendable {
        case connect, reconnect
    }

    public var status: String
    public var tone: Tone
    /// The one thing to do about it, if there is one, on the row itself.
    public var signIn: SignIn?
    /// Whether there's a login to disconnect.
    public var canDisconnect: Bool
    /// What went wrong last, and when, for people.
    public var detail: String?
    /// How many tools its agents see, and how they reach them.
    public var note: String?

    public init(_ service: ServiceInfo) {
        switch service.state {
        case .disconnected:
            status = "Not connected"
            tone = .off
            signIn = .connect
        case .connected:
            status = service.auth == .apiKey ? "Connected with an API key" : "Connected"
            tone = .good
            signIn = nil
        case .refreshing:
            status = "Refreshing…"
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
        canDisconnect = service.state != .disconnected
        detail = service.detail.map { detail in
            guard let at = service.detailAt else { return detail }
            return "\(detail) (\(at))"
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

/// What Settings says when a host can't say how its services stand. One running
/// an older vibed doesn't know these calls, which isn't for an owner to decode.
public func serviceFailure(_ error: any Error, host: String) -> String {
    if let client = error as? VibedClientError, case .rpc(let rpc) = client,
        rpc.code == RPCErrorCode.methodNotFound.rawValue
    {
        return "\(host) needs updating for shared services: Settings → Hosts → Update vibed."
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

/// The line that connects `service` on a Linux host from its own shell, which
/// is how a host mends itself while this Mac is away. The key goes in there:
/// it never crosses the link.
public func setKeyCommand(service: String) -> String {
    "vibe service set-key \(service)"
}
