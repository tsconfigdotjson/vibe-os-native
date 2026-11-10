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
        #expect(MenuBarGlyph.image(GlyphState(lit: 0, connected: false)).accessibilityDescription == "VibeOS, can't reach its background service")
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
        // A long silent tool run agent-tail guesses is a prompt, before vibed
        // confirms it: still at work, no hand.
        #expect(activitySymbol(try agent("a", state: .busy, activity: .awaitingApproval)) == "terminal")
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

// The tests below compare values of one explicit type on both sides, and
// keep closures and optional chains out of #expect: the macro type-checks
// each operand on its own, where an implicit member (`.fix(.git)`) seen
// through an optional has no type to come from.
typealias StepID = SetupProgress.Step.ID
typealias StepStatus = SetupProgress.Step.Status

@Suite struct SetupTests {
    /// `setup`'s step `id`, if it has one.
    static func step(_ setup: SetupProgress, _ id: StepID) -> SetupProgress.Step? {
        setup.steps.first(where: { (s: SetupProgress.Step) -> Bool in s.id == id })
    }

    /// What `setup`'s step `id` does; none if it does nothing, or isn't there.
    static func action(_ setup: SetupProgress, _ id: StepID) -> SetupAction? {
        step(setup, id)?.action
    }

    static func status(_ setup: SetupProgress, _ id: StepID) -> StepStatus? {
        step(setup, id)?.status
    }

    static func ids(_ setup: SetupProgress) -> [StepID] {
        setup.steps.map { (s: SetupProgress.Step) -> StepID in s.id }
    }

    @Test func aFreshMacHasThreeStepsToGo() {
        let fresh = SetupProgress(hasProject: false, hasKey: false, linuxGitHub: false)
        let freshIDs: [StepID] = [StepID.toolbox, StepID.project, StepID.key]
        #expect(Self.ids(fresh) == freshIDs)
        #expect(fresh.done == 0)
        #expect(!fresh.complete)
        #expect(fresh.lit == 0)
        let halfway = SetupProgress(hasProject: true, hasKey: false, linuxGitHub: true)
        let halfwayDone: [Bool] = halfway.steps.map { (s: SetupProgress.Step) -> Bool in s.done }
        #expect(halfwayDone == [true, true, false])
        #expect(halfway.done == 2)
        #expect(halfway.lit == 2)
        let all = SetupProgress(hasProject: true, hasKey: true, linuxGitHub: true)
        #expect(all.complete)
        #expect(all.lit == 4)
    }

    /// Agents' own sign-in is theirs to do: setup never walks through it
    /// (#46), and without a Linux toolbox there's no toolbox step either.
    /// A page is only a host's word, so only the provider's own opens with no click.
    @Test func onlyTheProvidersOwnPageCountsAsIt() {
        #expect(SignInWhat.github.isOwnPage("https://github.com/login/device"))
        #expect(SignInWhat.toolboxGithub.isOwnPage("https://GitHub.com/login/device"))
        #expect(SignInWhat.claude.isOwnPage("https://claude.com/cai/oauth/authorize?code=true"))
        #expect(SignInWhat.toolboxClaude.isOwnPage("https://claude.ai/oauth/authorize"))
        #expect(!SignInWhat.github.isOwnPage("https://github.com.evil.example/login/device"))
        #expect(!SignInWhat.github.isOwnPage("https://github.com@evil.example/login/device"))
        #expect(!SignInWhat.github.isOwnPage("https://evil.example/github.com/"))
        #expect(!SignInWhat.github.isOwnPage("http://github.com/login/device"))
        #expect(!SignInWhat.github.isOwnPage("https://github.com:8443/login/device"))
        #expect(!SignInWhat.github.isOwnPage("https://claude.com/"))
        #expect(!SignInWhat.claude.isOwnPage("https://github.com/"))
    }

    @Test func setupHasNoAgentSignInStep() {
        let setup = SetupProgress(hasProject: true, hasKey: true, linuxGitHub: nil)
        let want: [StepID] = [StepID.project, StepID.key]
        #expect(Self.ids(setup) == want)
        #expect(setup.complete)
        let all = Self.fiveMinute.steps.map { (s: SetupProgress.Step) -> String in s.title + " " + s.detail }
        #expect(!all.contains { (t: String) -> Bool in t.contains("setup-token") || t.contains("Log in to Claude") })
    }

