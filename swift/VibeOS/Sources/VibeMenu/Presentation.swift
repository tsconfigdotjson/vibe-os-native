import Foundation
import VibeRPC

// What the menu shows, worked out from vibed's answers alone (PRD: Menu bar
// and Terminal attach). Nothing here keeps state.

extension AgentState {
    /// Holding memory on its host: everything but asleep and exited.
    public var isAwake: Bool {
        switch self {
        case .asleep, .exited: false
        case .busy, .idle, .question, .paused: true
        }
    }
}

extension Agent {
    /// Unique across hosts: `vps:quiet-amber-otter`.
    public var key: String { "\(host):\(ref)" }

    public var isLocal: Bool { host == VibeRPC.localHost }
}

/// How an agent's state dot looks.
public enum StateDot: Equatable, Sendable {
    /// Pulsing.
    case busy
    /// Amber.
    case question
    /// Grey.
    case idle
    /// Hollow.
    case asleep
    /// Red: paused by a limit, or exited.
    case stopped

    public init(_ state: AgentState) {
        switch state {
        case .busy: self = .busy
        case .question: self = .question
        case .idle: self = .idle
        case .asleep: self = .asleep
        case .paused, .exited: self = .stopped
        }
    }
}

/// The menu bar icon: a glyph, the count of awake agents, an amber dot when
/// any has a question, and a red dot when any is paused by a limit.
public struct IconSummary: Equatable, Sendable {
    public var awake: Int
    public var question: Bool
    public var paused: Bool

    public init(agents: [Agent]) {
        awake = agents.filter { $0.state.isAwake }.count
        question = agents.contains { $0.state == .question }
        paused = agents.contains { $0.state == .paused }
    }
}

/// A host's agents in the menu, by project.
public struct HostGroup: Identifiable, Equatable, Sendable {
    public var host: String
    public var projects: [ProjectGroup]
    public var id: String { host }
}

public struct ProjectGroup: Identifiable, Equatable, Sendable {
    public var host: String
    public var project: String
    /// Oldest first, as vibed lists them.
    public var agents: [Agent]
    public var id: String { "\(host)/\(project)" }
}

/// Groups agents by host, then project: this host first, then the others
/// in `hosts` order, and projects by name.
public func groupAgents(_ agents: [Agent], hosts: [String]) -> [HostGroup] {
    var order = [VibeRPC.localHost]
    for host in hosts + agents.map(\.host) where !order.contains(host) {
        order.append(host)
    }
    return order.compactMap { host in
        let mine = agents.filter { $0.host == host }
        guard !mine.isEmpty else { return nil }
        let projects = Set(mine.map(\.project)).sorted()
        return HostGroup(
            host: host,
            projects: projects.map { project in
                ProjectGroup(host: host, project: project, agents: mine.filter { $0.project == project })
            })
    }
}

/// Reads vibed's times: RFC 3339 in UTC, with or without fractions.
public func parseTime(_ text: String) -> Date? {
    if let date = try? Date(text, strategy: .iso8601) { return date }
    return try? Date(text, strategy: Date.ISO8601FormatStyle(includingFractionalSeconds: true))
}

/// A duration for a row: `45s`, `12m`, `3h 5m`, `2d 4h`.
public func formatElapsed(_ seconds: TimeInterval) -> String {
    let s = max(0, Int(seconds))
    if s < 60 { return "\(s)s" }
    let m = s / 60
    if m < 60 { return "\(m)m" }
    let h = m / 60
    if h < 24 { return m % 60 == 0 ? "\(h)h" : "\(h)h \(m % 60)m" }
    let d = h / 24
    return h % 24 == 0 ? "\(d)d" : "\(d)d \(h % 24)h"
}

/// How long an agent has been going, since it started.
public func elapsed(_ agent: Agent, now: Date) -> String? {
    parseTime(agent.createdAt).map { formatElapsed(now.timeIntervalSince($0)) }
}

private let kib = 1024
private let mib = 1024 * kib
private let gib = 1024 * mib
private let tib = 1024 * gib

/// Bytes the way vibed writes them in `host.toml`: `4G`, `512M`, `1.5G`.
public func formatBytes(_ bytes: Int) -> String {
    let units: [(Int, String)] = [(tib, "T"), (gib, "G"), (mib, "M"), (kib, "K")]
    for (size, suffix) in units where bytes >= size {
        let value = Double(bytes) / Double(size)
        if bytes % size == 0 { return "\(bytes / size)\(suffix)" }
        if value >= 100 { return "\(Int(value.rounded()))\(suffix)" }
        return String(format: "%.1f", value) + suffix
    }
    return "\(bytes)B"
}

/// Bytes exactly, for a field to edit: the largest unit that divides them,
/// such as `4G` or `1500M`, so what's shown reads back as the same number.
public func formatSize(_ bytes: Int) -> String {
    let units: [(Int, String)] = [(tib, "T"), (gib, "G"), (mib, "M"), (kib, "K")]
    for (size, suffix) in units where bytes >= size && bytes % size == 0 {
        return "\(bytes / size)\(suffix)"
    }
    return "\(bytes)"
}

/// Reads a size as `host.toml` writes one: bytes, or a whole or decimal
/// number with K, M, G or T (and an optional B or iB). Nil if it isn't one.
public func parseBytes(_ text: String) -> Int? {
    var s = text.trimmingCharacters(in: .whitespaces).uppercased()
    for suffix in ["IB", "B"] where s.hasSuffix(suffix) && s.count > suffix.count {
        s.removeLast(suffix.count)
        break
    }
    let units: [Character: Int] = ["K": kib, "M": mib, "G": gib, "T": tib]
    var unit = 1
    if let last = s.last, let size = units[last] {
        unit = size
        s.removeLast()
    }
    s = s.trimmingCharacters(in: .whitespaces)
    if let whole = Int(s), whole >= 0 {
        let (bytes, overflow) = whole.multipliedReportingOverflow(by: unit)
        return overflow ? nil : bytes
    }
    guard unit > 1, let value = Double(s), value >= 0, value.isFinite,
        value * Double(unit) < Double(Int.max)
    else { return nil }
    return Int((value * Double(unit)).rounded())
}

