import Darwin
import Foundation

public enum VibedClientError: Error, Equatable, CustomStringConvertible {
    case socketPathTooLong(String)
    case connect(path: String, errno: Int32)
    case io(errno: Int32)
    case timedOut
    /// vibed closed the connection before answering.
    case closed
    case messageTooLarge
    /// vibed answered with something that isn't the expected response.
    case badResponse(String)
    /// vibed answered with an error.
    case rpc(RPCError)

    public var description: String {
        switch self {
        case .socketPathTooLong(let path): "the socket path is too long: \(path)"
        case .connect(let path, let code):
            "can't reach VibeOS's background service at \(path): \(String(cString: strerror(code))) (is it running?)"
        case .io(let code): "talking to VibeOS's background service: \(String(cString: strerror(code)))"
        case .timedOut: "VibeOS's background service didn't answer in time"
        case .closed: "VibeOS's background service closed the connection before answering"
        case .messageTooLarge: "the answer from VibeOS's background service is too large"
        case .badResponse(let why): "unexpected answer from VibeOS's background service: \(why)"
        case .rpc(let error): "VibeOS's background service: \(error)"
        }
    }
}

/// A blocking client for the vibed socket: one connection per call. Run it
/// off the main thread.
public struct VibedClient: Sendable {
    public var socketPath: String
    /// The limit for each read and write.
    public var timeout: TimeInterval

    public init(socketPath: String = VibedClient.defaultSocketPath(), timeout: TimeInterval = 5) {
        self.socketPath = socketPath
        self.timeout = timeout
    }

    /// `$VIBE_SOCKET` if set, otherwise the macOS default under `home`.
    public static func defaultSocketPath(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        home: String = NSHomeDirectory()
    ) -> String {
        if let path = environment[VibeRPC.socketEnv], !path.isEmpty {
            return path
        }
        return (home as NSString).appendingPathComponent(VibeRPC.macOSSocket)
    }

    public func hello(client: String) throws -> HelloResult {
        try call(Hello.self, HelloParams(client: client))
    }

    public func call<M: RPCMethod>(_ method: M.Type, _ params: M.Params) throws -> M.Result {
        let id = RPCID.number(1)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.withoutEscapingSlashes]
        var line = try encoder.encode(RPCRequest(id: id, method: M.name, params: params))
        line.append(0x0A)

        let fd = try connectSocket()
        defer { close(fd) }
        try writeAll(fd, line)
        let reply = try readLine(fd)

        let response: RPCResponse<M.Result>
        do {
            response = try JSONDecoder().decode(RPCResponse<M.Result>.self, from: reply)
        } catch {
            throw VibedClientError.badResponse("\(error)")
        }
        switch response.outcome {
        case .result(let result) where response.id == id:
            return result
        // vibed answers a message whose id it couldn't read, such as one
        // that's too large, with an error and a null id. That error is ours too.
        case .error(let error) where response.id == id || response.id == .null:
            throw VibedClientError.rpc(error)
        default:
            throw VibedClientError.badResponse("a response for another id: \(response.id)")
        }
    }

    func connectSocket() throws -> Int32 {
        var addr = sockaddr_un()
        let path = Array(socketPath.utf8)
        guard path.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            throw VibedClientError.socketPathTooLong(socketPath)
        }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: path) }

        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw VibedClientError.io(errno: errno) }
        var connected = false
        defer { if !connected { close(fd) } }

        // A write to a closed socket must fail, not kill the app with SIGPIPE.
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        let seconds = max(timeout, 0.001)
        var tv = timeval(
            tv_sec: Int(seconds),
            tv_usec: Int32((seconds - seconds.rounded(.down)) * 1_000_000))
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else { throw VibedClientError.connect(path: socketPath, errno: errno) }
        connected = true
        return fd
    }

    func writeAll(_ fd: Int32, _ data: Data) throws {
        try data.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < buf.count {
                let n = Darwin.write(fd, buf.baseAddress! + offset, buf.count - offset)
                if n < 0 {
                    let code = errno
                    if code == EINTR { continue }
                    throw code == EAGAIN ? VibedClientError.timedOut : VibedClientError.io(errno: code)
                }
                offset += n
            }
        }
    }

    /// Reads up to the first newline, which is dropped.
    private func readLine(_ fd: Int32) throws -> Data {
        var line = Data()
        var chunk = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            let n = chunk.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress, $0.count) }
            if n < 0 {
                let code = errno
                if code == EINTR { continue }
                throw code == EAGAIN ? VibedClientError.timedOut : VibedClientError.io(errno: code)
            }
            if n == 0 { throw VibedClientError.closed }
            if let newline = chunk[0..<n].firstIndex(of: 0x0A) {
                line.append(contentsOf: chunk[0..<newline])
                return line
            }
            line.append(contentsOf: chunk[0..<n])
            if line.count > VibeRPC.maxMessageBytes { throw VibedClientError.messageTooLarge }
        }
    }
}