    static func check(_ id: CheckId, _ state: CheckState, fixable: Bool = false, progress: Int? = nil) -> DoctorCheck {
        let hint: String? = state == CheckState.pass ? nil : "Do \(id.rawValue)."
        return DoctorCheck(
            id: id, state: state, detail: "\(id.rawValue) found.", hint: hint, fixable: fixable, progress: progress)
    }

    /// The five-minute Mac (PRD: Goals, Short setup): gh and claude logged
    /// in, the app just installed from the disk image.
    static let fiveMinute = SetupProgress(
        hasProject: false, hasKey: false, linuxGitHub: false,
        doctor: [
            check(CheckId.git, CheckState.pass), check(CheckId.gh, CheckState.pass),
            check(CheckId.ghGit, CheckState.fail, fixable: true), check(CheckId.claude, CheckState.pass),
            check(CheckId.claudeLogin, CheckState.pass), check(CheckId.power, CheckState.fail),
            check(CheckId.tailscale, CheckState.optional), check(CheckId.linux, CheckState.working, progress: 40),
            check(CheckId.pi, CheckState.optional),
        ],
        install: InstallStatus.justInstalled)

    /// The first launch and the first run in one list, in order: the vibe
    /// command, the doctor's checks with Terminal among them, then the
    /// Linux download, GitHub for linux agents, a project, a key. The doctor's
    /// Pi check, which `fiveMinute` has as `vibed` sends it, is no step: nothing
    /// needs it until a profile runs Pi.
    @Test func doctorChecksAreStepsInTheSameList() {
        let setup = Self.fiveMinute
        let order: [StepID] = [
            StepID.command, StepID.check(CheckId.git), StepID.check(CheckId.gh), StepID.check(CheckId.ghGit),
            StepID.check(CheckId.claude), StepID.check(CheckId.claudeLogin), StepID.terminal,
            StepID.check(CheckId.power), StepID.check(CheckId.tailscale), StepID.check(CheckId.linux),
            StepID.toolbox, StepID.project, StepID.key,
        ]
        #expect(Self.ids(setup) == order)
        #expect(!setup.complete)
        // What passed on this Mac folds into one row at the top; what's
        // left is a row each.
        guard let first = setup.rows.first, case SetupProgress.Row.passed(let passed) = first else {
            Issue.record("no passed row first: \(setup.rows)")
            return
        }
        let passedIDs: [StepID] = passed.map { (s: SetupProgress.Step) -> StepID in s.id }
        let wantPassed: [StepID] = [
            StepID.check(CheckId.git), StepID.check(CheckId.gh), StepID.check(CheckId.claude),
            StepID.check(CheckId.claudeLogin),
        ]
        #expect(passedIDs == wantPassed)
        #expect(setup.passedTitle == "4 checks passed")
        let rowCount = setup.rows.count
        let wantRows = 1 + setup.steps.count - passed.count
        #expect(rowCount == wantRows)
        // The next step is the first with something to do.
        let next: StepID? = setup.next
        let wantNext: StepID? = StepID.command
        #expect(next == wantNext)

        let ghGit: SetupAction? = Self.action(setup, StepID.check(CheckId.ghGit))
        let wantGhGit: SetupAction? = SetupAction.fix(CheckId.ghGit)
        #expect(ghGit == wantGhGit)
        let power: SetupAction? = Self.action(setup, StepID.check(CheckId.power))
        let wantPower: SetupAction? = SetupAction.terminal("sudo pmset -c sleep 0")
        #expect(power == wantPower)
        let tailscale: StepStatus? = Self.status(setup, StepID.check(CheckId.tailscale))
        let wantTailscale: StepStatus? = StepStatus.optional
        #expect(tailscale == wantTailscale)
        let tailscaleNeeded: Bool? = Self.step(setup, StepID.check(CheckId.tailscale))?.needed
        #expect(tailscaleNeeded == false)
        let linux: StepStatus? = Self.status(setup, StepID.check(CheckId.linux))
        let wantLinux: StepStatus? = StepStatus.working(40)
        #expect(linux == wantLinux)
        let linuxProgress: Int? = Self.step(setup, StepID.check(CheckId.linux))?.progress
        #expect(linuxProgress == 40)
        let linuxAction: SetupAction? = Self.action(setup, StepID.check(CheckId.linux))
        #expect(linuxAction == nil)
        let terminal: SetupAction? = Self.action(setup, StepID.terminal)
        let wantTerminal: SetupAction? = SetupAction.allowTerminal
        #expect(terminal == wantTerminal)
        // The toolbox waits on the download.
        let toolboxStep: SetupProgress.Step? = Self.step(setup, StepID.toolbox)
        #expect(toolboxStep != nil)
        let toolbox: SetupAction? = Self.action(setup, StepID.toolbox)
        #expect(toolbox == nil)
        // A step that isn't done says, in a line of its own, why it matters;
        // vibe doctor's hints are for Terminal.
        let powerDetail: String? = Self.step(setup, StepID.check(CheckId.power))?.detail
        #expect(powerDetail == "Agents can't answer while this Mac sleeps. Turning it off asks for your password.")
        let powerTitle: String? = Self.step(setup, StepID.check(CheckId.power))?.title
        #expect(powerTitle == "Keep this Mac awake")
        let linuxDetail: String? = Self.step(setup, StepID.check(CheckId.linux))?.detail
        #expect(linuxDetail == "linux found.")
        let withBackticks: [String] = setup.steps.map { (s: SetupProgress.Step) -> String in s.detail }
            .filter { (d: String) -> Bool in d.contains("`") }
        #expect(withBackticks.isEmpty)
        // Without Tailscale, there's nothing to turn on for other machines.
        let remote: SetupProgress.Step? = Self.step(setup, StepID.remote)
        #expect(remote == nil)
    }

