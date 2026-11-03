import Darwin
import Foundation
import Network

/// Local Network access for Linux hosts on the local network (PRD: Adding
/// a Linux host; issue #39).
///
/// macOS lets an app connect to an address on the local network only once
/// the owner allows it, and counts vibed (a LaunchAgent the app registers)
/// and the `ssh` it runs as the app's. But only the app, in the foreground,
/// can show the request: vibed's connections are refused without asking.
/// So before vibed installs on a host, the app finds where `ssh` would
/// connect, and when that's on the local network, connects there itself.
/// That shows the request if it's never been answered, and tells whether
/// access is off.
///
/// A host out on the internet needs no such access: the app doesn't connect
/// to it, and nothing is asked.
public enum LocalNetwork {
    /// Where Local Network access is turned on.
    public static let settingsPath = "System Settings > Privacy & Security > Local Network > VibeOS"

    /// System Settings, at Privacy & Security > Local Network.
    public static let settingsURL = URL(
        string: "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork")!

    /// What to say while access is off and adding `destination` waits for it.
    public static func offMessage(_ destination: String) -> String {
        "Local Network access is off for VibeOS, so it can't connect to \(destination) on your local "
            + "network. Allow it when macOS asks, or turn it on in \(settingsPath). Adding the host goes "
            + "on once it's on."
    }

    /// Where `ssh` connects for a destination.
    public struct Target: Equatable, Sendable {
        public var host: String
        public var port: UInt16

        public init(host: String, port: UInt16) {
            self.host = host
            self.port = port
        }
    }

    /// The host and port in `ssh -G`'s output, or nil when `ssh` goes
    /// through a proxy (`ProxyJump`, `ProxyCommand`), which it then doesn't
    /// connect to the host itself.
    public static func target(sshConfig: String) -> Target? {
        var host: String?
        var port: UInt16 = 22
        for line in sshConfig.split(whereSeparator: \.isNewline) {
            let parts = line.trimmingCharacters(in: .whitespaces).split(separator: " ", maxSplits: 1)
            guard parts.count == 2 else { continue }
            let value = parts[1].trimmingCharacters(in: .whitespaces)
            switch parts[0].lowercased() {
            case "hostname": host = value
            case "port":
                guard let p = UInt16(value) else { return nil }
                port = p
            case "proxyjump", "proxycommand":
                if value.lowercased() != "none" { return nil }
            default: break
            }
        }
        guard let host, !host.isEmpty else { return nil }
        return Target(host: host, port: port)
    }

