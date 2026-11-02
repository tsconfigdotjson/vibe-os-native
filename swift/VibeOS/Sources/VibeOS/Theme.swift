import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

// VibeOS's look, "Night Shift": one signal colour for anything alive, three
// semantic ones, SF Pro and SF Mono, a 4-point grid, and small, purposeful
// motion that stands still under Reduce Motion. The PR that brought it in
// ("Design pass: award-caliber polish") sets out the direction.

// MARK: Palette

extension NSColor {
    /// Phosphor: the lit windows of the app icon. It marks what's alive (a
    /// working agent, a connected host, MCP on), never text.
    static let vibeSignal = NSColor(name: NSColor.Name("VibeSignal")) { appearance in
        let match = appearance.bestMatch(from: [
            .aqua, .darkAqua, .accessibilityHighContrastAqua, .accessibilityHighContrastDarkAqua,
        ])
        switch match {
        case .darkAqua?: return NSColor(srgbRed: 0x3C / 255, green: 0xE3 / 255, blue: 0xB0 / 255, alpha: 1)
        case .accessibilityHighContrastDarkAqua?:
            return NSColor(srgbRed: 0x7A / 255, green: 0xF5 / 255, blue: 0xCF / 255, alpha: 1)
        case .accessibilityHighContrastAqua?:
            return NSColor(srgbRed: 0x00 / 255, green: 0x66 / 255, blue: 0x4F / 255, alpha: 1)
        default: return NSColor(srgbRed: 0x0B / 255, green: 0xA4 / 255, blue: 0x7F / 255, alpha: 1)
        }
    }
}

enum Palette {
    /// Alive: working, connected, on.
    static let signal = Color(nsColor: .vibeSignal)
    /// Needs you: a question, memory past 80%, a missing login.
    static let attention = Color.orange
    /// Stopped: paused, exited, a link down.
    static let trouble = Color.red
    /// At rest.
    static let rest = Color.gray
    /// The night of the app icon, for the mark.
    static let inkTop = Color(red: 0x26 / 255, green: 0x2B / 255, blue: 0x5E / 255)
    static let inkBottom = Color(red: 0x0A / 255, green: 0x0C / 255, blue: 0x1F / 255)
    static let moon = Color(red: 0xE6 / 255, green: 0xE4 / 255, blue: 0xFF / 255)
}

enum Metrics {
    /// The menu's width and side gutter.
    static let menuWidth: CGFloat = 360
    static let gutter: CGFloat = 14
    /// A highlighted row's inset from the menu's edge, and its corners.
    static let rowInset: CGFloat = 5
    static let rowRadius: CGFloat = 7
}

extension ExternalStatus.Tone {
    var color: Color {
        switch self {
        case .working: Palette.signal
        case .pending: .secondary
        case .trouble: Palette.attention
        }
    }
}

/// An animation, or none under Reduce Motion.
func motion(_ animation: Animation, reduce: Bool) -> Animation? {
    reduce ? nil : animation
}

// MARK: The state dot

/// An agent's state, as a dot (PRD: busy pulsing, question amber, idle
/// grey, asleep hollow, paused or exited red). Each has a shape of its own
/// as well as a colour: a working dot breathes inside a halo, a question
/// bounces once as it arrives, and a sleeping one is a ring.
struct StatusDot: View {
    let dot: StateDot
    var size: CGFloat = 9
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var bounce = false

    var body: some View {
        ZStack {
            switch dot {
            case .busy:
                if reduceMotion {
                    Circle().fill(Palette.signal)
                } else {
                    Circle().fill(Palette.signal)
                        .phaseAnimator([false, true]) { view, out in
                            view
                                .background {
                                    Circle()
                                        .fill(Palette.signal.opacity(out ? 0 : 0.45))
                                        .scaleEffect(out ? 2.1 : 1)
                                }
                                .opacity(out ? 0.75 : 1)
                        } animation: { out in
                            out ? Animation.easeOut(duration: 1.4) : Animation.easeIn(duration: 0.2)
                        }
                }
            case .question:
                Circle().fill(Palette.attention)
                    .scaleEffect(bounce ? 1.35 : 1)
            case .idle:
                Circle().fill(Palette.rest)
            case .asleep:
                Circle().strokeBorder(Color.secondary, lineWidth: max(1.25, size / 6))
            case .stopped:
                Circle().fill(Palette.trouble)
            }
        }
        .frame(width: size, height: size)
        .animation(motion(.smooth(duration: 0.3), reduce: reduceMotion), value: dot)
        .onChange(of: dot) { _, now in
            guard now == .question, !reduceMotion else { return }
            withAnimation(.spring(response: 0.22, dampingFraction: 0.45)) { bounce = true }
            Task {
                try? await Task.sleep(for: .milliseconds(180))
                withAnimation(.spring(response: 0.3, dampingFraction: 0.6)) { bounce = false }
            }
        }
        .accessibilityHidden(true)
    }
}

