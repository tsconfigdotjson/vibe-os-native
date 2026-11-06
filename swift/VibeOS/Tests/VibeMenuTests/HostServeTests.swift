import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

// A Linux host's MCP on its tailnet name: what the toggle in Settings → Hosts calls, and what
// comes of it. Its view is in the app, and isn't under test here.

@Suite @MainActor struct HostServeTests {
    let vps = "https://vps.tailnet-example.ts.net/mcp"

    func make(_ daemon: FakeDaemon) -> MenuModel {
        // A poll that never comes, so only what's called here moves it.
        MenuModel(
            daemon: daemon, terminal: FakeTerminal(), notifier: nil, vibePath: "/usr/local/bin/vibe", socket: nil,
            openPoll: .seconds(3600), closedPoll: .seconds(3600), endOfTurn: { true })
    }

    func hosted(link: HostLink = .connected) -> FakeDaemon {
        fakeDaemon(hosts: [hostJSON("local"), hostJSON("vps", link: link)])
    }

    @Test func theToggleNamesTheHostAndItsURLJoinsTheSetupLines() async throws {
        let daemon = hosted()
        let url = vps
        daemon.on(McpServeSet.name) { params in
            guard params["on"] as? Bool == true else { return ["on": false, "available": true] }
            return ["on": true, "available": true, "url": url]
        }
        let model = make(daemon)
        await model.refresh()
        #expect(model.hostServeURLs.isEmpty)

        try await model.setHostServe("vps", on: true)
        let sent = try #require(daemon.params(of: McpServeSet.name).last)
        #expect(sent["host"] as? String == "vps")
        #expect(sent["on"] as? Bool == true)
        #expect(daemon.timeout(of: McpServeSet.name) == MenuModel.hostServeTimeout)
        #expect(model.hostServes["vps"]?.on == true)
        #expect(model.hostServeURLs.map(\.url) == [vps])
        #expect(model.hostServeURLs.map(\.host) == ["vps"])

        try await model.setHostServe("vps", on: false)
        #expect(model.hostServes["vps"]?.on == false)
        #expect(model.hostServeURLs.isEmpty)
    }

    /// The Mac's own toggle is still `mcp.set`, with no host, and it's nothing to do with this.
    @Test func theMacsToggleStaysItsOwn() async throws {
        let daemon = hosted()
        daemon.on(McpSet.name) { _ in
            ["port": 7878, "tailscale": true, "url": "http://127.0.0.1:7878/mcp", "tailscale_urls": []]
        }
        let model = make(daemon)
        await model.refresh()
        try await model.setMCPTailscale(true)
        #expect(daemon.params(of: McpSet.name).last?["host"] == nil)
        #expect(!daemon.calls.contains(McpServeSet.name))
    }

    /// It runs `tailscale` on the host, so a poll doesn't ask, nor does a host that's down.
    @Test func itsAskedOnlyWhereItsShown() async throws {
        let up = hosted()
        up.on(McpServeGet.name) { _ in ["on": false, "available": true] }
        let model = make(up)
        await model.refresh()
        #expect(!up.calls.contains(McpServeGet.name))
        await model.refreshHostServes()
        #expect(up.params(of: McpServeGet.name).map { $0["host"] as? String } == ["vps"])

        let down = hosted(link: .down)
        let other = make(down)
        await other.refresh()
        await other.refreshHostServes()
        #expect(!down.calls.contains(McpServeGet.name))
    }

    @Test func aHostThatDoesntAnswerKeepsWhatItLastSaid() async throws {
        let daemon = hosted()
        let url = vps
        let answers = Box(true)
        daemon.on(McpServeGet.name) { _ in
            guard answers.value else {
                throw VibedClientError.rpc(RPCError(code: RPCErrorCode.hostUnavailable.rawValue, message: "down"))
            }
            return ["on": true, "available": true, "url": url]
        }
        let model = make(daemon)
        await model.refresh()
        await model.refreshHostServes()
        #expect(model.hostServes["vps"]?.url == vps)
        answers.value = false
        await model.refreshHostServes()
        #expect(model.hostServes["vps"]?.url == vps)
    }

    @Test func onlyAnHTTPSLinkIsOffered() throws {
        func refused(_ link: String?) -> any Error {
            VibedClientError.rpc(
                RPCError(
                    code: RPCErrorCode.commandFailed.rawValue, message: "no",
                    data: link.map { JSONValue.object(["tailscale_enable_url": JSONValue.string($0)]) }))
        }
        let link = "https://login.tailscale.com/f/serve?node=abc"
        #expect(tailscaleEnableURL(from: refused(link))?.absoluteString == link)
        #expect(tailscaleEnableURL(from: refused(nil)) == nil)
        #expect(tailscaleEnableURL(from: refused("file:///etc/passwd")) == nil)
        #expect(tailscaleEnableURL(from: refused("javascript:alert(1)")) == nil)
        #expect(tailscaleEnableURL(from: CocoaError(.fileNoSuchFile)) == nil)
    }

    @Test func theFixturesAnswerIt() throws {
        let fixtures = FixtureDaemon(.busy)
        #expect(try fixtures.call(McpServeGet.self, HostParams(host: "vps")).url != nil)
        #expect(try !fixtures.call(McpServeGet.self, HostParams(host: "local")).available)
        #expect(try !fixtures.call(McpServeSet.self, McpServeSetParams(host: "vps", on: false)).on)
    }
}
