import Darwin
import Foundation

/// A VM as the controller drives it: Virtualization.framework's in vibe-vm
/// (``VZMachine``), a fake in the tests. Everything runs on the main actor,
/// as Virtualization.framework requires.
@MainActor
public protocol Machine: AnyObject {
    var state: MachineState { get }
    /// The VM's RAM, in bytes.
    var memorySize: UInt64 { get }
    /// The least RAM the balloon may leave the guest.
    var minimumMemorySize: UInt64 { get }
    func start() async throws
    /// Reads a saved VM back; it's paused afterwards.
    func restore(from url: URL) async throws
    func pause() async throws
    func resume() async throws
    func save(to url: URL) async throws
    func stop() async throws
    func setBalloonTarget(_ bytes: UInt64)
    /// A stream to vsock `port` in the guest.
    func connect(port: UInt32) async throws -> GuestStream
    /// Closes a stream ``connect(port:)`` opened.
    func closeStream(_ id: Int)
}

/// A connection to a vsock port in the guest: a socket the server splices
/// a client to, closed with ``Machine/closeStream(_:)``.
public struct GuestStream: Equatable, Sendable {
    public var id: Int
    public var fd: Int32

    public init(id: Int, fd: Int32) {
        self.id = id
        self.fd = fd
    }
}

/// What the server does with a request's connection.
public enum Outcome: Sendable {
    /// Writes the reply line and closes it.
    case reply(Data)
    /// Writes the reply line, then splices it to the guest stream.
    case stream(Data, GuestStream)
    /// Writes the reply line, closes it, and exits with `code`.
    case exit(Data, code: Int32)
}

/// Runs one VM's life: its start, then each request on the control socket.
@MainActor
public final class Controller {
    /// How long a sleep keeps the balloon inflated, unless it says.
    public static let defaultBalloonMS: UInt64 = 2000

    public let config: VMConfig
    let machine: any Machine
    let pid: Int32
    let wait: @Sendable (UInt64) async -> Void
    let clock = ContinuousClock()
    public private(set) var started: Started?
    /// What is changing the VM's state; other such requests are refused.
    private var busy: String?
    /// Set before a stop this process asks for, so that stop isn't taken
    /// for the guest dying.
    public private(set) var stopping = false

    /// `wait` sleeps for a number of milliseconds; tests pass their own.
    public init(
        config: VMConfig, machine: any Machine, pid: Int32 = getpid(),
        wait: @escaping @Sendable (UInt64) async -> Void = { ms in
            try? await Task.sleep(for: .milliseconds(ms))
        }
    ) {
        self.config = config
        self.machine = machine
        self.pid = pid
        self.wait = wait
    }

    /// Boots the VM fresh. A snapshot left from before is deleted first: once
    /// the fresh VM writes to the disks, it could never be restored safely.
    public func boot(since processStart: ContinuousClock.Instant) async -> Result<Ready, Failure> {
        started = .boot
        busy = "starting"
        for stale in [config.snapshot, partialPath] where FileManager.default.fileExists(atPath: stale) {
            do {
                try FileManager.default.removeItem(atPath: stale)
                logMessage("deleted \(stale): a fresh boot makes it stale")
            } catch {
                return .failure(Failure(.bootFailed, "can't delete the stale snapshot \(stale): \(describe(error))"))
            }
        }
        let t = clock.now
        do {
            try await machine.start()
        } catch {
            return .failure(Failure(.bootFailed, "can't start the VM: \(describe(error))"))
        }
        busy = nil
        var timings = Timings()
        timings.startMS = milliseconds(clock.now - t)
        timings.totalMS = milliseconds(clock.now - processStart)
        return .success(Ready(pid: pid, started: .boot, timings: timings))
    }

    /// Restores the VM from its snapshot, deletes the snapshot, and resumes
    /// the VM. The snapshot goes before the VM runs: from then on its disks
    /// move on, so it must never be restored again. A snapshot that won't
    /// restore is kept, for vibed to discard (PRD: Failures).
    public func restore(since processStart: ContinuousClock.Instant) async -> Result<Ready, Failure> {
        started = .restore
        busy = "restoring"
        let snapshot = config.snapshot
        guard FileManager.default.fileExists(atPath: snapshot) else {
            return .failure(Failure(.noSnapshot, "there's no snapshot at \(snapshot)"))
        }
        var timings = Timings()
        var t = clock.now
        do {
            try await machine.restore(from: URL(fileURLWithPath: snapshot))
        } catch {
            return .failure(Failure(.restoreFailed, "can't restore \(snapshot): \(describe(error))"))
        }
        timings.restoreMS = milliseconds(clock.now - t)
        do {
            try FileManager.default.removeItem(atPath: snapshot)
        } catch {
            stopping = true
            try? await machine.stop()
            return .failure(
                Failure(.restoreFailed, "can't remove the snapshot \(snapshot) before resuming: \(describe(error))"))
        }
        t = clock.now
        do {
            try await machine.resume()
        } catch {
            return .failure(Failure(.restoreFailed, "can't resume the VM restored from \(snapshot): \(describe(error))"))
        }
        busy = nil
        timings.resumeMS = milliseconds(clock.now - t)
        timings.totalMS = milliseconds(clock.now - processStart)
        return .success(Ready(pid: pid, started: .restore, timings: timings))
    }

