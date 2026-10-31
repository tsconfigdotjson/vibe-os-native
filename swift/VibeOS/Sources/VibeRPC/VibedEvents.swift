import Darwin
import Foundation

/// What an event subscription yields, in order.
public enum EventMessage: Hashable, Sendable {
    /// The subscription is up. `seq` is the last event before it, so this is
    /// the moment to read everything again.
    case subscribed(seq: Int)
    case event(DaemonEvent)
}

extension VibedClient {
    /// vibed's events, on a connection of their own (protocol/README.md,
    /// Events): `.subscribed` once, then every event as it happens. It says
    /// hello as `client` first. The stream ends with an error when the
    /// connection does, such as when vibed restarts; stopping iterating
    /// closes the connection.
    public func events(client: String) -> AsyncThrowingStream<EventMessage, any Error> {
        AsyncThrowingStream { continuation in
            let connection = EventConnection(client: self, name: client)
            continuation.onTermination = { _ in connection.close() }
            // The socket blocks, so it gets a thread of its own rather than
            // one of the concurrency pool's.
            let thread = Thread { connection.run(continuation) }
            thread.name = "vibed events"
            thread.start()
        }
    }
}

/// One subscribed connection. `close` may come from any thread, and wakes
/// the reader by shutting the socket down.
final class EventConnection: @unchecked Sendable {
    private let client: VibedClient
    private let name: String
    private let lock = NSLock()
    private var fd: Int32 = -1
    private var closed = false

    init(client: VibedClient, name: String) {
        self.client = client
        self.name = name
    }

    func close() {
        lock.lock()
        defer { lock.unlock() }
        closed = true
        if fd >= 0 { Darwin.shutdown(fd, SHUT_RDWR) }
    }

    private var isClosed: Bool {
        lock.lock()
        defer { lock.unlock() }
        return closed
    }

    func run(_ continuation: AsyncThrowingStream<EventMessage, any Error>.Continuation) {
        do {
            try pump(continuation)
            continuation.finish(throwing: VibedClientError.closed)
        } catch {
            continuation.finish(throwing: error)
        }
        lock.lock()
        let fd = self.fd
        self.fd = -1
        lock.unlock()
        if fd >= 0 { Darwin.close(fd) }
    }

    private func pump(_ continuation: AsyncThrowingStream<EventMessage, any Error>.Continuation) throws {
        let fd = try client.connectSocket()
        lock.lock()
        if closed {
            lock.unlock()
            Darwin.close(fd)
            return
        }
        self.fd = fd
        lock.unlock()

        let encoder = JSONEncoder()
        encoder.outputFormatting = [.withoutEscapingSlashes]
        var lines = try encoder.encode(
            RPCRequest(id: EventConnection.helloID, method: Hello.name, params: HelloParams(client: name)))
        lines.append(0x0A)
        lines.append(
            try encoder.encode(
                RPCRequest(id: EventConnection.subscribeID, method: EventsSubscribe.name, params: NoParams())))
        lines.append(0x0A)
        try client.writeAll(fd, lines)

        var reader = LineReader(fd: fd)
        while let line = try reader.next(stopping: { self.isClosed }) {
            if let message = try EventConnection.message(from: line) {
                continuation.yield(message)
            }
        }
    }

    static let helloID = RPCID.string("hello")
    static let subscribeID = RPCID.string("subscribe")

    /// What a line from vibed means to a subscriber: the subscription's
    /// answer, an event, or nothing (the hello's answer). An error from
    /// vibed, such as a protocol mismatch, ends the subscription.
    static func message(from line: Data) throws -> EventMessage? {
        struct Envelope: Decodable {
            var id: RPCID?
            var method: String?
        }
        let decoder = JSONDecoder()
        let envelope: Envelope
        do {
            envelope = try decoder.decode(Envelope.self, from: line)
        } catch {
            throw VibedClientError.badResponse("\(error)")
        }
        do {
            if envelope.method == EventNotice.name {
                let note = try decoder.decode(RPCRequest<DaemonEvent>.self, from: line)
                return note.params.map(EventMessage.event)
            }
            if envelope.id == subscribeID {
                switch try decoder.decode(RPCResponse<Subscribed>.self, from: line).outcome {
                case .result(let subscribed): return .subscribed(seq: subscribed.seq)
                case .error(let error): throw VibedClientError.rpc(error)
                }
            }
            // The hello's answer, or an error for a line vibed couldn't
            // read, whose id is null.
            switch try decoder.decode(RPCResponse<HelloResult>.self, from: line).outcome {
            case .result: return nil
            case .error(let error): throw VibedClientError.rpc(error)
            }
        } catch let error as VibedClientError {
            throw error
        } catch {
            throw VibedClientError.badResponse("\(error)")
        }
    }
}

/// Lines from a socket, keeping what follows each newline for the next.
struct LineReader {
    let fd: Int32
    private var buffer = Data()

    init(fd: Int32) {
        self.fd = fd
    }

    /// The next line without its newline, or nil once the socket is closed.
    /// A read that times out is retried until `stopping` says otherwise.
    mutating func next(stopping: () -> Bool) throws -> Data? {
        var chunk = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            if let newline = buffer.firstIndex(of: 0x0A) {
                let line = Data(buffer[buffer.startIndex..<newline])
                buffer = Data(buffer[buffer.index(after: newline)...])
                return line
            }
            if buffer.count > VibeRPC.maxMessageBytes { throw VibedClientError.messageTooLarge }
            let n = chunk.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress, $0.count) }
            if n < 0 {
                let code = errno
                if code == EINTR { continue }
                if code == EAGAIN {
                    if stopping() { return nil }
                    continue
                }
                throw VibedClientError.io(errno: code)
            }
            if n == 0 { return nil }
            buffer.append(contentsOf: chunk[0..<n])
        }
    }
}
