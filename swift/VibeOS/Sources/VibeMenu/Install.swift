import AppKit
import CoreServices
import Foundation
import ServiceManagement
import VibeRPC

// The first launch (PRD: Key user flows, Install and setup). Run from
// Applications, the app registers vibed as a LaunchAgent with SMAppService
// and links vibe into /usr/local/bin; the welcome lists both, with vibe
// doctor's checks and the app's own permission to control Terminal. A copy
// that isn't installed, such as scripts/sign-macos.sh's build/VibeOS.app or
// a test's, does none of it: nothing outside it changes.

/// Where this copy of the app runs from.
public enum AppLocation: Equatable, Sendable {
    /// In /Applications or ~/Applications: installed.
    case applications
    /// On the disk image it came on, or moved aside by Gatekeeper because
    /// it was opened where it was downloaded: it has to be moved first.
    case notMoved
    /// Anywhere else, such as a development build: nothing is installed.
    case development

    public static func of(bundlePath: String, home: String = NSHomeDirectory()) -> AppLocation {
        let path = (bundlePath as NSString).standardizingPath
        if path.hasPrefix("/Volumes/") || path.contains("/AppTranslocation/") { return .notMoved }
        let parent = (path as NSString).deletingLastPathComponent
        let applications = ["/Applications", (home as NSString).appendingPathComponent("Applications")]
        return applications.contains(parent) ? .applications : .development
    }
}

/// vibed's LaunchAgent, as SMAppService has it.
public enum ServiceStatus: Equatable, Sendable {
    case enabled
    /// Registered, but turned off in System Settings → Login Items.
    case requiresApproval
    case notRegistered
    /// Registering failed, and why.
    case failed(String)
    /// Not this copy's to register: it isn't installed, or has no
    /// LaunchAgent in it.
    case notManaged
}

/// `/usr/local/bin/vibe`, which Terminal and ssh run.
public enum CommandLink: Equatable, Sendable {
    /// A link to this copy's vibe.
    case linked
    case missing
    /// Some other vibe, at this path.
    case other(String)
    /// Not this copy's to link.
    case notManaged
}

/// Whether macOS lets the app control Terminal, which a click on an agent
/// does (PRD: How a click becomes a Terminal window).
public enum TerminalPermission: Equatable, Sendable {
    case granted
    /// Turned off in Privacy & Security → Automation. Agents still open,
    /// through a `.command` file.
    case denied
    /// macOS will ask, the first time.
    case notAsked
    /// Can't tell: Terminal isn't running, so it can't be asked.
    case unknown
}

/// How this copy is installed, for the menu.
public struct InstallStatus: Equatable, Sendable {
    public var location: AppLocation
    public var service: ServiceStatus
    public var command: CommandLink
    public var terminal: TerminalPermission

    public init(location: AppLocation, service: ServiceStatus, command: CommandLink, terminal: TerminalPermission) {
        self.location = location
        self.service = service
        self.command = command
        self.terminal = terminal
    }

    /// A copy that installs nothing: the gallery's, and tests'.
    public static let notManaged = InstallStatus(
        location: .development, service: .notManaged, command: .notManaged, terminal: .unknown)

    /// Installed: in Applications, with vibed's LaunchAgent.
    public var managed: Bool { location == .applications && service != .notManaged }

    /// Why registering vibed's LaunchAgent failed, if it did.
    public var registerFailure: String? {
        if case .failed(let why) = service { return why }
        return nil
    }
}

/// What the first launch does to the Mac, so tests and the gallery can
/// stand in for it.
@MainActor
public protocol Installer: AnyObject {
    var location: AppLocation { get }
    var service: ServiceStatus { get }
    /// Registers vibed's LaunchAgent, which starts it, and says how that went.
    func registerService() -> ServiceStatus
    /// Restarts vibed's LaunchAgent, so an updated app's vibed takes over.
    func restartService() throws
    func openLoginItems()
    var command: CommandLink { get }
    /// Links vibe into /usr/local/bin. With `askingForPassword`, macOS asks
    /// for an administrator's password if the folder needs one; without,
    /// it only tries what needs none.
    func linkCommand(askingForPassword: Bool) throws
    /// Permission to control Terminal, without asking.
    func terminalPermission() async -> TerminalPermission
    /// Asks for it, starting Terminal if it isn't running; macOS asks once.
    func askTerminalPermission() async -> TerminalPermission
    func openAutomationSettings()
}

extension Installer {
    /// Everything, now.
    public func currentStatus() async -> InstallStatus {
        InstallStatus(
            location: location, service: service, command: command, terminal: await terminalPermission())
    }
}

/// The LaunchAgent's label, and its plist's name in the bundle's
/// Contents/Library/LaunchAgents.
public enum LaunchAgent {
    public static let label = "io.github.tsconfigdotjson.VibeOS.vibed"
    public static let plist = label + ".plist"
}

/// Where vibe is linked, for Terminal and ssh.
public let linkedVibePath = "/usr/local/bin/vibe"

/// Terminal's bundle id, as AppleScriptTerminal has it, for checks made
/// off the main actor.
let terminalBundleID = "com.apple.Terminal"