// MARK: Rows

/// The menu's rows, as buttons: a soft highlight on hover or keyboard
/// selection, and a slight give when pressed.
struct MenuRowStyle: ButtonStyle {
    var selected = false

    func makeBody(configuration: Configuration) -> some View {
        MenuRowBody(configuration: configuration, selected: selected)
    }
}

struct MenuRowBody: View {
    let configuration: ButtonStyleConfiguration
    let selected: Bool
    @State private var hovering = false
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorSchemeContrast) private var contrast

    var body: some View {
        configuration.label
            .contentShape(Rectangle())
            .background {
                RoundedRectangle(cornerRadius: Metrics.rowRadius, style: .continuous)
                    .fill(Color.primary.opacity(fill))
                    .padding(.horizontal, Metrics.rowInset)
            }
            .scaleEffect(configuration.isPressed && !reduceMotion ? 0.985 : 1)
            .animation(motion(.snappy(duration: 0.16), reduce: reduceMotion), value: hovering)
            .animation(motion(.snappy(duration: 0.12), reduce: reduceMotion), value: configuration.isPressed)
            .onHover { hovering = $0 }
    }

    private var fill: Double {
        guard isEnabled else { return 0 }
        let strong = contrast == .increased
        if configuration.isPressed { return strong ? 0.24 : 0.13 }
        return hovering || selected ? (strong ? 0.16 : 0.075) : 0
    }
}

/// The same highlight as `MenuRowStyle`, for a row that isn't a button,
/// such as a menu.
struct HoverRow: ViewModifier {
    @State private var hovering = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        content
            .contentShape(Rectangle())
            .background {
                RoundedRectangle(cornerRadius: Metrics.rowRadius, style: .continuous)
                    .fill(Color.primary.opacity(hovering ? 0.075 : 0))
                    .padding(.horizontal, Metrics.rowInset)
            }
            .animation(motion(.snappy(duration: 0.16), reduce: reduceMotion), value: hovering)
            .onHover { hovering = $0 }
    }
}

/// Lays its views out in lines, wrapping as a paragraph does: the header's
/// chips.
struct Flow: Layout {
    var spacing: CGFloat = 6
    var lineSpacing: CGFloat = 5

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? .infinity
        var x: CGFloat = 0
        var y: CGFloat = 0
        var line: CGFloat = 0
        var widest: CGFloat = 0
        for view in subviews {
            let size = view.sizeThatFits(.unspecified)
            if x > 0, x + size.width > width {
                x = 0
                y += line + lineSpacing
                line = 0
            }
            widest = max(widest, x + size.width)
            x += size.width + spacing
            line = max(line, size.height)
        }
        return CGSize(width: proposal.width ?? widest, height: y + line)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var x = bounds.minX
        var y = bounds.minY
        var line: CGFloat = 0
        for view in subviews {
            let size = view.sizeThatFits(.unspecified)
            if x > bounds.minX, x + size.width > bounds.maxX {
                x = bounds.minX
                y += line + lineSpacing
                line = 0
            }
            view.place(at: CGPoint(x: x, y: y), anchor: .topLeading, proposal: ProposedViewSize(size))
            x += size.width + spacing
            line = max(line, size.height)
        }
    }
}

