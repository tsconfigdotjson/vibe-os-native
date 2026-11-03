import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

@Suite struct PresentationTests {
    @Test func theIconCountsAwakeAgentsAndFlagsQuestionsAndPauses() throws {
        let agents = try [
            agent("a", state: .busy), agent("b", state: .idle), agent("c", state: .asleep),
            agent("d", state: .exited),
        ]
        #expect(IconSummary(agents: agents) == IconSummary(agents: agents))
        var icon = IconSummary(agents: agents)
        #expect((icon.awake, icon.question, icon.paused) == (2, false, false))
        icon = IconSummary(agents: agents + [try agent("e", state: .question), try agent("f", state: .paused)])
        #expect((icon.awake, icon.question, icon.paused) == (4, true, true))
        #expect(IconSummary(agents: []).awake == 0)
    }

    @Test func dotsFollowTheState() {
        #expect(StateDot(.busy) == .busy)
        #expect(StateDot(.question) == .question)
        #expect(StateDot(.idle) == .idle)
        #expect(StateDot(.asleep) == .asleep)
        #expect(StateDot(.paused) == .stopped)
        #expect(StateDot(.exited) == .stopped)
    }

    @Test func rowsGroupByHostThenProject() throws {
        let agents = try [
            agent("w1", host: "vps", project: "web"),
            agent("a1", project: "app"),
            agent("z1", project: "zebra"),
            agent("a2", project: "app"),
            agent("w2", host: "box", project: "web"),
        ]
        let groups = groupAgents(agents, hosts: ["local", "box", "vps"])
        #expect(groups.map(\.host) == ["local", "box", "vps"])
        #expect(groups[0].projects.map(\.project) == ["app", "zebra"])
        // Within a project, oldest first, as vibed lists them.
        #expect(groups[0].projects[0].agents.map(\.ref) == ["a1", "a2"])
        // A host that has agents but isn't listed still gets a group.
        #expect(groupAgents([try agent("x", host: "new")], hosts: ["local"]).map(\.host) == ["new"])
    }

    @Test func elapsedTime() {
        #expect(formatElapsed(-5) == "0s")
        #expect(formatElapsed(45) == "45s")
        #expect(formatElapsed(12 * 60 + 30) == "12m")
        #expect(formatElapsed(3 * 3600) == "3h")
        #expect(formatElapsed(3 * 3600 + 5 * 60) == "3h 5m")
        #expect(formatElapsed(2 * 86400 + 4 * 3600) == "2d 4h")
        #expect(parseTime("2026-10-30T12:00:00Z") != nil)
        #expect(parseTime("2026-10-30T12:00:00.250Z") != nil)
        #expect(parseTime("yesterday") == nil)
    }

    @Test func sizesReadAndWriteLikeHostToml() {
        #expect(formatBytes(4 << 30) == "4G")
        #expect(formatBytes(340 << 20) == "340M")
        #expect(formatBytes(3 << 29) == "1.5G")
        #expect(formatBytes(512) == "512B")
        #expect(formatBytes(1 << 40) == "1T")
        #expect(parseBytes("4G") == 4 << 30)
        #expect(parseBytes(" 512m ") == 512 << 20)
        #expect(parseBytes("1.5G") == 3 << 29)
        #expect(parseBytes("6144MiB") == 6 << 30)
        #expect(parseBytes("2GB") == 2 << 30)
        #expect(parseBytes("1024") == 1024)
        #expect(parseBytes("") == nil)
        #expect(parseBytes("G") == nil)
        #expect(parseBytes("-1G") == nil)
        #expect(parseBytes("4X") == nil)
        #expect(parseBytes("99999999999T") == nil)
        #expect(parseSeconds("60s") == 60)
        #expect(parseSeconds("5m") == 300)
        #expect(parseSeconds("120") == 120)
        #expect(parseSeconds("soon") == nil)
        // What a field shows reads back exactly.
        #expect(formatSize(1500 << 20) == "1500M")
        #expect(formatSize(4 << 30) == "4G")
        #expect(formatSize(1 << 40) == "1T")
        #expect(formatSize(1000) == "1000")
        for bytes in [4 << 30, 340 << 20, 1500 << 20, 20 << 30, 1 << 40, 12345, 3 << 29] {
            #expect(parseBytes(formatSize(bytes)) == bytes)
        }
    }

    @Test func theMemoryBarIsWhatItUsesAgainstItsLimit() throws {
        #expect(MemoryGauge(try agent("a")) == nil)
        let gauge = try #require(MemoryGauge(try agent("a", memoryUsed: 1 << 30)))
        #expect(gauge.fraction == 0.25)
        #expect(gauge.label == "1G / 4G")
        #expect(MemoryGauge(try agent("a", memoryUsed: 8 << 30))?.fraction == 1)
    }

    @Test func aUsageLimitShowsWhenItLifts() throws {
        var a = try agent("a", state: .idle, activity: .errored, title: "Watch the deploy")
        a.status.line = "$ gh run watch"
        #expect(rowLines(a).title == "Watch the deploy")
        #expect(rowLines(a).line == "$ gh run watch")
        a.status.stopReason = .usageLimit
        a.status.resetsAt = "2026-10-30T15:00:00Z"
        let lines = rowLines(a, timeZone: TimeZone(identifier: "UTC")!)
        #expect(lines.line?.hasPrefix("limited until ") == true)
        #expect(lines.line?.contains("3:00") == true || lines.line?.contains("15:00") == true)
        a.status.stopReason = nil
        a.status.error = "rate_limit"
        #expect(rowLines(a).line == "error: rate_limit")
    }

