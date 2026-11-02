import AppKit
import VibeRPC

/// What the menu bar icon shows (PRD: Menu bar icon): the app icon's four
/// windows, lit one by one as agents wake. While any agent has a question,
/// the top right window is an amber dot; while any is paused by a limit,
/// the bottom right one is a red dot. The count of awake agents sits beside
/// it as text.
public struct GlyphState: Equatable, Sendable {
    /// Windows lit, 0 to 4: one per awake agent, all four from four up.
    public var lit: Int
    public var connected: Bool
    public var question: Bool
    public var paused: Bool

    public init(lit: Int, connected: Bool = true, question: Bool = false, paused: Bool = false) {
        self.lit = max(0, min(lit, 4))
        self.connected = connected
        self.question = connected && question
        self.paused = connected && paused
    }

    public init(icon: IconSummary, connected: Bool) {
        self.init(lit: connected ? icon.awake : 0, connected: connected, question: icon.question, paused: icon.paused)
    }

    /// A badge needs colour, so the image can't be a template then.
    public var badged: Bool { question || paused }

    /// What VoiceOver says for the icon.
    public func accessibilityLabel(awake: Int) -> String {
        guard connected else { return "VibeOS, can't reach vibed" }
        var parts = ["VibeOS", awake == 1 ? "1 agent awake" : "\(awake) agents awake"]
        if question { parts.append("a question is waiting") }
        if paused { parts.append("an agent is paused") }
        return parts.joined(separator: ", ")
    }
}

/// Draws the menu bar glyph. It's drawn, not an asset: it stays sharp at
/// every scale, needs no resource bundle, and the windows can light up.
public enum MenuBarGlyph {
    /// The glyph's size in points: a menu bar icon's.
    public static let size = NSSize(width: 16, height: 16)

    /// The windows in the order they light: top left, bottom left, bottom
    /// right, then top right, the one the app icon shows asleep.
    static let order: [(column: Int, row: Int)] = [(0, 0), (0, 1), (1, 1), (1, 0)]
    static let cell: CGFloat = 6.5
    static let gap: CGFloat = 2
    static let inset: CGFloat = 0.5
    static let radius: CGFloat = 1.9

    /// A window's rectangle, in top-left-origin points.
    static func rect(column: Int, row: Int) -> NSRect {
        NSRect(
            x: inset + CGFloat(column) * (cell + gap), y: inset + CGFloat(row) * (cell + gap),
            width: cell, height: cell)
    }

    /// Where a badge goes: it takes over a window of the right column, as a
    /// round light, the question's top right and the pause's bottom right,
    /// so each reads by place and shape as well as colour.
    static func badgeRect(question: Bool) -> NSRect {
        rect(column: 1, row: question ? 0 : 1)
    }

    /// The glyph as an image. Without a badge it's a template, so the menu
    /// bar tints it as it does its own icons. A badge's colour needs a plain
    /// image, whose windows can't follow the menu bar on their own: they're
    /// drawn for `onDark`, the menu bar's look (MenuBarAppearance in the
    /// app), white on a dark bar and black on a light one. Drawing them in
    /// `labelColor` came out black on a dark bar, as the image is drawn
    /// against the app's appearance, not the menu bar's.
    public static func image(_ state: GlyphState, awake: Int? = nil, onDark: Bool = false) -> NSImage {
        let image = NSImage(size: size, flipped: true) { _ in
            draw(state, onDark: onDark)
            return true
        }
        image.isTemplate = !state.badged
        image.accessibilityDescription = state.accessibilityLabel(awake: awake ?? state.lit)
        return image
    }

    /// The windows' colour: a template's is ignored, so black; a badged
    /// glyph's is the menu bar's own text colour.
    static func ink(_ state: GlyphState, onDark: Bool) -> NSColor {
        state.badged && onDark ? .white : .black
    }

    /// Draws into the current context, top-left origin.
    static func draw(_ state: GlyphState, onDark: Bool = false) {
        let ink = Self.ink(state, onDark: onDark)
        NSGraphicsContext.saveGraphicsState()
        let alpha: CGFloat = state.connected ? 1 : 0.45
        for (index, window) in order.enumerated() {
            let r = rect(column: window.column, row: window.row)
            // A badge's window shows the badge instead.
            if window.column == 1, (window.row == 0 && state.question) || (window.row == 1 && state.paused) {
                continue
            }
            if index < state.lit {
                ink.withAlphaComponent(alpha).setFill()
                NSBezierPath(roundedRect: r, xRadius: radius, yRadius: radius).fill()
            } else {
                let line: CGFloat = 1.2
                let path = NSBezierPath(
                    roundedRect: r.insetBy(dx: line / 2, dy: line / 2), xRadius: radius - line / 2,
                    yRadius: radius - line / 2)
                path.lineWidth = line
                ink.withAlphaComponent(alpha).setStroke()
                path.stroke()
            }
        }
        if !state.connected {
            // Struck through: vibed can't be reached.
            let slash = NSBezierPath()
            slash.move(to: NSPoint(x: 1.5, y: size.height - 1.5))
            slash.line(to: NSPoint(x: size.width - 1.5, y: 1.5))
            slash.lineWidth = 1.6
            slash.lineCapStyle = .round
            ink.setStroke()
            slash.stroke()
        }
        NSGraphicsContext.restoreGraphicsState()

        if state.question {
            NSColor.systemOrange.setFill()
            NSBezierPath(ovalIn: badgeRect(question: true).insetBy(dx: -0.25, dy: -0.25)).fill()
        }
        if state.paused {
            NSColor.systemRed.setFill()
            NSBezierPath(ovalIn: badgeRect(question: false).insetBy(dx: -0.25, dy: -0.25)).fill()
        }
    }
}
