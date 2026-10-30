import Foundation

// The control protocol: what `vibe-vm` prints when it starts, and what it
// answers on its socket (protocol/vibe-vm/README.md). Every reply is one
// JSON line: `{"ok":true, ...}`, or a ``Failure``.

public enum ControlProtocol {
    /// In ``Ready`` and ``Status``.
    public static let version = 1
    /// The longest request line.
    public static let maxRequestBytes = 64 * 1024
}

public enum ErrorCode: String, Codable, Sendable, CaseIterable {
    case badRequest = "bad_request"
    case badConfig = "bad_config"
    case alreadyRunning = "already_running"
    case unsupported
    case noSnapshot = "no_snapshot"
    case bootFailed = "boot_failed"
    case restoreFailed = "restore_failed"
    case busy
    case badState = "bad_state"
    case saveFailed = "save_failed"
    case connectFailed = "connect_failed"
}

/// `{"ok":false,"code":...,"error":...}`.
public struct Failure: Error, Codable, Equatable, Sendable, CustomStringConvertible {
    public var ok = false
    public var code: ErrorCode
    public var error: String

    public init(_ code: ErrorCode, _ error: String) {
        self.code = code
        self.error = error
    }

    public var description: String { error }
}

public enum Started: String, Codable, Sendable {
    case boot, restore
}

/// Virtualization.framework's state of the VM.
public enum MachineState: String, Codable, Sendable {
    case starting, running, pausing, paused, resuming, saving, restoring, stopping, stopped, error
}

/// Durations in milliseconds; each step reports its own.
public struct Timings: Codable, Equatable, Sendable {
    public var startMS: UInt64?
    public var restoreMS: UInt64?
    public var resumeMS: UInt64?
    public var balloonMS: UInt64?
    public var pauseMS: UInt64?
    public var saveMS: UInt64?
    public var stopMS: UInt64?
    public var totalMS: UInt64?

    public init() {}

    enum CodingKeys: String, CodingKey {
        case startMS = "start_ms"
        case restoreMS = "restore_ms"
        case resumeMS = "resume_ms"
        case balloonMS = "balloon_ms"
        case pauseMS = "pause_ms"
        case saveMS = "save_ms"
        case stopMS = "stop_ms"
        case totalMS = "total_ms"
    }
}

/// What `boot` and `restore` print once the VM runs.
public struct Ready: Codable, Equatable, Sendable {
    public var ok = true
    public var protocolVersion = ControlProtocol.version
    public var pid: Int32
    public var started: Started
    public var timings: Timings

    public init(pid: Int32, started: Started, timings: Timings) {
        self.pid = pid
        self.started = started
        self.timings = timings
    }

    enum CodingKeys: String, CodingKey {
        case ok
        case protocolVersion = "protocol"
        case pid, started, timings
    }
}

/// What `create` prints: the identity it gave the VM.
public struct Created: Codable, Equatable, Sendable {
    public var ok = true
    public var machineID: String
    public var mac: String

    public init(machineID: String, mac: String) {
        self.machineID = machineID
        self.mac = mac
    }

    enum CodingKeys: String, CodingKey {
        case ok
        case machineID = "machine_id"
        case mac
    }
}

public struct Status: Codable, Equatable, Sendable {
    public var ok = true
    public var protocolVersion = ControlProtocol.version
    public var ref: String
    public var pid: Int32
    public var state: MachineState
    public var started: Started

    public init(ref: String, pid: Int32, state: MachineState, started: Started) {
        self.ref = ref
        self.pid = pid
        self.state = state
        self.started = started
    }

    enum CodingKeys: String, CodingKey {
        case ok
        case protocolVersion = "protocol"
        case ref, pid, state, started
    }
}

/// A sleep's reply, sent once the VM is saved and stopped.
public struct Slept: Codable, Equatable, Sendable {
    public var ok = true
    public var snapshot: String
    /// The snapshot's size on disk.
    public var snapshotBytes: UInt64
    public var timings: Timings

    public init(snapshot: String, snapshotBytes: UInt64, timings: Timings) {
        self.snapshot = snapshot
        self.snapshotBytes = snapshotBytes
        self.timings = timings
    }

    enum CodingKeys: String, CodingKey {
        case ok, snapshot
        case snapshotBytes = "snapshot_bytes"
        case timings
    }
}

/// The reply to `connect` and `stop`.
public struct Done: Codable, Equatable, Sendable {
    public var ok = true

    public init() {}
}

public enum Request: Equatable, Sendable {
    case status
    /// Balloon, pause, save, stop and exit. `balloonMS`: how long the
    /// balloon stays inflated; nil for the default, 0 to skip it.
    case sleep(balloonMS: UInt64?)
    /// Stop at once, without saving, and exit.
    case stop
    /// Make this connection a stream to `port` in the guest.
    case connect(port: UInt32)

    static let ops: Set<String> = ["status", "sleep", "stop", "connect"]

    /// Reads one request line; anything else is a `bad_request`.
    public static func parse(_ line: Data) -> Result<Request, Failure> {
        struct Op: Decodable { var op: String }
        guard let op = try? JSONDecoder().decode(Op.self, from: line) else {
            return .failure(Failure(.badRequest, "a request is one JSON object with an `op`, on one line"))
        }
        guard ops.contains(op.op) else {
            return .failure(Failure(.badRequest, "unknown op `\(op.op)`"))
        }
        do {
            return .success(try JSONDecoder().decode(Request.self, from: line))
        } catch {
            return .failure(Failure(.badRequest, "bad `\(op.op)` request: \(describe(error))"))
        }
    }
}

extension Request: Codable {
    enum CodingKeys: String, CodingKey {
        case op
        case balloonMS = "balloon_ms"
        case port
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let op = try c.decode(String.self, forKey: .op)
        switch op {
        case "status": self = .status
        case "sleep": self = .sleep(balloonMS: try c.decodeIfPresent(UInt64.self, forKey: .balloonMS))
        case "stop": self = .stop
        case "connect": self = .connect(port: try c.decode(UInt32.self, forKey: .port))
        default:
            throw DecodingError.dataCorruptedError(forKey: .op, in: c, debugDescription: "unknown op `\(op)`")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .status: try c.encode("status", forKey: .op)
        case .sleep(let balloonMS):
            try c.encode("sleep", forKey: .op)
            try c.encodeIfPresent(balloonMS, forKey: .balloonMS)
        case .stop: try c.encode("stop", forKey: .op)
        case .connect(let port):
            try c.encode("connect", forKey: .op)
            try c.encode(port, forKey: .port)
        }
    }
}

public enum Wire {
    /// One JSON line, newline included.
    public static func line(_ value: some Encodable) -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        // These types encode plain values only; that can't fail.
        var data = (try? encoder.encode(value)) ?? Data(#"{"ok":false,"code":"bad_request","error":"unencodable"}"#.utf8)
        data.append(0x0A)
        return data
    }
}