/// Reads a duration as `host.toml` writes one: seconds, or a number with
/// `s`, `m` or `h`.
public func parseSeconds(_ text: String) -> Int? {
    var s = text.trimmingCharacters(in: .whitespaces).lowercased()
    let units: [Character: Int] = ["s": 1, "m": 60, "h": 3600]
    var unit = 1
    if let last = s.last, let size = units[last] {
        unit = size
        s.removeLast()
    }
    guard let n = Int(s.trimmingCharacters(in: .whitespaces)), n >= 0 else { return nil }
    let (seconds, overflow) = n.multipliedReportingOverflow(by: unit)
    return overflow ? nil : seconds
}

/// The memory bar: what the agent uses against its limit, 0 to 1, with a
/// label. Nil while nothing measured it (asleep, exited, starting).
public struct MemoryGauge: Equatable, Sendable {
    public var fraction: Double
    public var label: String

    public init?(_ agent: Agent) {
        guard let used = agent.memoryUsed, agent.memory > 0 else { return nil }
        fraction = min(1, max(0, Double(used) / Double(agent.memory)))
        label = "\(formatBytes(used)) / \(formatBytes(agent.memory))"
    }
}

/// The second and third lines of a row: the session's title, and the latest
/// activity line, or "limited until …" while a usage limit holds it.
public func rowLines(_ agent: Agent, timeZone: TimeZone = .current) -> (title: String?, line: String?) {
    let status = agent.status
    var line = status.line
    if agent.state == .paused, let gauge = MemoryGauge(agent), gauge.fraction >= 0.995 {
        line = "Paused at its \(formatBytes(agent.memory)) memory limit"
    } else if status.stopReason == .usageLimit {
        if let resets = status.resetsAt.flatMap(parseTime) {
            var style = Date.FormatStyle(date: .omitted, time: .shortened)
            style.timeZone = timeZone
            line = "limited until \(resets.formatted(style))"
        } else {
            line = "limited by a usage limit"
        }
    } else if status.activity == .errored, let error = status.error {
        line = "error: \(error)"
    }
    return (status.title, line)
}

/// The MCP setup line a new key's Copy setup command gives (PRD: MCP
/// interface, Auth), as `vibe key create` prints it. `url` is where vibed
/// says MCP is served (`mcp.get`); before it has said, the default port's.
public enum MCPSetup {
    public static let url = "http://127.0.0.1:7878/mcp"

    public static func command(secret: String, url: String = MCPSetup.url) -> String {
        "claude mcp add --transport http vibeos \(url) --header \"Authorization: Bearer \(secret)\""
    }
}

/// What Settings → Linux Toolbox says about the toolbox (PRD: The toolbox
/// shell): what it's doing, and a note or two (a newer base image the
/// template is moving to, how the last run went). Only what vibed says now:
/// the shell shows as open only while it is.
public struct ToolboxSummary: Equatable, Sendable {
    /// What the toolbox is doing, in a word or two.
    public var state: String
    /// Whether it's busy with something that ends on its own.
    public var working: Bool
    public var notes: [String]
    /// Whether the shell is open, for Save & Close.
    public var closable: Bool

    public init(_ status: ToolboxStatus?) {
        working = false
        notes = []
        closable = status?.state == .open
        guard let status else {
            state = "Loading…"
            return
        }
        switch status.state {
        case .unavailable: state = "Not on this host"
        case .closed: state = "Closed"
        case .open:
            switch status.clients {
            // Not known: a boot under way, a shim too slow to ask, or a
            // vibed from before #54.
            case nil: state = "Open: exit the shell to save"
            case 0: state = "Open in the background, with no Terminal window"
            default: state = "Open in Terminal"
            }
        case .saving: state = "Saving…"
        case .updating: state = "Installing updates…"
        case .rebuilding: state = "Updating Linux…"
        }
        working = [.saving, .updating, .rebuilding].contains(status.state)
        guard status.state != .unavailable else { return }
        let held = status.rebuildHeld ?? []
        if let newest = status.newestBase, newest > (status.base ?? 0) {
            if held.isEmpty {
                notes.append("A newer Linux is ready. VibeOS updates new agents to it by itself.")
            } else {
                notes.append(
                    "A newer Linux is ready, but new agents stay on the current one for now. Updating would "
                        + "lose these, which you installed by hand and VibeOS can't put back: "
                        + held.joined(separator: ", ") + ".")
            }
        }
        if servicesNeedRebuild(status) {
            notes.append("Linux agents have no shared services until their Linux is updated.")
        }
        if let last = status.last {
            let what: String
            switch last.action {
            case .shell: what = "save"
            case .update: what = "tool update"
            case .rebuild: what = "Linux update"
            }
            if let error = last.error {
                notes.append("The last \(what) failed: \(error)")
            } else if !last.uncovered.isEmpty {
                notes.append(
                    "Left behind, as VibeOS can't put them back: "
                        + last.uncovered.joined(separator: ", "))
            }
        }
    }
}

/// Quotes `word` for a POSIX shell.
public func shellQuote(_ word: String) -> String {
    "'" + word.replacingOccurrences(of: "'", with: "'\\''") + "'"
}

/// A login's tool, for people.
public func toolName(_ tool: LoginTool) -> String {
    switch tool {
    case .gh: "GitHub (gh)"
    case .gcloud: "Google Cloud"
    case .git: "Git identity"
    case .ssh: "SSH key"
    }
}

