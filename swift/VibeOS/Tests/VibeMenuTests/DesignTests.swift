import AppKit
import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

// The design pass's logic, apart from its views: the menu bar glyph, drawn
// and checked pixel by pixel; what rows say, in words and symbols; the
// first-run steps; and the canned vibed every surface is drawn from.

@Suite @MainActor struct GlyphTests {
    /// The glyph drawn at 2x into a bitmap, 32 pixels square.
    func render(_ state: GlyphState) throws -> NSBitmapImageRep {
        let rep = try #require(
            NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: 32, pixelsHigh: 32, bitsPerSample: 8, samplesPerPixel: 4,
                hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0))
        rep.size = MenuBarGlyph.size
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        MenuBarGlyph.image(state).draw(in: NSRect(origin: .zero, size: MenuBarGlyph.size))
        NSGraphicsContext.restoreGraphicsState()
        return rep
    }

    /// The colour at a point, in points from the top left.
    func color(_ rep: NSBitmapImageRep, _ x: CGFloat, _ y: CGFloat) throws -> NSColor {
        try #require(rep.colorAt(x: Int(x * 2), y: Int(y * 2)))
    }

    /// The centre of a window, in points from the top left.
    func centre(column: Int, row: Int) -> (CGFloat, CGFloat) {
        let r = MenuBarGlyph.rect(column: column, row: row)
        return (r.midX, r.midY)
    }

    func filled(_ rep: NSBitmapImageRep, column: Int, row: Int) throws -> Bool {
        let (x, y) = centre(column: column, row: row)
        return try color(rep, x, y).alphaComponent > 0.5
    }

    @Test func itsStateFollowsTheIcon() throws {
        let agents = try [agent("a", state: .busy), agent("b", state: .question), agent("c", state: .asleep)]
        let state = GlyphState(icon: IconSummary(agents: agents), connected: true)
        #expect(state == GlyphState(lit: 2, question: true))
        // Four windows: one per agent awake, all of them from four up.
        #expect(GlyphState(lit: 9).lit == 4)
        #expect(GlyphState(lit: -1).lit == 0)
        // Without vibed, nothing is lit and nothing is flagged.
        let offline = GlyphState(icon: IconSummary(agents: agents), connected: false)
        #expect(offline == GlyphState(lit: 0, connected: false))
        #expect(!offline.badged)
    }

    @Test func itsATemplateUntilABadgeNeedsColour() {
        let plain = MenuBarGlyph.image(GlyphState(lit: 2), awake: 2)
        #expect(plain.isTemplate)
        #expect(plain.size == NSSize(width: 16, height: 16))
        #expect(plain.accessibilityDescription == "VibeOS, 2 agents awake")
        let badged = MenuBarGlyph.image(GlyphState(lit: 2, question: true, paused: true), awake: 2)
        #expect(!badged.isTemplate)
        #expect(badged.accessibilityDescription == "VibeOS, 2 agents awake, a question is waiting, an agent is paused")
        #expect(MenuBarGlyph.image(GlyphState(lit: 0, connected: false)).accessibilityDescription == "VibeOS, can't reach vibed")
        #expect(GlyphState(lit: 1).accessibilityLabel(awake: 1) == "VibeOS, 1 agent awake")
    }

    @Test func windowsLightInOrderAsAgentsWake() throws {
        // Top left, bottom left, bottom right, then top right.
        let lit: [[(Int, Int)]] = [[], [(0, 0)], [(0, 0), (0, 1)], [(0, 0), (0, 1), (1, 1)], [(0, 0), (0, 1), (1, 1), (1, 0)]]
        for (count, on) in lit.enumerated() {
            let rep = try render(GlyphState(lit: count))
            for column in 0..<2 {
                for row in 0..<2 {
                    let expected = on.contains { $0 == (column, row) }
                    #expect(try filled(rep, column: column, row: row) == expected, "\(count) lit: (\(column), \(row))")
                }
            }
        }
        // A dark window is still drawn, as an outline.
        let dark = try render(GlyphState(lit: 0))
        let r = MenuBarGlyph.rect(column: 0, row: 0)
        #expect(try color(dark, r.minX + 0.4, r.midY).alphaComponent > 0.3)
    }

    @Test func aQuestionIsAnAmberDotAndAPauseARedOne() throws {
        let rep = try render(GlyphState(lit: 4, question: true, paused: true))
        let (qx, qy) = centre(column: 1, row: 0)
        let question = try color(rep, qx, qy).usingColorSpace(.sRGB)
        #expect((question?.redComponent ?? 0) > 0.8 && (question?.blueComponent ?? 1) < 0.3)
        #expect((question?.greenComponent ?? 0) > 0.35)
        let (px, py) = centre(column: 1, row: 1)
        let paused = try color(rep, px, py).usingColorSpace(.sRGB)
        #expect((paused?.redComponent ?? 0) > 0.8 && (paused?.greenComponent ?? 1) < 0.4)
        // The dots are round: a window's corner, which a lit window fills,
        // stays clear.
        let corner = MenuBarGlyph.rect(column: 1, row: 0)
        #expect(try color(rep, corner.maxX - 0.3, corner.minY + 0.3).alphaComponent < 0.5)
        // The left column still shows the agents awake.
        #expect(try filled(rep, column: 0, row: 0))
        #expect(try filled(rep, column: 0, row: 1))
    }

    @Test func withoutVibedItsStruckThrough() throws {
        let rep = try render(GlyphState(lit: 0, connected: false))
        // The slash crosses the middle, where no window is.
        #expect(try color(rep, 8, 8).alphaComponent > 0.5)
        let plain = try render(GlyphState(lit: 0))
        #expect(try color(plain, 8, 8).alphaComponent < 0.1)
    }
}

