import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// `VibeOS --attach <ref>` attaches Terminal and exits, for scripts;
/// `VibeOS --gallery` and `--render-surfaces <dir>` draw every surface on
/// canned data, in light and dark (Gallery.swift); with no arguments it's
/// the menu bar app.
@main
enum Main {
    static func main() {
        let arguments = Array(CommandLine.arguments.dropFirst())
        if let refs = AttachCommandLine.refs(in: arguments) {
            let code = MainActor.assumeIsolated { AttachCommandLine.run(refs: refs) }
            exit(code)
        }
        if let request = GalleryRequest(arguments: arguments) {
            MainActor.assumeIsolated { Gallery.run(request) }
        }
        MainActor.assumeIsolated { VibeOSApp.main() }
    }
}

enum WindowID {
    static let newAgent = "new-agent"
    static let settings = "settings"
    static let logins = "linux-logins"
    static let addHost = "add-linux-host"
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

        Window("Add Linux Host", id: WindowID.addHost) {
            AddHostView(model: model)
        }
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(.suppressed)

        Window("Linux Logins", id: WindowID.logins) {
            LoginsView(model: model)
        }
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(.suppressed)

        Window("Settings", id: WindowID.settings) {
            SettingsView(model: model)
        }
        .defaultSize(width: 780, height: 560)
        .windowResizability(.contentMinSize)
        .defaultLaunchBehavior(.suppressed)
    }
}

/// The menu bar icon (PRD: Menu bar icon): the glyph, whose windows light
/// as agents wake and which carries the question and pause badges, and the
/// count of awake agents. A menu bar label takes one image and one text, so
/// the badges are drawn into the image (MenuBarGlyph).
struct MenuBarLabel: View {
    let icon: IconSummary
    let connected: Bool

    var body: some View {
        let state = GlyphState(icon: icon, connected: connected)
        HStack(spacing: 3) {
            Image(nsImage: MenuBarGlyph.image(state, awake: icon.awake))
            if connected {
                Text("\(icon.awake)").monospacedDigit()
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(state.accessibilityLabel(awake: icon.awake))
    }
}

/// Opens one of the app's windows in front: a menu bar app isn't active
/// until asked.
@MainActor
func show(_ id: String, with openWindow: OpenWindowAction) {
    openWindow(id: id)
    NSApp.activate()
}

/// Opens Settings on `pane`.
@MainActor
func showSettings(_ pane: SettingsPane, with openWindow: OpenWindowAction) {
    UserDefaults.standard.set(pane.rawValue, forKey: SettingsPane.storageKey)
    show(WindowID.settings, with: openWindow)
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