/// A login's tool, as an SF Symbol.
public func toolSymbol(_ tool: LoginTool) -> String {
    switch tool {
    case .gh: "chevron.left.forwardslash.chevron.right"
    case .gcloud: "cloud"
    case .git: "person.text.rectangle"
    case .ssh: "key"
    }
}

/// The logins every new Linux agent starts with, from the Linux home
/// (PRD: Core concepts, Linux home): which, or plainly that there are none
/// yet or that it isn't known. Agents' own sign-in is only a hint, there
/// when it's missing.
public struct LoginsSummary: Equatable, Sendable {
    public struct Row: Equatable, Sendable, Identifiable {
        public var tool: String
        public var account: String
        /// An SF Symbol for the tool.
        public var symbol: String
        public var id: String { tool + "\u{0}" + account }
    }

    /// What the rows are, or why there are none.
    public var headline: String
    public var rows: [Row]
    /// How Linux agents get signed in, while they aren't.
    public var signInHint: String?

    public init(_ logins: ToolboxLogins) {
        rows = (logins.logins ?? []).map {
            Row(tool: toolName($0.tool), account: $0.account, symbol: toolSymbol($0.tool))
        }
        switch (logins.home, logins.logins) {
        case (nil, _):
            headline = "No GitHub or other tool logins saved yet."
        case (_?, nil):
            headline = "VibeOS couldn't tell which logins are saved. Open the shell and exit it to look again."
        case (_?, let found?) where found.isEmpty:
            headline = "No GitHub or other tool logins saved yet."
        case (_?, _):
            headline = "New Linux agents start with these."
        }
        if logins.claude {
            signInHint = nil
        } else if let why = logins.claudeProblem {
            signInHint = sentence(why)
        } else {
            signInHint = "Linux agents aren't signed in yet."
        }
    }
}

/// What Settings' sidebar does with its selection changing to `picked`,
/// with `shown` the pane showing: a `List` clears its selection on a click
/// in its empty space or a Command-click on the row, which mustn't move
/// anything; it only puts the row's highlight back.
public enum SidebarPick<Pane: Equatable>: Equatable {
    case show(Pane)
    case restore
    case nothing
}

public func sidebarPick<Pane: Equatable>(_ picked: Pane?, shown: Pane) -> SidebarPick<Pane> {
    guard let picked else { return .restore }
    return picked == shown ? .nothing : .show(picked)
}

/// Where Settings is within its panes: the project drilled into, and the
/// host each per-host pane shows. Moving to another pane in the sidebar
/// takes it back to its first level, never a page drilled into before; the
/// hosts picked stay as they were.
public struct SettingsPlace: Equatable, Sendable {
    /// The Projects pane's navigation path: the project shown, if any.
    public var projects: [String] = []
    public var projectsHost = VibeRPC.localHost
    public var limitsHost = VibeRPC.localHost

    public init() {}

    /// A pane was picked in the sidebar.
    public mutating func picked() {
        projects.removeAll()
    }
}

/// When an API key was last used, as Settings → API Keys says it: "Last
/// used 3 minutes ago", or "Never used".
public func keyUsage(_ lastUsedAt: String?, now: Date) -> String {
    guard let used = lastUsedAt.flatMap(parseTime) else { return "Never used" }
    if now.timeIntervalSince(used) < 60 { return "Last used just now" }
    // In the app's own words, English, measured from `now`.
    let relative = RelativeDateTimeFormatter()
    relative.dateTimeStyle = .named
    relative.unitsStyle = .full
    relative.locale = Locale(identifier: "en_US")
    return "Last used \(relative.localizedString(for: used, relativeTo: now))"
}

/// What Settings → API Keys says while a Linux host hasn't had this Mac's latest keys,
/// if any: a key revoked here goes on working there until it has.
public func keysPending(on hosts: [String]) -> String? {
    guard !hosts.isEmpty else { return nil }
    let names = hosts.joined(separator: ", ")
    let verb = hosts.count == 1 ? "it's" : "they're"
    return "Key changes are pending on \(names). A key revoked here still works there until \(verb) next reachable."
}

/// What Settings → API Keys warns of, in one sentence: what a key can do, and
/// that a key is no lock on this Mac's own login, as vibed's socket answers
/// that login without one. Other accounts on this Mac can't reach that socket,
/// so this says "your Mac login", not "anyone on this Mac".
public func keysWarning(linuxHosts: Bool) -> String {
    let reach = linuxHosts ? "this Mac and your Linux hosts" : "this Mac"
    return "Keep API keys and your Mac login private, as anyone with either can have VibeOS agents do whatever "
        + "you can do on \(reach)."
}

// MARK: Hosts

/// A host's status under its name in Settings → Hosts, as one line:
/// `Connected · MCP on · VibeOS 0.1.9 · me@vps.example.com`. This Mac has no link or
/// MCP switch of its own to say.
public func hostStatusLine(_ host: HostInfo) -> String {
    var parts: [String] = []
    if host.link != .local {
        parts.append(host.link == .connected ? "Connected" : "Down")
        parts.append(host.mcp ? "MCP on" : "MCP off")
    }
    if let version = host.version { parts.append("VibeOS \(version)") }
    if let address = host.address { parts.append(address) }
    return parts.joined(separator: " · ")
}

// MARK: Reaching MCP from elsewhere

/// A word for Tailscale where MCP's settings are. It's optional, and VibeOS doesn't set it up.
public let tailscaleSuggestion =
    "To reach MCP from another machine, you can use [Tailscale](https://tailscale.com). It's optional, "
    + "and both machines need it installed and signed in."

// MARK: Rows, in words and symbols

