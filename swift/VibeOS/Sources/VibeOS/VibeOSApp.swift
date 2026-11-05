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
    static let addHost = "add-linux-host"
    static let signIn = "sign-in"
}

/// The menu bar app: UI only, holding no state. Everything it shows comes
/// from vibed over the socket, and quitting it leaves vibed and every agent
/// running.
struct VibeOSApp: App {
    @StateObject private var model: MenuModel
    @StateObject private var menuBar = MenuBarAppearance()

    init() {
        let model = MenuModel(
            daemon: VibedClient(), terminal: AppleScriptTerminal(), notifier: UserNotifier(),
            installer: SystemInstaller(), localNetwork: SystemLocalNetworkGate())
        // Sparkle, in a release build: checks for updates on its own.
        Updater.shared.start()
        model.start()
        _model = StateObject(wrappedValue: model)
    }

    var body: some Scene {
        MenuBarExtra {
            MenuView(model: model)
        } label: {
            MenuBarLabel(icon: model.icon, connected: model.connection == .connected, onDark: menuBar.dark)
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

        Window("Sign In", id: WindowID.signIn) {
            SignInView(model: model)
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
    /// Whether the menu bar is dark, for a badged glyph's windows.
    var onDark = false

    var body: some View {
        let state = GlyphState(icon: icon, connected: connected)
        HStack(spacing: 3) {
            Image(nsImage: MenuBarGlyph.image(state, awake: icon.awake, onDark: onDark))
            if connected {
                Text("\(icon.awake)").monospacedDigit()
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(state.accessibilityLabel(awake: icon.awake))
    }
}

/// Whether the menu bar is dark, as the status item sees it: on macOS 26 the
/// menu bar follows the wallpaper as well as Dark Mode. A badged glyph isn't
/// a template, so it's drawn for this. It's read from the view the
/// MenuBarExtra's status item draws in, watched once found; until then (a
/// moment at launch) a short timer looks for it, and the app's appearance
/// stands in.
@MainActor
final class MenuBarAppearance: ObservableObject {
    @Published private(set) var dark = false
    private var appObservation: NSKeyValueObservation?
    private var viewObservation: NSKeyValueObservation?
    private weak var watched: NSView?
    private var timer: Timer?

    init() {
        appObservation = NSApplication.shared.observe(\.effectiveAppearance) { [weak self] _, _ in
            DispatchQueue.main.async { MainActor.assumeIsolated { self?.update() } }
        }
        update()
    }

    func update() {
        if watched == nil {
            viewObservation = nil
            if let view = Self.statusItemView() {
                watched = view
                viewObservation = view.observe(\.effectiveAppearance) { [weak self] _, _ in
                    DispatchQueue.main.async { MainActor.assumeIsolated { self?.update() } }
                }
                timer?.invalidate()
                timer = nil
            } else if timer == nil {
                timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
                    DispatchQueue.main.async { MainActor.assumeIsolated { self?.update() } }
                }
            }
        }
        let appearance = watched?.effectiveAppearance ?? NSApplication.shared.effectiveAppearance
        let now = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        if now != dark { dark = now }
    }

    /// The view the status item draws in: its window is the app's
    /// NSStatusBarWindow.
    static func statusItemView() -> NSView? {
        NSApplication.shared.windows.first { String(describing: type(of: $0)).contains("StatusBarWindow") }?.contentView
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
