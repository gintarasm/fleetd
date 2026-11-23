// Renders an SVG to a square PNG keeping its alpha (qlmanage fills transparency white).
// Usage: swift scripts/render-svg.swift <in.svg> <out.png> <px>. Used by bundle-macos.sh (ADR 0104).
import AppKit
let a = CommandLine.arguments
guard a.count == 4, let px = Int(a[3]), let img = NSImage(contentsOf: URL(fileURLWithPath: a[1])) else {
  FileHandle.standardError.write("usage: render.swift <in.svg> <out.png> <px>\n".data(using: .utf8)!); exit(1)
}
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
  samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSColor.clear.set(); NSRect(x: 0, y: 0, width: px, height: px).fill()
img.draw(in: NSRect(x: 0, y: 0, width: px, height: px))
NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: a[2]))
