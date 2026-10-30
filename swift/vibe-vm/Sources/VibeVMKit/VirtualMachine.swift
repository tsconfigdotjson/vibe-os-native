import Foundation
@preconcurrency import Virtualization

/// Builds Virtualization.framework's configuration from a ``VMConfig``.
public enum VZBuilder {
    /// Every device a `linux` agent's VM has (PRD: Mounts; Sleep and wake):
    /// kernel and initrd boot, the disks and shares in `config`, NAT
    /// networking with the fixed MAC address, a memory balloon, entropy,
    /// vsock, and a console written to `console`.
    @MainActor
    public static func configuration(for config: VMConfig, console: FileHandle) throws
        -> VZVirtualMachineConfiguration
    {
        let files = FileManager.default
        func requireFile(_ what: String, _ path: String) throws {
            guard files.isReadableFile(atPath: path) else {
                throw Failure(.badConfig, "\(what) \(path) doesn't exist or can't be read")
            }
        }

        let vz = VZVirtualMachineConfiguration()
        vz.cpuCount = config.cpus
        vz.memorySize = config.memory

        try requireFile("the kernel", config.kernel)
        let boot = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: config.kernel))
        if let initrd = config.initrd {
            try requireFile("the initrd", initrd)
            boot.initialRamdiskURL = URL(fileURLWithPath: initrd)
        }
        boot.commandLine = config.cmdline
        vz.bootLoader = boot

        // The machine identifier and the MAC address must stay the same for
        // the VM's life, or a snapshot won't restore.
        guard let encoded = config.machineID, let data = Data(base64Encoded: encoded),
            let machineID = VZGenericMachineIdentifier(dataRepresentation: data)
        else {
            throw Failure(.badConfig, "the VM has no valid machine_id; run `vibe-vm create` on it first")
        }
        let platform = VZGenericPlatformConfiguration()
        platform.machineIdentifier = machineID
        vz.platform = platform

        guard let macString = config.mac, let mac = VZMACAddress(string: macString) else {
            throw Failure(.badConfig, "the VM has no valid mac; run `vibe-vm create` on it first")
        }
        let network = VZVirtioNetworkDeviceConfiguration()
        network.attachment = VZNATNetworkDeviceAttachment()
        network.macAddress = mac
        vz.networkDevices = [network]

        vz.storageDevices = try config.disks.map { disk in
            try requireFile("disk `\(disk.id)`", disk.path)
            let attachment: VZDiskImageStorageDeviceAttachment
            do {
                attachment = try VZDiskImageStorageDeviceAttachment(
                    url: URL(fileURLWithPath: disk.path), readOnly: disk.readOnly)
                try VZVirtioBlockDeviceConfiguration.validateBlockDeviceIdentifier(disk.id)
            } catch {
                throw Failure(.badConfig, "disk `\(disk.id)`: \(describe(error))")
            }
            let device = VZVirtioBlockDeviceConfiguration(attachment: attachment)
            device.blockDeviceIdentifier = disk.id
            return device
        }

        vz.directorySharingDevices = try config.shares.map { share in
            var isDirectory: ObjCBool = false
            guard files.fileExists(atPath: share.path, isDirectory: &isDirectory), isDirectory.boolValue else {
                throw Failure(.badConfig, "share `\(share.tag)`: \(share.path) isn't a directory")
            }
            do {
                try VZVirtioFileSystemDeviceConfiguration.validateTag(share.tag)
            } catch {
                throw Failure(.badConfig, "share `\(share.tag)`: \(describe(error))")
            }
            let device = VZVirtioFileSystemDeviceConfiguration(tag: share.tag)
            let directory = VZSharedDirectory(url: URL(fileURLWithPath: share.path), readOnly: share.readOnly)
            device.share = VZSingleDirectoryShare(directory: directory)
            return device
        }

        vz.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]
        vz.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        vz.socketDevices = [VZVirtioSocketDeviceConfiguration()]

        let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
        serial.attachment = VZFileHandleSerialPortAttachment(fileHandleForReading: nil, fileHandleForWriting: console)
        vz.serialPorts = [serial]
        return vz
    }

    /// Checks Virtualization.framework accepts the VM, and can save and
    /// restore it.
    @MainActor
    public static func validate(_ vz: VZVirtualMachineConfiguration) throws {
        do {
            try vz.validate()
        } catch {
            throw Failure(.badConfig, "Virtualization.framework refuses the VM: \(describe(error))")
        }
        do {
            try vz.validateSaveRestoreSupport()
        } catch {
            throw Failure(.unsupported, "this VM can't be saved and restored: \(describe(error))")
        }
    }

    /// A new machine identifier, base64.
    @MainActor
    public static func newMachineID() -> String {
        VZGenericMachineIdentifier().dataRepresentation.base64EncodedString()
    }
}