    @Test func theSetupCommandCarriesTheKey() {
        #expect(
            MCPSetup.command(secret: "vk_abc")
                == "claude mcp add --transport http vibeos http://127.0.0.1:7878/mcp --header \"Authorization: Bearer vk_abc\"")
        // And where vibed says MCP is served.
        #expect(
            MCPSetup.command(secret: "vk_abc", url: "http://100.101.102.103:9123/mcp")
                == "claude mcp add --transport http vibeos http://100.101.102.103:9123/mcp --header \"Authorization: Bearer vk_abc\"")
    }

    @Test func theLoginsSayWhatAgentsStartWithOrPlainlyThatItsNothing() throws {
        let known = LoginsSummary(
            try decode(
                ToolboxLogins.self,
                [
                    "home": 2, "claude": true,
                    "logins": [
                        ["tool": "gh", "account": "octocat on github.com"],
                        ["tool": "git", "account": "Octo Cat <octo@example.com>"],
                    ],
                ] as [String: Any]))
        #expect(known.headline == "New Linux agents start with these, from Linux home 2.")
        #expect(known.rows.map(\.tool) == ["GitHub (gh)", "Git identity"])
        #expect(known.rows.map(\.account) == ["octocat on github.com", "Octo Cat <octo@example.com>"])
        #expect(known.rows.map(\.symbol) == ["chevron.left.forwardslash.chevron.right", "person.text.rectangle"])
        // Signed in: nothing to say about it.
        #expect(known.signInHint == nil)

        let empty = LoginsSummary(try decode(ToolboxLogins.self, ["home": 2, "logins": [Any](), "claude": false] as [String: Any]))
        #expect(empty.headline == "No logins in Linux home 2 yet.")
        #expect(empty.rows.isEmpty)
        // Not signed in: a hint, no walk-through.
        #expect(empty.signInHint == "Linux agents aren't signed in yet.")
        let unknown = LoginsSummary(
            try decode(
                ToolboxLogins.self,
                ["home": 3, "claude": false, "claude_problem": "can't read the Claude login for linux agents: locked"]
                    as [String: Any]))
        #expect(unknown.headline.contains("couldn't tell which logins Linux home 3 holds"))
        // vibed's clause, as a sentence.
        #expect(unknown.signInHint == "Can't read the Claude login for linux agents: locked")
        let none = LoginsSummary(try decode(ToolboxLogins.self, ["logins": [Any](), "claude": true] as [String: Any]))
        #expect(none.headline.hasPrefix("No Linux home yet."))
        #expect(LoginTool.allCases.map(toolName) == ["GitHub (gh)", "Google Cloud", "Git identity", "SSH key"])
    }

    @Test func theToolboxSaysWhatItsDoingNow() throws {
        #expect(ToolboxSummary(nil).state == "Loading…")
        let none = ToolboxSummary(try decode(ToolboxStatus.self, ["state": "unavailable"]))
        #expect(none.state == "Not on this host")
        #expect(none.versions == nil && none.notes.isEmpty)
        var status = try decode(
            ToolboxStatus.self,
            [
                "state": "closed", "template": 3, "home": 2, "base": 1, "newest_base": 2,
                "last": ["action": "rebuild", "at": "2026-10-30T12:00:00Z", "uncovered": ["apt: htop"]] as [String: Any],
            ] as [String: Any])
        var summary = ToolboxSummary(status)
        // Closed says closed: only an open shell says to exit it (#46).
        #expect(summary.state == "Closed")
        #expect(!summary.working)
        #expect(summary.versions == "Template v3 · Linux home v2 · Base 1")
        #expect(summary.notes == ["Base 2 is installed. Rebuild Template moves to it.", "Not in toolbox.toml: apt: htop"])
        status.state = .open
        status.newestBase = 1
        status.last?.error = "installing gcloud exited with code 3"
        summary = ToolboxSummary(status)
        #expect(summary.state == "Open: exit the shell to save")
        #expect(summary.notes == ["The last rebuild failed: installing gcloud exited with code 3"])
        status.state = .saving
        #expect(ToolboxSummary(status).state == "Saving…")
        #expect(ToolboxSummary(status).working)
        // Copy for every agent, not one harness.
        for state in ToolboxState.allCases {
            status.state = state
            let words = ([ToolboxSummary(status).state] + ToolboxSummary(status).notes).joined(separator: " ")
            #expect(!words.contains("Claude"), "\(state)")
        }
    }

    /// Settings → Projects → a project, then a click in the sidebar: the
    /// pane starts again at its first level, and the hosts picked stay (#46).
    @Test func pickingAPaneGoesBackToItsFirstLevel() {
        var place = SettingsPlace()
        #expect(place.projects.isEmpty)
        place.projectsHost = "vps"
        place.limitsHost = "vps"
        place.projects = ["web"]
        place.picked()
        #expect(place.projects.isEmpty)
        #expect(place.projectsHost == "vps" && place.limitsHost == "vps")
        // Twice, as a click on the pane shown is, is the same.
        place.picked()
        #expect(place == {
            var p = SettingsPlace()
            p.projectsHost = "vps"
            p.limitsHost = "vps"
            return p
        }())
    }

    @Test func aKeySaysWhenItWasLastUsed() {
        let now = Date(timeIntervalSince1970: 1_793_361_600)
        #expect(keyUsage(nil, now: now) == "Never used")
        #expect(keyUsage("not a time", now: now) == "Never used")
        #expect(keyUsage("2026-10-30T11:59:40Z", now: now) == "Last used just now")
        #expect(keyUsage("2026-10-30T11:57:00Z", now: now) == "Last used 3 minutes ago")
        #expect(keyUsage("2026-10-29T12:00:00Z", now: now) == "Last used yesterday")
    }

    @Test func quoting() {
        #expect(shellQuote("it's") == "'it'\\''s'")
        #expect(appleScriptString(#"exec '/a b/vibe' attach "x" \ y"#) == #""exec '/a b/vibe' attach \"x\" \\ y""#)
    }

    @Test func vibeAndItsSocket() {
        #expect(defaultVibePath(environment: [:], bundled: nil) == "/usr/local/bin/vibe")
        #expect(defaultVibePath(environment: ["VIBE_CLI": "/x/vibe"]) == "/x/vibe")
        // Until setup links it, the app's own.
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("vibe-path-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let bundled = dir.appendingPathComponent("vibe").path
        FileManager.default.createFile(atPath: bundled, contents: Data("#!/bin/sh\n".utf8), attributes: [.posixPermissions: 0o755])
        let missing = dir.appendingPathComponent("not-linked").path
        #expect(defaultVibePath(environment: [:], linked: missing, bundled: bundled) == bundled)
        #expect(defaultVibePath(environment: [:], linked: bundled, bundled: "/nowhere/vibe") == bundled)
        #expect(defaultVibePath(environment: [:], linked: missing, bundled: "/nowhere/vibe") == missing)
        #expect(attachSocket(environment: [:]) == nil)
        #expect(attachSocket(environment: ["VIBE_SOCKET": ""]) == nil)
        #expect(attachSocket(environment: ["VIBE_SOCKET": "/tmp/v.sock"]) == "/tmp/v.sock")
    }

    @Test func commandLineRefs() {
        #expect(AttachCommandLine.refs(in: []) == nil)
        #expect(AttachCommandLine.refs(in: ["-NSDocumentRevisionsDebugMode", "YES"]) == nil)
        #expect(AttachCommandLine.refs(in: ["--attach", "a", "--attach", "b"]) == ["a", "b"])
        #expect(AttachCommandLine.refs(in: ["--attach"]) == nil)
    }
}