@Suite struct RowWordsTests {
    @Test func eachStateHasAWordAndEachActivityASymbol() throws {
        #expect(AgentState.allCases.map(\.spoken) == ["working", "idle", "has a question", "asleep", "paused by a limit", "exited"])
        #expect(activitySymbol(try agent("a", state: .busy, activity: .thinking)) == "sparkle")
        #expect(activitySymbol(try agent("a", state: .busy, activity: .runningTool)) == "terminal")
        #expect(activitySymbol(try agent("a", state: .question, activity: .awaitingAnswer)) == "questionmark.bubble")
        #expect(activitySymbol(try agent("a", state: .question, activity: .awaitingApproval)) == "hand.raised")
        #expect(activitySymbol(try agent("a", state: .idle, activity: .done)) == "checkmark")
        #expect(activitySymbol(try agent("a", state: .idle, activity: .unknown)) == nil)
        // Why it isn't working comes first.
        #expect(activitySymbol(try agent("a", state: .asleep, activity: .done)) == "moon.zzz")
        #expect(activitySymbol(try agent("a", state: .paused, activity: .runningTool)) == "pause.circle")
        #expect(activitySymbol(try agent("a", state: .exited, activity: .done)) == "stop.circle")
        var limited = try agent("a", state: .idle, activity: .errored)
        limited.status.stopReason = .usageLimit
        #expect(activitySymbol(limited) == "hourglass")
        #expect(environmentSymbol(.linux) == "cube")
        #expect(environmentSymbol(.macos) == "laptopcomputer")
    }

