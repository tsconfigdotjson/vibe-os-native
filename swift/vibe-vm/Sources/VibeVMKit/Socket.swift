import Darwin
import Foundation

// Plain POSIX plumbing for the control socket: binding it with the lock
// beside it (as `vibe_core::socket` does for vibed and the shims), reading a
// line, writing, and splicing a client to a guest stream.

/// A listening Unix socket, and the lock (`<path>.lock`) that makes this
/// process its only server for as long as it lives.
public final class ControlSocket: @unchecked Sendable {
    public let path: String
    let fd: Int32
    let lockFD: Int32

    private init(path: String, fd: Int32, lockFD: Int32) {
        self.path = path
        self.fd = fd
        self.lockFD = lockFD
    }

    deinit {
        close(fd)
        close(lockFD)
    }

    /// Takes the lock, replaces a socket left behind by a `vibe-vm` that
    /// died, and listens. The socket is private to this user.
    public static func bind(path: String) throws -> ControlSocket {
        var addr = sockaddr_un()
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            throw Failure(.badConfig, "the socket path is too long: \(path)")
        }
        let dir = (path as NSString).deletingLastPathComponent
        do {
            try FileManager.default.createDirectory(
                atPath: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        } catch {
            throw Failure(.badConfig, "can't create \(dir): \(describe(error))")
        }

        let lockFD = open(path + ".lock", O_RDWR | O_CREAT | O_CLOEXEC, 0o600)
        guard lockFD >= 0 else {
            throw Failure(.badConfig, "can't open \(path).lock: \(errnoString())")
        }
        guard flock(lockFD, LOCK_EX | LOCK_NB) == 0 else {
            let code = errno
            close(lockFD)
            if code == EWOULDBLOCK {
                throw Failure(.alreadyRunning, "another vibe-vm is already serving \(path)")
            }
            throw Failure(.badConfig, "can't lock \(path).lock: \(String(cString: strerror(code)))")
        }

        // attributesOfItem doesn't follow a symlink, so one is refused too.
        if let type = (try? FileManager.default.attributesOfItem(atPath: path))?[.type] as? FileAttributeType {
            guard type == .typeSocket else {
                close(lockFD)
                throw Failure(.badConfig, "\(path) exists and isn't a socket; not touching it")
            }
            unlink(path)
        }

        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            close(lockFD)
            throw Failure(.badConfig, "can't make a socket: \(errnoString())")
        }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: bytes) }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0, chmod(path, 0o600) == 0, listen(fd, 16) == 0 else {
            let why = errnoString()
            close(fd)
            close(lockFD)
            throw Failure(.badConfig, "can't serve \(path): \(why)")
        }
        return ControlSocket(path: path, fd: fd, lockFD: lockFD)
    }

    /// The next connection; blocks. Nil only if the socket itself is broken.
    public func accept() -> Int32? {
        while true {
            let client = Darwin.accept(fd, nil, nil)
            if client >= 0 {
                _ = fcntl(client, F_SETFD, FD_CLOEXEC)
                noSigpipe(client)
                return client
            }
            switch errno {
            case EINTR, ECONNABORTED: continue
            case EBADF, EINVAL, ENOTSOCK: return nil
            default:
                // Out of descriptors or memory, for now: try again shortly.
                usleep(100_000)
            }
        }
    }

    /// Removes the socket, so a probe finds nothing; the lock goes with the
    /// process.
    public func remove() {
        unlink(path)
    }

    /// Removes the socket and lets the lock go, so the next vibe-vm for this
    /// VM can start at once. Only right before exiting.
    public func release() {
        unlink(path)
        close(lockFD)
    }
}

/// Connects to the Unix socket at `path`.
public func connectUnix(_ path: String) throws -> Int32 {
    var addr = sockaddr_un()
    let bytes = Array(path.utf8)
    guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
        throw Failure(.badRequest, "the socket path is too long: \(path)")
    }
    addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
    addr.sun_family = sa_family_t(AF_UNIX)
    withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: bytes) }
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { throw Failure(.badRequest, "can't make a socket: \(errnoString())") }
    noSigpipe(fd)
    let rc = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
        }
    }
    guard rc == 0 else {
        let why = errnoString()
        close(fd)
        throw Failure(.badRequest, "can't connect to \(path): \(why)")
    }
    return fd
}

/// How long a read on `fd` may block; 0 for ever.
func setReceiveTimeout(_ fd: Int32, seconds: Int) {
    var tv = timeval(tv_sec: seconds, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
}

/// A write to a closed socket must fail, not kill vibe-vm with SIGPIPE.
func noSigpipe(_ fd: Int32) {
    var one: Int32 = 1
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
}

func errnoString() -> String {
    String(cString: strerror(errno))
}

/// Reads up to the first newline, which is dropped, a byte at a time: after
/// a `connect`, nothing past the request may be taken from the stream. Nil at
/// EOF before a newline.
public func readLine(_ fd: Int32, max: Int) throws -> Data? {
    var line = Data()
    var byte: UInt8 = 0
    while true {
        let n = read(fd, &byte, 1)
        if n < 0 {
            if errno == EINTR { continue }
            throw Failure(.badRequest, "can't read the request: \(errnoString())")
        }
        if n == 0 { return nil }
        if byte == 0x0A { return line }
        line.append(byte)
        if line.count > max { throw Failure(.badRequest, "the request is too long") }
    }
}

/// Writes all of `data`, or throws.
public func writeAll(_ fd: Int32, _ data: Data) throws {
    try data.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
        var offset = 0
        while offset < buf.count {
            let n = write(fd, buf.baseAddress! + offset, buf.count - offset)
            if n < 0 {
                if errno == EINTR { continue }
                throw Failure(.badRequest, "can't write: \(errnoString())")
            }
            offset += n
        }
    }
}

/// Copies bytes both ways between a client and a guest stream, each way on
/// a thread of its own, until both ways end. An end of input is passed on as
/// a half close, so a guest that sees one is expected to finish and close
/// its end too. Then the client is closed and `done` is called; the guest
/// stream is the caller's to close.
public func splice(client: Int32, guest: Int32, done: @escaping @Sendable () -> Void) {
    let finished = Countdown(2) {
        close(client)
        done()
    }
    for (from, to) in [(client, guest), (guest, client)] {
        let thread = Thread {
            pump(from: from, to: to)
            finished.tick()
        }
        thread.stackSize = 256 * 1024
        thread.start()
    }
}

private func pump(from: Int32, to: Int32) {
    var buffer = [UInt8](repeating: 0, count: 64 * 1024)
    while true {
        let n = buffer.withUnsafeMutableBytes { read(from, $0.baseAddress, $0.count) }
        if n < 0 && errno == EINTR { continue }
        if n <= 0 {
            // EOF: pass it on, and let the other way carry on.
            if n == 0 {
                shutdown(to, SHUT_WR)
            } else {
                shutdown(to, SHUT_RDWR)
                shutdown(from, SHUT_RDWR)
            }
            return
        }
        do {
            try writeAll(to, Data(buffer[0..<n]))
        } catch {
            // The other end is gone: end both ways.
            shutdown(to, SHUT_RDWR)
            shutdown(from, SHUT_RDWR)
            return
        }
    }
}

/// Runs `action` once `tick` has been called `count` times.
final class Countdown: @unchecked Sendable {
    private let lock = NSLock()
    private var left: Int
    private let action: @Sendable () -> Void

    init(_ count: Int, action: @escaping @Sendable () -> Void) {
        left = count
        self.action = action
    }

    func tick() {
        lock.lock()
        left -= 1
        let fire = left == 0
        lock.unlock()
        if fire { action() }
    }
}
