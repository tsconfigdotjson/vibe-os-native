import Foundation
import Network
import Testing

@testable import VibeMenu
import VibeRPC

/// Local Network access for Linux hosts on the local network (issue #39).
@Suite struct LocalNetworkTests {
    /// The app as built has what macOS shows when it asks for access; without
    /// it, the app's request has no reason to give.
    @Test func theAppSaysWhyItWantsLocalNetworkAccess() throws {
        let plist = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Support/Info.plist")
        let info = try #require(
            try PropertyListSerialization.propertyList(from: Data(contentsOf: plist), format: nil)
                as? [String: Any])
        let why = try #require(info["NSLocalNetworkUsageDescription"] as? String)
        #expect(why.contains("local network"))
        #expect(info["CFBundleIdentifier"] as? String == "io.github.tsconfigdotjson.VibeOS")
    }

    @Test func theTargetIsWhatSshGSays() {
        let config = "user me\nhostname nas.example.lan\nport 2222\nproxycommand none\nproxyjump none\n"
        #expect(LocalNetwork.target(sshConfig: config) == LocalNetwork.Target(host: "nas.example.lan", port: 2222))
        // Through a jump host or a command, ssh doesn't connect to the host
        // itself: there's nothing to ask for.
        #expect(LocalNetwork.target(sshConfig: "hostname nas\nport 22\nproxyjump bastion\n") == nil)
        #expect(LocalNetwork.target(sshConfig: "hostname nas\nproxycommand nc %h %p\n") == nil)
        #expect(LocalNetwork.target(sshConfig: "port 22\n") == nil)
    }

    @Test func theRealSshSaysWhereADestinationGoes() async {
        let target = await LocalNetwork.target(destination: "me@192.0.2.7")
        #expect(target?.host == "192.0.2.7")
        #expect(target?.port == 22)
    }

    @Test func localIsOnAnInterfacesNetworkOrLinkLocal() {
        let home: [(address: [UInt8], mask: [UInt8])] = [([192, 168, 2, 88], [255, 255, 255, 0])]
        #expect(LocalNetwork.isLocal([192, 168, 2, 20], networks: home))
        // Through the router: not local, whatever its address.
        #expect(!LocalNetwork.isLocal([54, 39, 97, 232], networks: home))
        #expect(!LocalNetwork.isLocal([10, 0, 0, 5], networks: home))
        #expect(LocalNetwork.isLocal([169, 254, 3, 4], networks: []))
        #expect(LocalNetwork.isLocal([224, 0, 0, 251], networks: []))
        #expect(!LocalNetwork.isLocal([127, 0, 0, 1], networks: home))
        let fe80: [UInt8] = [0xfe, 0x80] + [UInt8](repeating: 0, count: 13) + [1]
        #expect(LocalNetwork.isLocal(fe80, networks: []))
        #expect(!LocalNetwork.sameNetwork([10, 0, 0, 1], [10, 0, 0, 2], [0, 0, 0, 0]))
        #expect(LocalNetwork.isLocal(host: "nas.local"))
        #expect(LocalNetwork.isLocal(host: "169.254.1.1"))
        #expect(!LocalNetwork.isLocal(host: "127.0.0.1"))
        // Each broadcast-capable interface's own address is on its network.
        for network in LocalNetwork.localNetworks() {
            #expect(LocalNetwork.isLocal(network.address, networks: LocalNetwork.localNetworks()))
        }
    }

    /// A host out on the internet needs no access: nothing is asked, and
    /// the app doesn't connect to it.
    @Test func aHostOutThereNeedsNothing() async {
        let told = Box(false)
        let access = await SystemLocalNetworkGate().prepare("me@192.0.2.7") { told.value = true }
        #expect(access == .notNeeded)
        #expect(!told.value)
    }

    /// The connection the app makes to ask: it's allowed where something
    /// listens, and a refusal isn't taken for access being off.
    @Test func theAppsConnectionSaysWhatHappened() async throws {
        let listener = try NWListener(using: .tcp, on: .any)
        let ready = Box(false)
        listener.newConnectionHandler = { $0.cancel() }
        listener.stateUpdateHandler = { if case .ready = $0 { ready.value = true } }
        listener.start(queue: .global())
        defer { listener.cancel() }
        for _ in 0..<100 where !ready.value { try await Task.sleep(for: .milliseconds(20)) }
        let port = try #require(listener.port?.rawValue)
        let gate = SystemLocalNetworkGate(answerWithin: 5)
        let told = Box(false)
        #expect(await gate.connect(to: .init(host: "127.0.0.1", port: port)) { told.value = true } == .allowed)
        listener.cancel()
        try await Task.sleep(for: .milliseconds(100))
        #expect(await gate.connect(to: .init(host: "127.0.0.1", port: port)) { told.value = true } == .unknown)
        #expect(!told.value)
    }
}

