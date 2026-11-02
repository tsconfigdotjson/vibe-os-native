import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

// Every surface of the app, drawn on canned data (FixtureDaemon) in light
// and dark, for design review and for check-full.sh:
//
//   VibeOS --gallery [--appearance light|dark] [--surface <name>]…
//       opens each surface in a real window and prints a line per window,
//       `window <surface>-<appearance> <window number>`, then `ready` once
//       they've loaded. scripts/surfaces.sh screenshots each with
//       `screencapture -l`, materials and all. Runs until it's killed.
//   VibeOS --render-surfaces <dir> [--appearance …] [--surface …]
//       draws each into <dir>/<surface>-<appearance>.png offscreen (no
//       materials), lists them in <dir>/manifest.txt, and exits non-zero if
//       one came out blank.

struct GalleryRequest {
    enum Mode: Equatable {
        case show
        case render(URL)
    }

    var mode: Mode
    var appearances: [GalleryAppearance] = GalleryAppearance.allCases
    /// Only these surfaces, if any are named.
    var only: Set<String> = []

    init?(arguments: [String]) {
        if let i = arguments.firstIndex(of: "--render-surfaces"), arguments.indices.contains(i + 1) {
            mode = .render(URL(fileURLWithPath: arguments[i + 1], isDirectory: true))
        } else if arguments.contains("--gallery") {
            mode = .show
        } else {
            return nil
        }
        for (i, word) in arguments.enumerated() where arguments.indices.contains(i + 1) {
            let value = arguments[i + 1]
            if word == "--appearance", let appearance = GalleryAppearance(rawValue: value) {
                appearances = [appearance]
            } else if word == "--surface" {
                only.insert(value)
            }
        }
    }
}

enum GalleryAppearance: String, CaseIterable {
    case light, dark

    var appearance: NSAppearance? {
        NSAppearance(named: self == .light ? .aqua : .darkAqua)
    }
}

/// The fixture models every surface draws from.
@MainActor
struct GalleryModels {
    let busy: MenuModel
    let quiet: MenuModel
    let firstRun: MenuModel
    let offline: MenuModel

    static func load() async -> GalleryModels {
        let busy = await MenuModel.fixture(.busy)
        let quiet = await MenuModel.fixture(.quiet)
        let firstRun = await MenuModel.fixture(.firstRun)
        let offline = await MenuModel.fixture(.offline)
        return GalleryModels(busy: busy, quiet: quiet, firstRun: firstRun, offline: offline)
    }
}

/// One surface: its name, how it's framed, and the view.
@MainActor
struct GallerySurface {
    enum Frame {
        /// The menu bar's window: the menu material, no title bar.
        case menu
        /// A titled window, sized to fit or to a size.
        case window(String, CGSize?)
    }

    let name: String
    let frame: Frame
    let view: AnyView

    static func all(_ m: GalleryModels) -> [GallerySurface] {
        let fixture = FixtureDaemon(.busy)
        let install = try? fixture.call(HostsAdd.self, HostAddParams(name: "vps", address: "me@vps.example.com"))
        let key = try? fixture.call(KeysCreate.self, KeyCreateParams(name: "laptop Claude Code"))
        let settings = CGSize(width: 780, height: 600)
        var surfaces: [GallerySurface] = [
            GallerySurface(name: "menu", frame: .menu, view: AnyView(MenuView(model: m.busy))),
            GallerySurface(name: "menu-welcome", frame: .menu, view: AnyView(MenuView(model: m.firstRun))),
            GallerySurface(name: "menu-quiet", frame: .menu, view: AnyView(MenuView(model: m.quiet))),
            GallerySurface(name: "menu-offline", frame: .menu, view: AnyView(MenuView(model: m.offline))),
            GallerySurface(name: "menu-bar", frame: .window("Menu Bar Glyph", nil), view: AnyView(GlyphSheet())),
            GallerySurface(
                name: "new-agent", frame: .window("New Agent", nil), view: AnyView(NewAgentView(model: m.busy))),
            GallerySurface(
                name: "new-agent-empty", frame: .window("New Agent", nil),
                view: AnyView(NewAgentView(model: m.firstRun))),
            GallerySurface(
                name: "add-host", frame: .window("Add Linux Host", nil), view: AnyView(AddHostView(model: m.busy))),
            GallerySurface(
                name: "logins", frame: .window("Linux Logins", nil), view: AnyView(LoginsView(model: m.busy))),
            GallerySurface(
                name: "logins-first-run", frame: .window("Linux Logins", nil),
                view: AnyView(LoginsView(model: m.firstRun))),
        ]
        if let install {
            surfaces.append(
                GallerySurface(
                    name: "add-host-done", frame: .window("Add Linux Host", nil),
                    view: AnyView(AddHostView(model: m.busy, preview: install))))
        }
        for pane in SettingsPane.allCases {
            surfaces.append(
                GallerySurface(
                    name: "settings-\(pane.rawValue)", frame: .window("Settings", settings),
                    view: AnyView(SettingsView(model: m.busy, showing: pane))))
        }
        surfaces.append(
            GallerySurface(
                name: "settings-project", frame: .window("Settings", CGSize(width: 600, height: 760)),
                view: AnyView(
                    NavigationStack { ProjectDetail(model: m.busy, host: VibeRPC.localHost, name: "app") })))
        surfaces.append(
            GallerySurface(
                name: "settings-projects-first-run", frame: .window("Settings", settings),
                view: AnyView(SettingsView(model: m.firstRun, showing: .projects))))
        surfaces.append(
            GallerySurface(
                name: "new-key", frame: .window("New Key", nil), view: AnyView(NewKeySheet(model: m.busy))))
        if let key {
            surfaces.append(
                GallerySurface(
                    name: "new-key-made", frame: .window("New Key", nil),
                    view: AnyView(NewKeySheet(model: m.busy, preview: key))))
        }
        if let vps = m.busy.hosts.first(where: { $0.name == "vps" }) {
            surfaces.append(
                GallerySurface(
                    name: "external-address", frame: .window("External Address", nil),
                    view: AnyView(ExternalAddressSheet(model: m.busy, host: vps))))
        }
        return surfaces
    }
}