    @Test func eachCheckHasItsButton() {
        let fresh = SetupProgress(
            hasProject: true, hasKey: true, linuxGitHub: true,
            doctor: [
                Self.check(CheckId.git, CheckState.fail), Self.check(CheckId.gh, CheckState.fail),
                Self.check(CheckId.ghGit, CheckState.fail), Self.check(CheckId.claude, CheckState.fail),
                Self.check(CheckId.claudeLogin, CheckState.fail),
                Self.check(CheckId.linux, CheckState.fail, fixable: true),
            ],
            ghInstalled: false)
        let git: SetupAction? = Self.action(fresh, StepID.check(CheckId.git))
        let wantGit: SetupAction? = SetupAction.installCommandLineTools
        #expect(git == wantGit)
        let gh: SetupAction? = Self.action(fresh, StepID.check(CheckId.gh))
        // VibeOS is installing it: nothing to press.
        #expect(gh == nil)
        // git can't use gh before gh is logged in, nor can claude sign in
        // before it's installed: those steps are there, and wait.
        let ghGitStep: SetupProgress.Step? = Self.step(fresh, StepID.check(CheckId.ghGit))
        #expect(ghGitStep != nil)
        let ghGit: SetupAction? = Self.action(fresh, StepID.check(CheckId.ghGit))
        #expect(ghGit == nil)
        let claude: SetupAction? = Self.action(fresh, StepID.check(CheckId.claude))
        #expect(claude == nil)
        let loginStep: SetupProgress.Step? = Self.step(fresh, StepID.check(CheckId.claudeLogin))
        #expect(loginStep != nil)
        let login: SetupAction? = Self.action(fresh, StepID.check(CheckId.claudeLogin))
        #expect(login == nil)
        let linux: SetupAction? = Self.action(fresh, StepID.check(CheckId.linux))
        let wantLinux: SetupAction? = SetupAction.fix(CheckId.linux)
        #expect(linux == wantLinux)
        // Not installed (a development build): no command or Terminal steps.
        let ids = Self.ids(fresh)
        #expect(!ids.contains(StepID.command))
        #expect(!ids.contains(StepID.terminal))
        // With gh there, its button signs in, with no terminal.
        var withGh = fresh
        withGh.ghInstalled = true
        let logIn: SetupAction? = Self.action(withGh, StepID.check(CheckId.gh))
        let wantLogIn: SetupAction? = SetupAction.signIn(SignInWhat.github)
        #expect(logIn == wantLogIn)
    }

