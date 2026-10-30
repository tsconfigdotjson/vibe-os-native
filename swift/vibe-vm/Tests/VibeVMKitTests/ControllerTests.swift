import Darwin
import Foundation
import Testing

@testable import VibeVMKit

/// A VM that records what it's asked to do.
@MainActor
final class FakeMachine: Machine {
    var state: MachineState = .stopped
    let memorySize: UInt64 = 1 << 30
    // Not a whole MiB, so the balloon's floor must round it up.
    let minimumMemorySize: UInt64 = (1 << 20) + 5
    var calls: [String] = []
    var failStart = false
    var failRestore = false
    var failPause = false
    var failSave = false
    var failConnect = false
    var guestEnds: [Int32] = []
    /// If set, each resume notes whether this file is still there.
    var snapshot: String?

    func start() async throws {
        calls.append("start")
        if failStart { throw Failure(.bootFailed, "no kernel") }
        state = .running
    }

    func restore(from url: URL) async throws {
        calls.append("restore")
        if failRestore { throw Failure(.restoreFailed, "invalid argument") }
        #expect(FileManager.default.fileExists(atPath: url.path))
        state = .paused
    }

    func pause() async throws {
        calls.append("pause")
        if failPause { throw Failure(.saveFailed, "can't") }
        state = .paused
    }

    func resume() async throws {
        if let snapshot, FileManager.default.fileExists(atPath: snapshot) {
            calls.append("resume with the snapshot still there")
        }
        calls.append("resume")
        state = .running
    }

    func save(to url: URL) async throws {
        calls.append("save \(url.lastPathComponent)")
        #expect(state == .paused)
        try Data("snapshot".utf8).write(to: url)
        if failSave { throw Failure(.saveFailed, "No space left on device") }
    }

    func stop() async throws {
        calls.append("stop")
        state = .stopped
    }

    func setBalloonTarget(_ bytes: UInt64) {
        calls.append("balloon \(bytes >> 20)")
    }

    func connect(port: UInt32) async throws -> GuestStream {
        calls.append("connect \(port)")
        if failConnect { throw Failure(.connectFailed, "Connection reset by peer") }
        var fds: [Int32] = [0, 0]
        #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
        guestEnds.append(fds[1])
        return GuestStream(id: guestEnds.count, fd: fds[0])
    }

    func closeStream(_ id: Int) {
        calls.append("close \(id)")
    }
}

/// Records the balloon waits a controller asks for.
final class Waits: @unchecked Sendable {
    private let lock = NSLock()
    private var waits: [UInt64] = []
    var during: (@Sendable () async -> Void)?

    var all: [UInt64] {
        lock.lock()
        defer { lock.unlock() }
        return waits
    }

    func wait(_ ms: UInt64) async {
        lock.lock()
        waits.append(ms)
        lock.unlock()
        await during?()
    }
}

@MainActor
func makeController(_ machine: FakeMachine, waits: Waits = Waits()) throws -> (Controller, URL) {
    let dir = try temporaryDirectory()
    var config = try exampleConfig()
    config.snapshot = dir.appendingPathComponent("snapshots/quiet-amber-otter").path
    let controller = Controller(config: config, machine: machine, pid: 42, wait: { ms in await waits.wait(ms) })
    return (controller, dir)
}

func decode<T: Decodable>(_ type: T.Type, _ outcome: Outcome) throws -> T {
    let line: Data
    switch outcome {
    case .reply(let data), .stream(let data, _), .exit(let data, _): line = data
    }
    return try JSONDecoder().decode(T.self, from: line)
}

func failure(_ outcome: Outcome) throws -> Failure {
    guard case .reply = outcome else { throw Failure(.badRequest, "not a plain reply: \(outcome)") }
    return try decode(Failure.self, outcome)
}

