import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

private func service(
    _ state: ServiceState, role: ServiceRole = .follower, owner: String? = nil, until: String? = nil,
    enabled: Bool = true, readOnly: Bool = false, denied: [String] = [], tools: Int = 3, detail: String? = nil,
    at: String? = nil
) -> ServiceInfo {
    ServiceInfo(
        name: "linear", title: "Linear", state: state, role: role, owner: owner, accessUntil: until, enabled: enabled,
        readOnly: readOnly, deniedTools: denied, tools: tools, detail: detail, detailAt: at)
}

private func connection(_ state: ServiceState, signedIn: Bool = true, detail: String? = nil) -> ServiceConnection {
    ServiceConnection(
        ServiceOverview(name: "linear", title: "Linear", state: state, owner: "vps", signedIn: signedIn, detail: detail))
}

struct ServicesTests {
    @Test func theOneConnectionSaysWhereItStandsAndWhatToDo() {
        let off = connection(.disconnected, signedIn: false)
        #expect(off.status == "Not connected")
        #expect(off.tone == .off)
        #expect(off.signIn == .connect)
        #expect(!off.canDisconnect)

        let on = connection(.connected)
        #expect(on.status == "Connected")
        #expect(on.tone == .good)
        #expect(on.signIn == nil)
        #expect(on.canDisconnect)

        #expect(connection(.refreshing).tone == .working)
        #expect(connection(.refreshing).signIn == nil)
        // It's the owner that's away: signing in again wouldn't help, unless it's
        // moved, which is a choice of its own.
        #expect(connection(.ownerUnreachable).signIn == nil)
        #expect(connection(.ownerUnreachable).canDisconnect)

        for state in [ServiceState.needsReconnect, .revoked] {
            let lost = connection(state)
            #expect(lost.tone == .needsYou)
            #expect(lost.signIn == .reconnect)
            #expect(lost.canDisconnect)
        }
    }

    @Test func eachHostSaysWhatItHas() {
        #expect(ServiceSummary(service(.connected, role: .owner)).status == "Holds the login")
        #expect(ServiceSummary(service(.connected, owner: "vps")).status == "Has access")
        // Always tomorrow: a summary reads the clock, and one that's run out says so instead.
        let until = ISO8601DateFormatter().string(from: Date().addingTimeInterval(24 * 60 * 60))
        #expect(ServiceSummary(service(.connected, owner: "vps", until: until)).status.hasPrefix("Has access until "))
        #expect(ServiceSummary(service(.connected, role: .owner, until: until)).status.hasPrefix("Holds the login · access until "))
        let away = ServiceSummary(service(.ownerUnreachable, owner: "vps"))
        #expect(away.status == "No current access: can't reach vps")
        #expect(away.tone == .needsYou)
        #expect(ServiceSummary(service(.disconnected, tools: 0)).status == "No access")
        #expect(ServiceSummary(service(.disconnected, tools: 0)).note == nil)
        #expect(ServiceSummary(service(.connected)).note == "3 tools")
    }

    @Test func aFollowersTokenThatHasRunOutSaysSo() throws {
        let until = "2026-11-04T12:00:00Z"
        let time = try #require(ISO8601DateFormatter().date(from: until))
        let info = service(.connected, owner: "vps", until: until)
        let before = ServiceSummary(info, now: time.addingTimeInterval(-60))
        #expect(before.status.hasPrefix("Has access until"))
        #expect(before.tone == .good)
        let after = ServiceSummary(info, now: time.addingTimeInterval(60))
        #expect(after.status.hasPrefix("Access expired"))
        #expect(after.tone == .needsYou)
    }

    @Test func aTimeThatCantBeReadIsShownAsItWas() {
        #expect(serviceTime("soon") == "soon")
        #expect(serviceTime("2026-11-04T00:00:00Z") != "2026-11-04T00:00:00Z")
    }

    @Test func theLastErrorSaysWhen() {
        let lost = ServiceSummary(service(.needsReconnect, detail: "It refused", at: "2026-11-04T00:00:00Z"))
        #expect(lost.detail == "It refused (\(serviceTime("2026-11-04T00:00:00Z")))")
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

    @Test func anOlderHostIsToldToUpdate() {
        let old = VibedClientError.rpc(RPCError(code: RPCErrorCode.methodNotFound.rawValue, message: "Method not found: services.list"))
        #expect(serviceFailure(old, host: "vps") == "vps needs updating for shared services: Settings → Hosts → Update vibed.")
        let other = VibedClientError.rpc(RPCError(code: RPCErrorCode.hostUnavailable.rawValue, message: "vps is unavailable"))
        #expect(serviceFailure(other, host: "vps") == "vps is unavailable")
    }

    @Test func rebuildingIsOnlySuggestedWhenItWouldHelp() throws {
        func toolbox(_ fields: String) throws -> ToolboxStatus {
            try JSONDecoder().decode(ToolboxStatus.self, from: Data(#"{"state":"closed",\#(fields)}"#.utf8))
        }
        #expect(!servicesNeedRebuild(nil))
        #expect(!servicesNeedRebuild(try toolbox(#""base":3,"newest_base":3"#)))
        #expect(servicesNeedRebuild(try toolbox(#""base":2,"newest_base":3"#)))
        // The base that has them isn't down yet: a rebuild would change nothing.
        #expect(!servicesNeedRebuild(try toolbox(#""base":2,"newest_base":2"#)))
        let old = try toolbox(#""template":1,"home":1,"base":2,"newest_base":3"#)
        #expect(ToolboxSummary(old).notes.contains { $0.contains("no shared services") })
    }

    @Test func aProfileHasThemOnUnlessItSaysOtherwise() throws {
        let json = #"{"name":"p","environment":"linux","model":"opus","reasoning":"high","mode":"auto","memory":1}"#
        var p = try JSONDecoder().decode(Profile.self, from: Data(json.utf8))
        #expect(p.sharedServicesOn)
        #expect(!profileSummary(p).contains("shared"))
        p.sharedServices = false
        #expect(!p.sharedServicesOn)
        #expect(profileSummary(p).hasSuffix(" · No shared services"))
        p.sharedServices = true
        #expect(p.sharedServicesOn)
    }
}