/// A footer row that reads like a menu item: a symbol, a title, and its
/// shortcut or a chevron.
struct MenuItemLabel: View {
    let title: String
    let symbol: String
    var trailing: String?
    var trailingSymbol: String?
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        HStack(spacing: 9) {
            Image(systemName: symbol)
                .symbolRenderingMode(.hierarchical)
                .frame(width: 18)
                .foregroundStyle(.secondary)
            Text(title)
            Spacer(minLength: 8)
            if let trailing {
                Text(trailing)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
            }
            if let trailingSymbol {
                Image(systemName: trailingSymbol)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .accessibilityHidden(true)
            }
        }
        .foregroundStyle(isEnabled ? Color.primary : Color.secondary)
        .padding(.horizontal, Metrics.gutter)
        .padding(.vertical, 5)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A small capsule of metadata: a profile, a link, MCP. It leads with a
/// symbol, or with a coloured dot.
struct Chip: View {
    let text: String
    var symbol: String?
    var dot: Color?
    var tint: Color = .secondary
    @Environment(\.colorSchemeContrast) private var contrast

    var body: some View {
        HStack(spacing: 4) {
            if let dot {
                Circle().fill(dot).frame(width: 6, height: 6)
            } else if let symbol {
                Image(systemName: symbol).imageScale(.small).foregroundStyle(tint)
            }
            Text(text).lineLimit(1)
        }
        .font(.caption2.weight(.medium))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(Capsule().fill(Color.primary.opacity(contrast == .increased ? 0.12 : 0.06)))
        .overlay {
            if contrast == .increased {
                Capsule().strokeBorder(Color.primary.opacity(0.4), lineWidth: 1)
            }
        }
    }
}

/// A thin memory bar: phosphor while there's room, amber past 80%.
struct MemoryBar: View {
    let gauge: MemoryGauge

    var body: some View {
        HStack(spacing: 6) {
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.1))
                    Capsule()
                        .fill(gauge.fraction > 0.8 ? Palette.attention : Palette.signal)
                        .frame(width: max(3, proxy.size.width * gauge.fraction))
                }
            }
            .frame(width: 44, height: 4)
            Text(gauge.label)
                .font(.caption2)
                .monospacedDigit()
                .foregroundStyle(.secondary)
        }
        .accessibilityHidden(true)
    }
}

// MARK: Messages

/// A note in the menu or a window: what went wrong and why, dismissable.
struct InlineMessage: View {
    let text: String
    var symbol = "exclamationmark.triangle.fill"
    var tint: Color = Palette.attention
    var dismiss: (() -> Void)?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .accessibilityHidden(true)
            Text(text)
                .font(.callout)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            Spacer(minLength: 0)
            if let dismiss {
                Button(action: dismiss) {
                    Image(systemName: "xmark")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .padding(4)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Dismiss")
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(tint.opacity(0.12)))
        .accessibilityElement(children: .combine)
    }
}

/// An error line in a form, if there's one.
struct ErrorText: View {
    let error: String?

    var body: some View {
        if let error {
            Label {
                Text(sentence(error)).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            } icon: {
                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Palette.attention)
            }
            .font(.callout)
        }
    }
}

/// Work under way: a small spinner and what it's doing.
struct Working: View {
    let text: String

    var body: some View {
        HStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text(text).font(.callout).foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}

// MARK: Controls

/// Copies `text`, and says so: the button turns into a check for a moment.
struct CopyButton: View {
    let title: String
    let text: String
    var prominent = false
    @State private var copied = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let button = Button {
            WorktreeActions.copy(text)
            withAnimation(motion(.snappy, reduce: reduceMotion)) { copied = true }
            Task {
                try? await Task.sleep(for: .seconds(1.6))
                withAnimation(motion(.snappy, reduce: reduceMotion)) { copied = false }
            }
        } label: {
            Label(copied ? "Copied" : title, systemImage: copied ? "checkmark" : "doc.on.doc")
                .contentTransition(.symbolEffect(.replace))
        }
        .accessibilityLabel(copied ? "Copied" : title)
        if prominent {
            button.buttonStyle(.borderedProminent)
        } else {
            button
        }
    }
}

