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
    if status.stopReason == .usageLimit {
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

/// What the Linux toolbox menu says about the toolbox (PRD: The toolbox
/// shell): the versions new agents boot, what it's doing, a newer base
/// image that Rebuild template would move to, and how the last run went.
public func toolboxLines(_ status: ToolboxStatus?) -> [String] {
    guard let status else { return [] }
    guard status.state != .unavailable else { return ["This host has no Linux toolbox"] }
    var lines: [String] = []
    var versions: [String] = []
    if let template = status.template { versions.append("template v\(template)") }
    if let home = status.home { versions.append("Linux home v\(home)") }
    if let base = status.base { versions.append("base \(base)") }
    if !versions.isEmpty { lines.append("New agents boot " + versions.joined(separator: ", ")) }
    switch status.state {
    case .open: lines.append("The shell is open: exit it to save")
    case .saving: lines.append("Saving the template and Linux home…")
    case .updating: lines.append("Running toolbox.toml's [update]…")
    case .rebuilding: lines.append("Rebuilding the template…")
    case .closed, .unavailable: break
    }
    if let newest = status.newestBase, newest > (status.base ?? 0) {
        lines.append("Base \(newest) is installed: Rebuild Template moves to it")
    }
    if let last = status.last {
        let what: String
        switch last.action {
        case .shell: what = "shell"
        case .update: what = "update"
        case .rebuild: what = "rebuild"
        }
        if let error = last.error {
            lines.append("The last \(what) failed: \(error)")
        } else if !last.uncovered.isEmpty {
            lines.append("Not in toolbox.toml: " + last.uncovered.joined(separator: ", "))
        }
    }
    return lines
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

/// What See Logins says (PRD: Menu bar, Footer; Core concepts, Linux
/// home): what every new linux agent starts logged in to, or plainly that
/// there's nothing yet, or that it isn't known, and Claude's login.
public struct LoginsSummary: Equatable, Sendable {
    public struct Row: Equatable, Sendable, Identifiable {
        public var tool: String
        public var account: String
        public var id: String { tool + "\u{0}" + account }
    }

    public var headline: String
    public var rows: [Row]
    public var claude: String
    /// Whether linux agents still need the Claude login.
    public var needsClaude: Bool

    public init(_ logins: ToolboxLogins) {
        rows = (logins.logins ?? []).map { Row(tool: toolName($0.tool), account: $0.account) }
        switch (logins.home, logins.logins) {
        case (nil, _):
            headline = "There's no Linux home yet. Open the toolbox shell and log in to what your linux "
                + "agents need, such as gh auth login. Exiting the shell saves it."
        case (let home?, nil):
            headline = "VibeOS couldn't tell which logins Linux home \(home) holds when it was saved "
                + "(the look failed, or an older VibeOS saved it). Open the toolbox shell and exit it, "
                + "and they'll show here."
        case (let home?, let found?) where found.isEmpty:
            headline = "VibeOS found no gh, gcloud, git or SSH logins in Linux home \(home). Open the "
                + "toolbox shell and log in there (gh auth login, gcloud auth login, "
                + "git config --global user.name …). Exiting the shell saves them."
        case (let home?, _):
            headline = "Every new linux agent starts with what Linux home \(home) holds. "
                + "Its gh, gcloud, git and SSH logins:"
        }
        needsClaude = !logins.claude
        if logins.claude {
            claude = "Claude: linux agents are logged in."
        } else if let why = logins.claudeProblem {
            claude = "Claude: \(why)"
        } else {
            claude = "Claude: linux agents aren't logged in yet."
        }
    }
}

// MARK: External access

/// How the external address is doing, as the menu and Settings say it
/// (PRD: External access).
public struct ExternalStatus: Equatable, Sendable {
    public enum Tone: Equatable, Sendable {
        /// Outside clients reach it.
        case working
        /// On its way: the tunnel is opening, or a certificate is coming.
        case pending
        /// Something keeps it from working; `detail` says what.
        case trouble
    }

    /// A few words, for the menu: "public", "connecting…".
    public var short: String
    /// A sentence, for Settings.
    public var summary: String
    /// What's wrong or what it waits for, and how to fix it, as the host or
    /// the tunnel says.
    public var detail: String?
    /// An SF Symbol.
    public var symbol: String
    public var tone: Tone
}

/// How `host` is doing as the external address, if it is the one.
public func externalStatus(_ host: HostInfo) -> ExternalStatus? {
    guard host.external else { return nil }
    let detail = host.externalDetail
    switch host.tunnel {
    case .up? where detail == nil:
        return ExternalStatus(
            short: "public", summary: "Outside MCP clients reach agents on \(host.name) and on this Mac.",
            detail: nil, symbol: "globe", tone: .working)
    case .up?:
        return ExternalStatus(
            short: "almost public", summary: "The tunnel is up; \(host.name) isn't serving yet.",
            detail: detail, symbol: "globe", tone: .pending)
    case .connecting?:
        return ExternalStatus(
            short: "connecting…", summary: "Opening the tunnel from this Mac to \(host.name)…",
            detail: detail, symbol: "globe", tone: .pending)
    case .down?, nil:
        return ExternalStatus(
            short: "not reachable", summary: "Outside clients can't reach this Mac's agents through \(host.name) yet.",
            detail: detail, symbol: "exclamationmark.triangle.fill", tone: .trouble)
    }
}

/// The public name a server is likely reached at, from its SSH destination:
/// `me@vps.example.com:22` gives `vps.example.com`. Empty when it's an IP
/// address or a single name from `~/.ssh/config`.
public func suggestedDomain(fromSSH address: String) -> String {
    var host = address
    if host.hasPrefix("ssh://") { host.removeFirst("ssh://".count) }
    if let at = host.lastIndex(of: "@") { host = String(host[host.index(after: at)...]) }
    if host.hasPrefix("[") { return "" }
    if let colon = host.firstIndex(of: ":") { host = String(host[..<colon]) }
    return normalizedDomain(host) ?? ""
}

/// `text` as the public DNS name Let's Encrypt can certify, in lowercase,
/// or nil if it isn't one: as vibed checks it (`validate_domain`).
public func normalizedDomain(_ text: String) -> String? {
    var name = text.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    if name.hasSuffix(".") { name.removeLast() }
    let labels = name.split(separator: ".", omittingEmptySubsequences: false)
    guard name.count <= 253, labels.count >= 2 else { return nil }
    let allowed = Set("abcdefghijklmnopqrstuvwxyz0123456789-")
    for label in labels {
        guard !label.isEmpty, label.count <= 63, !label.hasPrefix("-"), !label.hasSuffix("-"),
            label.allSatisfy(allowed.contains)
        else { return nil }
    }
    // An IPv4 address's last part is all digits; a name's never is.
    if let last = labels.last, last.allSatisfy(\.isNumber) { return nil }
    return name
}