/// The real Mac.
@MainActor
public final class SystemInstaller: Installer {
    public let bundle: URL

    public init(bundle: URL = Bundle.main.bundleURL) {
        self.bundle = bundle
    }

    public var location: AppLocation { AppLocation.of(bundlePath: bundle.path) }

    private var bundledVibe: URL { bundle.appendingPathComponent("Contents/MacOS/vibe") }

    private var hasLaunchAgent: Bool {
        FileManager.default.fileExists(
            atPath: bundle.appendingPathComponent("Contents/Library/LaunchAgents/\(LaunchAgent.plist)").path)
    }

    private var agent: SMAppService { SMAppService.agent(plistName: LaunchAgent.plist) }

    public var service: ServiceStatus {
        guard location == .applications, hasLaunchAgent else { return .notManaged }
        switch agent.status {
        case .enabled: return .enabled
        case .requiresApproval: return .requiresApproval
        case .notRegistered, .notFound: return .notRegistered
        @unknown default: return .notRegistered
        }
    }

    public func registerService() -> ServiceStatus {
        guard location == .applications, hasLaunchAgent else { return .notManaged }
        do {
            try agent.register()
        } catch {
            // Turned off in Login Items: the register stands, waiting.
            if agent.status == .requiresApproval { return .requiresApproval }
            return .failed(describe(error))
        }
        return service
    }

    public func restartService() throws {
        let launchctl = Process()
        launchctl.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        launchctl.arguments = ["kickstart", "-k", "gui/\(getuid())/\(LaunchAgent.label)"]
        try launchctl.run()
        launchctl.waitUntilExit()
        guard launchctl.terminationStatus == 0 else {
            throw InstallError("VibeOS couldn't restart its background service (launchctl exit \(launchctl.terminationStatus))")
        }
    }

    public func openLoginItems() {
        SMAppService.openSystemSettingsLoginItems()
    }

    public var command: CommandLink {
        guard location == .applications, FileManager.default.isExecutableFile(atPath: bundledVibe.path) else {
            return .notManaged
        }
        return commandLink(at: linkedVibePath, ours: bundledVibe.path)
    }

    public func linkCommand(askingForPassword: Bool) throws {
        guard command != .notManaged else { return }
        let dir = (linkedVibePath as NSString).deletingLastPathComponent
        let fm = FileManager.default
        var isDir: ObjCBool = false
        if fm.fileExists(atPath: dir, isDirectory: &isDir), isDir.boolValue, fm.isWritableFile(atPath: dir) {
            try? fm.removeItem(atPath: linkedVibePath)
            try fm.createSymbolicLink(atPath: linkedVibePath, withDestinationPath: bundledVibe.path)
            return
        }
        guard askingForPassword else { throw InstallError("\(dir) needs an administrator's password") }
        // /usr/local/bin is root's on a new Mac: one line as an
        // administrator, which macOS asks for the password to run.
        let line = "/bin/mkdir -p \(shellQuote(dir)) && /bin/ln -sf \(shellQuote(bundledVibe.path)) "
            + shellQuote(linkedVibePath)
        let source = "do shell script \(appleScriptString(line)) with administrator privileges"
        guard let script = NSAppleScript(source: source) else { throw InstallError("can't make the link's script") }
        var info: NSDictionary?
        script.executeAndReturnError(&info)
        if let info {
            // -128: the owner cancelled the password prompt.
            let code = info[NSAppleScript.errorNumber] as? Int ?? 0
            if code == -128 { throw InstallError("cancelled") }
            throw InstallError(info[NSAppleScript.errorMessage] as? String ?? "error \(code)")
        }
    }

    /// Where the last answer macOS gave is kept: Terminal has to be
    /// running to be asked, and isn't always.
    static let lastTerminalAnswer = "terminalPermission"

    public func terminalPermission() async -> TerminalPermission {
        guard isTerminalRunning else { return remembered }
        return remember(await Task.detached { automationPermission(ask: false) }.value)
    }

    private var remembered: TerminalPermission {
        switch UserDefaults.standard.string(forKey: Self.lastTerminalAnswer) {
        case "granted": return .granted
        case "denied": return .denied
        default: return .unknown
        }
    }

    private func remember(_ answer: TerminalPermission) -> TerminalPermission {
        switch answer {
        case .granted: UserDefaults.standard.set("granted", forKey: Self.lastTerminalAnswer)
        case .denied: UserDefaults.standard.set("denied", forKey: Self.lastTerminalAnswer)
        case .notAsked: UserDefaults.standard.removeObject(forKey: Self.lastTerminalAnswer)
        case .unknown: break
        }
        return answer
    }