@Suite struct ExternalAddressTests {
    func host(_ tunnel: TunnelState?, external: Bool = true, detail: String? = nil) throws -> HostInfo {
        try decode(
            HostInfo.self,
            hostJSON("vps", link: .connected, external: external, tunnel: tunnel, externalDetail: detail))
    }

    @Test func itsStatusSaysWhetherOutsideClientsGetThrough() throws {
        #expect(externalStatus(try host(.up, external: false)) == nil)
        let working = try #require(externalStatus(try host(.up)))
        #expect(working.tone == .working)
        #expect(working.short == "public")
        #expect(working.summary == "Outside MCP clients reach agents on vps and on this Mac.")
        #expect(working.detail == nil)
        let certificate = try #require(
            externalStatus(try host(.up, detail: "getting a certificate for vps.example.com from Let's Encrypt")))
        #expect(certificate.tone == .pending)
        #expect(certificate.detail?.contains("Let's Encrypt") == true)
        #expect(try externalStatus(host(.connecting))?.tone == .pending)
        let down = try #require(externalStatus(try host(.down, detail: "can't listen on port 443")))
        #expect(down.tone == .trouble)
        #expect(down.short == "not reachable")
        #expect(down.detail == "can't listen on port 443")
    }

    @Test func aPublicNameIsSuggestedFromTheSSHAddress() {
        #expect(suggestedDomain(fromSSH: "me@vps.example.com") == "vps.example.com")
        #expect(suggestedDomain(fromSSH: "ssh://me@VPS.Example.com:2222") == "vps.example.com")
        #expect(suggestedDomain(fromSSH: "vps.example.com") == "vps.example.com")
        // A name from ~/.ssh/config, or an address, isn't a public name.
        #expect(suggestedDomain(fromSSH: "vps") == "")
        #expect(suggestedDomain(fromSSH: "root@203.0.113.7") == "")
        #expect(suggestedDomain(fromSSH: "me@[2001:db8::1]") == "")
    }

    @Test func onlyAPublicDNSNameIsTaken() {
        #expect(normalizedDomain(" VPS.example.com. ") == "vps.example.com")
        #expect(normalizedDomain("x-1.example.co.uk") == "x-1.example.co.uk")
        for bad in ["", "vps", "203.0.113.7", "*.example.com", "-a.example.com", "a..example.com",
            "https://vps.example.com", "vps.example.com:443", "vps example.com", String(repeating: "a", count: 64) + ".com"]
        {
            #expect(normalizedDomain(bad) == nil, "\(bad)")
        }
    }
}

