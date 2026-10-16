// Usage: swift macos/make-icon.swift <output.png> <pixels>
// Draws the app icon: two accounts' usage bars and the arrows that swap them.
import AppKit

let args = CommandLine.arguments
let output = URL(fileURLWithPath: args[1])
let pixels = Int(args[2])!

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat(hex >> 16 & 0xFF) / 255,
        green: CGFloat(hex >> 8 & 0xFF) / 255,
        blue: CGFloat(hex & 0xFF) / 255,
        alpha: alpha)
}

let space = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(
    data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0,
    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
ctx.scaleBy(x: CGFloat(pixels) / 1024, y: CGFloat(pixels) / 1024)

// macOS icon grid: an 824pt rounded square centred on a 1024pt canvas.
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let bodyPath = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 28, color: color(0x000000, 0.35))
ctx.addPath(bodyPath)
ctx.setFillColor(color(0x1C1D21))
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(bodyPath)
ctx.clip()
let background = CGGradient(
    colorsSpace: space, colors: [color(0x34363D), color(0x141518)] as CFArray,
    locations: [0, 1])!
ctx.drawLinearGradient(
    background, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
ctx.restoreGState()

func bar(y: CGFloat, fill: CGFloat, hex: UInt32) {
    let track = CGRect(x: 176, y: y - 46, width: 380, height: 92)
    ctx.addPath(CGPath(roundedRect: track, cornerWidth: 46, cornerHeight: 46, transform: nil))
    ctx.setFillColor(color(hex, 0.22))
    ctx.fillPath()
    var filled = track
    filled.size.width = max(92, track.width * fill)
    ctx.addPath(CGPath(roundedRect: filled, cornerWidth: 46, cornerHeight: 46, transform: nil))
    ctx.setFillColor(color(hex))
    ctx.fillPath()
}

bar(y: 616, fill: 0.82, hex: 0xFFD60A)
bar(y: 408, fill: 0.3, hex: 0x2F7BEA)

func arrow(x: CGFloat, from: CGFloat, to: CGFloat, hex: UInt32) {
    let dir: CGFloat = to > from ? 1 : -1
    let head: CGFloat = 50
    ctx.setStrokeColor(color(hex))
    ctx.setLineWidth(38)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.move(to: CGPoint(x: x, y: from))
    ctx.addLine(to: CGPoint(x: x, y: to))
    ctx.move(to: CGPoint(x: x - head, y: to - dir * head))
    ctx.addLine(to: CGPoint(x: x, y: to))
    ctx.addLine(to: CGPoint(x: x + head, y: to - dir * head))
    ctx.strokePath()
}

arrow(x: 680, from: 372, to: 652, hex: 0x2F7BEA)
arrow(x: 790, from: 652, to: 372, hex: 0xFFD60A)

let image = ctx.makeImage()!
let dest = CGImageDestinationCreateWithURL(output as CFURL, "public.png" as CFString, 1, nil)!
CGImageDestinationAddImage(dest, image, nil)
guard CGImageDestinationFinalize(dest) else { fatalError("could not write \(output.path)") }