    @Test func voiceOverHearsTheWholeRow() throws {
        var a = try agent("quiet-amber-otter", state: .busy, activity: .runningTool, title: "Fix the login race", memoryUsed: 1 << 30)
        a.status.line = "$ swift test"
        let now = try #require(parseTime("2026-10-30T14:05:00Z"))
        #expect(
            rowAccessibilityLabel(a, now: now)
                == "quiet-amber-otter, working, Fix the login race, $ swift test, software-eng-mac, "
                + "started 2 hours 5 minutes ago, memory 1G of 4G")
        let away = rowAccessibilityLabel(try agent("b", host: "vps", state: .idle), now: now, unavailable: true)
        #expect(away.hasPrefix("b, unavailable, vps can't be reached"))
    }

    @Test func durationsSpelledOut() {
        #expect(spokenElapsed(1) == "1 second")
        #expect(spokenElapsed(45) == "45 seconds")
        #expect(spokenElapsed(60) == "1 minute")
        #expect(spokenElapsed(3600) == "1 hour")
        #expect(spokenElapsed(2 * 3600 + 5 * 60) == "2 hours 5 minutes")
        #expect(spokenElapsed(86400 + 3600) == "1 day 1 hour")
        #expect(spokenElapsed(-3) == "0 seconds")
    }

    @Test func theHeaderCounts() throws {
        #expect(agentSummary([]) == "No agents")
        #expect(agentSummary([try agent("a", state: .busy)]) == "1 agent · 1 awake")
        #expect(
            agentSummary([try agent("a", state: .busy), try agent("b", state: .asleep), try agent("c", state: .paused)])
                == "3 agents · 2 awake")
    }

    @Test func vibedsClausesBecomeSentences() {
        #expect(sentence("can't reach vibed") == "Can't reach vibed")
        #expect(sentence("Already one") == "Already one")
        #expect(sentence("") == "")
        // A command's name stays as it's spelt.
        #expect(sentence("ssh: connect to host box port 22: timed out") == "ssh: connect to host box port 22: timed out")
        #expect(sentence("gh auth status failed") == "gh auth status failed")
        #expect(sentence("vibed: port 7878 is taken") == "vibed: port 7878 is taken")
    }
}

@Suite struct SetupTests {
    @Test func aFreshMacHasThreeStepsToGo() {
        let fresh = SetupProgress(hasProject: false, hasKey: false, linuxClaude: false)
        #expect(fresh.steps.map(\.id) == [.project, .claude, .key])
        #expect(fresh.done == 0)
        #expect(!fresh.complete)
        let halfway = SetupProgress(hasProject: true, hasKey: false, linuxClaude: true)
        #expect(halfway.steps.map(\.done) == [true, true, false])
        #expect(halfway.done == 2)
        #expect(SetupProgress(hasProject: true, hasKey: true, linuxClaude: true).complete)
    }

    @Test func withoutLinuxAgentsThereIsNoClaudeStep() {
        let setup = SetupProgress(hasProject: true, hasKey: true, linuxClaude: nil)
        #expect(setup.steps.map(\.id) == [.project, .key])
        #expect(setup.complete)
    }
}

@Suite @MainActor struct FixtureTests {
    @Test func theBusyMenuHasEveryState() async throws {
        let model = await MenuModel.fixture(.busy)
        #expect(model.connection == .connected)
        #expect(model.hosts.map(\.name) == ["local", "vps", "box"])
        #expect(model.unavailableHosts == ["box"])
        #expect(Set(model.agents.map(\.state)) == [.busy, .question, .paused, .idle, .asleep])
        #expect(model.groups.map(\.host) == ["local", "vps", "box"])
        #expect(model.icon.question && model.icon.paused)
        #expect(model.externalHost?.name == "vps")
        #expect(model.gh?.account == "octocat")
        #expect(model.mcp?.tailscaleUrls.isEmpty == false)
        // Rows read sensibly: the usage-limited one says until when.
        let limited = try #require(model.agent(host: "local", ref: "gentle-teal-finch"))
        #expect(rowLines(limited).line?.hasPrefix("limited until") == true)
    }

    @Test func theOtherScenarios() async throws {
        let quiet = await MenuModel.fixture(.quiet)
        #expect(quiet.connection == .connected)
        #expect(quiet.agents.isEmpty)
        let firstRun = await MenuModel.fixture(.firstRun)
        #expect(firstRun.agents.isEmpty)
        let offline = await MenuModel.fixture(.offline)
        guard case .unavailable(let why) = offline.connection else {
            Issue.record("offline is \(offline.connection)")
            return
        }
        #expect(why.contains("can't reach vibed"))
    }