    /// Where a save writes before it's renamed into place: beside the
    /// snapshot, under a name no ref can have (refs never start with a dot),
    /// so it's never another agent's snapshot.
    var partialPath: String {
        let snapshot = config.snapshot as NSString
        return snapshot.deletingLastPathComponent + "/." + snapshot.lastPathComponent + ".partial"
    }

    public func handle(_ request: Request) async -> Outcome {
        switch request {
        case .status:
            let status = Status(ref: config.ref, pid: pid, state: machine.state, started: started ?? .boot)
            return .reply(Wire.line(status))
        case .sleep(let balloonMS, let balloonFloor):
            return await sleep(balloonMS: balloonMS ?? Controller.defaultBalloonMS, floor: balloonFloor)
        case .stop:
            return await stop()
        case .connect(let port):
            return await connect(port: port)
        }
    }

    public func closeStream(_ id: Int) {
        machine.closeStream(id)
    }

    /// The VM stopped: the exit code vibe-vm should exit with, or nil if
    /// this process stopped it and will exit on its own.
    public func machineStopped(error: (any Error)?) -> Int32? {
        if stopping { return nil }
        if let error {
            logMessage("the VM stopped with an error: \(describe(error))")
            return 1
        }
        logMessage("the guest stopped the VM")
        return 0
    }

    private func busyFailure() -> Failure? {
        busy.map { Failure(.busy, "the VM is already \($0)") }
    }

