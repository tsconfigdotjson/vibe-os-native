/// `vibe-vm`'s command line. vibed drives it; the VM lifecycle (boot,
/// restore, save) lands in #7.
public enum Command: Equatable, Sendable {
    case version
    case help
    /// Reports whether this Mac and this binary can run VMs.
    case check
}

public struct UsageError: Error, Equatable, CustomStringConvertible {
    public var description: String
}

public enum VibeVM {
    public static let version = "0.1.0"

    public static let usage = """
        usage: vibe-vm <command>

        commands:
          check          report whether this Mac and this binary can run VMs, as JSON;
                         exits 0 only if both can
          -V, --version  print the version
          -h, --help     print this help
        """

    public static func parse(_ arguments: [String]) throws -> Command {
        guard let first = arguments.first else {
            throw UsageError(description: "missing a command")
        }
        guard arguments.count == 1 else {
            throw UsageError(description: "unexpected argument: \(arguments[1])")
        }
        switch first {
        case "-V", "--version": return .version
        case "-h", "--help": return .help
        case "check": return .check
        default: throw UsageError(description: "unknown command: \(first)")
        }
    }
}