@Suite struct NotificationTests {
    @Test func aQuestionAnErrorAndALimitNotify() throws {
        let before = try agent("a", state: .busy, activity: .thinking, title: "Dark mode")
        let question = try #require(
            notice(for: try event(.agentState, ref: "a", extra: ["state": "question"]), agent: before, endOfTurn: false))
        #expect(question.kind == .question)
        #expect(question.title == "a has a question")
        #expect(question.body == "Dark mode")
        #expect((question.host, question.ref) == ("local", "a"))
        // Where it works, and grouped with its other notices.
        #expect(question.subtitle == "app")
        #expect(question.thread == "local:a")
        let remote = try #require(
            notice(
                for: try event(.agentState, ref: "b", host: "vps", extra: ["state": "question"]), agent: nil,
                endOfTurn: false))
        #expect(remote.subtitle == "app on vps")
        #expect(remote.thread == "vps:b")

        #expect(notice(for: try event(.agentState, ref: "a", extra: ["state": "idle"]), agent: before, endOfTurn: true) == nil)
        // Once per question.
        let asking = try agent("a", state: .question)
        #expect(notice(for: try event(.agentState, ref: "a", extra: ["state": "question"]), agent: asking, endOfTurn: false) == nil)

        let limit = try #require(
            notice(
                for: try event(.agentLimit, ref: "a", extra: ["limit": "memory", "message": "paused at 100% of its 4G"]),
                agent: before, endOfTurn: false))
        #expect(limit.kind == .limit)
        #expect(limit.title == "a · memory limit")
        #expect(limit.body == "Paused at 100% of its 4G")

        let status: [String: Any] = [
            "activity": "errored", "certainty": "firm", "pending_wakeups": [], "error": "overloaded",
        ]
        let error = try #require(
            notice(for: try event(.agentStatus, ref: "a", extra: ["status": status]), agent: before, endOfTurn: false))
        #expect(error.kind == .error)
        #expect(error.body == "Overloaded")
        // Only as it starts: a status that stays errored doesn't again.
        let errored = try agent("a", activity: .errored)
        #expect(notice(for: try event(.agentStatus, ref: "a", extra: ["status": status]), agent: errored, endOfTurn: false) == nil)

        var usage = status
        usage["stop_reason"] = "usage_limit"
        usage["resets_at"] = "2026-10-30T15:00:00Z"
        let limited = try #require(
            notice(for: try event(.agentStatus, ref: "a", extra: ["status": usage]), agent: before, endOfTurn: false))
        #expect(limited.title == "a hit a usage limit")
        #expect(limited.body.hasPrefix("Limited until"))
    }

    @Test func theEndOfATurnNotifiesOnlyWhenAskedTo() throws {
        let working = try agent("a", activity: .runningTool, title: "Dark mode")
        let done: [String: Any] = ["activity": "done", "certainty": "firm", "pending_wakeups": []]
        let e = try event(.agentStatus, ref: "a", extra: ["status": done])
        #expect(notice(for: e, agent: working, endOfTurn: false) == nil)
        let end = try #require(notice(for: e, agent: working, endOfTurn: true))
        #expect(end.kind == .endOfTurn)
        #expect(end.body == "Dark mode")
        // Not when it was at rest already, such as after a restart.
        #expect(notice(for: e, agent: try agent("a", activity: .done), endOfTurn: true) == nil)
        #expect(notice(for: e, agent: nil, endOfTurn: true) == nil)
    }

    @Test func eventsAboutNoAgentDontNotify() throws {
        #expect(notice(for: try event(.limitsChanged), agent: nil, endOfTurn: true) == nil)
        #expect(notice(for: try event(.agentAdded, ref: "a", extra: ["state": "busy"]), agent: nil, endOfTurn: true) == nil)
    }
}

@Suite @MainActor struct AttachTests {
    func makeAttacher(_ terminal: FakeTerminal, clock: @escaping () -> Date = Date.init) -> Attacher {
        Attacher(terminal: terminal, vibePath: "/usr/local/bin/vibe", socket: nil, clock: clock)
    }

    @Test func clickingTwiceGivesOneTab() throws {
        let terminal = FakeTerminal()
        let attacher = makeAttacher(terminal)
        let a = try agent("quiet-amber-otter")
        // The first click opens a window running vibe attach.
        #expect(try attacher.attach(a, attached: []) == .opened(tty: "/dev/ttys001"))
        #expect(terminal.ran == ["exec '/usr/local/bin/vibe' attach 'quiet-amber-otter'"])
        // The second comes before vibed has seen that attach connect: the
        // tab just opened is raised all the same.
        #expect(try attacher.attach(a, attached: []) == .raised)
        // And once vibed reports it.
        #expect(try attacher.attach(a, attached: ["/dev/ttys001"]) == .raised)
        #expect(terminal.ran.count == 1)
        #expect(terminal.tabs.count == 1)
        #expect(terminal.raised == ["/dev/ttys001", "/dev/ttys001"])
    }

    @Test func aTabAttachedElsewhereIsRaisedToo() throws {
        let terminal = FakeTerminal()
        terminal.tabs["/dev/ttys007"] = "vibe attach a"
        #expect(try makeAttacher(terminal).attach(try agent("a"), attached: ["/dev/ttys007"]) == .raised)
        #expect(terminal.ran.isEmpty)
    }

    @Test func aClosedTabGetsANewOne() throws {
        let terminal = FakeTerminal()
        let attacher = makeAttacher(terminal)
        let a = try agent("a")
        _ = try attacher.attach(a, attached: [])
        terminal.close("/dev/ttys001")
        #expect(try attacher.attach(a, attached: []) == .opened(tty: "/dev/ttys002"))
        // vibed may still list a client that has just gone.
        terminal.close("/dev/ttys002")
        #expect(try attacher.attach(a, attached: ["/dev/ttys002"]) == .opened(tty: "/dev/ttys003"))
    }

    @Test func aTabThisAppOpenedCountsOnlyForAWhile() throws {
        let terminal = FakeTerminal()
        var now = Date()
        let attacher = makeAttacher(terminal) { now }
        let a = try agent("a")
        _ = try attacher.attach(a, attached: [])
        now += 61
        // Its tty may belong to another tab by now; vibed's word is what counts.
        terminal.raised = []
        #expect(try attacher.attach(a, attached: []) == .opened(tty: "/dev/ttys002"))
        #expect(terminal.raised.isEmpty)
    }

    @Test func aTabWhoseAttachEndedGetsANewOne() throws {
        let terminal = FakeTerminal()
        var now = Date()
        let attacher = makeAttacher(terminal) { now }
        let a = try agent("a")
        _ = try attacher.attach(a, attached: [])
        // Just opened, it counts even before its shell runs anything.
        terminal.end("/dev/ttys001")
        #expect(try attacher.attach(a, attached: []) == .raised)
        // Detached a while later (Ctrl-\), its tab is left as it is.
        now += 10
        #expect(try attacher.attach(a, attached: []) == .opened(tty: "/dev/ttys002"))
        // While its attach runs, it's raised.
        now += 10
        #expect(try attacher.attach(a, attached: []) == .raised)
        #expect(terminal.raised.last == "/dev/ttys002")
    }

    @Test func eachAgentHasItsOwnTab() throws {
        let terminal = FakeTerminal()
        let attacher = makeAttacher(terminal)
        _ = try attacher.attach(try agent("a"), attached: [])
        #expect(try attacher.attach(try agent("b"), attached: []) == .opened(tty: "/dev/ttys002"))
    }