    @Test func setupIsCompleteOnceNothingNeedsTheOwner() {
        var setup = Self.fiveMinute
        setup.hasProject = true
        setup.hasKey = true
        setup.linuxGitHub = true
        setup.install.command = CommandLink.linked
        setup.install.terminal = TerminalPermission.denied
        setup.doctor = setup.doctor.map { (old: DoctorCheck) -> DoctorCheck in
            var c = old
            if c.state != CheckState.optional { c.state = CheckState.pass }
            c.progress = nil
            return c
        }
        // Tailscale is optional, and Terminal turned off falls back to a
        // file Terminal opens: neither holds up the welcome.
        let terminal: StepStatus? = Self.status(setup, StepID.terminal)
        let wantTerminal: StepStatus? = StepStatus.optional
        #expect(terminal == wantTerminal)
        #expect(setup.complete)
        #expect(setup.passedTitle == "This Mac is ready")
        #expect(setup.lit == 4)
    }

    @Test func withTailscaleUpMCPCanBeServedThere() {
        var setup = Self.fiveMinute
        setup.remoteMCP = false
        // Before the key, so the key's setup line has the Tailscale address.
        let last3: [StepID] = Array(Self.ids(setup).suffix(3))
        let want: [StepID] = [StepID.project, StepID.remote, StepID.key]
        #expect(last3 == want)
        let remote: SetupAction? = Self.action(setup, StepID.remote)
        let wantRemote: SetupAction? = SetupAction.serveOnTailscale
        #expect(remote == wantRemote)
        let needed: Bool? = Self.step(setup, StepID.remote)?.needed
        #expect(needed == true)
        setup.remoteMCP = true
        let done: Bool? = Self.step(setup, StepID.remote)?.done
        #expect(done == true)
    }

    @Test func theCommandStepSaysWhatsThere() {
        var setup = Self.fiveMinute
        setup.install.command = CommandLink.other("/opt/homebrew/bin/vibe")
        let step: SetupProgress.Step? = Self.step(setup, StepID.command)
        let title: String? = step?.title
        #expect(title == "Point vibe at this VibeOS")
        let saysWhere: Bool = step?.detail.contains("/opt/homebrew/bin/vibe") ?? false
        #expect(saysWhere)
        setup.install.command = CommandLink.linked
        let done: Bool? = Self.step(setup, StepID.command)?.done
        #expect(done == true)
    }

    @Test func everyStepSpeaksItsState() {
        for step in Self.fiveMinute.steps {
            let id = "\(step.id)"
            #expect(!step.statusWords.isEmpty, "\(id)")
            #expect(step.detail.hasSuffix("."), "\(id): \(step.detail)")
            if step.action != nil {
                let titled: Bool = !(step.actionTitle ?? "").isEmpty
                #expect(titled, "\(id)")
            }
        }
        let working = SetupProgress.Step(
            id: StepID.key, title: "k", detail: "d.", status: StepStatus.working(7))
        let words: String = working.statusWords
        #expect(words == "under way, 7 percent")
    }
}

@Suite @MainActor struct InstallTests {
    @Test func whereTheAppRunsFrom() {
        let home = "/Users/me"
        let apps: AppLocation = AppLocation.of(bundlePath: "/Applications/VibeOS.app", home: home)
        #expect(apps == AppLocation.applications)
        let mine: AppLocation = AppLocation.of(bundlePath: "/Users/me/Applications/VibeOS.app", home: home)
        #expect(mine == AppLocation.applications)
        let disk: AppLocation = AppLocation.of(bundlePath: "/Volumes/VibeOS/VibeOS.app", home: home)
        #expect(disk == AppLocation.notMoved)
        let moved: AppLocation = AppLocation.of(
            bundlePath: "/private/var/folders/x/T/AppTranslocation/0A1B/d/VibeOS.app", home: home)
        #expect(moved == AppLocation.notMoved)
        let built: AppLocation = AppLocation.of(bundlePath: "/Users/me/src/vibe-os-native/build/VibeOS.app", home: home)
        #expect(built == AppLocation.development)
        let nested: AppLocation = AppLocation.of(bundlePath: "/Applications/Tools/VibeOS.app", home: home)
        #expect(nested == AppLocation.development)
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
        let none: CommandLink = commandLink(at: link, ours: ours)
        #expect(none == CommandLink.missing)
        try fm.createSymbolicLink(atPath: link, withDestinationPath: ours)
        let linked: CommandLink = commandLink(at: link, ours: ours)
        #expect(linked == CommandLink.linked)
        try fm.removeItem(atPath: link)
        try fm.createSymbolicLink(atPath: link, withDestinationPath: other)
        let elsewhere: CommandLink = commandLink(at: link, ours: ours)
        let otherResolved: String = (other as NSString).resolvingSymlinksInPath
        #expect(elsewhere == CommandLink.other(otherResolved))
        // A link to a copy since moved counts as missing.
        try fm.removeItem(atPath: link)
        try fm.createSymbolicLink(atPath: link, withDestinationPath: dir.appendingPathComponent("gone").path)
        let dangling: CommandLink = commandLink(at: link, ours: ours)
        #expect(dangling == CommandLink.missing)
    }

