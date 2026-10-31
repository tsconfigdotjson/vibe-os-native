import AppKit
import Foundation
import VibeRPC

// How a click becomes a Terminal window (PRD: Menu bar and Terminal attach).
// vibed says which terminals are attached already; one of those is raised
// if it's there, and otherwise Terminal runs `vibe attach` in a new window,
// through AppleScript, or through a `.command` file when the app may not
// script Terminal.

public enum TerminalError: Error, Equatable, CustomStringConvertible {
    /// The owner hasn't let this app control Terminal (Automation).
    case notPermitted
    case failed(String)

    public var description: String {
        switch self {
        case .notPermitted: "VibeOS isn't allowed to control Terminal (System Settings → Privacy & Security → Automation)"
        case .failed(let why): why
        }
    }
}

/// What the app asks of Terminal, so tests can stand in for it.
@MainActor
public protocol TerminalScripting: AnyObject {
    /// Brings forward a tab whose tty is one of `ttys`, or one of
    /// `busyOnly` that still runs `vibe attach`. False if there's none.
    func raise(ttys: [String], busyOnly: [String]) throws -> Bool
    /// Runs `command` in a new Terminal window and brings it forward.
    /// Returns its tab's tty.
    func run(_ command: String) throws -> String?
    /// Runs `command` in Terminal without scripting it, which needs no
    /// permission: a `.command` file, opened.
    func runWithoutScripting(_ command: String) throws
}

/// How an attach went.
public enum AttachOutcome: Equatable, Sendable {
    /// A tab already attached was raised.
    case raised
    /// A new window runs `vibe attach`, on this tty.
    case opened(tty: String?)
    /// Terminal opened a `.command` file, as it may not be scripted.
    case openedWithoutScripting
}

/// Attaches Terminal to agents, one tab per agent, and to the Linux
/// toolbox's shell, one tab for it.
@MainActor
public final class Attacher {
    /// The toolbox shell's key; an agent's has a host in it.
    public static let toolboxKey = "toolbox"

    public let terminal: TerminalScripting
    /// The `vibe` that local agents attach with.
    public let vibePath: String
    /// The socket to give `vibe attach`, when it isn't its default.
    public let socket: String?
    /// How long a tab this app opened counts as attached before vibed has
    /// seen its `vibe attach` connect: for `startup` whatever it's doing, as
    /// its shell may not have started yet, and then while it runs
    /// something, up to `grace`. A tab whose attach has ended is left.
    public var startup: TimeInterval = 3
    public var grace: TimeInterval = 60
    private var opened: [String: (tty: String, at: Date)] = [:]
    /// When a `.command` file was opened for each agent: its tab can't be
    /// found again, so a second click right after it does nothing.
    private var openedUnscripted: [String: Date] = [:]
    private let clock: () -> Date

    public init(
        terminal: TerminalScripting, vibePath: String, socket: String?,
        clock: @escaping () -> Date = Date.init
    ) {
        self.terminal = terminal
        self.vibePath = vibePath
        self.socket = socket
        self.clock = clock
    }

    /// This app's `vibe`, with the socket when it isn't the default, then
    /// `words`: shell words, quoted already where they need it.
    public func vibeCommand(_ words: [String]) -> String {
        var all = [shellQuote(vibePath)]
        if let socket { all += ["--socket", shellQuote(socket)] }
        return (all + words).joined(separator: " ")
    }

    /// The shell command a new window runs for `agent`. A local one goes
    /// through this app's `vibe`; another host's is the `ssh -t <host> vibe
    /// attach <ref>` line vibed gives it.
    public func command(for agent: Agent) -> String {
        guard agent.isLocal else { return "exec \(agent.attach)" }
        return "exec " + vibeCommand(["attach", shellQuote(agent.ref)])
    }

    /// The shell command a new window runs for the toolbox shell: `vibe
    /// toolbox` opens it, and attaches.
    public var toolboxCommand: String { "exec " + vibeCommand(["toolbox"]) }