    @Test func withoutAutomationACommandFileOpens() throws {
        let terminal = FakeTerminal()
        terminal.permitted = false
        let attacher = makeAttacher(terminal)
        #expect(try attacher.attach(try agent("a"), attached: ["/dev/ttys001"]) == .openedWithoutScripting)
        #expect(terminal.withoutScripting == ["exec '/usr/local/bin/vibe' attach 'a'"])
        // A second click right after can't find that tab to raise, so it
        // opens no other.
        #expect(try attacher.attach(try agent("a"), attached: []) == .openedWithoutScripting)
        #expect(terminal.withoutScripting.count == 1)
    }

    @Test func theToolboxShellHasOneTabToo() throws {
        let terminal = FakeTerminal()
        let attacher = makeAttacher(terminal)
        #expect(try attacher.openToolbox(attached: []) == .opened(tty: "/dev/ttys001"))
        #expect(terminal.ran == ["exec '/usr/local/bin/vibe' toolbox"])
        #expect(try attacher.openToolbox(attached: []) == .raised)
        // It's no agent's tab, even one named toolbox.
        #expect(try attacher.attach(try agent("toolbox"), attached: []) == .opened(tty: "/dev/ttys002"))
        // One attached elsewhere is raised.
        terminal.tabs["/dev/ttys009"] = "vibe toolbox"
        #expect(try makeAttacher(terminal).openToolbox(attached: ["/dev/ttys009"]) == .raised)
        #expect(terminal.raised.last == "/dev/ttys009")
    }

    @Test func commandsForLocalAndRemoteAgents() throws {
        let terminal = FakeTerminal()
        let withSocket = Attacher(
            terminal: terminal, vibePath: "/opt/my vibe/vibe", socket: "/tmp/v/vibed.sock")
        #expect(
            withSocket.command(for: try agent("a"))
                == "exec '/opt/my vibe/vibe' --socket '/tmp/v/vibed.sock' attach 'a'")
        // Another host's agent attaches over SSH, as vibed says, and its
        // ttys, which are the other host's, aren't looked for here.
        let remote = try agent("b", host: "vps")
        #expect(withSocket.command(for: remote) == "exec ssh -t vps vibe attach b")
        terminal.tabs["/dev/ttys001"] = "something else"
        #expect(try withSocket.attach(remote, attached: ["/dev/ttys001"]) == .opened(tty: "/dev/ttys002"))
    }
}

@Suite @MainActor struct ModelTests {
    func makeModel(_ daemon: FakeDaemon, terminal: FakeTerminal? = nil, notifier: FakeNotifier? = nil)
        -> MenuModel
    {
        // A poll that never comes, so only events move the menu.
        let model = MenuModel(
            daemon: daemon, terminal: terminal ?? FakeTerminal(), notifier: notifier, vibePath: "/usr/local/bin/vibe", socket: nil,
            openPoll: .seconds(3600), closedPoll: .seconds(3600), endOfTurn: { true })
        return model
    }

    @Test func theMenuTracksStateChangesLive() async throws {
        let agents = Box([agentJSON("quiet-amber-otter", state: .busy)])
        let daemon = fakeDaemon()
        daemon.on(AgentsList.name) { _ in ["agents": agents.value] }
        let notifier = FakeNotifier()
        let model = makeModel(daemon, notifier: notifier)
        model.start()
        defer { model.stop() }
        await eventually("a subscription") { daemon.subscriptions == 1 }
        daemon.send(.subscribed(seq: 0))
        await eventually("the first listing") { model.connection == .connected && model.agents.count == 1 }
        #expect(model.connection == .connected)
        #expect(model.live)
        #expect(model.icon == IconSummary(agents: model.agents))
        #expect(model.icon.awake == 1)

        // A question: the row and the icon change from the event alone.
        let lists = daemon.calls.filter { $0 == AgentsList.name }.count
        daemon.send(.event(try event(.agentState, ref: "quiet-amber-otter", seq: 2, extra: ["state": "question"])))
        await eventually("the question") { model.agents.first?.state == .question }
        #expect(model.icon.question)
        #expect(daemon.calls.filter { $0 == AgentsList.name }.count == lists)
        #expect(notifier.posted.map(\.kind) == [.question])

        // A new status: its title and line.
        let status: [String: Any] = [
            "activity": "running_tool", "certainty": "firm", "pending_wakeups": [], "title": "Dark mode",
            "line": "$ npm test",
        ]
        daemon.send(.event(try event(.agentStatus, ref: "quiet-amber-otter", seq: 3, extra: ["status": status])))
        await eventually("the status") { model.agents.first?.status.line == "$ npm test" }

        // A new agent, and a paused one: read again.
        agents.value.append(agentJSON("brave-copper-lynx", state: .paused))
        daemon.send(.event(try event(.agentAdded, ref: "brave-copper-lynx", seq: 4, extra: ["state": "busy"])))
        await eventually("the new agent") { model.agents.count == 2 }
        #expect(model.icon.paused)

        // A deleted one goes.
        agents.value.removeFirst()
        daemon.send(.event(try event(.agentDeleted, ref: "quiet-amber-otter", seq: 5)))
        await eventually("the delete") { model.agents.map(\.ref) == ["brave-copper-lynx"] }
    }

    @Test func aSlowListingDoesntUndoAnEventThatCameMeanwhile() async throws {
        let slow = Box(false)
        let daemon = fakeDaemon()
        daemon.on(AgentsList.name) { _ in
            // Read before the question, answered after it.
            if slow.value { Thread.sleep(forTimeInterval: 0.3) }
            return ["agents": [agentJSON("a", state: .busy)]]
        }
        let model = makeModel(daemon)
        await model.refresh()
        slow.value = true
        let lists = daemon.params(of: AgentsList.name).count
        let refreshing = Task { await model.refresh() }
        await eventually("the slow listing") { daemon.params(of: AgentsList.name).count > lists }
        await model.handle(try event(.agentState, ref: "a", seq: 2, extra: ["state": "question"]))
        #expect(model.agents.first?.state == .question)
        await refreshing.value
        #expect(model.agents.first?.state == .question)
        #expect(model.icon.question)
    }

