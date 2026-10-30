import Darwin
import Foundation

/// Serves the control socket. A thread accepts connections, and each gets a
/// thread of its own that reads its request line, has `handle` (the
/// controller, on the main actor) deal with it, and acts on the outcome.
public final class ControlServer: Sendable {
    let socket: ControlSocket
    let handle: @Sendable (Request) async -> Outcome
    let closeStream: @Sendable (Int) async -> Void
    let exit: @Sendable (Int32) -> Void

    public init(
        socket: ControlSocket,
        handle: @escaping @Sendable (Request) async -> Outcome,
        closeStream: @escaping @Sendable (Int) async -> Void,
        exit: @escaping @Sendable (Int32) -> Void
    ) {
        self.socket = socket
        self.handle = handle
        self.closeStream = closeStream
        self.exit = exit
    }

    public func start() {
        let acceptor = Thread { [self] in
            while let client = socket.accept() {
                let worker = Thread { [self] in serve(client) }
                worker.stackSize = 512 * 1024
                worker.start()
            }
            logMessage("can't accept on \(socket.path) any more")
            self.exit(1)
        }
        acceptor.start()
    }

    func serve(_ client: Int32) {
        let outcome: Outcome
        do {
            guard let line = try readLine(client, max: ControlProtocol.maxRequestBytes) else {
                close(client)
                return
            }
            switch Request.parse(line) {
            case .failure(let failure):
                outcome = .reply(Wire.line(failure))
            case .success(let request):
                let handle = self.handle
                outcome = blockingWait { await handle(request) }
            }
        } catch {
            outcome = .reply(Wire.line(error as? Failure ?? Failure(.badRequest, describe(error))))
        }
        switch outcome {
        case .reply(let line):
            try? writeAll(client, line)
            close(client)
        case .stream(let line, let stream):
            let closeStream = self.closeStream
            do {
                try writeAll(client, line)
            } catch {
                close(client)
                Task { await closeStream(stream.id) }
                return
            }
            splice(client: client, guest: stream.fd) {
                Task { await closeStream(stream.id) }
            }
        case .exit(let line, let code):
            try? writeAll(client, line)
            close(client)
            self.exit(code)
        }
    }
}

/// Runs `body` on the concurrency runtime and blocks this thread until it's
/// done. Only for the server's own threads, never the main one.
func blockingWait<T: Sendable>(_ body: @escaping @Sendable () async -> T) -> T {
    let box = Box<T>()
    let done = DispatchSemaphore(value: 0)
    Task {
        box.value = await body()
        done.signal()
    }
    done.wait()
    return box.value!
}

private final class Box<T>: @unchecked Sendable {
    var value: T?
}