    /// Raises `agent`'s tab if one is attached, by the ttys vibed reports
    /// (`attached`) or the one this app opened a moment ago, which vibed may
    /// not have seen yet; otherwise opens a new one.
    public func attach(_ agent: Agent, attached: [String]) throws -> AttachOutcome {
        try open(key: agent.key, command: command(for: agent), attached: agent.isLocal ? attached : [])
    }

    /// Raises the toolbox shell's tab if one is attached, or opens one.
    public func openToolbox(attached: [String]) throws -> AttachOutcome {
        try open(key: Attacher.toolboxKey, command: toolboxCommand, attached: attached)
    }

    /// The rule both follow: raise the tab for `key`, if vibed says one is
    /// attached (`attached`) or this app opened it a moment ago; otherwise
    /// run `command` in a new one.
    public func open(key: String, command: String, attached: [String]) throws -> AttachOutcome {
        var ttys = attached
        var busyOnly: [String] = []
        if let mine = opened[key], !ttys.contains(mine.tty) {
            let age = clock().timeIntervalSince(mine.at)
            if age < startup {
                ttys.append(mine.tty)
            } else if age < grace {
                busyOnly.append(mine.tty)
            } else {
                opened[key] = nil
            }
        }
        if ttys.isEmpty, let at = openedUnscripted[key], clock().timeIntervalSince(at) < startup {
            return .openedWithoutScripting
        }
        do {
            if !(ttys.isEmpty && busyOnly.isEmpty), try terminal.raise(ttys: ttys, busyOnly: busyOnly) {
                return .raised
            }
            let tty = try terminal.run(command)
            if let tty, !tty.isEmpty { opened[key] = (tty, clock()) }
            return .opened(tty: tty)
        } catch TerminalError.notPermitted {
            try terminal.runWithoutScripting(command)
            openedUnscripted[key] = clock()
            return .openedWithoutScripting
        }
    }

    /// Forgets the tab opened for `key`, as when the agent is deleted.
    public func forget(_ key: String) {
        opened[key] = nil
    }
}

/// Terminal.app, through `NSAppleScript`.
@MainActor
public final class AppleScriptTerminal: TerminalScripting {
    public static let bundleID = "com.apple.Terminal"

    /// The name `vibe attach` runs under in a tab, as Terminal lists its
    /// processes. With `exec` it replaces the shell, which Terminal may not
    /// count as busy.
    public let process: String

    /// How long one script may wait on Terminal before it gives up, in
    /// seconds; AppleScript's own default is two minutes.
    public var answerTimeout = 10
    /// How long a Terminal that wasn't running gets to start and answer.
    public var launchTimeout: TimeInterval = 20

    public init(vibePath: String = defaultVibePath()) {
        process = (vibePath as NSString).lastPathComponent
    }

    // NSAppleScript is for the main thread only, so a call blocks the menu
    // while Terminal answers, and while macOS asks for Automation the first
    // time; both are bounded by the timeouts above, and only on a click.