    @Test func settingsEventsBumpTheRevision() async throws {
        let daemon = fakeDaemon()
        let model = makeModel(daemon)
        let before = model.settingsRevision
        for kind in [EventKind.projectAdded, .projectRemoved, .profilesChanged, .limitsChanged, .keysChanged] {
            await model.handle(try event(kind))
        }
        #expect(model.settingsRevision == before + 5)
        await model.handle(try event(.lagged))
        #expect(model.settingsRevision == before + 6)
        #expect(daemon.calls.contains(AgentsList.name))
    }

    @Test func theToolboxAndMCPFollowTheirEvents() async throws {
        let state = Box("closed")
        let serving = Box(false)
        let daemon = fakeDaemon()
        daemon.on(HostsList.name) { _ in ["hosts": [hostJSON("local", mcp: serving.value)]] }
        daemon.on(ToolboxStatusGet.name) { _ in
            ["state": state.value, "template": 1, "home": 1, "base": 1, "newest_base": 1]
        }
        daemon.on(McpGet.name) { _ in
            ["port": 7878, "tailscale": false, "url": "http://127.0.0.1:7878/mcp", "tailscale_urls": []]
        }
        let model = makeModel(daemon)
        await model.refresh()
        #expect(model.toolbox?.state == .closed)
        #expect(model.mcp?.url == "http://127.0.0.1:7878/mcp")
        #expect(model.localHost?.mcp == false)

        state.value = "open"
        await model.handle(try event(.toolboxChanged, extra: ["message": "The Linux toolbox is open"]))
        #expect(model.toolbox?.state == .open)

        // MCP served now: the header says so, and open settings re-read.
        serving.value = true
        let before = model.settingsRevision
        await model.handle(try event(.mcpChanged, extra: ["message": "MCP is served"]))
        #expect(model.localHost?.mcp == true)
        #expect(model.settingsRevision == before + 1)
    }

    @Test func openShellGivesOneTabAndRaisesAnOpenOne() async throws {
        let sessions = Box([[String: Any]]())
        let daemon = fakeDaemon()
        daemon.on(SessionsList.name) { _ in ["sessions": sessions.value] }
        let terminal = FakeTerminal()
        let model = makeModel(daemon, terminal: terminal)
        // Not running: `vibe toolbox` opens it, in Terminal, once for two
        // clicks. The app never opens it itself, which could boot it.
        await model.openToolbox()
        await model.openToolbox()
        #expect(terminal.ran == ["exec '/usr/local/bin/vibe' toolbox"])
        #expect(terminal.raised == ["/dev/ttys001"])
        #expect(!daemon.calls.contains(ToolboxOpen.name))
        // Running, in a tab vibed knows of: that one is raised.
        sessions.value = [
            ["host": "local", "ref": "toolbox", "state": "running", "title": "Linux toolbox", "clients": 1,
             "ttys": ["/dev/ttys007"]],
        ]
        terminal.tabs["/dev/ttys007"] = "vibe toolbox"
        await model.openToolbox()
        #expect(terminal.raised.last == "/dev/ttys007")
        #expect(terminal.ran.count == 1)
        #expect(model.message == nil)
    }

    @Test func seeLoginsAsksVibedAndOpenShellFollowsTheToolbox() async throws {
        let state = Box("closed")
        let daemon = fakeDaemon()
        daemon.on(ToolboxStatusGet.name) { _ in ["state": state.value] }
        daemon.on(ToolboxLoginsGet.name) { _ in
            ["home": 1, "logins": [["tool": "ssh", "account": "id_ed25519 (me@laptop)"]], "claude": true]
        }
        let model = makeModel(daemon)
        #expect(!model.toolboxCanOpen)
        await model.refresh()
        #expect(model.toolboxCanOpen)
        let logins = try await model.toolboxLogins()
        #expect(logins.logins?.first?.tool == .ssh)
        #expect(!daemon.calls.contains(ToolboxOpen.name))
        state.value = "rebuilding"
        await model.handle(try event(.toolboxChanged))
        #expect(!model.toolboxCanOpen)
    }

    @Test func rebuildAndTheClaudeLoginRunInTerminal() async throws {
        let terminal = FakeTerminal()
        let model = makeModel(fakeDaemon(), terminal: terminal)
        model.runInTerminal(["toolbox", "rebuild"], exec: false)
        model.runInTerminal(["setup-token"], exec: true)
        #expect(terminal.ran == ["'/usr/local/bin/vibe' toolbox rebuild", "exec '/usr/local/bin/vibe' setup-token"])
        // Without Automation, through a .command file.
        terminal.permitted = false
        model.runInTerminal(["setup-token"], exec: true)
        #expect(terminal.withoutScripting == ["exec '/usr/local/bin/vibe' setup-token"])
        #expect(model.message == nil)
    }

    @Test func whenVibedGoesAwayTheMenuSaysSoAndReconnects() async throws {
        let daemon = fakeDaemon(agents: ["local": [agentJSON("a")]])
        let model = makeModel(daemon)
        model.start()
        defer { model.stop() }
        await eventually("a subscription") { daemon.subscriptions == 1 }
        daemon.send(.subscribed(seq: 0))
        await eventually("connected") { model.connection == .connected && model.agents.count == 1 }
        daemon.on(HostsList.name) { _ in
            throw VibedClientError.connect(path: "/x", errno: ENOENT)
        }
        daemon.drop()
        await eventually("unavailable") { model.connection != .connected && model.connection != .connecting }
        #expect(!model.live)
        // Its rows stay, as last known, until vibed is back.
        #expect(model.agents.count == 1)
        daemon.on(HostsList.name) { _ in ["hosts": [hostJSON("local")]] }
        await eventually("a new subscription", within: 10) { daemon.subscriptions == 1 }
        daemon.send(.subscribed(seq: 9))
        await eventually("connected again") { model.connection == .connected && model.live }
    }

