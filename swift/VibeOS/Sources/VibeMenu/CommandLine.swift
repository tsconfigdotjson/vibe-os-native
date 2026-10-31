import Foundation
import VibeRPC

/// `VibeOS --attach <ref> [--attach <ref>…]`: what clicking each agent's row
/// does, one after the other in one process, with no menu. It's how
/// scripts/check-full.sh checks that two clicks give one Terminal tab.
/// Prints how each went; exits non-zero if one failed.
@MainActor
public enum AttachCommandLine {
    /// The refs to attach, if the arguments ask for any.
    public static func refs(in arguments: [String]) -> [String]? {
        var refs: [String] = []
        var i = arguments.startIndex
        while i < arguments.endIndex {
            if arguments[i] == "--attach", arguments.index(after: i) < arguments.endIndex {
                refs.append(arguments[arguments.index(after: i)])
                i = arguments.index(i, offsetBy: 2)
            } else {
                i = arguments.index(after: i)
            }
        }
        return refs.isEmpty ? nil : refs
    }

    public static func run(
        refs: [String], daemon: any DaemonConnection = VibedClient(),
        terminal: TerminalScripting = AppleScriptTerminal(vibePath: defaultVibePath())
    ) -> Int32 {
        let attacher = Attacher(terminal: terminal, vibePath: defaultVibePath(), socket: attachSocket())
        for ref in refs {
            do {
                let agents = try daemon.call(AgentsList.self, HostParams()).agents
                guard let agent = agents.first(where: { $0.ref == ref }) else {
                    FileHandle.standardError.write(Data("VibeOS: no agent \(ref) on this host\n".utf8))
                    return 1
                }
                let session = try daemon.call(SessionsAttach.self, SessionParams(host: agent.host, ref: ref))
                switch try attacher.attach(agent, attached: session.ttys) {
                case .raised: print("\(ref): raised its tab")
                case .opened(let tty): print("\(ref): opened a window on \(tty ?? "an unknown tty")")
                case .openedWithoutScripting: print("\(ref): opened a .command file, without Automation")
                }
            } catch {
                FileHandle.standardError.write(Data("VibeOS: can't attach \(ref): \(describe(error))\n".utf8))
                return 1
            }
        }
        return 0
    }
}