    private var isRunning: Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: AppleScriptTerminal.bundleID).isEmpty
    }

    public func raise(ttys: [String], busyOnly: [String]) throws -> Bool {
        // Asking a Terminal that isn't running would start it.
        guard isRunning, !(ttys.isEmpty && busyOnly.isEmpty) else { return false }
        let list = { (ttys: [String]) in ttys.map(appleScriptString).joined(separator: ", ") }
        let source = """
            set anyOf to {\(list(ttys))}
            set busyOf to {\(list(busyOnly))}
            with timeout of \(answerTimeout) seconds
            tell application "Terminal"
                repeat with w in windows
                    repeat with t in tabs of w
                        set theTTY to tty of t
                        if theTTY is in anyOf or (theTTY is in busyOf and ((busy of t) or ((processes of t) contains \(appleScriptString(process))))) then
                            set selected of t to true
                            set miniaturized of w to false
                            set index of w to 1
                            activate
                            return true
                        end if
                    end repeat
                end repeat
            end tell
            end timeout
            return false
            """
        return try execute(source).booleanValue
    }

    public func run(_ command: String) throws -> String? {
        let started = !isRunning
        if started { try launch() }
        // A Terminal that has just started opens a window of its own: the
        // command goes in that one rather than a second, if it's the only
        // one and a shell waits in it, not a restored tab running something.
        let script = appleScriptString(command)
        let source = """
            with timeout of \(answerTimeout) seconds
            tell application "Terminal"
                set t to missing value
                if \(started ? "true" : "false") then
                    repeat 30 times
                        if (count of windows) > 0 then exit repeat
                        delay 0.1
                    end repeat
                    if (count of windows) = 1 and (count of tabs of window 1) = 1 and not (busy of tab 1 of window 1) then
                        set t to do script \(script) in tab 1 of window 1
                    end if
                end if
                if t is missing value then set t to do script \(script)
                activate
                return tty of t
            end tell
            end timeout
            """
        return try execute(source).stringValue
    }

    /// Starts Terminal as Finder would, through Launch Services, and waits
    /// until it answers Apple events, for up to `launchTimeout`. Asked
    /// before it's ready, Terminal can hold an event until AppleScript's
    /// two-minute timeout.
    private func launch() throws {
        let open = Process()
        open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        open.arguments = ["-b", AppleScriptTerminal.bundleID]
        try open.run()
        open.waitUntilExit()
        guard open.terminationStatus == 0 else {
            throw TerminalError.failed("Terminal couldn't be started (open exited with \(open.terminationStatus))")
        }
        let ping = """
            with timeout of 2 seconds
            tell application "Terminal" to count windows
            end timeout
            """
        let deadline = Date().addingTimeInterval(launchTimeout)
        var why = "it never started"
        repeat {
            if isRunning {
                do {
                    _ = try execute(ping)
                    return
                } catch TerminalError.notPermitted {
                    throw TerminalError.notPermitted
                } catch {
                    why = describe(error)
                }
            }
            Thread.sleep(forTimeInterval: 0.25)
        } while Date() < deadline
        throw TerminalError.failed(
            "Terminal didn't answer within \(Int(launchTimeout)) s of starting (\(why)). "
                + "It has to run in the logged-in GUI session.")
    }

    public func runWithoutScripting(_ command: String) throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("VibeOS", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let file = dir.appendingPathComponent("attach-\(UUID().uuidString).command")
        // It removes itself before it becomes the attach.
        try "#!/bin/sh\nrm -f \"$0\"\n\(command)\n".write(to: file, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: file.path)
        let open = Process()
        open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        open.arguments = ["-b", AppleScriptTerminal.bundleID, file.path]
        try open.run()
        open.waitUntilExit()
        guard open.terminationStatus == 0 else {
            throw TerminalError.failed("`open` couldn't hand \(file.path) to Terminal")
        }
    }

    private func execute(_ source: String) throws -> NSAppleEventDescriptor {
        guard let script = NSAppleScript(source: source) else {
            throw TerminalError.failed("the AppleScript for Terminal doesn't compile")
        }
        var info: NSDictionary?
        let result = script.executeAndReturnError(&info)
        if let info {
            let code = info[NSAppleScript.errorNumber] as? Int ?? 0
            // errAEEventNotPermitted, errAEEventWouldRequireUserConsent
            if code == -1743 || code == -1744 { throw TerminalError.notPermitted }
            // errAETimeout: Terminal is running but didn't answer in time.
            if code == -1712 {
                throw TerminalError.failed("Terminal didn't answer in time; it may be busy or still starting")
            }
            let message = info[NSAppleScript.errorMessage] as? String ?? "error \(code)"
            throw TerminalError.failed("Terminal: \(message)")
        }
        return result
    }
}

/// `text` as an AppleScript string literal.
public func appleScriptString(_ text: String) -> String {
    let escaped = text
        .replacingOccurrences(of: "\\", with: "\\\\")
        .replacingOccurrences(of: "\"", with: "\\\"")
    return "\"\(escaped)\""
}