    @Test func aHostThatsDownKeepsItsRowsUnavailable() async throws {
        let vpsLink = Box(HostLink.connected)
        let daemon = fakeDaemon(agents: ["local": [agentJSON("a")], "vps": [agentJSON("b", host: "vps")]])
        daemon.on(HostsList.name) { _ in
            ["hosts": [hostJSON("local", mcp: true), hostJSON("vps", link: vpsLink.value, external: true)]]
        }
        let model = makeModel(daemon)
        await model.refresh()
        #expect(model.agents.map(\.key) == ["local:a", "vps:b"])
        #expect(model.unavailableHosts.isEmpty)
        #expect(model.localHost?.mcp == true)
        #expect(model.otherHosts.map(\.name) == ["vps"])
        #expect(model.groups.map(\.host) == ["local", "vps"])
        // Down, its rows are as vibed last saw them, unavailable.
        vpsLink.value = .down
        await model.refresh()
        #expect(model.agents.map(\.key) == ["local:a", "vps:b"])
        #expect(model.unavailableHosts == ["vps"])
        // And if vibed can't list them, the menu keeps the rows it had.
        daemon.on(AgentsList.name) { params in
            if (params["host"] as? String) == "vps" {
                throw VibedClientError.rpc(RPCError(code: RPCErrorCode.hostUnavailable.rawValue, message: "vps is unavailable"))
            }
            return ["agents": [agentJSON("a")]]
        }
        await model.refresh()
        #expect(model.agents.map(\.key) == ["local:a", "vps:b"])
        #expect(model.unavailableHosts == ["vps"])
    }

    @Test func aDownHostsRowsShowAfterTheAppRestarts() async throws {
        // vibed keeps a down host's agents as it last listed them.
        let daemon = fakeDaemon(agents: ["vps": [agentJSON("b", host: "vps")]])
        daemon.on(HostsList.name) { _ in
            ["hosts": [hostJSON("local"), hostJSON("vps", link: .down, detail: "it stopped answering")]]
        }
        let model = makeModel(daemon)
        await model.refresh()
        #expect(model.agents.map(\.key) == ["vps:b"])
        #expect(model.unavailableHosts == ["vps"])
        #expect(model.otherHosts.first?.detail == "it stopped answering")
    }

    @Test func addingAHostInstallsOverSSHAndRereads() async throws {
        let daemon = fakeDaemon()
        let hosts = Box([hostJSON("local")])
        daemon.on(HostsList.name) { _ in ["hosts": hosts.value] }
        daemon.on(HostsAdd.name) { params in
            #expect(params["name"] as? String == "vps")
            #expect(params["address"] as? String == "me@vps.example.com")
            let vps = hostJSON("vps", link: .down, missing: ["claude"], detail: "not connected yet")
            hosts.value.append(vps)
            return ["host": vps, "changed": true, "notes": ["Lingering is on."]]
        }
        let model = makeModel(daemon)
        await model.refresh()
        let revision = model.settingsRevision
        let installed = try await model.addHost(name: "vps", address: "me@vps.example.com")
        #expect(installed.host.missing == ["claude"])
        #expect(installed.notes == ["Lingering is on."])
        // An install can take minutes, so it isn't held to the usual timeout.
        #expect(daemon.timeout(of: HostsAdd.name) == MenuModel.installTimeout)
        #expect(model.otherHosts.map(\.name) == ["vps"])
        #expect(model.settingsRevision > revision)

        daemon.on(HostsUpgrade.name) { _ in ["host": hosts.value[1], "changed": false, "notes": []] }
        let again = try await model.upgradeHost("vps")
        #expect(!again.changed)
        #expect(daemon.timeout(of: HostsUpgrade.name) == MenuModel.installTimeout)

        daemon.on(HostsRemove.name) { params in
            #expect(params["host"] as? String == "vps")
            let removed = hosts.value.removeLast()
            return removed
        }
        try await model.removeHost("vps")
        #expect(model.otherHosts.isEmpty)
    }