    /// The first launch from Applications: vibed's LaunchAgent registered,
    /// vibe linked where no password is needed, and nothing at all done by
    /// a copy that isn't installed.
    @Test func theFirstLaunchRegistersVibedAndLinksVibe() async {
        let fresh = FixedInstaller(
            InstallStatus(
                location: AppLocation.applications, service: ServiceStatus.notRegistered,
                command: CommandLink.missing, terminal: TerminalPermission.notAsked))
        fresh.linkNeedsPassword = false
        let model = MenuModel(daemon: FixtureDaemon(.firstRun), terminal: FixtureTerminal(), installer: fresh)
        await model.firstLaunch()
        #expect(fresh.calls == ["register", "link"])
        #expect(model.install.service == ServiceStatus.enabled)
        #expect(model.install.command == CommandLink.linked)

        // /usr/local/bin needs a password: the welcome's step asks for it.
        let rooted = FixedInstaller(InstallStatus.justInstalled)
        let model2 = MenuModel(daemon: FixtureDaemon(.firstRun), terminal: FixtureTerminal(), installer: rooted)
        await model2.firstLaunch()
        #expect(rooted.calls == ["link"])
        #expect(model2.install.command == CommandLink.missing)
        await model2.linkCommand()
        #expect(rooted.calls == ["link", "link, asking"])
        #expect(model2.install.command == CommandLink.linked)
        await model2.allowTerminal()
        #expect(model2.install.terminal == TerminalPermission.granted)

        let dev = FixedInstaller()
        let model3 = MenuModel(daemon: FixtureDaemon(.firstRun), terminal: FixtureTerminal(), installer: dev)
        await model3.firstLaunch()
        #expect(dev.calls.isEmpty)
        #expect(model3.install == InstallStatus.notManaged)
    }

    @Test func aNoteForEachWayVibedIsntThere() async throws {
        let fromDisk = await MenuModel.fixture(.offline, install: InstallStatus.fromDiskImage)
        #expect(fromDisk.install.location == AppLocation.notMoved)
        let moveNote = try #require(InstallNote(fromDisk.install))
        #expect(moveNote.action == InstallNote.Action.reveal)

        let off = await MenuModel.fixture(.offline, install: InstallStatus.turnedOff)
        #expect(off.install.service == ServiceStatus.requiresApproval)
        let note = try #require(InstallNote(off.install))
        #expect(note.action == InstallNote.Action.loginItems)
        #expect(note.title == "Let VibeOS run in the background")
        let installer = try #require(off.installer as? FixedInstaller)
        await off.act(on: note)
        let lastCall: String? = installer.calls.last
        #expect(lastCall == "login items")

        let failed = InstallStatus(
            location: AppLocation.applications, service: ServiceStatus.failed("operation not permitted"),
            command: CommandLink.linked, terminal: TerminalPermission.granted)
        let why: String? = failed.registerFailure
        #expect(why == "operation not permitted")
        let failedNote = try #require(InstallNote(failed))
        #expect(failedNote.action == InstallNote.Action.register)
        #expect(failedNote.detail == "Operation not permitted")
        // Installed and running: no note; nor for a development build.
        let running: InstallNote? = InstallNote(InstallStatus.justInstalled)
        #expect(running == nil)
        let development: InstallNote? = InstallNote(InstallStatus.notManaged)
        #expect(development == nil)
    }

    /// A register that failed stays failed in the menu, though SMAppService
    /// says only "not registered", until a Try Again works.
    @Test func aFailedRegisterIsKept() async {
        final class Refusing: FixedInstaller {
            override func registerService() -> ServiceStatus { ServiceStatus.failed("Operation not permitted") }
        }
        let refusing = Refusing(
            InstallStatus(
                location: AppLocation.applications, service: ServiceStatus.notRegistered,
                command: CommandLink.linked, terminal: TerminalPermission.granted))
        let model = MenuModel(daemon: FixtureDaemon(.offline), terminal: FixtureTerminal(), installer: refusing)
        await model.firstLaunch()
        let why: String? = model.install.registerFailure
        #expect(why == "Operation not permitted")
    }

