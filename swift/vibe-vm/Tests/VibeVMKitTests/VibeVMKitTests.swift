import Foundation
import Testing

@testable import VibeVMKit

@Suite struct CommandTests {
    @Test func parsesCommands() throws {
        #expect(try VibeVM.parse(["check"]) == .check)
        #expect(try VibeVM.parse(["--version"]) == .version)
        #expect(try VibeVM.parse(["-V"]) == .version)
        #expect(try VibeVM.parse(["-h"]) == .help)
        #expect(try VibeVM.parse(["create", "--config", "/a/vm.json"]) == .create(config: "/a/vm.json"))
        #expect(
            try VibeVM.parse(["boot", "--config", "/a/vm.json", "--socket", "/r/x.vm"])
                == .boot(config: "/a/vm.json", socket: "/r/x.vm"))
        #expect(
            try VibeVM.parse(["restore", "--socket", "/r/x.vm", "--config", "/a/vm.json"])
                == .restore(config: "/a/vm.json", socket: "/r/x.vm"))
    }

    @Test func rejectsBadArguments() {
        let cases: [([String], String)] = [
            ([], "missing a command"),
            (["dance"], "unknown command: dance"),
            (["check", "x"], "unexpected argument: x"),
            (["create"], "create needs --config"),
            (["create", "--config"], "--config needs a value"),
            (["create", "--config", ""], "--config needs a value"),
            (["create", "--config", "a", "--socket", "b"], "unexpected argument: --socket"),
            (["boot", "--config", "a"], "boot needs --socket"),
            (["restore", "--socket", "b"], "restore needs --config"),
            (["boot", "--config", "a", "--config", "b", "--socket", "s"], "--config is given twice"),
        ]
        for (arguments, message) in cases {
            #expect(throws: UsageError(description: message)) { try VibeVM.parse(arguments) }
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

@Suite struct ConfigTests {
    @Test func theExampleIsValid() throws {
        try exampleConfig().validate()
    }

    @Test func badConfigsSayWhy() throws {
        let cases: [((inout VMConfig) -> Void, String)] = [
            ({ $0.version = 2 }, "version 2 isn't supported"),
            ({ $0.ref = "a/b" }, "isn't a valid ref"),
            ({ $0.ref = "vibed" }, "isn't a valid ref"),
            ({ $0.ref = "mac" }, "isn't a valid ref"),
            ({ $0.ref = "Tunnel" }, "isn't a valid ref"),
            ({ $0.ref = String(repeating: "a", count: 33) }, "isn't a valid ref"),
            ({ $0.cpus = 0 }, "cpus must be at least 1"),
            ({ $0.memory = (1 << 20) + 1 }, "whole number of MiB"),
            ({ $0.kernel = "Image" }, "kernel must be an absolute path"),
            ({ $0.initrd = "initrd" }, "initrd must be an absolute path"),
            ({ $0.snapshot = "s" }, "snapshot must be an absolute path"),
            ({ $0.consoleLog = "c" }, "console_log must be an absolute path"),
            ({ $0.mac = "02:00" }, "isn't a unicast MAC"),
            ({ $0.mac = "03:00:00:00:00:01" }, "isn't a unicast MAC"),
            ({ $0.machineID = "not base64!" }, "machine_id must be base64"),
            ({ $0.disks[1].id = "base" }, "disk id `base` is used twice"),
            ({ $0.disks[0].id = String(repeating: "a", count: 21) }, "must be 1 to 20"),
            ({ $0.disks[0].path = "base.img" }, "disk `base` must be an absolute path"),
            ({ $0.shares[1].tag = "worktree" }, "share tag `worktree` is used twice"),
            ({ $0.shares[0].tag = "a b" }, "share tag `a b` must be"),
            ({ $0.shares[0].tag = String(repeating: "t", count: 37) }, "must be 1 to 36"),
            ({ $0.shares[0].path = "wt" }, "share `worktree` must be an absolute path"),
        ]
        for (change, message) in cases {
            var config = try exampleConfig()
            change(&config)
            do {
                try config.validate()
                Issue.record("\(message): accepted")
            } catch let failure as Failure {
                #expect(failure.code == .badConfig)
                #expect(failure.error.contains(message), "\(failure.error)")
            }
        }
        var config = try exampleConfig()
        config.disks[0].id = String(repeating: "a", count: 20)
        config.shares[0].tag = String(repeating: "t", count: 36)
        config.mac = nil
        config.machineID = nil
        config.initrd = nil
        config.consoleLog = nil
        try config.validate()
    }

    @Test func macs() {
        #expect(parseMAC("02:5f:3a:11:9c:e4") == [0x02, 0x5f, 0x3a, 0x11, 0x9c, 0xe4])
        #expect(parseMAC("02:5F:3A:11:9C:E4") != nil)
        for bad in [
            "", "02:5f:3a:11:9c", "02:5f:3a:11:9c:e4:00", "2:5f:3a:11:9c:e4", "zz:5f:3a:11:9c:e4", "01:00:00:00:00:00",
            "+2:5f:3a:11:9c:e4", "02:-0:3a:11:9c:e4",
        ] {
            #expect(parseMAC(bad) == nil, "\(bad)")
        }
        for _ in 0..<100 {
            let mac = randomMAC()
            #expect(parseMAC(mac).map { $0[0] & 0x03 } == 0x02, "\(mac): locally administered unicast")
        }
    }

    @Test func savesAndLoads() throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("vm.json").path
        let config = try exampleConfig()
        try config.save(path: path)
        #expect(try VMConfig.load(path: path) == config)
        // Nothing but the file itself.
        #expect(try FileManager.default.contentsOfDirectory(atPath: dir.path) == ["vm.json"])
        try Data("{".utf8).write(to: URL(fileURLWithPath: path))
        #expect(throws: Failure.self) { try VMConfig.load(path: path) }
        let missing = dir.appendingPathComponent("none.json").path
        #expect(throws: Failure.self) { try VMConfig.load(path: missing) }
    }
}

/// A directory of its own under /tmp: short enough for a socket path.
func temporaryDirectory() throws -> URL {
    let dir = URL(fileURLWithPath: "/tmp/vibe-vm-test-\(UUID().uuidString.prefix(8))")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir
}

func exampleConfig() throws -> VMConfig {
    try JSONDecoder().decode(VMConfig.self, from: Data(contentsOf: contractDir.appendingPathComponent("config.json")))
}
