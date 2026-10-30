import Foundation
import Testing

@testable import VibeVMKit

@Suite struct CommandTests {
    @Test func parsesCommands() throws {
        #expect(try VibeVM.parse(["check"]) == .check)
        #expect(try VibeVM.parse(["--version"]) == .version)
        #expect(try VibeVM.parse(["-V"]) == .version)
        #expect(try VibeVM.parse(["-h"]) == .help)
    }

    @Test func rejectsBadArguments() {
        #expect(throws: UsageError(description: "missing a command")) { try VibeVM.parse([]) }
        #expect(throws: UsageError(description: "unknown command: boot")) { try VibeVM.parse(["boot"]) }
        #expect(throws: UsageError(description: "unexpected argument: x")) {
            try VibeVM.parse(["check", "x"])
        }
    }
}

@Suite struct HostSupportTests {
    @Test @MainActor func reportsTheHost() throws {
        // `supported` isn't asserted: it's false inside a VM, such as a CI
        // runner. scripts/check-full.sh requires it on real hardware.
        let support = HostSupport.current()
        #expect(support.minCPUs >= 1)
        #expect(support.maxCPUs >= support.minCPUs)
        #expect(support.maxMemoryBytes >= support.minMemoryBytes)
        #expect(support.ready == (support.supported && support.entitled))
    }

    @Test func jsonIsStable() throws {
        let support = HostSupport(
            supported: true, entitled: false, minCPUs: 1, maxCPUs: 8,
            minMemoryBytes: 1, maxMemoryBytes: 2)
        #expect(
            support.json
                == #"{"entitled":false,"max_cpus":8,"max_memory_bytes":2,"min_cpus":1,"min_memory_bytes":1,"supported":true}"#
        )
        let decoded = try JSONDecoder().decode(HostSupport.self, from: Data(support.json.utf8))
        #expect(decoded == support)
    }

    @Test func unknownEntitlementsAreAbsent() {
        // The signed and unsigned binaries are both checked end to end by
        // scripts/check-full.sh: only the signed one may report entitled.
        #expect(!hasEntitlement("com.example.not-an-entitlement"))
    }
}
