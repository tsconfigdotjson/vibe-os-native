import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

private func service(
    _ state: ServiceState, auth: ServiceAuth? = .oauth, enabled: Bool = true, readOnly: Bool = false,
    denied: [String] = [], tools: Int = 3, detail: String? = nil, at: String? = nil
) -> ServiceInfo {
    ServiceInfo(
        name: "linear", title: "Linear", state: state, auth: auth, enabled: enabled, readOnly: readOnly,
        deniedTools: denied, tools: tools, detail: detail, detailAt: at)
}

struct ServicesTests {
    @Test func everyStateSaysWhereItStandsAndWhatToDo() {
        let off = ServiceSummary(service(.disconnected, auth: nil, tools: 0))
        #expect(off.status == "Not connected")
        #expect(off.tone == .off)
        #expect(off.signIn == .connect)
        #expect(!off.canDisconnect)
        #expect(off.note == nil)

        let on = ServiceSummary(service(.connected))
        #expect(on.status == "Connected")
        #expect(on.tone == .good)
        #expect(on.signIn == nil)
        #expect(on.canDisconnect)
        #expect(on.note == "3 tools")

        #expect(ServiceSummary(service(.connected, auth: .apiKey)).status == "Connected with an API key")

        let refreshing = ServiceSummary(service(.refreshing))
        #expect(refreshing.tone == .working)
        #expect(refreshing.signIn == nil)

        for state in [ServiceState.needsReconnect, .revoked] {
            let lost = ServiceSummary(service(state))
            #expect(lost.tone == .needsYou)
            #expect(lost.signIn == .reconnect)
            #expect(lost.canDisconnect)
        }
    }

    @Test func theLastErrorSaysWhen() {
        let lost = ServiceSummary(service(.needsReconnect, detail: "It refused", at: "2026-11-04T00:00:00Z"))
        #expect(lost.detail == "It refused (2026-11-04T00:00:00Z)")
        #expect(ServiceSummary(service(.connected)).detail == nil)
    }

    @Test func itsSettingsShow() {
        let s = ServiceSummary(service(.connected, readOnly: true, denied: ["a", "b"], tools: 1))
        #expect(s.note == "1 tool · Read-only · 2 tools denied")
        let off = ServiceSummary(service(.connected, enabled: false))
        #expect(off.note == "Turned off: agents see no tools")
    }

    @Test func deniedToolsAreNamesHoweverTheyreTyped() {
        #expect(parseToolNames("  a, b  c,,a\n") == ["a", "b", "c"])
        #expect(parseToolNames("") == [])
    }

    @Test func aKeyIsSetOnTheHostItsFor() {
        #expect(setKeyCommand(service: "linear") == "vibe service set-key linear")
    }

    @Test func aProfileHasThemOnUnlessItSaysOtherwise() throws {
        let json = #"{"name":"p","environment":"linux","model":"opus","reasoning":"high","mode":"auto","memory":1}"#
        var p = try JSONDecoder().decode(Profile.self, from: Data(json.utf8))
        #expect(p.sharedServicesOn)
        p.sharedServices = false
        #expect(!p.sharedServicesOn)
        p.sharedServices = true
        #expect(p.sharedServicesOn)
    }
}
