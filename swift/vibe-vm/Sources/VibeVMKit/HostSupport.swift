import Foundation
import Security
import Virtualization

/// What `vibe-vm check` reports.
public struct HostSupport: Codable, Equatable, Sendable {
    public static let entitlement = "com.apple.security.virtualization"

    /// Virtualization.framework can run VMs on this Mac.
    public var supported: Bool
    /// This process is signed with the virtualization entitlement.
    public var entitled: Bool
    public var minCPUs: Int
    public var maxCPUs: Int
    public var minMemoryBytes: UInt64
    public var maxMemoryBytes: UInt64

    enum CodingKeys: String, CodingKey {
        case supported, entitled
        case minCPUs = "min_cpus"
        case maxCPUs = "max_cpus"
        case minMemoryBytes = "min_memory_bytes"
        case maxMemoryBytes = "max_memory_bytes"
    }

    /// Both the host and this binary can run VMs.
    public var ready: Bool { supported && entitled }

    @MainActor
    public static func current() -> HostSupport {
        HostSupport(
            supported: VZVirtualMachine.isSupported,
            entitled: hasEntitlement(entitlement),
            minCPUs: VZVirtualMachineConfiguration.minimumAllowedCPUCount,
            maxCPUs: VZVirtualMachineConfiguration.maximumAllowedCPUCount,
            minMemoryBytes: VZVirtualMachineConfiguration.minimumAllowedMemorySize,
            maxMemoryBytes: VZVirtualMachineConfiguration.maximumAllowedMemorySize)
    }

    /// One line of JSON with sorted keys.
    public var json: String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        // Encoding a struct of plain values can't fail.
        return String(decoding: try! encoder.encode(self), as: UTF8.self)
    }
}

/// Whether this process's code signature grants `entitlement`.
func hasEntitlement(_ entitlement: String) -> Bool {
    guard let task = SecTaskCreateFromSelf(nil) else { return false }
    let value = SecTaskCopyValueForEntitlement(task, entitlement as CFString, nil)
    return (value as? Bool) == true
}
