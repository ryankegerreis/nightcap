import AppKit
let size = 1024.0
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
let path = NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185)
NSGradient(starting: NSColor(srgbRed: 0.16, green: 0.16, blue: 0.25, alpha: 1),
           ending: NSColor(srgbRed: 0.06, green: 0.06, blue: 0.10, alpha: 1))!.draw(in: path, angle: -90)
// Crescent: big disc minus an offset disc.
let ctx = NSGraphicsContext.current!.cgContext
ctx.saveGState()
ctx.addEllipse(in: CGRect(x: 262, y: 262, width: 500, height: 500)); ctx.clip()
let moon = CGMutablePath()
moon.addEllipse(in: CGRect(x: 512 - 250, y: 512 - 250, width: 500, height: 500))
moon.addEllipse(in: CGRect(x: 512 - 250 + 150, y: 512 - 250 + 110, width: 470, height: 470))
ctx.addPath(moon)
ctx.setFillColor(CGColor(srgbRed: 0.40, green: 0.39, blue: 0.94, alpha: 1))
ctx.fillPath(using: .evenOdd)
ctx.restoreGState()
// Clip away the part of the offset disc that pokes outside the big disc.
img.unlockFocus()
let rep = NSBitmapImageRep(data: img.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
