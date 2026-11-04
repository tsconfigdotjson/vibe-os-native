import Darwin
import Foundation
import Testing

@testable import VibeRPC

@Suite struct EventTests {
    func line(_ text: String) -> Data { Data(text.utf8) }

    @Test func linesBecomeMessages() throws {
        #expect(
            try EventConnection.message(
                from: line(#"{"jsonrpc":"2.0","id":"subscribe","result":{"seq":10}}"#)) == .subscribed(seq: 10))
        #expect(
            try EventConnection.message(
                from: line(
                    #"{"jsonrpc":"2.0","id":"hello","result":{"daemon":"vibed","version":"0.1.0","protocol":2,"os":"macos","arch":"aarch64","pid":1}}"#
                )) == nil)
        let event = try EventConnection.message(
            from: line(
                #"{"jsonrpc":"2.0","method":"events.event","params":{"seq":12,"at":"2026-10-30T12:00:00Z","host":"local","kind":"agent.state","project":"web","ref":"quiet-amber-otter","state":"question"}}"#
            ))
        guard case .event(let e) = event else {
            Issue.record("not an event: \(String(describing: event))")
            return
        }
        #expect(e.seq == 12)
        #expect(e.kind == .agentState)
        #expect(e.ref == "quiet-amber-otter")
        #expect(e.state == "question")
    }

    @Test func errorsEndTheSubscription() {
        #expect(
            throws: VibedClientError.rpc(RPCError(code: -32000, message: "Protocol version mismatch"))
        ) {
            try EventConnection.message(
                from: line(#"{"jsonrpc":"2.0","id":"hello","error":{"code":-32000,"message":"Protocol version mismatch"}}"#))
        }
        #expect(throws: VibedClientError.rpc(RPCError(code: -32002, message: "already subscribed"))) {
            try EventConnection.message(
                from: line(#"{"jsonrpc":"2.0","id":"subscribe","error":{"code":-32002,"message":"already subscribed"}}"#))
        }
        #expect(throws: VibedClientError.self) {
            try EventConnection.message(from: line("not json"))
        }
    }

    @Test func linesSplitAcrossReadsAndJoinedInOne() throws {
        var fds: [Int32] = [0, 0]
        #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
        defer {
            close(fds[0])
            close(fds[1])
        }
        let write = { (text: String) in
            _ = text.utf8CString.withUnsafeBufferPointer { Darwin.write(fds[1], $0.baseAddress, $0.count - 1) }
        }
        var reader = LineReader(fd: fds[0])
        write("one\ntw")
        #expect(try reader.next(stopping: { false }) == Data("one".utf8))
        write("o\nthree\n")
        #expect(try reader.next(stopping: { false }) == Data("two".utf8))
        #expect(try reader.next(stopping: { false }) == Data("three".utf8))
        shutdown(fds[1], SHUT_WR)
        #expect(try reader.next(stopping: { false }) == nil)
    }

    @Test func noDaemonEndsTheStream() async {
        let client = VibedClient(socketPath: "/tmp/vibe-no-such-\(UUID()).sock")
        do {
            for try await _ in client.events(client: "test") {}
            Issue.record("the stream ended without an error")
        } catch let error as VibedClientError {
            #expect(error == .connect(path: client.socketPath, errno: ENOENT))
        } catch {
            Issue.record("unexpected error: \(error)")
        }
    }
}