extension AgentState {
    /// The state in a word or two, for people and VoiceOver.
    public var spoken: String {
        switch self {
        case .busy: "working"
        case .idle: "idle"
        case .question: "has a question"
        case .asleep: "asleep"
        case .paused: "paused by a limit"
        case .exited: "exited"
        }
    }
}

/// The SF Symbol before a row's activity line: what the agent is doing, or
/// why it isn't. Nil when there's nothing to say.
public func activitySymbol(_ agent: Agent) -> String? {
    switch agent.state {
    case .asleep: return "moon.zzz"
    case .paused: return "pause.circle"
    case .exited: return "stop.circle"
    case .busy, .idle, .question: break
    }
    if agent.status.stopReason == .usageLimit { return "hourglass" }
    switch agent.status.activity {
    case .thinking: return "sparkle"
    case .runningTool: return "terminal"
    case .backgroundWork: return "gearshape.2"
    case .awaitingAnswer: return "questionmark.bubble"
    // An inferred prompt vibed hasn't confirmed is still a tool at work:
    // only a question raises the hand.
    case .awaitingApproval: return agent.state == .question ? "hand.raised" : "terminal"
    case .done: return "checkmark"
    case .interrupted: return "pause"
    case .errored: return "exclamationmark.triangle"
    case .unknown: return nil
    }
}

// The app's one voice for vibed's values: display names, as a person
// writes them, never the wire's lowercase. Profile names are the owner's
// own and are shown as typed.

extension AgentEnvironment {
    /// `Linux`, `macOS`.
    public var title: String {
        switch self {
        case .linux: "Linux"
        case .macos: "macOS"
        }
    }
}

extension AgentKind {
    /// `Claude`, `Pi`.
    public var title: String {
        switch self {
        case .claude: "Claude"
        case .pi: "Pi"
        }
    }
}

extension ClaudeModel {
    public var title: String {
        switch self {
        case .opus: "Opus"
        case .sonnet: "Sonnet"
        }
    }
}

extension ReasoningLevel {
    public var title: String {
        switch self {
        case .low: "Low"
        case .medium: "Medium"
        case .high: "High"
        case .xhigh: "Extra High"
        case .max: "Max"
        }
    }
}

extension PermissionMode {
    public var title: String {
        switch self {
        case .auto: "Auto"
        case .bypass: "Bypass"
        }
    }
}

/// An alert as it's safe and short enough to show, as `vibed` says it already:
/// part of one can have come from a guest, whichever `vibed` this is.
public func cleanAlert(_ alert: String) -> String {
    let clean = String.UnicodeScalarView(
        alert.unicodeScalars.filter { scalar in
            !scalar.properties.isBidiControl && !CharacterSet.controlCharacters.contains(scalar)
        })
    let text = String(clean)
    return text.count > 400 ? String(text.prefix(400)) + "…" : text
}

/// A profile's settings in a line, in the app's voice: `Linux · Opus · High
/// · Auto · 4G`.
public func profileSummary(_ profile: Profile) -> String {
    if profile.kind == .pi {
        var parts = [
            "Pi", profile.environment.title, profile.piModel ?? "Pi's default model",
            profile.reasoning.title, "No approvals", formatBytes(profile.memory),
        ]
        if !profile.sharedServicesOn { parts.append("No shared services") }
        return parts.joined(separator: " · ")
    }
    var parts = [
        profile.environment.title, profile.model.title, profile.reasoning.title, profile.mode.title,
        formatBytes(profile.memory),
    ]
    // On is the default, so it says nothing unless it's off.
    if !profile.sharedServicesOn { parts.append("No shared services") }
    return parts.joined(separator: " · ")
}

/// The SF Symbol for an environment: a compartment, or this Mac.
public func environmentSymbol(_ environment: AgentEnvironment) -> String {
    switch environment {
    case .linux: "cube"
    case .macos: "laptopcomputer"
    }
}

/// A duration spelled out, for VoiceOver: `2 hours 5 minutes`.
public func spokenElapsed(_ seconds: TimeInterval) -> String {
    let s = max(0, Int(seconds))
    func unit(_ n: Int, _ word: String) -> String { n == 1 ? "1 \(word)" : "\(n) \(word)s" }
    if s < 60 { return unit(s, "second") }
    let m = s / 60
    if m < 60 { return unit(m, "minute") }
    let h = m / 60
    if h < 24 { return m % 60 == 0 ? unit(h, "hour") : "\(unit(h, "hour")) \(unit(m % 60, "minute"))" }
    let d = h / 24
    return h % 24 == 0 ? unit(d, "day") : "\(unit(d, "day")) \(unit(h % 24, "hour"))"
}

/// Everything a row shows, in one sentence, for VoiceOver.
public func rowAccessibilityLabel(_ agent: Agent, now: Date, unavailable: Bool = false) -> String {
    let lines = rowLines(agent)
    var parts = [agent.ref, unavailable ? "unavailable, \(agent.host) can't be reached" : agent.state.spoken]
    if let title = lines.title { parts.append(title) }
    if let line = lines.line { parts.append(line) }
    parts.append(agent.profile)
    if agent.kind == .pi { parts.append("runs Pi") }
    if let started = parseTime(agent.createdAt) {
        parts.append("started \(spokenElapsed(now.timeIntervalSince(started))) ago")
    }
    if let gauge = MemoryGauge(agent) {
        parts.append("memory \(gauge.label.replacingOccurrences(of: " / ", with: " of "))")
    }
    return parts.joined(separator: ", ")
}

/// The header's count: `5 agents · 3 awake`, or `No agents`.
public func agentSummary(_ agents: [Agent]) -> String {
    guard !agents.isEmpty else { return "No agents" }
    let awake = agents.filter { $0.state.isAwake }.count
    return "\(agents.count == 1 ? "1 agent" : "\(agents.count) agents") · \(awake) awake"
}

