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
        #expect(fresh.steps.map(\.id) == [.claude, .project, .key])
        #expect(fresh.done == 0)
        #expect(!fresh.complete)
        #expect(fresh.lit == 0)
        let halfway = SetupProgress(hasProject: true, hasKey: false, linuxClaude: true)
        #expect(halfway.steps.map(\.done) == [true, true, false])
        #expect(halfway.done == 2)
        #expect(halfway.lit == 2)
        let all = SetupProgress(hasProject: true, hasKey: true, linuxClaude: true)
        #expect(all.complete)
        #expect(all.lit == 4)
    }

    @Test func withoutLinuxAgentsThereIsNoClaudeStep() {
        let setup = SetupProgress(hasProject: true, hasKey: true, linuxClaude: nil)
        #expect(setup.steps.map(\.id) == [.project, .key])
        #expect(setup.complete)
    }

    static func check(_ id: CheckId, _ state: CheckState, fixable: Bool = false, progress: Int? = nil) -> DoctorCheck {
        DoctorCheck(
            id: id, state: state, detail: "\(id.rawValue) found.", hint: state == .pass ? nil : "Do \(id.rawValue).",
            fixable: fixable, progress: progress)
    }

    /// The five-minute Mac (PRD: Goals, Short setup): gh and claude logged
    /// in, the app just installed from the disk image.
    static let fiveMinute = SetupProgress(
        hasProject: false, hasKey: false, linuxClaude: false, linuxGitHub: false,
        doctor: [
            check(.git, .pass), check(.gh, .pass), check(.ghGit, .fail, fixable: true), check(.claude, .pass),
            check(.claudeLogin, .pass), check(.power, .fail), check(.tailscale, .optional),
            check(.linux, .working, progress: 40),
        ],
        install: .justInstalled)

    /// The first launch and the first run in one list, in order: the vibe
    /// command, the doctor's checks with Terminal among them, then the
    /// Linux download, Claude and GitHub for linux agents, a project, a key.
    @Test func doctorChecksAreStepsInTheSameList() {
        let setup = Self.fiveMinute
        #expect(
            setup.steps.map(\.id) == [
                .command, .check(.git), .check(.gh), .check(.ghGit), .check(.claude), .check(.claudeLogin),
                .terminal, .check(.power), .check(.tailscale), .check(.linux), .claude, .toolbox, .project, .key,
            ])
        #expect(!setup.complete)
        // What passed on this Mac folds into one row at the top; what's
        // left is a row each.
        guard case .passed(let passed) = setup.rows.first else {
            Issue.record("no passed row: \(setup.rows)")
            return
        }
        #expect(passed.map(\.id) == [.check(.git), .check(.gh), .check(.claude), .check(.claudeLogin)])
        #expect(setup.passedTitle == "4 checks passed")
        #expect(setup.rows.count == 1 + setup.steps.count - passed.count)
        // The next step is the first with something to do.
        #expect(setup.next == .command)
        let step = { (id: SetupProgress.Step.ID) in setup.steps.first { $0.id == id } }
        #expect(step(.check(.ghGit))?.action == .fix(.ghGit))
        #expect(step(.check(.power))?.action == .terminal("sudo pmset -c sleep 0"))
        #expect(step(.check(.tailscale))?.status == .optional)
        #expect(step(.check(.tailscale))?.needed == false)
        #expect(step(.check(.linux))?.status == .working(40))
        #expect(step(.check(.linux))?.progress == 40)
        #expect(step(.check(.linux))?.action == nil)
        #expect(step(.terminal)?.action == .allowTerminal)
        #expect(step(.claude)?.action == .vibe(["setup-token"]))
        // The toolbox waits on the download.
        #expect(step(.toolbox)?.action == nil)
        // A step that isn't done says, in a line of its own, why it matters;
        // vibe doctor's hints are for Terminal.
        #expect(
            step(.check(.power))?.detail
                == "Agents can't answer while this Mac sleeps. Turning it off asks for your password.")
        #expect(step(.check(.power))?.title == "Keep this Mac awake")
        #expect(step(.check(.linux))?.detail == "linux found.")
        #expect(setup.steps.allSatisfy { !$0.detail.contains("`") })
        // Without Tailscale, there's nothing to turn on for other machines.
        #expect(step(.remote) == nil)
    }

    @Test func eachCheckHasItsButton() {
        let fresh = SetupProgress(
            hasProject: true, hasKey: true, linuxClaude: true, linuxGitHub: true,
            doctor: [
                check(.git, .fail), check(.gh, .fail), check(.ghGit, .fail), check(.claude, .fail),
                check(.claudeLogin, .fail), check(.linux, .fail, fixable: true),
            ],
            ghInstalled: false)
        let actions = Dictionary(uniqueKeysWithValues: fresh.steps.map { ($0.id, $0.action) })
        #expect(actions[.check(.git)] == .installCommandLineTools)
        #expect(actions[.check(.gh)] == .open("https://cli.github.com"))
        // git can't use gh before gh is logged in, nor can claude sign in
        // before it's installed: they wait.
        #expect(actions[.check(.ghGit)] == .some(nil))
        #expect(actions[.check(.claude)] == .open("https://claude.com/claude-code"))
        #expect(actions[.check(.claudeLogin)] == .some(nil))
        #expect(actions[.check(.linux)] == .fix(.linux))
        // Not installed (a development build): no command or Terminal steps.
        #expect(!fresh.steps.contains { $0.id == .command || $0.id == .terminal })
        // With gh there, its button logs in, in Terminal.
        var withGh = fresh
        withGh.ghInstalled = true
        #expect(withGh.steps.first { $0.id == .check(.gh) }?.action == .terminal("gh auth login"))
    }

    @Test func setupIsCompleteOnceNothingNeedsTheOwner() {
        var setup = Self.fiveMinute
        setup.hasProject = true
        setup.hasKey = true
        setup.linuxClaude = true
        setup.linuxGitHub = true
        setup.install.command = .linked
        setup.install.terminal = .denied
        setup.doctor = setup.doctor.map { c in
            var c = c
            if c.state != .optional { c.state = .pass }
            c.progress = nil
            return c
        }
        // Tailscale is optional, and Terminal turned off falls back to a
        // file Terminal opens: neither holds up the welcome.
        #expect(setup.steps.first { $0.id == .terminal }?.status == .optional)
        #expect(setup.complete)
        #expect(setup.passedTitle == "This Mac is ready")
        #expect(setup.lit == 4)
    }

    @Test func withTailscaleUpMCPCanBeServedThere() {
        var setup = Self.fiveMinute
        setup.remoteMCP = false
        let ids = setup.steps.map(\.id)
        // Before the key, so the key's setup line has the Tailscale address.
        #expect(ids.suffix(3) == [.project, .remote, .key])
        let remote = setup.steps.first { $0.id == .remote }
        #expect(remote?.action == .serveOnTailscale)
        #expect(remote?.needed == true)
        setup.remoteMCP = true
        #expect(setup.steps.first { $0.id == .remote }?.done == true)
    }

    @Test func theCommandStepSaysWhatsThere() {
        var setup = Self.fiveMinute
        setup.install.command = .other("/opt/homebrew/bin/vibe")
        let step = setup.steps.first { $0.id == .command }
        #expect(step?.title == "Point vibe at this VibeOS")
        #expect(step?.detail.contains("/opt/homebrew/bin/vibe") == true)
        setup.install.command = .linked
        #expect(setup.steps.first { $0.id == .command }?.done == true)
    }

    @Test func everyStepSpeaksItsState() {
        for step in Self.fiveMinute.steps {
            #expect(!step.statusWords.isEmpty)
            #expect(step.detail.hasSuffix("."), "\(step.id): \(step.detail)")
            if step.action != nil { #expect(step.actionTitle?.isEmpty == false, "\(step.id)") }
        }
        #expect(SetupProgress.Step(id: .key, title: "k", detail: "d.", status: .working(7)).statusWords == "under way, 7 percent")
    }
}