/// The menu bar glyph in each of its states, at size and enlarged, on a
/// strip like the menu bar.
struct GlyphSheet: View {
    private let states: [(String, GlyphState, Int)] = [
        ("None awake", GlyphState(lit: 0), 0),
        ("1 awake", GlyphState(lit: 1), 1),
        ("2 awake", GlyphState(lit: 2), 2),
        ("3 awake", GlyphState(lit: 3), 3),
        ("7 awake", GlyphState(lit: 7), 7),
        ("A question", GlyphState(lit: 3, question: true), 3),
        ("A pause", GlyphState(lit: 2, paused: true), 2),
        ("Both", GlyphState(lit: 4, question: true, paused: true), 5),
        ("No vibed", GlyphState(lit: 0, connected: false), 0),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(spacing: 18) {
                BrandMark(lit: 3, size: 72)
                BrandMark(lit: 0, size: 72)
                BrandMark(lit: 3, size: 72, night: false)
            }
            Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 10) {
                ForEach(states.indices, id: \.self) { i in
                    let state = states[i].1
                    let awake = states[i].2
                    GridRow {
                        Text(states[i].0).foregroundStyle(.secondary)
                        HStack(spacing: 3) {
                            glyph(state, awake: awake, size: 16)
                            if state.connected { Text("\(awake)").monospacedDigit() }
                        }
                        .padding(.horizontal, 8)
                        .padding(.vertical, 3)
                        .background(Capsule().fill(Color.primary.opacity(0.06)))
                        glyph(state, awake: awake, size: 64)
                    }
                }
            }
        }
        .padding(24)
    }

    private func glyph(_ state: GlyphState, awake: Int, size: CGFloat) -> some View {
        Image(nsImage: MenuBarGlyph.image(state, awake: awake))
            .resizable()
            .renderingMode(state.badged ? .original : .template)
            .foregroundStyle(.primary)
            .frame(width: size, height: size)
    }
}

/// The menu bar's material, behind a menu drawn in a plain window.
struct MenuMaterial: NSViewRepresentable {
    func makeNSView(context: Context) -> NSVisualEffectView {
        let view = NSVisualEffectView()
        view.material = .menu
        view.blendingMode = .behindWindow
        view.state = .active
        return view
    }

    func updateNSView(_ view: NSVisualEffectView, context: Context) {}
}

@MainActor
enum Gallery {
    static func run(_ request: GalleryRequest) {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        let runner = GalleryRunner(request: request)
        app.delegate = runner
        withExtendedLifetime(runner) { app.run() }
        exit(0)
    }
}

@MainActor
final class GalleryRunner: NSObject, NSApplicationDelegate {
    let request: GalleryRequest
    private var windows: [(name: String, window: NSWindow)] = []

    init(request: GalleryRequest) {
        self.request = request
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        Task { await open() }
    }

    private func open() async {
        let models = await GalleryModels.load()
        let surfaces = GallerySurface.all(models).filter { request.only.isEmpty || request.only.contains($0.name) }
        var origin = CGPoint(x: 40, y: 60)
        for appearance in request.appearances {
            for surface in surfaces {
                let window = makeWindow(surface, appearance: appearance)
                window.setFrameTopLeftPoint(NSPoint(x: origin.x, y: (NSScreen.main?.visibleFrame.maxY ?? 900) - origin.y))
                window.orderFrontRegardless()
                origin.x += 28
                origin.y += 22
                if origin.y > 400 { origin = CGPoint(x: origin.x + 120, y: 60) }
                windows.append((name: "\(surface.name)-\(appearance.rawValue)", window: window))
            }
        }
        // Let each surface's own loads (projects, keys, limits…) land.
        try? await Task.sleep(for: .seconds(2))
        switch request.mode {
        case .show:
            for (name, window) in windows { print("window \(name) \(window.windowNumber)") }
            print("ready")
            fflush(stdout)
        case .render(let directory):
            exit(render(into: directory))
        }
    }

