// Renders the ImageSmith app icon as a 1024×1024 PNG.
// Usage: swift make-icon.swift <out.png>
import AppKit

let size = 1024.0
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// macOS icon grid: 824pt rounded square centered in 1024.
let inset = 100.0
let body = NSRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let shape = NSBezierPath(roundedRect: body, xRadius: 185, yRadius: 185)
NSGradient(starting: NSColor(calibratedRed: 0.16, green: 0.42, blue: 0.80, alpha: 1),
           ending: NSColor(calibratedRed: 0.07, green: 0.16, blue: 0.36, alpha: 1))!.draw(in: shape, angle: -90)

// A disk image: stacked partition bars, the last one highlighted as the rootfs.
let bars: [(Double, NSColor)] = [
    (0.18, NSColor(white: 1, alpha: 0.55)),
    (0.30, NSColor(white: 1, alpha: 0.75)),
    (0.52, NSColor(calibratedRed: 1.0, green: 0.74, blue: 0.27, alpha: 1)),
]
var x = 230.0
let y = 420.0, h = 190.0, total = 564.0
for (frac, color) in bars {
    let w = total * frac - 14
    color.setFill()
    NSBezierPath(roundedRect: NSRect(x: x, y: y, width: w, height: h), xRadius: 26, yRadius: 26).fill()
    x += total * frac
}
// Hammer-strike sparks above the rootfs bar.
NSColor(calibratedRed: 1.0, green: 0.85, blue: 0.45, alpha: 1).setStroke()
for (dx, dy) in [(0.0, 1.0), (-0.6, 0.8), (0.6, 0.8)] {
    let p = NSBezierPath()
    let cx = 650.0, cy = 650.0
    p.move(to: NSPoint(x: cx + dx * 40, y: cy + dy * 40))
    p.line(to: NSPoint(x: cx + dx * 110, y: cy + dy * 110))
    p.lineWidth = 30
    p.lineCapStyle = .round
    p.stroke()
}
NSColor(white: 1, alpha: 0.9).setFill()
NSBezierPath(roundedRect: NSRect(x: 230, y: 300, width: 564, height: 40), xRadius: 20, yRadius: 20).fill()

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