    /// After an update, an older vibed is restarted from the new app, once;
    /// a newer one, or the same, is left be.
    @Test func anOlderVibedIsRestartedOnce() async {
        let installer = FixedInstaller(InstallStatus.justInstalled)
        let model = MenuModel(daemon: FixtureDaemon(.quiet), terminal: FixtureTerminal(), installer: installer)
        // Read vibed first, while the install isn't known yet, so this
        // test's calls are the only ones that can restart it.
        await model.refresh()
        await model.refreshInstall()
        let before: [String] = installer.calls
        // The fixture's vibed is 0.1.0.
        model.takeOverIfUpdated(appVersion: "0.1.0")
        model.takeOverIfUpdated(appVersion: "0.0.9")
        #expect(installer.calls == before)
        model.takeOverIfUpdated(appVersion: "0.2.0")
        model.takeOverIfUpdated(appVersion: "0.3.0")
        let after: [String] = before + ["restart"]
        #expect(installer.calls == after)
        #expect(versionIsOlder("0.9.0", than: "0.10.0"))
        #expect(!versionIsOlder("1.0", than: "1.0.0"))
        #expect(!versionIsOlder("2.0.0", than: "1.9.9"))
    }

    /// Once permission to control Terminal is refused, the button opens the
    /// settings it's turned back on in.
    @Test func terminalPermission() async {
        let denied = FixedInstaller(
            InstallStatus(
                location: AppLocation.applications, service: ServiceStatus.enabled, command: CommandLink.linked,
                terminal: TerminalPermission.denied))
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
        #expect(model.gh?.account == "octocat")
        #expect(model.mcp?.tailscaleUrls.isEmpty == false)
        // Rows read sensibly: the usage-limited one says until when.
        let limited = try #require(model.agent(host: "local", ref: "gentle-teal-finch"))
        #expect(rowLines(limited).line?.hasPrefix("limited until") == true)
    }

    /// What the README's screenshots draw: a fictional team, working, with
    /// no host down.
    @Test func theDemoIsFictional() async throws {
        let model = await MenuModel.fixture(.demo)
        #expect(model.connection == .connected)
        #expect(model.hosts.map(\.name) == ["local", "atlas", "forge"])
        #expect(model.unavailableHosts.isEmpty)
        #expect(Set(model.agents.map(\.state)) == [.busy, .question, .idle, .asleep])
        #expect(model.gh?.account == "larkfield-dev")
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
        #expect(why.contains("can't reach VibeOS's background service"))
    }