    /// Where `ssh` (at `ssh`) connects for `destination`, from its config.
    public static func target(destination: String, ssh: String = "/usr/bin/ssh", config: String? = nil) async
        -> Target?
    {
        await Task.detached {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: ssh)
            process.arguments = (config.map { ["-F", $0] } ?? []) + ["-G", "--", destination]
            let out = Pipe()
            process.standardOutput = out
            process.standardError = FileHandle.nullDevice
            process.standardInput = FileHandle.nullDevice
            guard (try? process.run()) != nil else { return nil }
            // It only reads config; a moment is plenty.
            let stop = DispatchWorkItem { if process.isRunning { process.terminate() } }
            DispatchQueue.global().asyncAfter(deadline: .now() + 5, execute: stop)
            let data = out.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            stop.cancel()
            guard process.terminationStatus == 0 else { return nil }
            return target(sshConfig: String(decoding: data, as: UTF8.self))
        }.value
    }

    /// Whether `host` is on the local network as macOS counts it: a `.local`
    /// name, or a name or address with an address on a network of one of
    /// this Mac's broadcast-capable interfaces (Wi-Fi, Ethernet; not
    /// loopback or a VPN's), link-local, or multicast.
    public static func isLocal(host: String) -> Bool {
        let name = host.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "."))
        if name == "local" || name.hasSuffix(".local") { return true }
        let networks = localNetworks()
        return addresses(of: host).contains { isLocal($0, networks: networks) }
    }

    /// Whether `address` (4 or 16 bytes) is local, among `networks`.
    public static func isLocal(_ address: [UInt8], networks: [(address: [UInt8], mask: [UInt8])]) -> Bool {
        if address.count == 4 {
            if address[0] == 169 && address[1] == 254 { return true }  // link-local
            if address[0] >= 224 && address[0] <= 239 { return true }  // multicast
            if address == [255, 255, 255, 255] { return true }
            if address[0] == 127 { return false }
        } else if address.count == 16 {
            if address[0] == 0xfe && address[1] & 0xc0 == 0x80 { return true }  // fe80::/10
            if address[0] == 0xff { return true }  // multicast
            if address == [UInt8](repeating: 0, count: 15) + [1] { return false }  // ::1
        }
        return networks.contains { sameNetwork(address, $0.address, $0.mask) }
    }

    /// Whether `address` is on the network of `net` with `mask`.
    public static func sameNetwork(_ address: [UInt8], _ net: [UInt8], _ mask: [UInt8]) -> Bool {
        guard address.count == net.count, net.count == mask.count, mask.contains(where: { $0 != 0 }) else {
            return false
        }
        return zip(zip(address, net), mask).allSatisfy { pair, m in pair.0 & m == pair.1 & m }
    }

    /// The addresses `host` resolves to, as bytes.
    static func addresses(of host: String) -> [[UInt8]] {
        var hints = addrinfo()
        hints.ai_socktype = SOCK_STREAM
        var list: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, nil, &hints, &list) == 0, let first = list else { return [] }
        defer { freeaddrinfo(first) }
        var found: [[UInt8]] = []
        for info in sequence(first: first, next: { $0.pointee.ai_next }) {
            if let bytes = bytes(of: info.pointee.ai_addr) { found.append(bytes) }
        }
        return found
    }

    /// The networks of this Mac's interfaces that are up and can broadcast,
    /// but aren't loopback or point-to-point: each an address and its mask.
    static func localNetworks() -> [(address: [UInt8], mask: [UInt8])] {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0, let first = list else { return [] }
        defer { freeifaddrs(first) }
        var found: [(address: [UInt8], mask: [UInt8])] = []
        for i in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let flags = Int32(i.pointee.ifa_flags)
            guard flags & IFF_UP != 0, flags & IFF_BROADCAST != 0,
                flags & (IFF_LOOPBACK | IFF_POINTOPOINT) == 0,
                let addr = i.pointee.ifa_addr, let address = bytes(of: addr),
                let mask = bytes(of: i.pointee.ifa_netmask, family: Int32(addr.pointee.sa_family)),
                address.count == mask.count
            else { continue }
            found.append((address, mask))
        }
        return found
    }

    /// The address in `sa`, read as one of `family` (its own unless given).
    /// A netmask's sockaddr is cut short after its last non-zero byte, its
    /// `sa_len` saying so, and may lack its family: only the bytes it has
    /// are read, and the rest are zero.
    static func bytes(of sa: UnsafeMutablePointer<sockaddr>?, family: Int32? = nil) -> [UInt8]? {
        guard let sa else { return nil }
        let offset: Int
        let count: Int
        switch family ?? Int32(sa.pointee.sa_family) {
        case AF_INET:
            offset = MemoryLayout<sockaddr_in>.offset(of: \sockaddr_in.sin_addr)!
            count = 4
        case AF_INET6:
            offset = MemoryLayout<sockaddr_in6>.offset(of: \sockaddr_in6.sin6_addr)!
            count = 16
        default:
            return nil
        }
        let have = max(0, min(Int(sa.pointee.sa_len) - offset, count))
        let raw = UnsafeRawPointer(sa)
        return (0..<count).map { $0 < have ? raw.load(fromByteOffset: offset + $0, as: UInt8.self) : 0 }
    }
}

/// How asking for Local Network access went.
public enum LocalNetworkAccess: Equatable, Sendable {
    /// The host isn't on the local network, or `ssh` reaches it through a
    /// proxy: there's nothing to ask.
    case notNeeded
    /// The app connected to it.
    case allowed
    /// Access stayed off for as long as adding waited.
    case off
    /// The app couldn't tell: the host didn't answer it, say. vibed's own
    /// connection will say why.
    case unknown
}