    @Test func aHostBecomesTheExternalAddressAndStops() async throws {
        let external = Box(false)
        let daemon = fakeDaemon()
        daemon.on(HostsList.name) { _ in
            ["hosts": [hostJSON("local"), hostJSON("vps", link: .connected, external: external.value)]]
        }
        daemon.on(HostsExternal.name) { params in
            #expect(params["host"] as? String == "vps")
            external.value = params["on"] as? Bool == true
            if external.value { #expect(params["domain"] as? String == "vps.example.com") }
            return hostJSON("vps", link: .connected, external: external.value)
        }
        let model = makeModel(daemon)
        await model.refresh()
        #expect(model.externalHost == nil)
        let on = try await model.setExternal("vps", domain: "vps.example.com")
        #expect(on.url == "https://vps.example.com/mcp")
        #expect(on.tunnel == .up)
        #expect(model.externalHost?.name == "vps")
        let off = try await model.setExternal("vps", domain: nil)
        #expect(!off.external)
        #expect(daemon.params(of: HostsExternal.name).last?["domain"] == nil)
        #expect(model.externalHost == nil)
    }

    @Test func aFailedInstallSaysWhy() async throws {
        let daemon = fakeDaemon()
        daemon.on(HostsAdd.name) { _ in
            throw VibedClientError.rpc(
                RPCError(code: RPCErrorCode.commandFailed.rawValue, message: "can't reach me@vps over SSH: Permission denied"))
        }
        let model = makeModel(daemon)
        do {
            _ = try await model.addHost(name: "vps", address: "me@vps")
            Issue.record("the install didn't fail")
        } catch {
            #expect(describe(error) == "can't reach me@vps over SSH: Permission denied")
        }
    }

    @Test func hostEventsRereadTheHosts() async throws {
        let link = Box(HostLink.connected)
        let daemon = fakeDaemon()
        daemon.on(HostsList.name) { _ in ["hosts": [hostJSON("local"), hostJSON("vps", link: link.value)]] }
        let model = makeModel(daemon)
        await model.refresh()
        #expect(model.otherHosts.first?.link == .connected)
        link.value = .down
        await model.handle(try event(.hostLink, host: "vps", extra: ["state": "down", "message": "it stopped answering"]))
        #expect(model.otherHosts.first?.link == .down)
        #expect(model.unavailableHosts == ["vps"])
        let revision = model.settingsRevision
        await model.handle(try event(.hostRemoved, host: "vps"))
        #expect(model.settingsRevision > revision)
    }

    @Test func aRemoteRowAttachesOverSSH() async throws {
        let daemon = fakeDaemon(agents: ["vps": [agentJSON("b", host: "vps")]])
        daemon.on(HostsList.name) { _ in ["hosts": [hostJSON("local"), hostJSON("vps", link: .connected)]] }
        daemon.on(SessionsAttach.name) { params in
            ["host": "vps", "ref": params["ref"] as? String ?? "", "title": "t", "socket": "/home/me/s.sock",
             "ttys": ["/dev/pts/3"]]
        }
        let terminal = FakeTerminal()
        let model = makeModel(daemon, terminal: terminal)
        await model.refresh()
        let b = try #require(model.agent(host: "vps", ref: "b"))
        await model.attach(b)
        // The host's ttys are its own, so nothing here is raised for them.
        #expect(terminal.raised.isEmpty)
        #expect(terminal.ran == ["exec ssh -t vps vibe attach b"])
        #expect(daemon.params(of: SessionsAttach.name).first?["host"] as? String == "vps")
    }

    @Test func clickingARowTwiceGivesOneTab() async throws {
        let daemon = fakeDaemon(agents: ["local": [agentJSON("quiet-amber-otter")]])
        // vibed hasn't seen the new tab's vibe attach yet, either time.
        daemon.on(SessionsAttach.name) { params in
            ["host": "local", "ref": params["ref"] as? String ?? "", "title": "t", "socket": "/s", "ttys": []]
        }
        let terminal = FakeTerminal()
        let model = makeModel(daemon, terminal: terminal)
        await model.refresh()
        let a = try #require(model.agents.first)
        await model.attach(a)
        await model.attach(a)
        #expect(terminal.ran.count == 1)
        #expect(terminal.raised == ["/dev/ttys001"])
        #expect(model.message == nil)
        #expect(daemon.params(of: SessionsAttach.name).map { $0["ref"] as? String } == ["quiet-amber-otter", "quiet-amber-otter"])
    }

    @Test func clicksThatOverlapAttachOnce() async throws {
        let daemon = fakeDaemon(agents: ["local": [agentJSON("a")]])
        daemon.on(SessionsAttach.name) { _ in
            Thread.sleep(forTimeInterval: 0.2)
            return ["host": "local", "ref": "a", "title": "t", "socket": "/s", "ttys": []]
        }
        let terminal = FakeTerminal()
        let model = makeModel(daemon, terminal: terminal)
        await model.refresh()
        let a = try #require(model.agents.first)
        async let first: Void = model.attach(a)
        async let second: Void = model.attach(a)
        _ = await (first, second)
        #expect(terminal.ran.count == 1)
        #expect(daemon.params(of: SessionsAttach.name).count == 1)
    }

    @Test func anAttachThatFailsSaysWhy() async throws {
        let daemon = fakeDaemon(agents: ["local": [agentJSON("a", state: .exited)]])
        daemon.on(SessionsAttach.name) { _ in
            throw VibedClientError.rpc(
                RPCError(code: RPCErrorCode.notFound.rawValue, message: "session a isn't running: its command exited"))
        }
        let terminal = FakeTerminal()
        let model = makeModel(daemon, terminal: terminal)
        await model.refresh()
        await model.attach(try #require(model.agents.first))
        #expect(model.message == "Can't attach a: session a isn't running: its command exited")
        #expect(terminal.ran.isEmpty)
    }

    @Test func aNotificationClickAttaches() async throws {
        let daemon = fakeDaemon(agents: ["local": [agentJSON("a")]])
        daemon.on(SessionsAttach.name) { _ in
            ["host": "local", "ref": "a", "title": "t", "socket": "/s", "ttys": []]
        }
        let terminal = FakeTerminal()
        let notifier = FakeNotifier()
        let model = makeModel(daemon, terminal: terminal, notifier: notifier)
        notifier.onClick?("local", "a")
        await eventually("the attach") { terminal.ran.count == 1 }
        #expect(model.message == nil)
    }

    @Test func interruptAndDelete() async throws {
        let listed = Box([agentJSON("a")])
        let daemon = fakeDaemon()
        daemon.on(AgentsList.name) { _ in ["agents": listed.value] }
        daemon.on(AgentsInterrupt.name) { _ in agentJSON("a", activity: .interrupted) }
        daemon.on(AgentsDelete.name) { params in
            if params["force"] == nil {
                throw VibedClientError.rpc(
                    RPCError(code: RPCErrorCode.conflict.rawValue, message: "a has unsaved work"))
            }
            listed.value = []
            return agentJSON("a", state: .exited)
        }
        let model = makeModel(daemon)
        await model.refresh()
        let a = try #require(model.agents.first)
        await model.interrupt(a)
        #expect(model.agents.first?.status.activity == .interrupted)
        #expect(daemon.params(of: AgentsInterrupt.name).first?["ref"] as? String == "a")

        #expect(await model.delete(a, force: false) == .refused("a has unsaved work"))
        #expect(model.agents.count == 1)
        #expect(await model.delete(a, force: true) == .deleted)
        #expect(daemon.params(of: AgentsDelete.name).last?["force"] as? Bool == true)
        #expect(model.agents.isEmpty)
    }
}