/// A clause from vibed (`can't reach…`), capitalised to stand on its own as
/// a sentence. One that starts with a command's name (`ssh: connect…`,
/// `gh auth …`), or a name with capitals of its own (`macOS says…`), is
/// left as it is: the name is spelt that way.
public func sentence(_ text: String) -> String {
    guard let first = text.first else { return text }
    let word = text.prefix { $0.isLetter || $0.isNumber || $0 == "-" || $0 == "_" }
    if commandNames.contains(String(word)) || word.dropFirst().contains(where: \.isUppercase) { return text }
    return first.uppercased() + text.dropFirst()
}

/// Commands whose names a sentence may start with, in lowercase.
let commandNames: Set<String> = [
    "ssh", "scp", "gh", "git", "gcloud", "claude", "pi", "vibe", "vibed", "vibe-shim", "vibe-vm", "systemctl", "sudo",
    "apt", "npm",
]

// MARK: First run

extension SignInWhat {
    /// Whether `link` is the provider's own sign-in page. A page comes from the host's
    /// word alone, and a host can be taken over: this one opens by itself, as the owner
    /// just asked for it, and anything else takes a click that says where it goes.
    public func isOwnPage(_ link: String) -> Bool {
        guard let parts = URLComponents(string: link), parts.scheme?.lowercased() == "https",
            parts.user == nil, parts.password == nil, parts.port == nil,
            let host = parts.host?.lowercased()
        else { return false }
        switch self {
        case .github, .toolboxGithub:
            return host == "github.com"
        case .claude, .toolboxClaude:
            return ["claude.com", "claude.ai", "platform.claude.com", "console.anthropic.com"].contains(host)
        }
    }
}

/// What a setup step's button does.
public enum SetupAction: Equatable, Sendable {
    /// Links vibe into /usr/local/bin.
    case linkCommand
    /// Asks macOS for permission to control Terminal, or after a no, opens
    /// where it's turned back on.
    case allowTerminal
    /// Has vibed fix a doctor check (`doctor.fix`).
    case fix(CheckId)
    /// Signs in, with one click: the provider's own flow, and its page in the browser.
    case signIn(SignInWhat)
    /// Runs a command line in Terminal, in the owner's own shell.
    case terminal(String)
    /// Runs `vibe <words>` in Terminal, as its shell.
    case vibe([String])
    /// Opens a web page or a System Settings pane.
    case open(String)
    /// Apple's Command Line Tools installer.
    case installCommandLineTools
    /// Serves MCP on this Mac's Tailscale address too (`mcp.set`).
    case serveOnTailscale
    case openToolbox
    case projects, keys
}

/// How far the owner is through setting up (PRD: Key user flows, Install
/// and setup), as one list in order. First launch: the vibe command, vibe
/// doctor's checks (git, gh and git using it, Claude Code and its login,
/// system sleep, Tailscale) and the app's permission to control Terminal.
/// First run: the Linux download, the Linux toolbox's GitHub login, a
/// project, and an API key for MCP clients.
/// The menu shows it as a welcome until agents exist.
public struct SetupProgress: Equatable, Sendable {
    public struct Step: Equatable, Sendable, Identifiable {
        public enum ID: Hashable, Sendable {
            case command
            case check(CheckId)
            case terminal
            case toolbox, project
            /// MCP on Tailscale, for Claude Code on the owner's other machines.
            case remote
            case key
        }

        public enum Status: Equatable, Sendable {
            case todo
            case done
            /// Under way, and how far in percent when that's known.
            case working(Int?)
            /// Missing, but not needed.
            case optional
        }

        public var id: ID
        public var title: String
        /// What it is, or what to do, as a sentence.
        public var detail: String
        public var status: Status
        /// The button that does it, and its title; none while it's done,
        /// under way, or waiting on another step.
        public var action: SetupAction?
        public var actionTitle: String?

        public init(
            id: ID, title: String, detail: String, status: Status, action: SetupAction? = nil,
            actionTitle: String? = nil
        ) {
            self.id = id
            self.title = title
            self.detail = detail
            self.status = status
            self.action = action
            self.actionTitle = actionTitle
        }

        public var done: Bool { status == .done }
        /// While it's under way, how far in percent, if that's known.
        public var progress: Int? {
            if case .working(let p) = status { return p }
            return nil
        }
        public var isWorking: Bool {
            if case .working = status { return true }
            return false
        }
        /// Its state in words, for VoiceOver.
        public var statusWords: String {
            switch status {
            case .done: return "done"
            case .todo: return "to do"
            case .optional: return "optional"
            case .working(let p?): return "under way, \(p) percent"
            case .working(nil): return "under way"
            }
        }
        /// Whether it holds up the welcome.
        public var needed: Bool {
            switch status {
            case .todo, .working: true
            case .done, .optional: false
            }
        }
        /// One of this Mac's own checks, as against a first-run step.
        public var onThisMac: Bool {
            switch id {
            case .command, .terminal: true
            case .check(let c): c != .linux
            default: false
            }
        }
    }

    /// A row of the welcome: a step, or this Mac's checks that passed,
    /// folded into one.
    public enum Row: Equatable, Sendable, Identifiable {
        case step(Step)
        case passed([Step])

        public var id: String {
            switch self {
            case .step(let s): "\(s.id)"
            case .passed: "passed"
            }
        }
    }

    public var hasProject: Bool
    public var hasKey: Bool
    /// Whether the Linux home has a gh login; nil where there's no toolbox,
    /// or VibeOS couldn't tell.
    public var linuxGitHub: Bool?
    /// `vibe doctor`'s checks; none from a vibed too old to have them.
    public var doctor: [DoctorCheck]
    /// How this copy is installed.
    public var install: InstallStatus
    /// Whether gh is installed at all, which the gh check's button needs.
    public var ghInstalled: Bool
    /// Whether other machines can reach MCP here, on Tailscale. Nil where
    /// Tailscale isn't up, so there's nothing to turn on.
    public var remoteMCP: Bool?

