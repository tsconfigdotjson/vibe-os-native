import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// `VibeOS --attach <ref>` attaches Terminal and exits, for scripts; with
/// no arguments it's the menu bar app.
@main
enum Main {
    static func main() {
        if let refs = AttachCommandLine.refs(in: Array(CommandLine.arguments.dropFirst())) {
            let code = MainActor.assumeIsolated { AttachCommandLine.run(refs: refs) }
            exit(code)
        }
        MainActor.assumeIsolated { VibeOSApp.main() }
    }
}

enum WindowID {
    static let newAgent = "new-agent"
    static let settings = "settings"
    static let logins = "linux-logins"
}

/// The menu bar app: UI only, holding no state. Everything it shows comes
/// from vibed over the socket, and quitting it leaves vibed and every agent
/// running.
struct VibeOSApp: App {
    @StateObject private var model: MenuModel

    init() {
        let model = MenuModel(daemon: VibedClient(), terminal: AppleScriptTerminal(), notifier: UserNotifier())
        model.start()
        _model = StateObject(wrappedValue: model)
    }

    var body: some Scene {
        MenuBarExtra {
            MenuView(model: model)
        } label: {
            MenuBarLabel(icon: model.icon, connected: model.connection == .connected)
        }
        .menuBarExtraStyle(.window)

        Window("New Agent", id: WindowID.newAgent) {
            NewAgentView(model: model)
        }
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(.suppressed)

        Window("Linux Logins", id: WindowID.logins) {
            LoginsView(model: model)
        }
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(.suppressed)

        Window("VibeOS Settings", id: WindowID.settings) {
            SettingsView(model: model)
        }
        .defaultLaunchBehavior(.suppressed)
    }
}

/// The glyph, the count of awake agents, and an amber dot for a question or
/// a red one for an agent paused by a limit.
struct MenuBarLabel: View {
    let icon: IconSummary
    let connected: Bool

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: connected ? "square.stack.3d.up.fill" : "square.stack.3d.up.slash")
            if connected {
                Text("\(icon.awake)").monospacedDigit()
            }
            if icon.question {
                Image(nsImage: dot(.systemOrange))
            }
            if icon.paused {
                Image(nsImage: dot(.systemRed))
            }
        }
        .accessibilityLabel(accessibility)
    }

    private var accessibility: String {
        guard connected else { return "VibeOS: vibed is unavailable" }
        var text = "VibeOS: \(icon.awake) awake"
        if icon.question { text += ", a question waiting" }
        if icon.paused { text += ", an agent paused" }
        return text
    }

    /// A coloured dot. The menu bar draws template images in its own
    /// colour, so this one isn't one.
    private func dot(_ color: NSColor) -> NSImage {
        let image = NSImage(size: NSSize(width: 7, height: 7), flipped: false) { rect in
            color.setFill()
            NSBezierPath(ovalIn: rect).fill()
            return true
        }
        image.isTemplate = false
        return image
    }
}

/// Opens one of the app's windows in front: a menu bar app isn't active
/// until asked.
@MainActor
func show(_ id: String, with openWindow: OpenWindowAction) {
    openWindow(id: id)
    NSApp.activate()
}

/// Asks a question in an alert, in front of everything. True for the first
/// button.
@MainActor
func confirm(_ message: String, detail: String, action: String, destructive: Bool = true) -> Bool {
    NSApp.activate()
    let alert = NSAlert()
    alert.messageText = message
    alert.informativeText = detail
    alert.alertStyle = destructive ? .warning : .informational
    let button = alert.addButton(withTitle: action)
    button.hasDestructiveAction = destructive
    alert.addButton(withTitle: "Cancel")
    return alert.runModal() == .alertFirstButtonReturn
}