/// True for whoever claims it first.
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var claimed = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if claimed { return false }
        claimed = true
        return true
    }
}

/// A Virtualization.framework VM, on the main queue.
@MainActor
public final class VZMachine: NSObject, Machine, VZVirtualMachineDelegate {
    let vm: VZVirtualMachine
    public let memorySize: UInt64
    public var minimumMemorySize: UInt64 { VZVirtualMachineConfiguration.minimumAllowedMemorySize }
    /// Called when the VM stops on its own: the guest powered off (nil), or
    /// it failed.
    public var onStop: ((_ error: (any Error)?) -> Void)?
    private var streams: [Int: VZVirtioSocketConnection] = [:]
    private var nextStream = 0

    public init(configuration: VZVirtualMachineConfiguration) {
        vm = VZVirtualMachine(configuration: configuration)
        memorySize = configuration.memorySize
        super.init()
        vm.delegate = self
    }

    public var state: MachineState {
        switch vm.state {
        case .stopped: .stopped
        case .running: .running
        case .paused: .paused
        case .error: .error
        case .starting: .starting
        case .pausing: .pausing
        case .resuming: .resuming
        case .stopping: .stopping
        case .saving: .saving
        case .restoring: .restoring
        @unknown default: .error
        }
    }

    public func start() async throws { try await vm.start() }
    public func restore(from url: URL) async throws { try await vm.restoreMachineStateFrom(url: url) }
    public func pause() async throws { try await vm.pause() }
    public func resume() async throws { try await vm.resume() }
    public func save(to url: URL) async throws { try await vm.saveMachineStateTo(url: url) }
    public func stop() async throws { try await vm.stop() }

    public func setBalloonTarget(_ bytes: UInt64) {
        for case let balloon as VZVirtioTraditionalMemoryBalloonDevice in vm.memoryBalloonDevices {
            balloon.targetVirtualMachineMemorySize = bytes
        }
    }

    /// How long a connect may wait for the guest. Virtualization.framework
    /// may never answer one to a port nothing in the guest listens on.
    static let connectTimeout = 5

    public func connect(port: UInt32) async throws -> GuestStream {
        guard let device = vm.socketDevices.first as? VZVirtioSocketDevice else {
            throw Failure(.connectFailed, "the VM has no vsock device")
        }
        let once = Once()
        let connection: VZVirtioSocketConnection = try await withCheckedThrowingContinuation { continuation in
            device.connect(toPort: port) { result in
                guard once.claim() else {
                    // Too late: the caller has been told it failed.
                    if case .success(let late) = result { late.close() }
                    return
                }
                continuation.resume(with: result)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + .seconds(VZMachine.connectTimeout)) {
                guard once.claim() else { return }
                continuation.resume(throwing: Failure(.connectFailed, "no answer in \(VZMachine.connectTimeout)s"))
            }
        }
        nextStream += 1
        streams[nextStream] = connection
        return GuestStream(id: nextStream, fd: connection.fileDescriptor)
    }

    public func closeStream(_ id: Int) {
        streams.removeValue(forKey: id)?.close()
    }

    nonisolated public func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        MainActor.assumeIsolated { onStop?(nil) }
    }

    nonisolated public func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: any Error) {
        MainActor.assumeIsolated { onStop?(error) }
    }
}