    public init(
        hasProject: Bool, hasKey: Bool, linuxGitHub: Bool? = nil, doctor: [DoctorCheck] = [],
        install: InstallStatus = .notManaged, ghInstalled: Bool = true, remoteMCP: Bool? = nil
    ) {
        self.remoteMCP = remoteMCP
        self.hasProject = hasProject
        self.hasKey = hasKey
        self.linuxGitHub = linuxGitHub
        self.doctor = doctor
        self.install = install
        self.ghInstalled = ghInstalled
    }

    public var steps: [Step] {
        var steps: [Step] = []
        if let command = commandStep { steps.append(command) }
        // Pi is optional and VibeOS installs it once a profile runs it, so the
        // welcome doesn't bring it up: Settings → Agents has it. Nor git using gh
        // while gh isn't logged in: vibed sets that up itself once it is. Nor
        // lingering, which is a Linux host's, and this welcome is about the Mac.
        let machine = doctor.filter {
            $0.id != .linux && $0.id != .pi && $0.id != .linger && !($0.id == .ghGit && $0.state == .optional)
        }
        for check in machine {
            steps.append(step(for: check))
            // Terminal goes with the tools agents need, before the Mac's
            // power and Tailscale.
            if check.id == .claudeLogin, let terminal = terminalStep { steps.append(terminal) }
        }
        if !machine.contains(where: { $0.id == .claudeLogin }), let terminal = terminalStep {
            steps.append(terminal)
        }
        let linux = doctor.first { $0.id == .linux }
        if let linux { steps.append(step(for: linux)) }
        if let linuxGitHub {
            // Downloaded: the template's own move is no reason to wait.
            let ready = linux.map { $0.state == .pass || $0.state == .optional } ?? true
            steps.append(
                Step(
                    id: .toolbox, title: "Set up the Linux toolbox",
                    detail: linuxGitHub
                        ? "Linux agents start logged in to GitHub."
                        : ready
                            ? "One click, in your browser. Open Shell in Settings → Linux Toolbox installs anything "
                                + "else agents need."
                            : "Once Linux is downloaded: signs Linux agents in to GitHub.",
                    status: linuxGitHub ? .done : .todo,
                    action: linuxGitHub || !ready ? nil : .signIn(.toolboxGithub), actionTitle: "Sign In…"))
        }
        steps.append(
            Step(
                id: .project, title: "Add a project",
                detail: "A GitHub repo or a folder on this Mac. It comes with two profiles, ready to use.",
                status: hasProject ? .done : .todo, action: hasProject ? nil : .projects, actionTitle: "Add…"))
        if let remoteMCP {
            steps.append(
                Step(
                    id: .remote, title: "Reach MCP from your other machines",
                    detail: remoteMCP
                        ? "Claude Code on your other machines can reach MCP here."
                        : "Serves MCP on this Mac's Tailscale address too, for Claude Code on your laptop.",
                    status: remoteMCP ? .done : .todo, action: remoteMCP ? nil : .serveOnTailscale,
                    actionTitle: "Turn On"))
        }
        steps.append(
            Step(
                id: .key, title: "Make an API key",
                detail: "So Claude Code and other MCP clients can start agents here.",
                status: hasKey ? .done : .todo, action: hasKey ? nil : .keys, actionTitle: "Make…"))
        return steps
    }

    private var commandStep: Step? {
        switch install.command {
        case .notManaged:
            return nil
        case .linked:
            return Step(
                id: .command, title: "vibe command", detail: "Terminal and ssh can find the vibe command.",
                status: .done)
        case .missing:
            return Step(
                id: .command, title: "Add the vibe command",
                detail: "Lets Terminal and ssh find the vibe command. macOS asks for your password.",
                status: .todo, action: .linkCommand, actionTitle: "Add…")
        case .other(let path):
            return Step(
                id: .command, title: "Point vibe at this VibeOS",
                detail: "Another vibe command is already installed, at \(path).", status: .todo, action: .linkCommand,
                actionTitle: "Replace…")
        }
    }

    private var terminalStep: Step? {
        guard install.managed else { return nil }
        switch install.terminal {
        case .granted:
            return Step(
                id: .terminal, title: "Terminal", detail: "Clicking an agent opens Terminal attached to it.",
                status: .done)
        case .denied:
            return Step(
                id: .terminal, title: "Let VibeOS control Terminal",
                detail: "It's off in Privacy & Security. Agents still open in Terminal, in a new window each time.",
                status: .optional, action: .allowTerminal, actionTitle: "Settings…")
        case .notAsked, .unknown:
            return Step(
                id: .terminal, title: "Let VibeOS control Terminal",
                detail: "So clicking an agent opens, or brings back, the Terminal window attached to it.",
                status: .todo, action: .allowTerminal, actionTitle: "Allow…")
        }
    }