    /// Every call a surface makes is answered, and decodes, so the gallery
    /// draws each one with data.
    @Test func everyCallTheSurfacesMakeDecodes() throws {
        for scenario in [FixtureDaemon.Scenario.busy, .quiet, .firstRun, .freshMac, .demo] {
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
            let checks: Int = try d.call(DoctorCheckGet.self, NoParams()).checks.count
            #expect(checks == CheckId.allCases.count)
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
            hasKey: !(try d.call(KeysList.self, NoParams()).keys.isEmpty))
        #expect(setup.done == 0)
        // With the doctor's checks, it's the five-minute Mac: some passed,
        // some to do, Linux downloading.
        let doctor = try d.call(DoctorCheckGet.self, NoParams()).checks
        let doctorIDs: [CheckId] = doctor.map { (c: DoctorCheck) -> CheckId in c.id }
        #expect(doctorIDs == CheckId.allCases)
        let linux = try #require(doctor.first { (c: DoctorCheck) -> Bool in c.id == CheckId.linux })
        #expect(linux.state == CheckState.working)
        #expect(linux.progress == 40)
        // Pi comes after it, as `vibed` says them: optional, with nothing to do.
        let pi = try #require(doctor.last)
        #expect(pi.id == CheckId.pi)
        #expect(pi.state == CheckState.optional)
        #expect(pi.fixable == false)
        let fresh = try FixtureDaemon(.freshMac).call(DoctorCheckGet.self, NoParams()).checks
        let failing: Int = fresh.filter { (c: DoctorCheck) -> Bool in c.state == CheckState.fail }.count
        #expect(failing == 5)
        // git using gh waits on gh's login with nothing to ask of the owner, so
        // it's no step in the welcome.
        let waiting = SetupProgress(hasProject: false, hasKey: false, doctor: fresh)
        let ghGitStep: SetupProgress.Step? = SetupTests.step(waiting, StepID.check(CheckId.ghGit))
        #expect(ghGitStep == nil)
        // Nor is lingering: it's a Linux host's, and the welcome is about the Mac.
        let lingerStep: SetupProgress.Step? = SetupTests.step(waiting, StepID.check(CheckId.linger))
        #expect(lingerStep == nil)
        #expect(fresh.last?.id == CheckId.pi)
        #expect(fresh.last?.state == CheckState.optional)
        // What Settings shows of Pi agrees with the doctor, either way.
        for scenario in [FixtureDaemon.Scenario.firstRun, FixtureDaemon.Scenario.freshMac] {
            #expect(try FixtureDaemon(scenario).call(PiStatusGet.self, HostParams()).native == nil)
        }
        #expect(try FixtureDaemon(.quiet).call(PiStatusGet.self, HostParams()).native != nil)
        let freshGh: GhState = try FixtureDaemon(.freshMac).call(GhStatusGet.self, GhStatusParams()).state
        #expect(freshGh == GhState.notInstalled)
        let quiet = FixtureDaemon(.quiet)
        #expect(
            SetupProgress(
                hasProject: !(try quiet.call(ProjectsList.self, HostParams()).projects.isEmpty),
                hasKey: !(try quiet.call(KeysList.self, NoParams()).keys.isEmpty)
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
            decode(HostInfo.self, hostJSON("vps", link: .connected)),
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
        // A gh login missing.
        let out = try decode(GhStatus.self, ["state": "logged_out"])
        let worse = headerAlerts(gh: out, hosts: [hosts[1]])
        #expect(worse.map(\.title) == ["GitHub isn't signed in"])
        #expect(worse.map(\.tone) == [.attention])
        // Nothing wrong, nothing shown.
        #expect(headerAlerts(gh: gh, hosts: Array(hosts.prefix(2))).isEmpty)
        #expect(headerAlerts(gh: nil, hosts: []).isEmpty)
    }

    @Test func theSteadyFactsAreAllThereOnRequest() throws {
        let hosts = try [
            decode(HostInfo.self, hostJSON("local", mcp: true)),
            decode(HostInfo.self, hostJSON("vps", link: .connected)),
            decode(HostInfo.self, hostJSON("box", link: .down)),
        ]
        let gh = try decode(GhStatus.self, ["state": "logged_in", "account": "octocat"])
        let facts = headerFacts(memory: HostMemory(used: 3 << 30, total: 32 << 30), gh: gh, mcp: true, hosts: hosts)
        #expect(facts.map(\.label) == ["Memory", "GitHub", "MCP", "vps", "box"])
        #expect(facts.map(\.value) == ["3G of 32G", "octocat", "On", "Connected", "Down"])
        #expect(facts.map(\.tone) == [.normal, .normal, .normal, .normal, .trouble])
        // A host may be named anything, "Memory" too: ids never collide.
        #expect(facts.map(\.id) == ["memory", "gh", "mcp", "host:vps", "host:box"])
        let named = try decode(HostInfo.self, hostJSON("Memory", link: .connected))
        let ids = headerFacts(memory: HostMemory(used: 1, total: 2), gh: nil, mcp: true, hosts: [named]).map(\.id)
        #expect(Set(ids).count == ids.count)
        #expect(headerFacts(memory: nil, gh: nil, mcp: false, hosts: []).map(\.value) == ["Checking…", "Off"])
    }

    @Test func eachHostsMemoryIsWhatItReallyUses() throws {
        var mac = try decode(HostInfo.self, hostJSON("local", mcp: true))
        mac.memory = MemoryUse(total: 64 << 30, used: 58 << 30, swapTotal: 4 << 30, swapUsed: 3 << 30)
        var vps = try decode(HostInfo.self, hostJSON("vps", link: .connected))
        vps.memory = MemoryUse(total: 12 << 30, used: 5 << 30)
        // vibed's reading of the Mac wins over the app's own, with the swap
        // in use; a Linux host's sits beside its link.
        let facts = headerFacts(memory: HostMemory(used: 1, total: 2), gh: nil, mcp: true, hosts: [mac, vps])
        #expect(facts.map(\.value) == ["58G of 64G · 3G swap", "Checking…", "On", "Connected · 5G of 12G"])
        #expect(facts.map(\.tone) == [.normal, .normal, .normal, .normal])
        // No alert while there's no pressure, however full: memory is each
        // agent's ceiling, and the host swaps.
        #expect(headerAlerts(gh: nil, hosts: [mac, vps]).isEmpty)
        // A little swap, as a Mac nearly always has, isn't worth the words.
        #expect(memorySummary(MemoryUse(total: 16 << 30, used: 9 << 30, swapTotal: 1 << 30, swapUsed: 12 << 20)) == "9G of 16G")
        #expect(memorySummary(MemoryUse(total: 16 << 30, used: 9 << 30, swapTotal: 2 << 30, swapUsed: 1 << 30)) == "9G of 16G · 1G swap")

        // Short of memory: a warning, never a block, with the kernel's
        // words for a tooltip.
        mac.memory?.pressure = "macOS warns of memory pressure"
        vps.memory?.pressure = "its programs waited on memory 45% of the last 10 s"
        let alerts = headerAlerts(gh: nil, hosts: [mac, vps])
        #expect(alerts.map(\.title) == ["This Mac is short of memory", "vps is short of memory"])
        #expect(alerts.map(\.tone) == [.attention, .attention])
        #expect(alerts.map(\.reason) == ["macOS warns of memory pressure", "Its programs waited on memory 45% of the last 10 s"])
        #expect(alerts.first?.detail == "Agents keep running, but may slow down. Delete agents you're done with to free some memory.")
        let warned = headerFacts(memory: nil, gh: nil, mcp: true, hosts: [mac, vps])
        #expect(warned.first?.tone == .attention)
        #expect(warned.last?.tone == .attention)
        // A host that's down says nothing of its memory.
        vps.link = .down
        #expect(headerAlerts(gh: nil, hosts: [vps]).map(\.title) == ["vps is down"])
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
        #expect(ClaudeModel.allCases.map(\.title) == ["Opus", "Sonnet", "Fable"])
        #expect(ReasoningLevel.allCases.map(\.title) == ["Low", "Medium", "High", "Extra High", "Max"])
        #expect(PermissionMode.allCases.map(\.title) == ["Auto", "Bypass"])
        let profile = try decode(
            Profile.self,
            ["name": "reviewer", "environment": "linux", "model": "sonnet", "reasoning": "xhigh", "mode": "bypass",
             "memory": 4 << 30] as [String: Any])
        #expect(profileSummary(profile) == "Linux · Sonnet · Extra High · Bypass · 4G")
        // Part of an alert can come from a guest: it's shown as text, and short.
        #expect(cleanAlert("a\u{1b}[2Jb\u{07}c\u{202e}d") == "a[2Jbcd")
        #expect(cleanAlert(String(repeating: "x", count: 1000)).count == 401)
        // Pi says what it runs, has a model of its own, and no modes.
        var pi = profile
        pi.agent = .pi
        pi.piModel = "openai/gpt-5"
        #expect(profileSummary(pi) == "Pi · Linux · openai/gpt-5 · Extra High · No approvals · 4G")
        pi.piModel = nil
        #expect(profileSummary(pi) == "Pi · Linux · Pi's default model · Extra High · No approvals · 4G")
        // The step's words, wherever it sits in the list (SetupTests has
        // the order).
        let setup = SetupProgress(hasProject: false, hasKey: false, linuxGitHub: false)
        let toolbox: SetupProgress.Step? = SetupTests.step(setup, StepID.toolbox)
        let toolboxTitle: String? = toolbox?.title
        #expect(toolboxTitle == "Set up the Linux toolbox")
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

@Test func aHostsStatusReadsAsOneLine() throws {
    let linked = try decode(HostInfo.self, hostJSON("vps", link: .connected, mcp: true))
    #expect(hostStatusLine(linked) == "Connected · MCP on · VibeOS 0.1.0 · me@vps.example.com")
    let down = try decode(HostInfo.self, hostJSON("box", link: .down))
    #expect(hostStatusLine(down) == "Down · MCP off · VibeOS 0.1.0 · me@box.example.com")
    let mac = try decode(HostInfo.self, hostJSON("local", mcp: true))
    #expect(hostStatusLine(mac) == "VibeOS 0.1.0")
}
