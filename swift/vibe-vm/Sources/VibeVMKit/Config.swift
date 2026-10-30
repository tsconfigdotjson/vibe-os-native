import Foundation

/// A `linux` agent's VM, as vibed writes it to the agent's `vm.json`
/// (protocol/vibe-vm/config.json). Everything here is fixed for the agent's
/// life: a snapshot restores only into the exact configuration it was saved
/// from. `vibe_env::vm::VmConfig` is the Rust side.
public struct VMConfig: Codable, Equatable, Sendable {
    public static let currentVersion = 1
    /// Virtualization.framework's limit for a virtio block device's serial.
    public static let maxDiskID = 20
    public static let maxShareTag = 36

    public var version: Int
    public var ref: String
    public var cpus: Int
    /// Bytes: a whole number of MiB.
    public var memory: UInt64
    /// An uncompressed arm64 Linux kernel image.
    public var kernel: String
    public var initrd: String?
    public var cmdline: String
    /// Base64 of `VZGenericMachineIdentifier.dataRepresentation`, made by
    /// `vibe-vm create`.
    public var machineID: String?
    /// The NIC's fixed MAC address: the default, random on every start,
    /// breaks restoring.
    public var mac: String?
    public var disks: [Disk]
    public var shares: [Share]
    public var consoleLog: String?
    public var snapshot: String

    public struct Disk: Codable, Equatable, Sendable {
        /// The block device's serial, which the guest finds it by.
        public var id: String
        public var path: String
        public var readOnly: Bool

        enum CodingKeys: String, CodingKey {
            case id, path
            case readOnly = "read_only"
        }
    }

    public struct Share: Codable, Equatable, Sendable {
        public var tag: String
        public var path: String
        public var readOnly: Bool

        enum CodingKeys: String, CodingKey {
            case tag, path
            case readOnly = "read_only"
        }
    }

    enum CodingKeys: String, CodingKey {
        case version, ref, cpus, memory, kernel, initrd, cmdline
        case machineID = "machine_id"
        case mac, disks, shares
        case consoleLog = "console_log"
        case snapshot
    }

    /// Checks what can be checked without Virtualization.framework, with the
    /// same rules as the Rust side. Throws a ``Failure``.
    public func validate() throws {
        func bad(_ why: String) -> Failure { Failure(.badConfig, why) }
        guard version == VMConfig.currentVersion else {
            throw bad("version \(version) isn't supported; this reads version \(VMConfig.currentVersion)")
        }
        guard isRef(ref) else {
            throw bad("`\(ref)` isn't a valid ref")
        }
        guard cpus >= 1 else { throw bad("cpus must be at least 1") }
        guard memory > 0, memory % (1 << 20) == 0 else {
            throw bad("memory must be a whole number of MiB, not \(memory) bytes")
        }
        var paths: [(String, String)] = [("kernel", kernel), ("snapshot", snapshot)]
        if let initrd { paths.append(("initrd", initrd)) }
        if let consoleLog { paths.append(("console_log", consoleLog)) }
        paths += disks.map { ("disk `\($0.id)`", $0.path) }
        paths += shares.map { ("share `\($0.tag)`", $0.path) }
        for (what, path) in paths where !path.hasPrefix("/") {
            throw bad("\(what) must be an absolute path, not `\(path)`")
        }
        if let mac, parseMAC(mac) == nil {
            throw bad("`\(mac)` isn't a unicast MAC address like 02:5f:3a:11:9c:e4")
        }
        if let machineID, machineID.isEmpty || Data(base64Encoded: machineID) == nil {
            throw bad("machine_id must be base64")
        }
        var ids = Set<String>()
        for disk in disks {
            guard isLabel(disk.id, max: VMConfig.maxDiskID) else {
                throw bad("disk id `\(disk.id)` must be 1 to \(VMConfig.maxDiskID) letters, digits, `.`, `_` and `-`")
            }
            guard ids.insert(disk.id).inserted else { throw bad("disk id `\(disk.id)` is used twice") }
        }
        var tags = Set<String>()
        for share in shares {
            guard isLabel(share.tag, max: VMConfig.maxShareTag) else {
                throw bad("share tag `\(share.tag)` must be 1 to \(VMConfig.maxShareTag) letters, digits, `.`, `_` and `-`")
            }
            guard tags.insert(share.tag).inserted else { throw bad("share tag `\(share.tag)` is used twice") }
        }
    }

    /// Reads and checks `vm.json`.
    public static func load(path: String) throws -> VMConfig {
        let config: VMConfig
        do {
            let data = try Data(contentsOf: URL(fileURLWithPath: path))
            config = try JSONDecoder().decode(VMConfig.self, from: data)
        } catch {
            throw Failure(.badConfig, "can't read \(path): \(describe(error))")
        }
        do {
            try config.validate()
        } catch {
            throw Failure(.badConfig, "\(path): \(describe(error))")
        }
        return config
    }

    /// Writes `vm.json`, replacing it in one step.
    public func save(path: String) throws {
        try validate()
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        do {
            var data = try encoder.encode(self)
            data.append(0x0A)
            try data.write(to: URL(fileURLWithPath: path), options: .atomic)
        } catch {
            throw Failure(.badConfig, "can't write \(path): \(describe(error))")
        }
    }
}

/// Like `vibe_core::model::validate_ref`: up to 32 letters, digits, `.`, `_`
/// and `-`, starting with a letter or digit, and not `vibed`.
func isRef(_ s: String) -> Bool {
    guard let first = s.unicodeScalars.first, s != "vibed", s.utf8.count <= 32 else { return false }
    return isAlnum(first) && isLabel(s, max: 32)
}

/// A disk id or share tag: what the guest looks devices up by.
func isLabel(_ s: String, max: Int) -> Bool {
    !s.isEmpty && s.utf8.count <= max
        && s.unicodeScalars.allSatisfy { isAlnum($0) || $0 == "." || $0 == "_" || $0 == "-" }
}

private func isAlnum(_ c: Unicode.Scalar) -> Bool {
    switch c.value {
    case 0x30...0x39, 0x41...0x5A, 0x61...0x7A: true
    default: false
    }
}

/// The bytes of a unicast MAC address written `02:5f:3a:11:9c:e4`.
public func parseMAC(_ s: String) -> [UInt8]? {
    let parts = s.split(separator: ":", omittingEmptySubsequences: false)
    guard parts.count == 6 else { return nil }
    var bytes: [UInt8] = []
    for part in parts {
        let hex = part.utf8.allSatisfy { (0x30...0x39).contains($0) || (0x41...0x46).contains($0) || (0x61...0x66).contains($0) }
        guard part.count == 2, hex, let byte = UInt8(part, radix: 16) else { return nil }
        bytes.append(byte)
    }
    return bytes[0] & 1 == 0 ? bytes : nil
}

/// A random, locally administered, unicast MAC address.
public func randomMAC() -> String {
    var bytes = (0..<6).map { _ in UInt8.random(in: 0...255) }
    bytes[0] = (bytes[0] & 0xFC) | 0x02
    return bytes.map { String(format: "%02x", $0) }.joined(separator: ":")
}

/// An error's message for a person: NSError's description, or the error.
func describe(_ error: any Error) -> String {
    if let failure = error as? Failure { return failure.error }
    return (error as NSError).localizedDescription
}
