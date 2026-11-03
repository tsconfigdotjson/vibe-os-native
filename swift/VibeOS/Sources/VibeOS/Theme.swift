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
    /// Phosphor: the lit windows of the brand mark. It marks what's alive (a
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
    /// The night behind the brand mark.
    static let inkTop = Color(red: 0x26 / 255, green: 0x2B / 255, blue: 0x5E / 255)
    static let inkBottom = Color(red: 0x0A / 255, green: 0x0C / 255, blue: 0x1F / 255)
    static let moon = Color(red: 0xE6 / 255, green: 0xE4 / 255, blue: 0xFF / 255)
}

enum Metrics {
    /// Every grouped box, card and field: one corner radius.
    static let cardRadius: CGFloat = 10
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

/// A footer row that reads like a menu item: a symbol, a title, and its
/// shortcut.
struct MenuItemLabel: View {
    let title: String
    let symbol: String
    var trailing: String?
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
        }
        .foregroundStyle(isEnabled ? Color.primary : Color.secondary)
        .padding(.horizontal, Metrics.gutter)
        .padding(.vertical, 5)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A row's memory: a thin, quiet bar against the profile's limit (PRD:
/// Menu bar, agent rows). Near the limit it becomes a state, an amber
/// badge saying how near; at the limit it says so. Paused there, the bar
/// only turns red: the status line says the rest.
struct MemoryBar: View {
    let gauge: MemoryGauge
    var note: MemoryNote?

    var body: some View {
        let tint = note?.tone == .trouble ? Palette.trouble : Palette.attention
        let badge = note?.text != nil
        HStack(spacing: 5) {
            if let text = note?.text {
                Text(text)
                    .font(.caption.weight(.medium))
                    .monospacedDigit()
                    .foregroundStyle(tint)
            }
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.12))
                    Capsule()
                        .fill(note == nil ? Color.primary.opacity(0.35) : tint)
                        .frame(width: max(3, proxy.size.width * gauge.fraction))
                }
            }
            .frame(width: 32, height: 3)
        }
        .padding(.horizontal, badge ? 7 : 0)
        .padding(.vertical, badge ? 2 : 0)
        .background {
            if badge { Capsule().fill(tint.opacity(0.14)) }
        }
        .help("Memory: \(gauge.label.replacingOccurrences(of: " / ", with: " of "))")
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
        .background(RoundedRectangle(cornerRadius: Metrics.cardRadius, style: .continuous).fill(tint.opacity(0.12)))
        // Contain, not combine, so Dismiss stays a button of its own.
        .accessibilityElement(children: .contain)
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
    @State private var reset: Task<Void, Never>?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let button = Button {
            WorktreeActions.copy(text)
            withAnimation(motion(.snappy, reduce: reduceMotion)) { copied = true }
            reset?.cancel()
            reset = Task {
                try? await Task.sleep(for: .seconds(1.6))
                guard !Task.isCancelled else { return }
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
    /// Numbers line up on the right; words read from the left.
    var trailing = true
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
            // Bordered, so it reads as a field wherever it sits: a grouped
            // form otherwise draws its text fields as plain text.
            TextField(title, text: $text, prompt: prompt.map { Text($0) })
                .textFieldStyle(.roundedBorder)
                .labelsHidden()
                .multilineTextAlignment(trailing ? .trailing : .leading)
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
        let sent = text
        let took = await commit(sent)
        saving = false
        guard took else {
            // Show what's really saved; the form says why it didn't take.
            if text == sent { text = value }
            return
        }
        // Edited again while that was on its way: send the newer text too.
        if text != sent {
            await save()
            return
        }
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

/// The menu bar glyph's four windows, drawn large: three lit by agents at
/// work, one asleep with a moon. `lit` lights them in the glyph's order (top
/// left, bottom left, bottom right, then top right), so the welcome can
/// light one per setup step done.
struct BrandMark: View {
    var lit = 3
    var size: CGFloat = 56
    /// The night behind the windows; off, it sits on whatever is behind it.
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

// MARK: Scrollers

/// Gives a scroll view overlay scrollers, which show only while scrolling,
/// as the menu and the prompt want, unless the owner set "Show scroll bars"
/// to Always (System Settings → Appearance), which it leaves alone. Put it
/// in a ScrollView's content (`.enclosing`), or beside a TextEditor
/// (`.nearbyTextView`), whose scroll view it finds among its neighbours.
/// It sets the style again when the system's preferred style changes, as
/// when a mouse is plugged in, and on every update.
struct OverlayScrollers: NSViewRepresentable {
    enum Target {
        case enclosing, nearbyTextView
    }

    var target: Target = .enclosing

    func makeNSView(context: Context) -> Probe { Probe(target: target) }

    func updateNSView(_ view: Probe, context: Context) { view.apply() }

    final class Probe: NSView {
        let target: Target

        init(target: Target) {
            self.target = target
            super.init(frame: .zero)
            NotificationCenter.default.addObserver(
                self, selector: #selector(styleChanged), name: NSScroller.preferredScrollerStyleDidChangeNotification,
                object: nil)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { fatalError("not from a nib") }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            apply()
            // SwiftUI may set its scroll view up after this view arrives.
            DispatchQueue.main.async { [weak self] in MainActor.assumeIsolated { self?.apply() } }
        }

        @objc private func styleChanged(_ notification: Notification) {
            // After the scroll view's own response to the same change.
            DispatchQueue.main.async { [weak self] in MainActor.assumeIsolated { self?.apply() } }
        }

        func apply() {
            if UserDefaults.standard.string(forKey: "AppleShowScrollBars") == "Always" { return }
            for scroll in scrollViews() {
                if scroll.scrollerStyle != .overlay { scroll.scrollerStyle = .overlay }
                scroll.autohidesScrollers = true
            }
        }

        private func scrollViews() -> [NSScrollView] {
            switch target {
            case .enclosing:
                return enclosingScrollView.map { [$0] } ?? []
            case .nearbyTextView:
                var ancestor = superview
                for _ in 0..<4 {
                    guard let view = ancestor else { break }
                    let found = Self.textScrollViews(in: view)
                    if !found.isEmpty { return found }
                    ancestor = view.superview
                }
                return []
            }
        }

        private static func textScrollViews(in view: NSView) -> [NSScrollView] {
            var found: [NSScrollView] = []
            for sub in view.subviews {
                if let scroll = sub as? NSScrollView, scroll.documentView is NSTextView {
                    found.append(scroll)
                } else {
                    found += textScrollViews(in: sub)
                }
            }
            return found
        }
    }
}

// MARK: Aligned forms

/// A form row: its label right-aligned in a column of `labelWidth`, its
/// control leading-aligned at `controlWidth`, so every row's label ends and
/// every control starts at the same x. Laid out explicitly, rather than by
/// a form style, so it comes out the same everywhere.
struct FormRow<Content: View>: View {
    let label: String
    var labelWidth: CGFloat = 92
    var controlWidth: CGFloat = 260
    @ViewBuilder let content: Content

    var body: some View {
        HStack(alignment: .center, spacing: 8) {
            Text(label)
                .foregroundStyle(.secondary)
                .frame(width: labelWidth, alignment: .trailing)
                .accessibilityHidden(true)
            content
                .frame(width: controlWidth, alignment: .leading)
            Spacer(minLength: 0)
        }
    }
}

/// A native pop-up button that takes the width it's given, not its own:
/// SwiftUI's menu Picker sizes to its longest item and centres in a wider
/// frame, so pop-ups in a column came out at different widths.
struct PopUp<Value: Hashable>: NSViewRepresentable {
    struct Item {
        let title: String
        let value: Value
        var symbol: String?
    }

    let label: String
    let items: [Item]
    @Binding var selection: Value

    func makeNSView(context: Context) -> NSPopUpButton {
        let button = NSPopUpButton(frame: .zero, pullsDown: false)
        button.target = context.coordinator
        button.action = #selector(PopUpTarget.changed(_:))
        button.setContentHuggingPriority(.defaultLow, for: .horizontal)
        button.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        return button
    }

    func updateNSView(_ button: NSPopUpButton, context: Context) {
        let items = self.items
        let selection = $selection
        context.coordinator.select = { index in
            if items.indices.contains(index) { selection.wrappedValue = items[index].value }
        }
        button.setAccessibilityLabel(label)
        // The menu is made again only when its items change, not on every
        // update (each keystroke in the prompt is one), so an open menu
        // stays as it is. Items go in as menu items, as addItem(withTitle:)
        // drops a repeated title.
        let shape = items.map { [$0.title, $0.symbol ?? ""] }
        if shape != context.coordinator.shape {
            context.coordinator.shape = shape
            let menu = NSMenu()
            for item in items {
                let entry = NSMenuItem(title: item.title, action: nil, keyEquivalent: "")
                if let symbol = item.symbol {
                    entry.image = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)
                }
                menu.addItem(entry)
            }
            button.menu = menu
        }
        if let index = items.firstIndex(where: { $0.value == self.selection }) {
            button.selectItem(at: index)
        }
        button.isEnabled = context.environment.isEnabled
    }

    /// The width proposed, if it's a real one; its own otherwise.
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: NSPopUpButton, context: Context) -> CGSize? {
        let natural = nsView.intrinsicContentSize
        let width = proposal.width.flatMap { $0.isFinite ? $0 : nil } ?? natural.width
        return CGSize(width: width, height: natural.height)
    }

    func makeCoordinator() -> PopUpTarget { PopUpTarget() }
}

/// A PopUp's target: what choosing an item does, and the items its menu
/// was last made from.
@MainActor
final class PopUpTarget: NSObject {
    var select: @MainActor (Int) -> Void = { _ in }
    var shape: [[String]] = []

    @objc func changed(_ sender: NSPopUpButton) {
        select(sender.indexOfSelectedItem)
    }
}