@Suite @MainActor struct InstallTests {
    @Test func whereTheAppRunsFrom() {
        #expect(AppLocation.of(bundlePath: "/Applications/VibeOS.app", home: "/Users/me") == .applications)
        #expect(AppLocation.of(bundlePath: "/Users/me/Applications/VibeOS.app", home: "/Users/me") == .applications)
        #expect(AppLocation.of(bundlePath: "/Volumes/VibeOS/VibeOS.app", home: "/Users/me") == .notMoved)
        #expect(
            AppLocation.of(
                bundlePath: "/private/var/folders/x/T/AppTranslocation/0A1B/d/VibeOS.app", home: "/Users/me")
                == .notMoved)
        #expect(AppLocation.of(bundlePath: "/Users/me/src/vibe-os-native/build/VibeOS.app", home: "/Users/me") == .development)
        #expect(AppLocation.of(bundlePath: "/Applications/Tools/VibeOS.app", home: "/Users/me") == .development)
    }

    @Test func theCommandLink() throws {
        let fm = FileManager.default
        let dir = fm.temporaryDirectory.appendingPathComponent("vibe-link-\(UUID().uuidString)")
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let ours = dir.appendingPathComponent("VibeOS.app-vibe").path
        let other = dir.appendingPathComponent("other-vibe").path
        fm.createFile(atPath: ours, contents: Data())
        fm.createFile(atPath: other, contents: Data())
        let link = dir.appendingPathComponent("vibe").path
        #expect(commandLink(at: link, ours: ours) == .missing)
        try fm.createSymbolicLink(atPath: link, withDestinationPath: ours)
        #expect(commandLink(at: link, ours: ours) == .linked)
        try fm.removeItem(atPath: link)
        try fm.createSymbolicLink(atPath: link, withDestinationPath: other)
        #expect(commandLink(at: link, ours: ours) == .other((other as NSString).resolvingSymlinksInPath))
        // A link to a copy since moved counts as missing.
        try fm.removeItem(atPath: link)
        try fm.createSymbolicLink(atPath: link, withDestinationPath: dir.appendingPathComponent("gone").path)
        #expect(commandLink(at: link, ours: ours) == .missing)
    }

    /// The first launch from Applications: vibed's LaunchAgent registered,
    /// vibe linked where no password is needed, and nothing at all done by
    /// a copy that isn't installed.
    @Test func theFirstLaunchRegistersVibedAndLinksVibe() async {
        let fresh = FixedInstaller(
            InstallStatus(location: .applications, service: .notRegistered, command: .missing, terminal: .notAsked))
        fresh.linkNeedsPassword = false
        let model = MenuModel(daemon: FixtureDaemon(.firstRun), terminal: FixtureTerminal(), installer: fresh)
        await model.firstLaunch()
        #expect(fresh.calls == ["register", "link"])
        #expect(model.install.service == .enabled)
        #expect(model.install.command == .linked)

        // /usr/local/bin needs a password: the welcome's step asks for it.
        let rooted = FixedInstaller(.justInstalled)
        let model2 = MenuModel(daemon: FixtureDaemon(.firstRun), terminal: FixtureTerminal(), installer: rooted)
        await model2.firstLaunch()
        #expect(rooted.calls == ["link"])
        #expect(model2.install.command == .missing)
        await model2.linkCommand()
        #expect(rooted.calls == ["link", "link, asking"])
        #expect(model2.install.command == .linked)
        await model2.allowTerminal()
        #expect(model2.install.terminal == .granted)

        let dev = FixedInstaller()
        let model3 = MenuModel(daemon: FixtureDaemon(.firstRun), terminal: FixtureTerminal(), installer: dev)
        await model3.firstLaunch()
        #expect(dev.calls.isEmpty)
        #expect(model3.install == .notManaged)
    }

    @Test func aNoteForEachWayVibedIsntThere() async throws {
        let fromDisk = await MenuModel.fixture(.offline, install: .fromDiskImage)
        #expect(fromDisk.install.location == .notMoved)
        #expect(InstallNote(fromDisk.install)?.action == .reveal)
        let off = await MenuModel.fixture(.offline, install: .turnedOff)
        #expect(off.install.service == .requiresApproval)
        let note = InstallNote(off.install)
        #expect(note?.action == .loginItems)
        #expect(note?.title == "Let VibeOS run in the background")
        let installer = off.installer as? FixedInstaller
        await off.act(on: try #require(note))
        #expect(installer?.calls.last == "login items")
        let failed = InstallStatus(
            location: .applications, service: .failed("operation not permitted"), command: .linked, terminal: .granted)
        #expect(failed.registerFailure == "operation not permitted")
        #expect(InstallNote(failed)?.action == .register)
        #expect(InstallNote(failed)?.detail == "Operation not permitted")
        // Installed and running: no note; nor for a development build.
        #expect(InstallNote(.justInstalled) == nil)
        #expect(InstallNote(.notManaged) == nil)
    }

    /// A register that failed stays failed in the menu, though SMAppService
    /// says only "not registered", until a Try Again works.
    @Test func aFailedRegisterIsKept() async {
        final class Refusing: FixedInstaller {
            override func registerService() -> ServiceStatus { .failed("Operation not permitted") }
        }
        let refusing = Refusing(
            InstallStatus(location: .applications, service: .notRegistered, command: .linked, terminal: .granted))
        let model = MenuModel(daemon: FixtureDaemon(.offline), terminal: FixtureTerminal(), installer: refusing)
        await model.firstLaunch()
        #expect(model.install.registerFailure == "Operation not permitted")
    }

    /// After an update, an older vibed is restarted from the new app, once;
    /// a newer one, or the same, is left be.
    @Test func anOlderVibedIsRestartedOnce() async {
        let installer = FixedInstaller(.justInstalled)
        let model = MenuModel(daemon: FixtureDaemon(.quiet), terminal: FixtureTerminal(), installer: installer)
        // Read vibed first, while the install isn't known yet, so this
        // test's calls are the only ones that can restart it.
        await model.refresh()
        await model.refreshInstall()
        let before = installer.calls
        // The fixture's vibed is 0.1.0.
        model.takeOverIfUpdated(appVersion: "0.1.0")
        model.takeOverIfUpdated(appVersion: "0.0.9")
        #expect(installer.calls == before)
        model.takeOverIfUpdated(appVersion: "0.2.0")
        model.takeOverIfUpdated(appVersion: "0.3.0")
        #expect(installer.calls == before + ["restart"])
        #expect(versionIsOlder("0.9.0", than: "0.10.0"))
        #expect(!versionIsOlder("1.0", than: "1.0.0"))
        #expect(!versionIsOlder("2.0.0", than: "1.9.9"))
    }

    /// Permission to control Terminal, once given, isn't asked for again;
    /// once refused, the button opens the settings it's turned on in.
    @Test func terminalPermission() async {
        let denied = FixedInstaller(
            InstallStatus(location: .applications, service: .enabled, command: .linked, terminal: .denied))
        let model = MenuModel(daemon: FixtureDaemon(.quiet), terminal: FixtureTerminal(), installer: denied)
        await model.refreshInstall()
        await model.allowTerminal()
        #expect(denied.calls == ["automation settings"])
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
        for scenario in [FixtureDaemon.Scenario.busy, .quiet, .firstRun, .freshMac] {
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
            #expect(try d.call(DoctorCheckGet.self, NoParams()).checks.count == CheckId.allCases.count)
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
        // With the doctor's checks, it's the five-minute Mac: some passed,
        // some to do, Linux downloading.
        let doctor = try d.call(DoctorCheckGet.self, NoParams()).checks
        #expect(doctor.map(\.id) == CheckId.allCases)
        #expect(doctor.last?.state == .working && doctor.last?.progress == 40)
        let fresh = try FixtureDaemon(.freshMac).call(DoctorCheckGet.self, NoParams()).checks
        #expect(fresh.filter { $0.state == .fail }.count == 6)
        #expect(try FixtureDaemon(.freshMac).call(GhStatusGet.self, GhStatusParams()).state == .notInstalled)
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

    @Test func theHeaderShowsOnlyWhatNeedsTheOwner() throws {
        let hosts = try [
            decode(HostInfo.self, hostJSON("local", mcp: true)),
            decode(HostInfo.self, hostJSON("vps", link: .connected, external: true, tunnel: .up)),
            decode(HostInfo.self, hostJSON("box", link: .down, detail: "ssh: timed out")),
        ]
        let gh = try decode(GhStatus.self, ["state": "logged_in", "account": "octocat"])
        // All well but box: one row, for box, with the reason kept for a
        // tooltip, as spelt.
        let alerts = headerAlerts(gh: gh, hosts: hosts)
        #expect(alerts.map(\.title) == ["box is down"])
        #expect(alerts.first?.tone == .trouble)
        #expect(alerts.first?.reason == "ssh: timed out")
        #expect(alerts.first?.detail == "Its agents keep running. VibeOS reconnects on its own.")
        // A gh login missing and an external address outside can't reach.
        let out = try decode(GhStatus.self, ["state": "logged_out"])
        let unreachable = try decode(HostInfo.self, hostJSON("vps", link: .connected, external: true, tunnel: .down))
        let worse = headerAlerts(gh: out, hosts: [unreachable])
        #expect(worse.map(\.title) == ["vps isn't reachable from outside", "GitHub CLI is logged out"])
        #expect(worse.map(\.tone) == [.attention, .attention])
        // Nothing wrong, nothing shown.
        #expect(headerAlerts(gh: gh, hosts: Array(hosts.prefix(2))).isEmpty)
        #expect(headerAlerts(gh: nil, hosts: []).isEmpty)
    }

    @Test func theSteadyFactsAreAllThereOnRequest() throws {
        let hosts = try [
            decode(HostInfo.self, hostJSON("local", mcp: true)),
            decode(HostInfo.self, hostJSON("vps", link: .connected, external: true, tunnel: .up)),
            decode(HostInfo.self, hostJSON("box", link: .down)),
        ]
        let gh = try decode(GhStatus.self, ["state": "logged_in", "account": "octocat"])
        let facts = headerFacts(memory: HostMemory(used: 3 << 30, total: 32 << 30), gh: gh, mcp: true, hosts: hosts)
        #expect(facts.map(\.label) == ["Memory", "GitHub", "MCP", "vps", "box"])
        #expect(facts.map(\.value) == ["3G of 32G", "octocat", "On", "Connected, public", "Down"])
        #expect(facts.map(\.tone) == [.normal, .normal, .normal, .normal, .trouble])
        // A host may be named anything, "Memory" too: ids never collide.
        #expect(facts.map(\.id) == ["memory", "gh", "mcp", "host:vps", "host:box"])
        let named = try decode(HostInfo.self, hostJSON("Memory", link: .connected))
        let ids = headerFacts(memory: HostMemory(used: 1, total: 2), gh: nil, mcp: true, hosts: [named]).map(\.id)
        #expect(Set(ids).count == ids.count)
        #expect(headerFacts(memory: nil, gh: nil, mcp: false, hosts: []).map(\.value) == ["Checking…", "Off"])
    }

    @Test func memorySpeaksUpOnlyNearItsLimit() throws {
        // A quarter: the quiet bar alone.
        #expect(memoryNote(try agent("a", memoryUsed: 1 << 30)) == nil)
        #expect(memoryNote(try agent("a")) == nil)
        // Past 80%: how near, in amber.
        let near = try #require(memoryNote(try agent("a", memoryUsed: 3_543_348_019)))
        #expect(near.tone == .attention)
        #expect(near.text == "3.3G of 4G")
        // At it: a state, in amber words.
        #expect(memoryNote(try agent("a", state: .busy, memoryUsed: 4 << 30)) == MemoryNote(text: "At its memory limit", tone: .attention))
        // Paused there, it's said once, in the status line; the bar only
        // turns red.
        let paused = try agent("a", state: .paused, memoryUsed: 4 << 30)
        #expect(memoryNote(paused) == MemoryNote(text: nil, tone: .trouble))
        #expect(rowLines(paused).line == "Paused at its 4G memory limit")
        // Paused below it (processes, disk), vibed's own line stands.
        var processes = try agent("b", state: .paused, memoryUsed: 1 << 30)
        processes.status.line = "Paused: past max_processes"
        #expect(rowLines(processes).line == "Paused: past max_processes")
        #expect(memoryNote(processes) == nil)
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