    /// A doctor check as a step: its title, what it found, and the button
    /// that does something about it.
    private func step(for check: DoctorCheck) -> Step {
        let status: Step.Status
        switch check.state {
        case .pass: status = .done
        case .fail: status = .todo
        case .optional: status = .optional
        case .working: status = .working(check.progress)
        }
        let passed = check.state == .pass
        let claudeReady = doctor.contains { $0.id == .claude && $0.state == .pass }
        var step = Step(
            id: .check(check.id),
            title: check.id == .linux && check.state == .optional
                ? checkTitle(.linux, passed: true) : checkTitle(check.id, passed: passed),
            detail: welcomeDetail(check, claudeReady: claudeReady), status: status)
        guard !passed, check.state != .working else { return step }
        switch check.id {
        case .git:
            step.action = .installCommandLineTools
            step.actionTitle = "Install…"
        case .gh:
            if check.fixable {
                // VibeOS installs it by itself, and this is its Try Again.
                step.action = .fix(.gh)
                step.actionTitle = "Try Again"
            } else if ghInstalled {
                step.action = .signIn(.github)
                step.actionTitle = "Sign In…"
            }
        case .ghGit:
            if check.fixable {
                step.action = .fix(.ghGit)
                step.actionTitle = "Try Again"
            }
        case .claude:
            if check.fixable {
                step.action = .fix(.claude)
                step.actionTitle = "Try Again"
            }
        case .claudeLogin:
            if claudeReady {
                step.action = .signIn(.claude)
                step.actionTitle = "Sign In…"
            }
        case .power:
            step.action = .terminal("sudo pmset -c sleep 0")
            step.actionTitle = "Turn Off…"
        case .linger:
            // Only a Linux host's, and a Mac's welcome is about the Mac.
            break
        case .tailscale:
            step.action = .open("https://tailscale.com/download/mac")
            step.actionTitle = "Get Tailscale"
        case .linux:
            if check.fixable {
                step.action = .fix(.linux)
                step.actionTitle = "Try Again"
            }
        case .pi:
            break
        }
        return step
    }

    /// What a check says in the welcome: what it found once it passed, or
    /// while it's under way; otherwise why it matters and what the button
    /// does, in a line. `vibe doctor`'s hints are for Terminal, and longer.
    private func welcomeDetail(_ check: DoctorCheck, claudeReady: Bool) -> String {
        if check.state == .pass || check.state == .working { return check.detail }
        switch check.id {
        case .git: return "Agents need git, from Apple's Command Line Tools."
        case .gh:
            return ghInstalled
                ? "One click, in your browser, so agents can clone, push and open pull requests."
                : check.detail
        case .ghGit:
            return check.fixable ? check.detail : "Once GitHub is signed in."
        case .claude: return check.detail
        case .claudeLogin: return claudeReady ? "One click, in your browser." : "Once Claude Code is installed."
        case .power: return "Agents can't answer while this Mac sleeps. Turning it off asks for your password."
        case .tailscale: return "Lets your other machines reach your agents over MCP."
        case .linger, .linux, .pi: return check.detail
        }
    }

    /// The welcome's rows: this Mac's checks that passed fold into one, at
    /// the top; every other step is a row of its own.
    public var rows: [Row] {
        let all = steps
        let passed = all.filter { $0.onThisMac && $0.done }
        var rows: [Row] = passed.isEmpty ? [] : [.passed(passed)]
        rows += all.filter { !($0.onThisMac && $0.done) }.map(Row.step)
        return rows
    }

    /// The step to do next: the first that's needed and has a button.
    public var next: Step.ID? {
        steps.first { $0.status == .todo && $0.action != nil }?.id
    }

    /// The row this Mac's passed checks fold into: ready, once nothing on
    /// this Mac still needs the owner.
    public var passedTitle: String {
        let all = steps.filter(\.onThisMac)
        if all.allSatisfy({ !$0.needed }) { return "This Mac is ready" }
        let n = all.filter(\.done).count
        return n == 1 ? "1 check passed" : "\(n) checks passed"
    }

    public var done: Int { steps.filter(\.done).count }
    public var complete: Bool { steps.allSatisfy { !$0.needed } }
    /// How many of the mark's four windows are lit: a share of the steps
    /// done, all four only once it's complete.
    public var lit: Int {
        if complete { return 4 }
        let count = max(1, steps.count)
        return min(3, done * 4 / count)
    }
}

/// A doctor check's name, as the welcome lists it: what it is once it
/// passed, what to do while it hasn't.
public func checkTitle(_ id: CheckId, passed: Bool) -> String {
    switch (id, passed) {
    case (.git, true): "git"
    case (.git, false): "Install git"
    case (.gh, true): "GitHub"
    case (.gh, false): "Sign in to GitHub"
    case (.ghGit, true): "Git uses your GitHub sign-in"
    case (.ghGit, false): "Let Git use your GitHub sign-in"
    case (.claude, true): "Claude Code"
    case (.claude, false): "Install Claude Code"
    case (.claudeLogin, true): "Claude login"
    case (.claudeLogin, false): "Sign in to Claude Code"
    case (.power, true): "Sleep off"
    case (.power, false): "Keep this Mac awake"
    case (.linger, true): "VibeOS keeps running"
    case (.linger, false): "Keep VibeOS running when you log out"
    case (.tailscale, true): "Tailscale"
    case (.tailscale, false): "Tailscale"
    case (.linux, true): "Linux"
    case (.linux, false): "Download Linux"
    case (.pi, true): "Pi"
    case (.pi, false): "Install Pi"
    }
}

// MARK: The header

/// How loud a part of the header is: quiet when all is well, in colour
/// only when something isn't.
public enum StatusTone: Equatable, Sendable {
    case normal, attention, trouble
}

/// Something in the header that needs the owner: a host that's down, or a
/// `gh` login missing. Each gets a row of its own.
public struct HeaderAlert: Equatable, Sendable, Identifiable {
    public var id: String
    /// An SF Symbol.
    public var symbol: String
    /// The host it's about, if any: shown as a name, in monospace, as
    /// agents' refs are, so `box is down` doesn't read as a typo.
    public var name: String?
    /// The rest of the headline, after the name.
    public var headline: String
    /// The whole headline, for VoiceOver.
    public var title: String { name.map { "\($0) \(headline)" } ?? headline }
    /// What it means, plainly.
    public var detail: String
    /// The technical reason, for a tooltip.
    public var reason: String?
    public var tone: StatusTone
}