/// Asks for Local Network access before a Linux host is added, from the
/// app, so tests can stand in for it.
public protocol LocalNetworkGate: Sendable {
    /// Connects to where `ssh` goes for `destination` when that's on the
    /// local network, showing macOS's request for access if it hasn't been
    /// answered. While access is off it calls `off` (once) and waits, up to
    /// a few minutes, for it to be turned on. Cancelling ends the wait.
    func prepare(_ destination: String, off: @escaping @Sendable () -> Void) async -> LocalNetworkAccess
}

/// The real thing: `ssh -G`, the resolver, and Network framework's
/// connection, which waits for access to be allowed (TN3179).
public struct SystemLocalNetworkGate: LocalNetworkGate {
    public var ssh: String
    /// The ssh config to read, if not the user's (for tests).
    public var config: String?
    /// How long adding waits for access to be turned on.
    public var patience: TimeInterval
    /// How long a host that doesn't answer is given.
    public var answerWithin: TimeInterval

    /// `answerWithin` is long enough for the owner to answer macOS's
    /// request, should the connection wait for it unrefused.
    public init(
        ssh: String = "/usr/bin/ssh", config: String? = nil, patience: TimeInterval = 180,
        answerWithin: TimeInterval = 20
    ) {
        self.ssh = ssh
        self.config = config
        self.patience = patience
        self.answerWithin = answerWithin
    }

    public func prepare(_ destination: String, off: @escaping @Sendable () -> Void) async -> LocalNetworkAccess {
        guard let target = await LocalNetwork.target(destination: destination, ssh: ssh, config: config),
            await Task.detached(operation: { LocalNetwork.isLocal(host: target.host) }).value
        else { return .notNeeded }
        return await connect(to: target, off: off)
    }

    func connect(to target: LocalNetwork.Target, off: @escaping @Sendable () -> Void) async -> LocalNetworkAccess {
        guard let port = NWEndpoint.Port(rawValue: target.port) else { return .unknown }
        let connection = NWConnection(host: NWEndpoint.Host(target.host), port: port, using: .tcp)
        let queue = DispatchQueue(label: "io.github.tsconfigdotjson.VibeOS.local-network")
        let wait = Wait(off: off)
        let result = await withTaskCancellationHandler {
            await withCheckedContinuation { (continuation: CheckedContinuation<LocalNetworkAccess, Never>) in
                queue.async {
                    if wait.cancelled {
                        continuation.resume(returning: .unknown)
                        return
                    }
                    wait.continuation = continuation
                    connection.stateUpdateHandler = { state in
                        let denied = connection.currentPath?.unsatisfiedReason == .localNetworkDenied
                        switch state {
                        case .ready:
                            wait.finish(.allowed)
                        case .waiting where denied:
                            // Network framework connects once it's allowed.
                            wait.turnedAway()
                        case .failed where denied:
                            wait.finish(.off)
                        case .waiting, .failed:
                            // Refused, say: access isn't what's wrong.
                            wait.finish(.unknown)
                        case .cancelled:
                            wait.finish(.unknown)
                        default:
                            break
                        }
                    }
                    connection.start(queue: queue)
                    queue.asyncAfter(deadline: .now() + answerWithin) {
                        if !wait.waitingForAccess { wait.finish(.unknown) }
                    }
                    queue.asyncAfter(deadline: .now() + patience) { wait.finish(.off) }
                }
            }
        } onCancel: {
            queue.async {
                wait.cancelled = true
                wait.finish(.unknown)
                connection.cancel()
            }
        }
        connection.cancel()
        return result
    }

    /// One connection's wait, touched only on its queue.
    private final class Wait: @unchecked Sendable {
        let off: @Sendable () -> Void
        var continuation: CheckedContinuation<LocalNetworkAccess, Never>?
        var waitingForAccess = false
        var cancelled = false

        init(off: @escaping @Sendable () -> Void) { self.off = off }

        func turnedAway() {
            guard !waitingForAccess, continuation != nil else { return }
            waitingForAccess = true
            off()
        }

        func finish(_ access: LocalNetworkAccess) {
            continuation?.resume(returning: access)
            continuation = nil
        }
    }
}