    /// Every call a surface makes is answered, and decodes, so the gallery
    /// draws each one with data.
    @Test func everyCallTheSurfacesMakeDecodes() throws {
        for scenario in [FixtureDaemon.Scenario.busy, .quiet, .firstRun] {
            let d = FixtureDaemon(scenario)
            for host in ["local", "vps"] {
                _ = try d.call(ProjectsList.self, HostParams(host: host))
                _ = try d.call(LimitsGet.self, HostParams(host: host))
            }
            _ = try d.call(KeysList.self, NoParams())
            _ = try d.call(McpGet.self, HostParams())
            _ = try d.call(ToolboxStatusGet.self, NoParams())
            _ = try d.call(ToolboxLoginsGet.self, NoParams())
            _ = try d.call(GhStatusGet.self, GhStatusParams())
            _ = try d.call(SessionsList.self, HostParams())
            #expect(try d.call(KeysCreate.self, KeyCreateParams(name: "laptop")).key.name == "laptop")
            #expect(try d.call(HostsAdd.self, HostAddParams(name: "vps", address: "me@vps.example.com")).host.name == "vps")
        }
        let busy = FixtureDaemon(.busy)
        #expect(try busy.call(ProjectsList.self, HostParams()).projects.map(\.name) == ["app", "web"])
        #expect(try busy.call(ProjectsList.self, HostParams(host: "vps")).projects.map(\.name) == ["api"])
        #expect(try FixtureDaemon(.firstRun).call(ProjectsList.self, HostParams()).projects.isEmpty)
        #expect(throws: VibedClientError.self) { try FixtureDaemon(.offline).call(KeysList.self, NoParams()) }
    }

    @Test func aFirstRunHasEverythingStillToDo() throws {
        let d = FixtureDaemon(.firstRun)
        let setup = SetupProgress(
            hasProject: !(try d.call(ProjectsList.self, HostParams()).projects.isEmpty),
            hasKey: !(try d.call(KeysList.self, NoParams()).keys.isEmpty),
            linuxClaude: try d.call(ToolboxLoginsGet.self, NoParams()).claude)
        #expect(setup.done == 0)
        let quiet = FixtureDaemon(.quiet)
        #expect(
            SetupProgress(
                hasProject: !(try quiet.call(ProjectsList.self, HostParams()).projects.isEmpty),
                hasKey: !(try quiet.call(KeysList.self, NoParams()).keys.isEmpty),
                linuxClaude: try quiet.call(ToolboxLoginsGet.self, NoParams()).claude
            ).complete)
    }
}

