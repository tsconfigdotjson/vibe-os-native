/// `vibe-vm`'s command line. vibed drives it (`vibe_env::vm` in Rust); the
/// contract is in protocol/vibe-vm/.
public enum Command: Equatable, Sendable {
    case version
    case help
    /// Reports whether this Mac and this binary can run VMs.
    case check
    /// Gives the VM in `config` its machine identifier and MAC address, and
    /// checks Virtualization.framework accepts it, save and restore included.
    case create(config: String)
    /// Boots the VM fresh, then serves `socket`.
    case boot(config: String, socket: String)
    /// Restores the VM from its snapshot, then serves `socket`.
    case restore(config: String, socket: String)
}

public struct UsageError: Error, Equatable, CustomStringConvertible {
    public var description: String
}

public enum VibeVM {
    public static let version = "0.1.0"

    public static let usage = """
        usage: vibe-vm <command>

        commands:
          check                                   report whether this Mac and this binary can
                                                  run VMs, as JSON; exits 0 only if both can
          create --config VM.JSON                 give the VM its machine identifier and MAC
                                                  address, and check it can be saved and restored
          boot --config VM.JSON --socket PATH     boot the VM, print one JSON line once it runs,
                                                  then serve its control socket, detached
          restore --config VM.JSON --socket PATH  the same, from the VM's snapshot
          -V, --version                           print the version
          -h, --help                              print this help

        vibed runs vibe-vm; see protocol/vibe-vm/README.md.
        """

    public static func parse(_ arguments: [String]) throws -> Command {
        guard let first = arguments.first else {
            throw UsageError(description: "missing a command")
        }
        let rest = Array(arguments.dropFirst())
        switch first {
        case "-V", "--version", "-h", "--help", "check":
            if let extra = rest.first {
                throw UsageError(description: "unexpected argument: \(extra)")
            }
            switch first {
            case "check": return .check
            case "-h", "--help": return .help
            default: return .version
            }
        case "create":
            let options = try parseOptions(rest, allowed: ["--config"])
            return .create(config: try require(options, "--config", first))
        case "boot", "restore":
            let options = try parseOptions(rest, allowed: ["--config", "--socket"])
            let config = try require(options, "--config", first)
            let socket = try require(options, "--socket", first)
            return first == "boot"
                ? .boot(config: config, socket: socket) : .restore(config: config, socket: socket)
        default:
            throw UsageError(description: "unknown command: \(first)")
        }
    }

    private static func parseOptions(_ arguments: [String], allowed: Set<String>) throws -> [String: String] {
        var options: [String: String] = [:]
        var iterator = arguments.makeIterator()
        while let name = iterator.next() {
            guard allowed.contains(name) else {
                throw UsageError(description: "unexpected argument: \(name)")
            }
            guard let value = iterator.next(), !value.isEmpty else {
                throw UsageError(description: "\(name) needs a value")
            }
            guard options[name] == nil else {
                throw UsageError(description: "\(name) is given twice")
            }
            options[name] = value
        }
        return options
    }

    private static func require(_ options: [String: String], _ name: String, _ command: String) throws -> String {
        guard let value = options[name] else {
            throw UsageError(description: "\(command) needs \(name)")
        }
        return value
    }
}
