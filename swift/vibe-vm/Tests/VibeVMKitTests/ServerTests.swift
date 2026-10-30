import Darwin
import Foundation
import Testing
import Virtualization

@testable import VibeVMKit

/// Things seen on other threads.
final class Seen: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [Int] = []

    func add(_ value: Int) {
        lock.lock()
        values.append(value)
        lock.unlock()
    }

    var all: [Int] {
        lock.lock()
        defer { lock.unlock() }
        return values
    }

    /// Waits up to 5 s for `value`.
    func waitFor(_ value: Int) -> Bool {
        for _ in 0..<500 {
            if all.contains(value) { return true }
            usleep(10_000)
        }
        return false
    }
}

func setTimeout(_ fd: Int32) {
    var tv = timeval(tv_sec: 5, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
}

/// Sends one request line; returns the connection and the reply line.
func request(_ path: String, _ line: String) throws -> (Int32, String) {
    let fd = try connectUnix(path)
    setTimeout(fd)
    try writeAll(fd, Data((line + "\n").utf8))
    let reply = try #require(try readLine(fd, max: 1 << 16))
    return (fd, String(decoding: reply, as: UTF8.self))
}

func readSome(_ fd: Int32) -> String {
    var buffer = [UInt8](repeating: 0, count: 256)
    let n = read(fd, &buffer, buffer.count)
    return n > 0 ? String(decoding: buffer[0..<n], as: UTF8.self) : ""
}

@Suite struct ServerTests {
    @Test func oneServerPerSocket() throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("run/x.vm").path
        var first: ControlSocket? = try ControlSocket.bind(path: path)
        withExtendedLifetime(first) {
            #expect(throws: Failure(.alreadyRunning, "another vibe-vm is already serving \(path)")) {
                try ControlSocket.bind(path: path)
            }
        }
        // The run directory is made private.
        let mode = try FileManager.default.attributesOfItem(atPath: dir.appendingPathComponent("run").path)[.posixPermissions]
        #expect(mode as? Int == 0o700)
        // Its server gone, the socket left behind is replaced.
        first = nil
        _ = first
        let second = try ControlSocket.bind(path: path)
        #expect(second.path == path)

        let file = dir.appendingPathComponent("file.vm").path
        try Data().write(to: URL(fileURLWithPath: file))
        #expect(throws: Failure(.badConfig, "\(file) exists and isn't a socket; not touching it")) {
            try ControlSocket.bind(path: file)
        }
        let long = "/tmp/" + String(repeating: "x", count: 120)
        #expect(throws: Failure(.badConfig, "the socket path is too long: \(long)")) {
            try ControlSocket.bind(path: long)
        }
    }

    @Test func serverRepliesSplicesAndExits() throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("x.vm").path
        let socket = try ControlSocket.bind(path: path)
        var guest: [Int32] = [0, 0]
        #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &guest) == 0)
        let serverEnd = guest[0]
        let exits = Seen()
        let closed = Seen()
        let server = ControlServer(
            socket: socket,
            handle: { request in
                switch request {
                case .status: .reply(Wire.line(Status(ref: "x", pid: 1, state: .running, started: .boot)))
                case .connect: .stream(Wire.line(Done()), GuestStream(id: 7, fd: serverEnd))
                case .stop: .exit(Wire.line(Done()), code: 3)
                case .sleep: .reply(Wire.line(Failure(.busy, "the VM is already saving")))
                }
            },
            closeStream: { id in closed.add(id) },
            exit: { code in exits.add(Int(code)) })
        server.start()

        var (fd, reply) = try request(path, #"{"op":"status"}"#)
        close(fd)
        #expect(reply == #"{"ok":true,"pid":1,"protocol":1,"ref":"x","started":"boot","state":"running"}"#)

        (fd, reply) = try request(path, "nonsense")
        close(fd)
        #expect(reply.contains(#""code":"bad_request""#))

        (fd, reply) = try request(path, #"{"op":"sleep"}"#)
        close(fd)
        #expect(reply == #"{"code":"busy","error":"the VM is already saving","ok":false}"#)

        // A connect hands the connection to the guest, both ways.
        let guestEnd = guest[1]
        setTimeout(guestEnd)
        (fd, reply) = try request(path, #"{"op":"connect","port":1024}"#)
        #expect(reply == #"{"ok":true}"#)
        try writeAll(fd, Data("ping\n".utf8))
        #expect(readSome(guestEnd) == "ping\n")
        try writeAll(guestEnd, Data("pong\n".utf8))
        #expect(readSome(fd) == "pong\n")
        // The client's end of input reaches the guest as one.
        shutdown(fd, SHUT_WR)
        #expect(readSome(guestEnd) == "")
        // The guest can still answer after that.
        try writeAll(guestEnd, Data("bye\n".utf8))
        #expect(readSome(fd) == "bye\n")
        close(guestEnd)
        #expect(readSome(fd) == "")
        close(fd)
        #expect(closed.waitFor(7), "the guest stream was never closed")

        (fd, reply) = try request(path, #"{"op":"stop"}"#)
        close(fd)
        #expect(reply == #"{"ok":true}"#)
        #expect(exits.waitFor(3))
    }

    @Test func requestLinesHaveALimit() throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("x.vm").path
        let server = ControlServer(
            socket: try ControlSocket.bind(path: path),
            handle: { _ in .reply(Wire.line(Done())) }, closeStream: { _ in }, exit: { _ in })
        server.start()
        let (fd, reply) = try request(path, String(repeating: "x", count: ControlProtocol.maxRequestBytes + 2))
        close(fd)
        #expect(reply == #"{"code":"bad_request","error":"the request is too long","ok":false}"#)
    }
}

@Suite struct BuilderTests {
    /// A config whose files all exist, under `dir`.
    @MainActor
    func realConfig(_ dir: URL) throws -> VMConfig {
        let files = FileManager.default
        for name in ["Image", "initrd", "base.img", "agent.img"] {
            try Data(count: 1 << 20).write(to: dir.appendingPathComponent(name))
        }
        try files.createDirectory(at: dir.appendingPathComponent("worktree"), withIntermediateDirectories: true)
        var config = try exampleConfig()
        config.kernel = dir.appendingPathComponent("Image").path
        config.initrd = dir.appendingPathComponent("initrd").path
        config.disks = [
            .init(id: "base", path: dir.appendingPathComponent("base.img").path, readOnly: true),
            .init(id: "agent", path: dir.appendingPathComponent("agent.img").path, readOnly: false),
        ]
        config.shares = [.init(tag: "worktree", path: dir.appendingPathComponent("worktree").path, readOnly: false)]
        config.machineID = VZBuilder.newMachineID()
        config.mac = "02:5f:3a:11:9c:e4"
        return config
    }

    @Test @MainActor func everyDeviceIsThere() throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let config = try realConfig(dir)
        let vz = try VZBuilder.configuration(for: config, console: FileHandle.nullDevice)
        #expect(vz.cpuCount == config.cpus)
        #expect(vz.memorySize == config.memory)
        let boot = try #require(vz.bootLoader as? VZLinuxBootLoader)
        #expect(boot.kernelURL.path == config.kernel)
        #expect(boot.initialRamdiskURL?.path == config.initrd)
        #expect(boot.commandLine == config.cmdline)
        let platform = try #require(vz.platform as? VZGenericPlatformConfiguration)
        #expect(platform.machineIdentifier.dataRepresentation == Data(base64Encoded: try #require(config.machineID)))

        let disks = vz.storageDevices.compactMap { $0 as? VZVirtioBlockDeviceConfiguration }
        #expect(disks.map(\.blockDeviceIdentifier) == ["base", "agent"])
        let readOnly = disks.map { ($0.attachment as? VZDiskImageStorageDeviceAttachment)?.isReadOnly }
        #expect(readOnly == [true, false])
        let shares = vz.directorySharingDevices.compactMap { $0 as? VZVirtioFileSystemDeviceConfiguration }
        #expect(shares.map(\.tag) == ["worktree"])
        let network = try #require(vz.networkDevices.first as? VZVirtioNetworkDeviceConfiguration)
        #expect(network.macAddress.string == "02:5f:3a:11:9c:e4")
        #expect(network.attachment is VZNATNetworkDeviceAttachment)
        #expect(vz.memoryBalloonDevices.count == 1)
        #expect(vz.entropyDevices.count == 1)
        #expect(vz.socketDevices.count == 1)
        #expect(vz.serialPorts.count == 1)
        // Only a binary signed with the entitlement may validate; `swift
        // test` isn't one. The vm-cycle step in check-full.sh validates
        // through the signed vibe-vm.
        if HostSupport.current().ready {
            try VZBuilder.validate(vz)
        }
    }

    @Test @MainActor func missingPiecesAreRefused() throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let good = try realConfig(dir)
        let cases: [((inout VMConfig) -> Void, String)] = [
            ({ $0.machineID = nil }, "no valid machine_id"),
            ({ $0.machineID = "AAAA" }, "no valid machine_id"),
            ({ $0.mac = nil }, "no valid mac"),
            ({ $0.kernel = dir.appendingPathComponent("none").path }, "the kernel"),
            ({ $0.initrd = dir.appendingPathComponent("none").path }, "the initrd"),
            ({ $0.disks[1].path = dir.appendingPathComponent("none").path }, "disk `agent`"),
            ({ $0.shares[0].path = good.kernel }, "isn't a directory"),
        ]
        for (change, message) in cases {
            var config = good
            change(&config)
            do {
                _ = try VZBuilder.configuration(for: config, console: FileHandle.nullDevice)
                Issue.record("\(message): accepted")
            } catch let failure as Failure {
                #expect(failure.code == .badConfig)
                #expect(failure.error.contains(message), "\(failure.error)")
            }
        }
    }
}