/// A text field that saves itself: on Return, or when focus leaves it. A
/// value it can't take shows red and isn't sent; a saved one shows a check
/// for a moment. No Save button, as in System Settings.
struct CommitField: View {
    let title: String
    let value: String
    var prompt: String?
    var width: CGFloat = 120
    var help: String?
    let isValid: (String) -> Bool
    /// Saves the text; true if it took.
    let commit: (String) async -> Bool
    @State private var text = ""
    @State private var saved = false
    /// A save under way: Return then a blur sends it once.
    @State private var saving = false
    @FocusState private var focused: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "checkmark.circle.fill")
                .foregroundStyle(Palette.signal)
                .opacity(saved ? 1 : 0)
                .scaleEffect(saved || reduceMotion ? 1 : 0.6)
                .accessibilityHidden(true)
            TextField(title, text: $text, prompt: prompt.map { Text($0) })
                .labelsHidden()
                .multilineTextAlignment(.trailing)
                .monospacedDigit()
                .foregroundStyle(isValid(text) ? Color.primary : Palette.trouble)
                .frame(width: width)
                .focused($focused)
                .onSubmit { Task { await save() } }
                .help(help ?? "")
                .accessibilityValue(isValid(text) ? text : "\(text), not valid")
        }
        .onAppear { text = value }
        .onChange(of: value) { _, now in text = now }
        .onChange(of: focused) { _, now in
            if !now { Task { await save() } }
        }
    }

    private func save() async {
        guard !saving, text != value, isValid(text) else { return }
        saving = true
        let took = await commit(text)
        saving = false
        guard took else { return }
        withAnimation(motion(.spring(response: 0.3, dampingFraction: 0.6), reduce: reduceMotion)) { saved = true }
        try? await Task.sleep(for: .seconds(1.4))
        withAnimation(motion(.easeOut(duration: 0.3), reduce: reduceMotion)) { saved = false }
    }
}

/// A coloured tile behind a symbol, as System Settings' sidebar has.
struct IconTile: View {
    let symbol: String
    let color: Color
    var size: CGFloat = 20

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: size * 0.56, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: size, height: size)
            .background(
                RoundedRectangle(cornerRadius: size * 0.26, style: .continuous)
                    .fill(color.gradient)
            )
            .accessibilityHidden(true)
    }
}

// MARK: The mark

/// The app icon's four windows, drawn: three lit by agents at work, one
/// asleep with a moon. `lit` lights them in the menu bar glyph's order (top
/// left, bottom left, bottom right, then top right), so the welcome can
/// light one per setup step done.
struct BrandMark: View {
    var lit = 3
    var size: CGFloat = 56
    /// The night behind the windows, as on the icon; off, it sits on
    /// whatever is behind it.
    var night = true
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private static let order = [(0, 0), (0, 1), (1, 1), (1, 0)]

    var body: some View {
        let cell = size * 0.36
        let gap = size * 0.075
        ZStack {
            if night {
                RoundedRectangle(cornerRadius: size * 0.225, style: .continuous)
                    .fill(LinearGradient(colors: [Palette.inkTop, Palette.inkBottom], startPoint: .top, endPoint: .bottom))
                    .shadow(color: .black.opacity(0.25), radius: size * 0.04, y: size * 0.02)
            }
            Grid(horizontalSpacing: gap, verticalSpacing: gap) {
                ForEach(0..<2, id: \.self) { row in
                    GridRow {
                        ForEach(0..<2, id: \.self) { column in
                            window(on: isLit(column: column, row: row), cell: cell)
                        }
                    }
                }
            }
        }
        .frame(width: size, height: size)
        .animation(motion(.smooth(duration: 0.5), reduce: reduceMotion), value: lit)
        .accessibilityHidden(true)
    }

    private func isLit(column: Int, row: Int) -> Bool {
        guard let index = Self.order.firstIndex(where: { $0 == (column, row) }) else { return false }
        return index < lit
    }

    @ViewBuilder
    private func window(on: Bool, cell: CGFloat) -> some View {
        let shape = RoundedRectangle(cornerRadius: cell * 0.24, style: .continuous)
        ZStack {
            shape.fill(night ? Color.black.opacity(0.35) : Color.primary.opacity(0.08))
            shape.strokeBorder(Color.white.opacity(night ? 0.14 : 0), lineWidth: 1)
            if !on {
                Image(systemName: "moon.fill")
                    .font(.system(size: cell * 0.42))
                    .foregroundStyle(night ? Palette.moon : Color.secondary)
            }
            shape
                .fill(
                    LinearGradient(
                        colors: [Color(red: 0.55, green: 0.98, blue: 0.84), Color(red: 0.12, green: 0.76, blue: 0.59)],
                        startPoint: .top, endPoint: .bottom)
                )
                .shadow(color: Palette.signal.opacity(0.6), radius: cell * 0.18)
                .opacity(on ? 1 : 0)
        }
        .frame(width: cell, height: cell)
    }
}