@Suite @MainActor struct ControllerTests {
    @Test func bootReportsTimingsAndStatus() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        let ready = try await controller.boot(since: ContinuousClock.now).get()
        #expect(ready.started == .boot)
        #expect(ready.pid == 42)
        #expect(ready.timings.startMS != nil && ready.timings.totalMS != nil)
        #expect(ready.timings.restoreMS == nil)
        let status = try decode(Status.self, await controller.handle(.status))
        #expect(status == Status(ref: "quiet-amber-otter", pid: 42, state: .running, started: .boot))
    }

    @Test func aFailedBootSaysWhy() async throws {
        let machine = FakeMachine()
        machine.failStart = true
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        let result = await controller.boot(since: ContinuousClock.now)
        #expect(result == .failure(Failure(.bootFailed, "can't start the VM: no kernel")))
    }

    @Test func sleepBalloonsPausesSavesAndStops() async throws {
        let machine = FakeMachine()
        let waits = Waits()
        let (controller, dir) = try makeController(machine, waits: waits)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        let outcome = await controller.handle(.sleep(balloonMS: nil, balloonFloor: nil))
        guard case .exit(_, let code) = outcome else {
            Issue.record("a sleep must end vibe-vm: \(outcome)")
            return
        }
        #expect(code == 0)
        let slept = try decode(Slept.self, outcome)
        #expect(slept.snapshot == controller.config.snapshot)
        #expect(slept.snapshotBytes > 0)
        for step in [slept.timings.balloonMS, slept.timings.pauseMS, slept.timings.saveMS, slept.timings.stopMS, slept.timings.totalMS] {
            #expect(step != nil)
        }
        // The balloon goes down to the floor (1 MiB and 5 bytes, rounded up
        // to whole MiB), then back to the VM's 1 GiB, before the pause.
        #expect(
            machine.calls == [
                "start", "balloon 2", "balloon 1024", "pause", "save .quiet-amber-otter.partial", "stop",
            ])
        #expect(waits.all == [Controller.defaultBalloonMS])
        #expect(controller.stopping)
        // Until vibe-vm exits, nothing else may touch the stopped VM.
        #expect(try failure(await controller.handle(.stop)) == Failure(.busy, "the VM is already stopped"))
        // The VM stopping now isn't the guest dying.
        #expect(controller.machineStopped(error: nil) == nil)
        let snapshots = (controller.config.snapshot as NSString).deletingLastPathComponent
        #expect(try FileManager.default.contentsOfDirectory(atPath: snapshots) == ["quiet-amber-otter"])
        let mode = try FileManager.default.attributesOfItem(atPath: controller.config.snapshot)[.posixPermissions]
        #expect(mode as? Int == 0o600, "the snapshot holds the guest's memory")
    }

    @Test func vibedCanSetTheBalloonsFloor() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        _ = try decode(Slept.self, await controller.handle(.sleep(balloonMS: 5, balloonFloor: 300 << 20)))
        #expect(machine.calls.prefix(3) == ["start", "balloon 300", "balloon 1024"])
        #expect(controller.balloonTarget(floor: nil) == 2 << 20)
        #expect(controller.balloonTarget(floor: (300 << 20) + 1) == 301 << 20)
        #expect(controller.balloonTarget(floor: 1 << 40) == 1 << 30)
        #expect(controller.balloonTarget(floor: .max) == 1 << 30)
    }

    @Test func aFreshBootDeletesAStaleSnapshot() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        let snapshot = controller.config.snapshot
        try FileManager.default.createDirectory(
            atPath: (snapshot as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
        for path in [snapshot, controller.partialPath] {
            try Data("old".utf8).write(to: URL(fileURLWithPath: path))
        }
        _ = try await controller.boot(since: ContinuousClock.now).get()
        #expect(!FileManager.default.fileExists(atPath: snapshot))
        #expect(!FileManager.default.fileExists(atPath: controller.partialPath))
    }

    @Test func thePartialSnapshotIsNeverAnotherAgents() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        let name = (controller.partialPath as NSString).lastPathComponent
        #expect(name == ".quiet-amber-otter.partial")
        #expect(!isRef(name))
    }

    @Test func aSleepCanSkipTheBalloon() async throws {
        let machine = FakeMachine()
        let waits = Waits()
        let (controller, dir) = try makeController(machine, waits: waits)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        let slept = try decode(Slept.self, await controller.handle(.sleep(balloonMS: 0, balloonFloor: nil)))
        #expect(slept.timings.balloonMS != nil)
        #expect(machine.calls == ["start", "pause", "save .quiet-amber-otter.partial", "stop"])
        #expect(waits.all.isEmpty)
    }

    @Test func aFailedSaveResumesAndLeavesNothing() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        machine.failSave = true
        let refused = try failure(await controller.handle(.sleep(balloonMS: 0, balloonFloor: nil)))
        #expect(refused == Failure(.saveFailed, "can't save the VM: No space left on device; it was resumed"))
        #expect(machine.state == .running)
        #expect(!controller.stopping)
        let snapshots = (controller.config.snapshot as NSString).deletingLastPathComponent
        #expect(try FileManager.default.contentsOfDirectory(atPath: snapshots).isEmpty)
        // It isn't left busy: the next sleep works.
        machine.failSave = false
        _ = try decode(Slept.self, await controller.handle(.sleep(balloonMS: 0, balloonFloor: nil)))
    }

    @Test func aFailedPauseKeepsRunning() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        machine.failPause = true
        let refused = try failure(await controller.handle(.sleep(balloonMS: 0, balloonFloor: nil)))
        #expect(refused.code == .saveFailed)
        #expect(machine.state == .running)
        #expect(!machine.calls.contains { $0.hasPrefix("save") })
    }

    @Test func aPausedVMSleepsWithoutTheBalloon() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        machine.state = .paused
        _ = try decode(Slept.self, await controller.handle(.sleep(balloonMS: nil, balloonFloor: nil)))
        #expect(machine.calls == ["start", "save .quiet-amber-otter.partial", "stop"])
    }

    @Test func requestsWhileSavingAreRefused() async throws {
        let machine = FakeMachine()
        let waits = Waits()
        let (controller, dir) = try makeController(machine, waits: waits)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        let refusals = Refusals()
        // While the balloon is up, everything that changes the VM waits its
        // turn by being refused; a status still answers.
        waits.during = {
            for request in [Request.sleep(balloonMS: 0, balloonFloor: nil), .stop, .connect(port: 1024)] {
                refusals.add(try? failure(await controller.handle(request)))
            }
            let status = try? decode(Status.self, await controller.handle(.status))
            refusals.state = status?.state
        }
        _ = try decode(Slept.self, await controller.handle(.sleep(balloonMS: 1, balloonFloor: nil)))
        #expect(refusals.all == Array(repeating: Failure(.busy, "the VM is already saving"), count: 3))
        #expect(refusals.state == .running)
    }

    @Test func requestsNeedTheRightState() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(try failure(await controller.handle(.sleep(balloonMS: 0, balloonFloor: nil))) == Failure(.badState, "the VM is stopped"))
        _ = await controller.boot(since: ContinuousClock.now)
        machine.state = .paused
        #expect(try failure(await controller.handle(.connect(port: 1))) == Failure(.badState, "the VM is paused"))
    }

    @Test func connectGivesAGuestStream() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        let outcome = await controller.handle(.connect(port: 1024))
        guard case .stream(_, let stream) = outcome else {
            Issue.record("not a stream: \(outcome)")
            return
        }
        #expect(stream.id == 1)
        _ = try decode(Done.self, outcome)
        controller.closeStream(stream.id)
        #expect(machine.calls.suffix(2) == ["connect 1024", "close 1"])
        close(stream.fd)
        machine.guestEnds.forEach { close($0) }

        machine.failConnect = true
        let refused = try failure(await controller.handle(.connect(port: 9)))
        #expect(refused == Failure(.connectFailed, "nothing in the guest accepted vsock port 9: Connection reset by peer"))
    }

    @Test func stopEndsVibeVM() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        guard case .exit(_, 0) = await controller.handle(.stop) else {
            Issue.record("a stop must end vibe-vm")
            return
        }
        #expect(machine.calls == ["start", "stop"])
        #expect(controller.machineStopped(error: nil) == nil)
    }

    @Test func theGuestStoppingEndsVibeVM() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await controller.boot(since: ContinuousClock.now)
        #expect(controller.machineStopped(error: nil) == 0)
        #expect(controller.machineStopped(error: Failure(.bootFailed, "boom")) == 1)
    }

    @Test func restoreResumesAndDeletesTheSnapshot() async throws {
        let machine = FakeMachine()
        let (controller, dir) = try makeController(machine)
        defer { try? FileManager.default.removeItem(at: dir) }
        let snapshot = controller.config.snapshot

        #expect(
            await controller.restore(since: ContinuousClock.now)
                == .failure(Failure(.noSnapshot, "there's no snapshot at \(snapshot)")))

        try FileManager.default.createDirectory(
            atPath: (snapshot as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
        try Data("saved".utf8).write(to: URL(fileURLWithPath: snapshot))
        machine.failRestore = true
        let failed = await controller.restore(since: ContinuousClock.now)
        #expect(failed == .failure(Failure(.restoreFailed, "can't restore \(snapshot): invalid argument")))
        // vibed decides what to do with a snapshot that won't restore.
        #expect(FileManager.default.fileExists(atPath: snapshot))

        machine.failRestore = false
        machine.calls = []
        machine.snapshot = snapshot
        let ready = try await controller.restore(since: ContinuousClock.now).get()
        #expect(ready.started == .restore)
        #expect(ready.timings.restoreMS != nil && ready.timings.resumeMS != nil && ready.timings.totalMS != nil)
        // The snapshot is gone before the VM runs again.
        #expect(machine.calls == ["restore", "resume"])
        #expect(machine.state == .running)
        #expect(!FileManager.default.fileExists(atPath: snapshot))
        let status = try decode(Status.self, await controller.handle(.status))
        #expect(status.started == .restore)
    }

    @Test func millisecondsRoundDown() {
        #expect(milliseconds(.milliseconds(1500)) == 1500)
        #expect(milliseconds(.microseconds(2999)) == 2)
        #expect(milliseconds(.seconds(-1)) == 0)
    }
}

final class Refusals: @unchecked Sendable {
    private let lock = NSLock()
    private var failures: [Failure] = []
    var state: MachineState?

    func add(_ failure: Failure?) {
        lock.lock()
        if let failure { failures.append(failure) }
        lock.unlock()
    }

    var all: [Failure] {
        lock.lock()
        defer { lock.unlock() }
        return failures
    }
}