    public func askTerminalPermission() async -> TerminalPermission {
        if !isTerminalRunning, let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: terminalBundleID) {
            let config = NSWorkspace.OpenConfiguration()
            config.activates = false
            _ = try? await NSWorkspace.shared.openApplication(at: url, configuration: config)
            // A moment to come up and answer Apple events.
            for _ in 0..<40 where !isTerminalRunning {
                try? await Task.sleep(for: .milliseconds(250))
            }
            try? await Task.sleep(for: .milliseconds(500))
        }
        guard isTerminalRunning else { return remembered }
        // It waits on the owner's answer, so it's off the main thread.
        return remember(await Task.detached { automationPermission(ask: true) }.value)
    }

    public func openAutomationSettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation") {
            NSWorkspace.shared.open(url)
        }
    }

    private var isTerminalRunning: Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: terminalBundleID).isEmpty
    }
}

/// What `path` is, as the link to `ours`.
public func commandLink(at path: String, ours: String) -> CommandLink {
    let fm = FileManager.default
    guard (try? fm.attributesOfItem(atPath: path)) != nil else { return .missing }
    let resolved = (path as NSString).resolvingSymlinksInPath
    let mine = (ours as NSString).resolvingSymlinksInPath
    if resolved == mine { return .linked }
    // A link left pointing at nothing, such as at a copy since moved.
    if !fm.fileExists(atPath: resolved) { return .missing }
    return .other(resolved)
}

/// Whether the app may send Terminal Apple events, asking the owner if
/// `ask` and macOS hasn't yet.
func automationPermission(ask: Bool) -> TerminalPermission {
    let target = NSAppleEventDescriptor(bundleIdentifier: terminalBundleID)
    let status = AEDeterminePermissionToAutomateTarget(
        target.aeDesc, AEEventClass(typeWildCard), AEEventID(typeWildCard), ask)
    switch status {
    case 0: return .granted
    // errAEEventNotPermitted
    case -1743: return .denied
    // errAEEventWouldRequireUserConsent
    case -1744: return .notAsked
    // procNotFound, and anything else: Terminal can't be asked now.
    default: return .unknown
    }
}

public struct InstallError: Error, CustomStringConvertible, Equatable {
    public var description: String

    public init(_ description: String) {
        self.description = description
    }
}

/// An installer that installs nothing and says what it's told, for the
/// gallery and tests.
@MainActor
public class FixedInstaller: Installer {
    public var state: InstallStatus
    /// What each call did, in order.
    public private(set) var calls: [String] = []
    /// Whether linkCommand without a password works.
    public var linkNeedsPassword = true

    public init(_ status: InstallStatus = .notManaged) {
        self.state = status
    }

    public var location: AppLocation { state.location }
    public var service: ServiceStatus { state.service }
    public var command: CommandLink { state.command }

    public func registerService() -> ServiceStatus {
        calls.append("register")
        if state.service == .notRegistered { state.service = .enabled }
        return state.service
    }

    public func restartService() throws { calls.append("restart") }
    public func openLoginItems() { calls.append("login items") }

    public func linkCommand(askingForPassword: Bool) throws {
        calls.append(askingForPassword ? "link, asking" : "link")
        if linkNeedsPassword && !askingForPassword { throw InstallError("needs a password") }
        state.command = .linked
    }

    public func terminalPermission() async -> TerminalPermission { state.terminal }

    public func askTerminalPermission() async -> TerminalPermission {
        calls.append("ask terminal")
        if state.terminal == .notAsked || state.terminal == .unknown { state.terminal = .granted }
        return state.terminal
    }

    public func openAutomationSettings() { calls.append("automation settings") }
}

/// What stands between this copy and a running vibed, when it's how it's
/// installed: opened from the disk image, turned off in Login Items, or a
/// LaunchAgent macOS wouldn't take. The menu shows it instead of "Can't
/// reach vibed".
public struct InstallNote: Equatable, Sendable {
    public enum Action: Equatable, Sendable {
        /// Shows the app in Finder, to drag to Applications.
        case reveal
        case loginItems
        case register
    }

    public var title: String
    public var detail: String
    public var action: Action
    public var actionTitle: String

    public init?(_ install: InstallStatus) {
        if install.location == .notMoved {
            title = "Move VibeOS to Applications"
            detail = "VibeOS runs from your Applications folder. Drag it there, then open it from there."
            action = .reveal
            actionTitle = "Show in Finder"
        } else if install.service == .requiresApproval {
            title = "Let VibeOS run in the background"
            detail = "VibeOS's background service, which runs your agents, is turned off in Login Items. Turn on VibeOS there."
            action = .loginItems
            actionTitle = "Open Login Items"
        } else if let why = install.registerFailure {
            title = "VibeOS couldn't start its background service"
            detail = sentence(why)
            action = .register
            actionTitle = "Try Again"
        } else {
            return nil
        }
    }
}

/// Whether version `a` comes before `b`, part by part: `0.9.0` before
/// `0.10.0`. Parts that aren't numbers count as 0.
public func versionIsOlder(_ a: String, than b: String) -> Bool {
    let parts = { (v: String) in v.split(separator: ".").map { Int($0) ?? 0 } }
    let (x, y) = (parts(a), parts(b))
    for i in 0..<max(x.count, y.count) {
        let (p, q) = (i < x.count ? x[i] : 0, i < y.count ? y[i] : 0)
        if p != q { return p < q }
    }
    return false
}
