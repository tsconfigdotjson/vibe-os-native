// Renders VibeOS's app icon, swift/VibeOS/Support/AppIcon.svg (the VibeOS
// icon, public/icon-app.svg in vibe-os), into an .iconset folder: every size
// iconutil wants, each drawn from the SVG at its exact pixel size, in sRGB.
//
//   swift scripts/app-icon.swift swift/VibeOS/Support/AppIcon.svg swift/VibeOS/Support/AppIcon.iconset
//   swift scripts/app-icon.swift --check <svg> <iconset>   # as a fresh render draws it?
//
// The iconset is checked in; run this again only when the SVG changes.
// scripts/sign-macos.sh and scripts/release-macos.sh make the .icns from it.

import AppKit

func fail(_ message: String, _ code: Int32 = 1) -> Never {
    FileHandle.standardError.write("app-icon: \(message)\n".data(using: .utf8)!)
    exit(code)
}

var args = Array(CommandLine.arguments.dropFirst())
let check = args.first == "--check"
if check { args.removeFirst() }
guard args.count == 2 else { fail("usage: swift scripts/app-icon.swift [--check] <icon.svg> <out.iconset>", 2) }
let svg = URL(fileURLWithPath: args[0])
let out = URL(fileURLWithPath: args[1], isDirectory: true)
guard let image = NSImage(contentsOf: svg) else { fail("can't read \(svg.path)") }

// iconutil's names, and each one's size in pixels.
let sizes: [(String, Int)] = [
    ("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64),
    ("128x128", 128), ("128x128@2x", 256), ("256x256", 256), ("256x256@2x", 512),
    ("512x512", 512), ("512x512@2x", 1024),
]

/// A px-by-px sRGB bitmap, with `draw` filling it edge to edge.
func render(_ px: Int, _ draw: (NSRect) -> Void) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: px * 4, bitsPerPixel: 32)!.retagging(with: .sRGB)!
    rep.size = NSSize(width: px, height: px)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current?.imageInterpolation = .high
    draw(NSRect(x: 0, y: 0, width: px, height: px))
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

if !check { try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true) }
for (name, px) in sizes {
    let file = out.appendingPathComponent("icon_\(name).png")
    let fresh = render(px) { image.draw(in: $0, from: .zero, operation: .copy, fraction: 1) }
    if check {
        guard let committed = NSBitmapImageRep(data: (try? Data(contentsOf: file)) ?? Data()) else {
            fail("no \(file.lastPathComponent) in \(out.path)")
        }
        guard committed.pixelsWide == px, committed.pixelsHigh == px else {
            fail("\(file.lastPathComponent) is \(committed.pixelsWide)x\(committed.pixelsHigh), not \(px)x\(px)")
        }
        // Drawn again into the same kind of bitmap, so only pixels compare.
        let seen = render(px) {
            committed.draw(in: $0, from: .zero, operation: .copy, fraction: 1, respectFlipped: false, hints: nil)
        }
        // Near enough, not byte for byte: another macOS may antialias an
        // edge a little differently. Each channel within 2, but for a few
        // edge pixels, which may be off by more.
        let (a, b) = (seen.bitmapData!, fresh.bitmapData!)
        var off = 0
        for i in 0..<(px * px) {
            if (0..<4).contains(where: { abs(Int(a[i * 4 + $0]) - Int(b[i * 4 + $0])) > 2 }) { off += 1 }
        }
        guard off * 200 <= px * px else {
            fail("\(file.lastPathComponent) isn't \(svg.lastPathComponent) rendered (\(off) of \(px * px) pixels differ); run swift scripts/app-icon.swift \(args[0]) \(args[1])")
        }
    } else {
        try fresh.representation(using: .png, properties: [:])!.write(to: file)
    }
}
print(check ? "app-icon: \(out.lastPathComponent) matches \(svg.lastPathComponent)" : "app-icon: \(sizes.count) sizes in \(out.path)")