/// Stands in for asking macOS: says access is off, then waits until told
/// to go on.
final class FakeLocalNetworkGate: LocalNetworkGate, @unchecked Sendable {
    let answer: LocalNetworkAccess
    let turnsOff: Bool
    let asked = Box<[String]>([])
    let goOn = Box<CheckedContinuation<Void, Never>?>(nil)
    let waiting = Box(false)

    init(answer: LocalNetworkAccess, turnsOff: Bool) {
        self.answer = answer
        self.turnsOff = turnsOff
    }

    func prepare(_ destination: String, off: @escaping @Sendable () -> Void) async -> LocalNetworkAccess {
        asked.value.append(destination)
        if turnsOff {
            off()
            await withTaskCancellationHandler {
                await withCheckedContinuation { c in
                    goOn.value = c
                    waiting.value = true
                }
            } onCancel: {
                self.release()
            }
        }
        return answer
    }

    func release() {
        let c = goOn.value
        goOn.value = nil
        c?.resume()
    }
}

@Suite @MainActor struct AddingAHostOnTheLocalNetworkTests {
    func model(_ daemon: FakeDaemon, _ gate: FakeLocalNetworkGate) -> MenuModel {
        MenuModel(
            daemon: daemon, terminal: FakeTerminal(), localNetwork: gate, vibePath: "/usr/local/bin/vibe",
            socket: nil, openPoll: .seconds(3600), closedPoll: .seconds(3600), endOfTurn: { true })
    }

    func addable() -> FakeDaemon {
        let daemon = fakeDaemon()
        daemon.on(HostsAdd.name) { _ in
            ["host": hostJSON("nas", link: .down), "changed": true, "notes": []]
        }
        return daemon
    }

    /// While access is off the model says so, plainly, and how to turn it
    /// on; once it's on, vibed adds the host.
    @Test func addingWaitsForAccessAndSaysWhy() async throws {
        let daemon = addable()
        let gate = FakeLocalNetworkGate(answer: .allowed, turnsOff: true)
        let model = model(daemon, gate)
        let adding = Task { try await model.addHost(name: "nas", address: "me@nas.local") }
        for _ in 0..<200 where !(gate.waiting.value && model.localNetworkOff != nil) {
            try await Task.sleep(for: .milliseconds(10))
        }
        let off = try #require(model.localNetworkOff)
        #expect(off.hasPrefix("Local Network access is off for VibeOS, so it can't connect to me@nas.local"))
        #expect(off.contains("System Settings > Privacy & Security > Local Network > VibeOS"))
        #expect(!daemon.calls.contains(HostsAdd.name))
        gate.release()
        let installed = try await adding.value
        #expect(installed.host.name == "nas")
        #expect(gate.asked.value == ["me@nas.local"])
        #expect(model.localNetworkOff == nil)
        #expect(daemon.calls.contains(HostsAdd.name))
    }

    /// Closing the window while it waits stops there: nothing is added.
    @Test func cancellingTheWaitAddsNothing() async throws {
        let daemon = addable()
        let gate = FakeLocalNetworkGate(answer: .unknown, turnsOff: true)
        let model = model(daemon, gate)
        let adding = Task { try await model.addHost(name: "nas", address: "me@nas.local") }
        for _ in 0..<200 where !gate.waiting.value { try await Task.sleep(for: .milliseconds(10)) }
        adding.cancel()
        await #expect(throws: CancellationError.self) { try await adding.value }
        #expect(!daemon.calls.contains(HostsAdd.name))
        #expect(model.localNetworkOff == nil)
    }

    /// Still off after the wait: vibed tries anyway, and its answer, which
    /// it logs too, says Local Network access is off.
    @Test func accessStillOffIsVibedsToSay() async throws {
        let daemon = fakeDaemon()
        let said =
            "Local Network access is off for VibeOS, so it can't connect to me@nas (192.168.2.20) on your local "
            + "network. Turn it on in System Settings > Privacy & Security > Local Network > VibeOS, then try again."
        daemon.on(HostsAdd.name) { _ in
            throw VibedClientError.rpc(RPCError(code: RPCErrorCode.commandFailed.rawValue, message: said))
        }
        let gate = FakeLocalNetworkGate(answer: .off, turnsOff: false)
        let model = model(daemon, gate)
        do {
            _ = try await model.addHost(name: "nas", address: "me@nas")
            Issue.record("the add didn't fail")
        } catch {
            #expect(describe(error) == said)
        }
        #expect(gate.asked.value == ["me@nas"])
    }
}
