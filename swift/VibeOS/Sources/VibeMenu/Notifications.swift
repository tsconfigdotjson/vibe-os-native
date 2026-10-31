import Foundation
import UserNotifications
import VibeRPC

/// A notification for the owner (PRD: Menu bar, Notifications): a question,
/// an error, a limit hit, or, if asked for, the end of a turn. Clicking one
/// attaches to its agent.
public struct Notice: Equatable, Sendable {
    public enum Kind: String, Sendable {
        case question, error, limit, endOfTurn
    }

    public var kind: Kind
    public var host: String
    public var ref: String
    public var title: String
    public var body: String
}

extension AgentActivity {
    /// Claude is in the middle of a turn.
    public var isWorking: Bool {
        switch self {
        case .thinking, .runningTool, .backgroundWork: true
        default: false
        }
    }
}

/// What, if anything, to tell the owner about `event`. `agent` is the agent
/// as the menu had it before the event, for its title and its last status.
public func notice(for event: DaemonEvent, agent: Agent?, endOfTurn: Bool) -> Notice? {
    guard let ref = event.ref else { return nil }
    let title = agent?.status.title
    func make(_ kind: Notice.Kind, _ headline: String, _ body: String?) -> Notice {
        Notice(kind: kind, host: event.host, ref: ref, title: headline, body: body ?? "")
    }
    switch event.kind {
    case .agentState:
        guard event.state == AgentState.question.rawValue, agent?.state != .question else { return nil }
        return make(.question, "\(ref) has a question", title ?? agent?.status.line)
    case .agentLimit:
        let limit = event.limit ?? "a"
        return make(.limit, "\(ref) hit its \(limit) limit", event.message)
    case .agentStatus:
        guard let status = event.status else { return nil }
        let before = agent?.status.activity
        if status.activity == .errored, before != .errored {
            if status.stopReason == .usageLimit {
                var why = "limited by a Claude usage limit"
                if let resets = status.resetsAt.flatMap(parseTime) {
                    why = "limited until \(resets.formatted(date: .omitted, time: .shortened))"
                }
                return make(.error, "\(ref) hit a usage limit", why)
            }
            return make(.error, "\(ref) stopped with an error", status.error ?? status.title ?? title)
        }
        if endOfTurn, status.activity == .done, before?.isWorking == true {
            return make(.endOfTurn, "\(ref) finished its turn", status.title ?? title)
        }
        return nil
    default:
        return nil
    }
}

/// Where notices go, so tests can stand in.
@MainActor
public protocol Notifier: AnyObject {
    /// Called with the host and ref of a notice the owner clicked.
    var onClick: ((String, String) -> Void)? { get set }
    func post(_ notice: Notice)
}

/// Notification Center. Only an app bundle may use it, so `init` fails for
/// a bare executable, such as one run by `swift run`.
@MainActor
public final class UserNotifier: NSObject, Notifier, UNUserNotificationCenterDelegate {
    public var onClick: ((String, String) -> Void)?
    private let center: UNUserNotificationCenter

    public init?(bundle: Bundle = .main) {
        guard bundle.bundleIdentifier != nil, bundle.bundleURL.pathExtension == "app" else { return nil }
        center = UNUserNotificationCenter.current()
        super.init()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    public func post(_ notice: Notice) {
        let content = UNMutableNotificationContent()
        content.title = notice.title
        content.body = notice.body
        content.userInfo = ["host": notice.host, "ref": notice.ref]
        if notice.kind != .endOfTurn { content.sound = .default }
        let request = UNNotificationRequest(
            identifier: "\(notice.host):\(notice.ref):\(notice.kind.rawValue):\(UUID().uuidString)",
            content: content, trigger: nil)
        center.add(request) { _ in }
    }

    nonisolated public func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let info = response.notification.request.content.userInfo
        let host = info["host"] as? String
        let ref = info["ref"] as? String
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                if let host, let ref { self.onClick?(host, ref) }
            }
            completionHandler()
        }
    }

    nonisolated public func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .list, .sound])
    }
}