    private func sleep(balloonMS: UInt64, floor: UInt64?) async -> Outcome {
        if let failure = busyFailure() { return .reply(Wire.line(failure)) }
        let state = machine.state
        guard state == .running || state == .paused else {
            return .reply(Wire.line(Failure(.badState, "the VM is \(state.rawValue)")))
        }
        // Stays set once the save succeeds: vibe-vm is on its way out.
        busy = "saving"
        let wasRunning = state == .running
        let start = clock.now
        var timings = Timings()

        // 1. Inflate the balloon, so the guest hands back the pages it has
        // freed, then let it go: the pages stay untouched, and out of the
        // snapshot, until the guest uses them again.
        var t = clock.now
        if wasRunning && balloonMS > 0 {
            machine.setBalloonTarget(balloonTarget(floor: floor))
            await wait(balloonMS)
            machine.setBalloonTarget(machine.memorySize)
        }
        timings.balloonMS = milliseconds(clock.now - t)

        // 2. Pause.
        t = clock.now
        if wasRunning {
            do {
                try await machine.pause()
            } catch {
                busy = nil
                return .reply(Wire.line(Failure(.saveFailed, "can't pause the VM: \(describe(error))")))
            }
        }
        timings.pauseMS = milliseconds(clock.now - t)

        // 3. Save, to a partial file made durable and renamed into place: a
        // crash or power loss never leaves something that looks like a
        // snapshot but isn't one. It holds all the guest's memory, secrets
        // included, so only this user may read it.
        t = clock.now
        let snapshot = config.snapshot
        let partial = partialPath
        let dir = (snapshot as NSString).deletingLastPathComponent
        let files = FileManager.default
        do {
            try? files.removeItem(atPath: partial)
            try files.createDirectory(
                atPath: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try await machine.save(to: URL(fileURLWithPath: partial))
            try makeDurable(partial)
            try? files.removeItem(atPath: snapshot)
            guard rename(partial, snapshot) == 0 else {
                throw Failure(.saveFailed, "can't move it to \(snapshot): \(errnoString())")
            }
            try makeDurable(dir)
        } catch {
            // Nothing may look like a snapshot of a VM that runs on.
            busy = nil
            try? files.removeItem(atPath: partial)
            try? files.removeItem(atPath: snapshot)
            var why = "can't save the VM: \(describe(error))"
            if wasRunning {
                do {
                    try await machine.resume()
                    why += "; it was resumed"
                } catch {
                    why += "; and it can't be resumed: \(describe(error))"
                }
            }
            return .reply(Wire.line(Failure(.saveFailed, why)))
        }
        timings.saveMS = milliseconds(clock.now - t)
        let bytes = diskUsage(snapshot)

        // 4. Stop; the server exits once the reply is out.
        busy = "stopped"
        t = clock.now
        stopping = true
        do {
            try await machine.stop()
        } catch {
            logMessage("stopping the saved VM: \(describe(error)); exiting anyway")
        }
        timings.stopMS = milliseconds(clock.now - t)
        timings.totalMS = milliseconds(clock.now - start)
        logMessage("saved to \(snapshot) (\(bytes) bytes) in \(timings.totalMS ?? 0) ms")
        return .exit(Wire.line(Slept(snapshot: snapshot, snapshotBytes: bytes, timings: timings)), code: 0)
    }

    /// The balloon's inflated target, in whole MiB: what vibed asked for
    /// (what the guest really uses, with some headroom, so nothing in it
    /// runs short), or else the least the VM may have; never more than the
    /// VM has.
    func balloonTarget(floor: UInt64?) -> UInt64 {
        let mib: UInt64 = 1 << 20
        let least = max(floor ?? 0, machine.minimumMemorySize)
        let target = least >= UInt64.max - mib ? least : (least + mib - 1) / mib * mib
        return min(target, machine.memorySize)
    }

    private func stop() async -> Outcome {
        if let failure = busyFailure() { return .reply(Wire.line(failure)) }
        busy = "stopping"
        stopping = true
        do {
            try await machine.stop()
        } catch {
            logMessage("stopping the VM: \(describe(error)); exiting anyway")
        }
        return .exit(Wire.line(Done()), code: 0)
    }

    private func connect(port: UInt32) async -> Outcome {
        if let failure = busyFailure() { return .reply(Wire.line(failure)) }
        let state = machine.state
        guard state == .running else {
            return .reply(Wire.line(Failure(.badState, "the VM is \(state.rawValue)")))
        }
        do {
            let stream = try await machine.connect(port: port)
            return .stream(Wire.line(Done()), stream)
        } catch {
            let why = "nothing in the guest accepted vsock port \(port): \(describe(error))"
            return .reply(Wire.line(Failure(.connectFailed, why)))
        }
    }
}

/// Flushes `path` (a file or a directory) to the disk, so a rename after it
/// never lands before the data, and leaves it readable by this user alone.
/// `fsync`, not `F_FULLFSYNC`: v1 assumes the Mac stays powered (PRD), and
/// draining the drive's cache would add to every save.
func makeDurable(_ path: String) throws {
    let fd = open(path, O_RDONLY)
    guard fd >= 0 else { throw Failure(.saveFailed, "can't open \(path): \(errnoString())") }
    defer { close(fd) }
    var isDirectory: ObjCBool = false
    _ = FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory)
    let mode: mode_t = isDirectory.boolValue ? 0o700 : 0o600
    guard fchmod(fd, mode) == 0, fsync(fd) == 0 else {
        throw Failure(.saveFailed, "can't flush \(path): \(errnoString())")
    }
}

func milliseconds(_ duration: Duration) -> UInt64 {
    let (seconds, attoseconds) = duration.components
    guard seconds >= 0, attoseconds >= 0 else { return 0 }
    return UInt64(seconds) * 1000 + UInt64(attoseconds / 1_000_000_000_000_000)
}

/// How much disk `path` takes, whether a snapshot is a file or a bundle.
func diskUsage(_ path: String) -> UInt64 {
    let keys: Set<URLResourceKey> = [.isDirectoryKey, .totalFileAllocatedSizeKey]
    let url = URL(fileURLWithPath: path)
    guard let values = try? url.resourceValues(forKeys: keys) else { return 0 }
    guard values.isDirectory == true else { return UInt64(values.totalFileAllocatedSize ?? 0) }
    var total: UInt64 = 0
    let children = FileManager.default.enumerator(at: url, includingPropertiesForKeys: Array(keys))
    while let child = children?.nextObject() as? URL {
        total += UInt64((try? child.resourceValues(forKeys: keys))?.totalFileAllocatedSize ?? 0)
    }
    return total
}

/// One line on stderr, which vibed points at vibe-vm's log.
public func logMessage(_ message: String) {
    let stamp = ISO8601DateFormatter().string(from: Date())
    try? writeAll(STDERR_FILENO, Data("\(stamp) vibe-vm[\(getpid())]: \(message)\n".utf8))
}