    private func makeWindow(_ surface: GallerySurface, appearance: GalleryAppearance) -> NSWindow {
        let root: AnyView
        let style: NSWindow.StyleMask
        let title: String
        switch surface.frame {
        case .menu:
            root = AnyView(surface.view.background { MenuMaterial() })
            style = [.titled, .fullSizeContentView]
            title = "Menu"
        case .window(let name, let size):
            if let size {
                root = AnyView(surface.view.frame(width: size.width, height: size.height))
            } else {
                root = surface.view
            }
            style = [.titled, .closable]
            title = name
        }
        let controller = NSHostingController(rootView: root)
        controller.sizingOptions = [.preferredContentSize]
        let window = NSWindow(contentViewController: controller)
        window.styleMask = style
        window.title = title
        window.appearance = appearance.appearance
        window.isReleasedWhenClosed = false
        if case .menu = surface.frame {
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.isMovableByWindowBackground = true
            window.backgroundColor = .clear
            window.isOpaque = false
            for button in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
                window.standardWindowButton(button)?.isHidden = true
            }
        }
        window.setContentSize(controller.view.fittingSize)
        return window
    }

    /// Draws every window into `directory`; 0 if each came out drawn.
    private func render(into directory: URL) -> Int32 {
        var failed = false
        var manifest: [String] = []
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        } catch {
            FileHandle.standardError.write(Data("VibeOS: can't make \(directory.path): \(error)\n".utf8))
            return 1
        }
        for (name, window) in windows {
            guard let view = window.contentView else { continue }
            view.layoutSubtreeIfNeeded()
            view.display()
            guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else {
                FileHandle.standardError.write(Data("VibeOS: \(name) has nothing to draw\n".utf8))
                failed = true
                continue
            }
            view.cacheDisplay(in: view.bounds, to: rep)
            // SwiftUI draws through layers, which cacheDisplay can miss: if
            // it came out blank, draw the layer tree instead.
            let drawn = looksDrawn(rep) ? rep : (layerImage(view) ?? rep)
            let file = directory.appendingPathComponent("\(name).png")
            guard let png = drawn.representation(using: .png, properties: [:]), (try? png.write(to: file)) != nil else {
                FileHandle.standardError.write(Data("VibeOS: can't write \(file.path)\n".utf8))
                failed = true
                continue
            }
            if !looksDrawn(drawn) {
                FileHandle.standardError.write(Data("VibeOS: \(name) came out blank\n".utf8))
                failed = true
            }
            manifest.append("\(name).png \(drawn.pixelsWide)x\(drawn.pixelsHigh)")
            print("rendered \(name) \(drawn.pixelsWide)x\(drawn.pixelsHigh)")
        }
        let list = manifest.joined(separator: "\n") + "\n"
        try? list.write(to: directory.appendingPathComponent("manifest.txt"), atomically: true, encoding: .utf8)
        return failed ? 1 : 0
    }

    /// The view's layer tree, drawn into a bitmap at the window's scale.
    private func layerImage(_ view: NSView) -> NSBitmapImageRep? {
        guard let layer = view.layer else { return nil }
        let scale = view.window?.backingScaleFactor ?? 2
        let width = Int(view.bounds.width * scale)
        let height = Int(view.bounds.height * scale)
        guard width > 0, height > 0,
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height, bitsPerSample: 8,
                samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                bytesPerRow: 0, bitsPerPixel: 0),
            let context = NSGraphicsContext(bitmapImageRep: rep)
        else { return nil }
        rep.size = view.bounds.size
        context.cgContext.scaleBy(x: scale, y: scale)
        if layer.isGeometryFlipped == false && view.isFlipped {
            context.cgContext.translateBy(x: 0, y: view.bounds.height)
            context.cgContext.scaleBy(x: 1, y: -1)
        }
        layer.render(in: context.cgContext)
        return rep
    }

    /// Whether an image holds a drawing rather than one flat colour: it
    /// samples a grid and wants a spread of colours.
    private func looksDrawn(_ rep: NSBitmapImageRep) -> Bool {
        var seen = Set<Int>()
        let step = max(1, min(rep.pixelsWide, rep.pixelsHigh) / 48)
        for y in stride(from: 0, to: rep.pixelsHigh, by: step) {
            for x in stride(from: 0, to: rep.pixelsWide, by: step) {
                guard let c = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB) else { continue }
                let key = Int(c.redComponent * 31) << 10 | Int(c.greenComponent * 31) << 5 | Int(c.blueComponent * 31)
                seen.insert(key)
            }
        }
        return seen.count >= 6
    }
}