@Suite @MainActor struct RoundOneTests {
    /// The glyph drawn at 2x, as in GlyphTests, for a menu bar's look.
    func render(_ state: GlyphState, onDark: Bool) throws -> NSBitmapImageRep {
        let rep = try #require(
            NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: 32, pixelsHigh: 32, bitsPerSample: 8, samplesPerPixel: 4,
                hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0))
        rep.size = MenuBarGlyph.size
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        MenuBarGlyph.image(state, onDark: onDark).draw(in: NSRect(origin: .zero, size: MenuBarGlyph.size))
        NSGraphicsContext.restoreGraphicsState()
        return rep
    }

    /// The colour at the centre of the top left window.
    func topLeft(_ rep: NSBitmapImageRep) throws -> NSColor {
        let r = MenuBarGlyph.rect(column: 0, row: 0)
        return try #require(rep.colorAt(x: Int(r.midX * 2), y: Int(r.midY * 2))?.usingColorSpace(.sRGB))
    }

    @Test func aBadgedGlyphsWindowsFollowTheMenuBar() throws {
        let badged = GlyphState(lit: 3, question: true, paused: true)
        // White on a dark menu bar: black there was invisible.
        let dark = try topLeft(try render(badged, onDark: true))
        #expect(dark.alphaComponent > 0.9)
        #expect(dark.redComponent > 0.9 && dark.greenComponent > 0.9 && dark.blueComponent > 0.9)
        let light = try topLeft(try render(badged, onDark: false))
        #expect(light.alphaComponent > 0.9)
        #expect(light.redComponent < 0.1 && light.greenComponent < 0.1 && light.blueComponent < 0.1)
        // The dots keep their colours either way.
        let (qx, qy) = (MenuBarGlyph.badgeRect(question: true).midX, MenuBarGlyph.badgeRect(question: true).midY)
        let dot = try #require(try render(badged, onDark: true).colorAt(x: Int(qx * 2), y: Int(qy * 2))?.usingColorSpace(.sRGB))
        #expect(dot.redComponent > 0.8 && dot.blueComponent < 0.3)
        // A template is tinted by the menu bar, so its look doesn't matter.
        #expect(MenuBarGlyph.image(GlyphState(lit: 2), onDark: true).isTemplate)
        #expect(MenuBarGlyph.ink(GlyphState(lit: 2), onDark: true) == .black)
    }

    @Test func theHeaderIsOneQuietLineWithColourOnlyForTrouble() throws {
        let hosts = try [
            decode(HostInfo.self, hostJSON("local", mcp: true)),
            decode(HostInfo.self, hostJSON("vps", link: .connected, external: true, tunnel: .up)),
            decode(HostInfo.self, hostJSON("box", link: .down, detail: "ssh: timed out")),
        ]
        let gh = try decode(GhStatus.self, ["state": "logged_in", "account": "octocat"])
        let parts = headerStatus(memory: HostMemory(used: 3 << 30, total: 32 << 30), gh: gh, mcp: true, hosts: hosts)
        #expect(parts.map(\.text) == ["3G of 32G", "gh octocat", "MCP on", "vps public", "box down"])
        #expect(parts.map(\.tone) == [.normal, .normal, .normal, .normal, .trouble])
        #expect(parts.last?.spoken == "box, link down")
        // Trouble shows in colour, and only trouble.
        let out = try decode(GhStatus.self, ["state": "logged_out"])
        let unreachable = try decode(HostInfo.self, hostJSON("vps", link: .connected, external: true, tunnel: .down))
        let worse = headerStatus(memory: nil, gh: out, mcp: false, hosts: [unreachable])
        #expect(worse.map(\.text) == ["gh logged out", "MCP off", "vps not reachable"])
        #expect(worse.map(\.tone) == [.attention, .normal, .attention])
        // Before gh has been checked, it says nothing.
        #expect(headerStatus(memory: nil, gh: nil, mcp: true, hosts: []).map(\.text) == ["MCP on"])
    }

    @Test func valuesHaveOneVoice() throws {
        #expect(AgentEnvironment.allCases.map(\.title) == ["Linux", "macOS"])
        #expect(ClaudeModel.allCases.map(\.title) == ["Opus", "Sonnet"])
        #expect(ReasoningLevel.allCases.map(\.title) == ["Low", "Medium", "High", "Extra High", "Max"])
        #expect(PermissionMode.allCases.map(\.title) == ["Auto", "Bypass"])
        let profile = try decode(
            Profile.self,
            ["name": "reviewer", "environment": "linux", "model": "sonnet", "reasoning": "xhigh", "mode": "bypass",
             "memory": 4 << 30] as [String: Any])
        #expect(profileSummary(profile) == "Linux · Sonnet · Extra High · Bypass · 4G")
        #expect(SetupProgress(hasProject: false, hasKey: false, linuxClaude: false).steps[1].title
            == "Log in to Claude for Linux agents")
    }

    /// An image of `size` pixels, filled with white, with `draw` on top.
    func image(_ size: Int, draw: (CGContext) -> Void = { _ in }) throws -> CGImage {
        let context = try #require(
            CGContext(
                data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        context.setFillColor(CGColor(gray: 1, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: size, height: size))
        draw(context)
        return try #require(context.makeImage())
    }

    @Test func theBlankCheckFailsAFlatImageAndPassesASparseDrawing() throws {
        // A window that drew nothing: one colour.
        let blank = SurfaceInk(try image(400))
        #expect(!blank.drawn)
        #expect(blank.share == 0)
        // An empty state's worth: a small antialiased symbol and a line of
        // text-sized marks on a big plain background. The old grid sample
        // could miss all of it.
        let sparse = SurfaceInk(
            try image(800) { c in
                c.setShouldAntialias(true)
                c.setFillColor(CGColor(gray: 0.45, alpha: 1))
                c.fillEllipse(in: CGRect(x: 380, y: 500, width: 40, height: 40))
                for i in 0..<30 {
                    c.setFillColor(CGColor(gray: 0.1 + Double(i % 7) * 0.08, alpha: 1))
                    c.fill(CGRect(x: 250.5 + Double(i) * 10, y: 420.3, width: 6.4, height: 11.7))
                }
            })
        #expect(sparse.drawn)
        #expect(sparse.share < 0.05)
    }
}