/// A steady fact about the hosts: the Mac's memory, `gh`, MCP, each Linux
/// host's link. They show together, on request, under the header.
public struct HeaderFact: Equatable, Sendable, Identifiable {
    /// `memory`, `gh`, `mcp`, or `host:<name>`: a host may be named anything.
    public var id: String
    public var label: String
    public var value: String
    public var symbol: String
    public var tone: StatusTone
}

/// What needs the owner, worst first (PRD: Menu bar, Header).
public func headerAlerts(gh: GhStatus?, hosts: [HostInfo]) -> [HeaderAlert] {
    var alerts: [HeaderAlert] = []
    for host in hosts where host.link == .down {
        alerts.append(
            HeaderAlert(
                id: "host:\(host.name)", symbol: "exclamationmark.triangle.fill", name: host.name, headline: "is down",
                detail: "Its agents keep running. VibeOS reconnects on its own.",
                reason: host.detail.map(sentence), tone: .trouble))
    }
    // Short of memory: a warning, never a block. Nothing refuses a start
    // or a wake for memory; each agent's memory is only its ceiling.
    for host in hosts where host.link != .down {
        guard let why = host.memory?.pressure else { continue }
        let isMac = host.link == .local
        alerts.append(
            HeaderAlert(
                id: "memory:\(host.name)", symbol: "memorychip", name: isMac ? nil : host.name,
                headline: isMac ? "This Mac is short of memory" : "is short of memory",
                detail: "Agents keep running, but may slow down. Delete agents you're done with to free some memory.",
                reason: sentence(why), tone: .attention))
    }
    let ghAlert: (String, String)?
    switch gh?.state {
    case .loggedOut?: ghAlert = ("GitHub isn't signed in", "Agents on this Mac need it to push their work. Sign in from Settings → Hosts.")
    case .notInstalled?: ghAlert = ("GitHub's gh tool isn't installed", "Agents on this Mac need it to push their work. VibeOS is installing it.")
    case .error?: ghAlert = ("Can't check the GitHub login", "GitHub's gh tool didn't answer.")
    case .loggedIn?, .unknown?, nil: ghAlert = nil
    }
    if let ghAlert {
        alerts.append(
            HeaderAlert(
                id: "gh", symbol: "person.crop.circle.badge.exclamationmark", headline: ghAlert.0, detail: ghAlert.1,
                reason: gh?.detail, tone: .attention))
    }
    return alerts
}

/// A host's memory in a few words: `58G of 64G`, and its swap once there's
/// enough of it to matter: `58G of 64G · 3G swap`. A Mac nearly always has
/// a few MB swapped, which isn't worth the words.
public func memorySummary(_ memory: MemoryUse) -> String {
    var text = "\(formatBytes(memory.used)) of \(formatBytes(memory.total))"
    if memory.swapUsed >= swapWorthShowing { text += " · \(formatBytes(memory.swapUsed)) swap" }
    return text
}

/// The swap in use past which the header mentions it.
public let swapWorthShowing = 1 << 30

/// Every steady fact the header can show, in order: memory, `gh`, MCP, then
/// each Linux host. The Mac's memory is what vibed measures, or the app's
/// own reading until vibed says.
public func headerFacts(memory: HostMemory?, gh: GhStatus?, mcp: Bool, hosts: [HostInfo]) -> [HeaderFact] {
    var facts: [HeaderFact] = []
    let local = hosts.first { $0.link == .local }?.memory
    if let mac = local ?? memory.map({ MemoryUse(total: $0.total, used: $0.used) }) {
        facts.append(
            HeaderFact(
                id: "memory", label: "Memory", value: memorySummary(mac), symbol: "memorychip",
                tone: mac.pressure == nil ? .normal : .attention))
    }
    let gitHub: (String, StatusTone)
    switch gh?.state {
    case .loggedIn?: gitHub = (gh?.account ?? "Signed in", .normal)
    case .loggedOut?: gitHub = ("Signed out", .attention)
    case .notInstalled?: gitHub = ("Not installed", .attention)
    case .error?: gitHub = ("Can't check", .attention)
    case .unknown?, nil: gitHub = ("Checking…", .normal)
    }
    facts.append(HeaderFact(id: "gh", label: "GitHub", value: gitHub.0, symbol: "person.crop.circle", tone: gitHub.1))
    facts.append(HeaderFact(id: "mcp", label: "MCP", value: mcp ? "On" : "Off", symbol: "network", tone: .normal))
    for host in hosts where host.link != .local {
        var value = host.link == .down ? "Down" : "Connected"
        var tone: StatusTone = host.link == .down ? .trouble : .normal
        if host.link == .connected, let memory = host.memory {
            value += " · \(memorySummary(memory))"
            if tone == .normal, memory.pressure != nil { tone = .attention }
        }
        facts.append(
            HeaderFact(id: "host:\(host.name)", label: host.name, value: value, symbol: "server.rack", tone: tone))
    }
    return facts
}

// MARK: Memory in a row

/// What a row says about memory, beyond its quiet bar: only something
/// notable. Near the limit, how near; at it, that it's at it, as a state.
/// Paused there, the status line already says so: the bar just turns red,
/// with no words of its own.
public struct MemoryNote: Equatable, Sendable {
    /// The badge's words; nil when the status line says it.
    public var text: String?
    public var tone: StatusTone
}

public func memoryNote(_ agent: Agent) -> MemoryNote? {
    guard let gauge = MemoryGauge(agent) else { return nil }
    if gauge.fraction >= 0.995 {
        if agent.state == .paused { return MemoryNote(text: nil, tone: .trouble) }
        return MemoryNote(text: "At its memory limit", tone: .attention)
    }
    if gauge.fraction >= 0.8 {
        return MemoryNote(text: gauge.label.replacingOccurrences(of: " / ", with: " of "), tone: .attention)
    }
    return nil
}
