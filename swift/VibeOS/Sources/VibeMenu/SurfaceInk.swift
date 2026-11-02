import CoreGraphics

/// How much of an image is drawing rather than background, for the check
/// that every surface draws (`VibeOS --render-surfaces`).
public struct SurfaceInk: Equatable, Sendable {
    /// The share of pixels unlike the commonest colour, the background.
    public var share: Double
    /// Distinct colours, at 5 bits a channel, alpha included.
    public var colours: Int

    public init(share: Double, colours: Int) {
        self.share = share
        self.colours = colours
    }

    /// Drawn, not blank. A blank window is one flat colour; even a sparse
    /// surface (an empty state: a symbol, two lines and a button) has
    /// thousands of pixels of its own and dozens of antialiased shades.
    public var drawn: Bool { share >= 0.002 && colours >= 8 }

    /// Counts every pixel, after drawing the image into an 8-bit sRGB
    /// buffer, so its own layout doesn't matter.
    public init(_ image: CGImage) {
        guard image.width > 0, image.height > 0, let space = CGColorSpace(name: CGColorSpace.sRGB),
            let context = CGContext(
                data: nil, width: image.width, height: image.height, bitsPerComponent: 8, bytesPerRow: 0,
                space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue),
            let data = context.data
        else {
            self.init(share: 0, colours: 0)
            return
        }
        context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
        let bytes = data.bindMemory(to: UInt8.self, capacity: context.bytesPerRow * image.height)
        var counts = [Int32](repeating: 0, count: 1 << 20)
        for y in 0..<image.height {
            let row = bytes + y * context.bytesPerRow
            for x in 0..<image.width {
                let p = row + x * 4
                counts[Int(p[0] >> 3) << 15 | Int(p[1] >> 3) << 10 | Int(p[2] >> 3) << 5 | Int(p[3] >> 3)] += 1
            }
        }
        let total = image.width * image.height
        let commonest = Int(counts.max() ?? 0)
        self.init(
            share: Double(total - commonest) / Double(total), colours: counts.reduce(0) { $1 > 0 ? $0 + 1 : $0 })
    }
}
