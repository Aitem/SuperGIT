// Renders the SuperGIT app icon (1024×1024 PNG) with AppKit.
// Usage: swift scripts/make-icon.swift out.png
import AppKit

let size: CGFloat = 1024
let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
rep.size = NSSize(width: size, height: size)

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// macOS icon grid: 824pt body centred on a 1024 canvas.
let inset: CGFloat = 100
let body = NSRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let shape = NSBezierPath(roundedRect: body, xRadius: 186, yRadius: 186)

let shadow = NSShadow()
shadow.shadowColor = NSColor.black.withAlphaComponent(0.35)
shadow.shadowOffset = NSSize(width: 0, height: -10)
shadow.shadowBlurRadius = 24
NSGraphicsContext.saveGraphicsState()
shadow.set()
NSColor.black.setFill()
shape.fill()
NSGraphicsContext.restoreGraphicsState()

NSGradient(
    starting: NSColor(srgbRed: 0.09, green: 0.08, blue: 0.19, alpha: 1),
    ending: NSColor(srgbRed: 0.31, green: 0.22, blue: 0.70, alpha: 1)
)!.draw(in: shape, angle: 90)

// Subtle top highlight.
NSGraphicsContext.saveGraphicsState()
shape.addClip()
NSGradient(
    starting: NSColor.white.withAlphaComponent(0.10),
    ending: NSColor.white.withAlphaComponent(0.0)
)!.draw(in: NSRect(x: inset, y: size / 2, width: body.width, height: body.height / 2), angle: 270)
NSGraphicsContext.restoreGraphicsState()

func symbol(_ name: String, _ points: CGFloat, _ color: NSColor) -> NSImage {
    let config = NSImage.SymbolConfiguration(pointSize: points, weight: .semibold)
        .applying(NSImage.SymbolConfiguration(paletteColors: [color]))
    return NSImage(systemSymbolName: name, accessibilityDescription: nil)!
        .withSymbolConfiguration(config)!
}

let branch = symbol("arrow.triangle.branch", 420, .white)
branch.draw(in: NSRect(
    x: (size - branch.size.width) / 2 - 24,
    y: (size - branch.size.height) / 2 - 36,
    width: branch.size.width, height: branch.size.height))

let spark = symbol("sparkle", 190, NSColor(srgbRed: 0.85, green: 0.47, blue: 0.34, alpha: 1))
spark.draw(in: NSRect(
    x: size - inset - spark.size.width - 64,
    y: size - inset - spark.size.height - 64,
    width: spark.size.width, height: spark.size.height))

NSGraphicsContext.restoreGraphicsState()
let png = rep.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
