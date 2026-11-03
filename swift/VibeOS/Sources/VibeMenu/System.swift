import AppKit
import Darwin
import Foundation
import VibeRPC

/// vibed as the menu talks to it, so tests can stand in for it. Calls
/// block; the model makes them off the main actor.
public protocol DaemonConnection: Sendable {
    func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params) throws -> M.Result
    /// A call that may take minutes, such as an install over SSH: it waits
    /// up to `timeout` seconds for the answer.
    func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params, timeout: TimeInterval) throws -> M.Result
    func events(client: String) -> AsyncThrowingStream<EventMessage, any Error>
}

extension DaemonConnection {
    public func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params, timeout: TimeInterval) throws -> M.Result {
        try call(method, params)
    }
}

extension VibedClient: DaemonConnection {
    public func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params, timeout: TimeInterval) throws -> M.Result {
        var patient = self
        patient.timeout = timeout
        return try patient.call(method, params)
    }
}

/// The Mac's memory, for the menu's header: used as Activity Monitor counts
/// it (app memory, wired and compressed), and the total.
public struct HostMemory: Equatable, Sendable {
    public var used: Int
    public var total: Int

    public init(used: Int, total: Int) {
        self.used = used
        self.total = total
    }

    /// The host port, got once: each `mach_host_self()` adds a reference
    /// to it that's never given back.
    private static let hostPort = mach_host_self()

    public static func current() -> HostMemory? {
        var stats = vm_statistics64()
        var count = mach_msg_type_number_t(
            MemoryLayout<vm_statistics64_data_t>.size / MemoryLayout<integer_t>.size)
        let result = withUnsafeMutablePointer(to: &stats) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                host_statistics64(hostPort, HOST_VM_INFO64, $0, &count)
            }
        }
        guard result == KERN_SUCCESS else { return nil }
        let page = Int(getpagesize())
        let app = Int(stats.internal_page_count) - Int(stats.purgeable_count)
        let used = (max(0, app) + Int(stats.wire_count) + Int(stats.compressor_page_count)) * page
        return HostMemory(used: used, total: Int(ProcessInfo.processInfo.physicalMemory))
    }
}

/// The menu's own preferences, which are the app's alone: vibed keeps all
/// other state.
public enum Preferences {
    /// Notify at the end of every turn; off by default.
    public static let notifyEndOfTurn = "notifyEndOfTurn"
    /// The bundle id of the app Open in editor uses.
    public static let editor = "editorBundleID"
}

/// The worktree actions in a row's menu.
@MainActor
public enum WorktreeActions {
    /// Editors tried in order when none is chosen: VS Code, Cursor, Zed,
    /// Xcode.
    public static let editors = [
        "com.microsoft.VSCode", "com.todesktop.230313mzl4w4u92", "dev.zed.Zed", "com.apple.dt.Xcode",
    ]

    public static func reveal(_ path: String) {
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    /// The editor to open worktrees in: the chosen one if it's installed,
    /// otherwise the first of `editors` that is.
    public static func editor(preferred: String?) -> URL? {
        for id in [preferred].compactMap({ $0 }) + editors {
            if let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: id) { return url }
        }
        return nil
    }

    /// Opens `path` in the editor, or in Finder if there's none.
    public static func openInEditor(_ path: String, preferred: String?) {
        let url = URL(fileURLWithPath: path, isDirectory: true)
        guard let app = editor(preferred: preferred) else {
            NSWorkspace.shared.open(url)
            return
        }
        NSWorkspace.shared.open(
            [url], withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration(),
            completionHandler: nil)
    }

    public static func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

/// Where the `vibe` CLI is: `$VIBE_CLI` if set (for development), else
/// `/usr/local/bin/vibe`, where setup links it; until it's linked, the one
/// in the app bundle, beside the app.
public func defaultVibePath(
    environment: [String: String] = ProcessInfo.processInfo.environment, linked: String = linkedVibePath,
    bundled: String? = Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent("vibe").path
) -> String {
    if let path = environment["VIBE_CLI"], !path.isEmpty { return path }
    let fm = FileManager.default
    if !fm.isExecutableFile(atPath: linked), let bundled, fm.isExecutableFile(atPath: bundled) { return bundled }
    return linked
}

/// What setup's steps do that isn't vibed's or Terminal's.
@MainActor
public enum SetupActions {
    /// Apple's Command Line Tools, which git needs: macOS's own installer.
    public static func installCommandLineTools() {
        let installer = Process()
        installer.executableURL = URL(fileURLWithPath: "/usr/bin/xcode-select")
        installer.arguments = ["--install"]
        try? installer.run()
    }
}

/// The socket `vibe attach` must be told about: the app's, when it isn't
/// the one `vibe` would find on its own.
public func attachSocket(environment: [String: String] = ProcessInfo.processInfo.environment) -> String? {
    guard let path = environment[VibeRPC.socketEnv], !path.isEmpty else { return nil }
    return path
}
