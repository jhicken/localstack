// Renders the LStack app icon. Run: swift macos/icon/make-icon.swift <out-dir>
// Produces <out-dir>/icon-1024.png; macos/icon/build-icons.sh turns that into AppIcon.icns + the web favicon.
import AppKit
import CoreGraphics

let size = 1024.0
let out = CommandLine.arguments.dropFirst().first ?? "."

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

let cs = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
// Work in top-left coordinates.
ctx.translateBy(x: 0, y: size)
ctx.scaleBy(x: 1, y: -1)

func linear(_ colors: [CGColor], _ from: CGPoint, _ to: CGPoint) {
    let g = CGGradient(colorsSpace: cs, colors: colors as CFArray, locations: nil)!
    ctx.drawLinearGradient(g, start: from, end: to, options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
}
func radial(_ colors: [CGColor], _ c: CGPoint, _ r: CGFloat) {
    let g = CGGradient(colorsSpace: cs, colors: colors as CFArray, locations: nil)!
    ctx.drawRadialGradient(g, startCenter: c, startRadius: 0, endCenter: c, endRadius: r, options: [])
}
func poly(_ pts: [CGPoint]) -> CGPath {
    let p = CGMutablePath()
    p.addLines(between: pts)
    p.closeSubpath()
    return p
}

// --- squircle body (Apple grid: 824pt body inset 100pt, ~185pt corner radius)
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let squircle = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: 14), blur: 30, color: rgb(0x000000, 0.45))
ctx.addPath(squircle)
ctx.setFillColor(rgb(0x111428))
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(squircle)
ctx.clip()
linear([rgb(0x1e1b4b), rgb(0x0b1020)], CGPoint(x: 200, y: 100), CGPoint(x: 820, y: 924))
radial([rgb(0x7c3aed, 0.45), rgb(0x7c3aed, 0)], CGPoint(x: 300, y: 260), 520)
radial([rgb(0x06b6d4, 0.30), rgb(0x06b6d4, 0)], CGPoint(x: 820, y: 860), 520)
// top sheen
linear([rgb(0xffffff, 0.10), rgb(0xffffff, 0)], CGPoint(x: 512, y: 100), CGPoint(x: 512, y: 420))
ctx.restoreGState()

// inner hairline
ctx.saveGState()
ctx.addPath(CGPath(roundedRect: body.insetBy(dx: 1.5, dy: 1.5), cornerWidth: 184, cornerHeight: 184, transform: nil))
ctx.setStrokeColor(rgb(0xffffff, 0.10))
ctx.setLineWidth(3)
ctx.strokePath()
ctx.restoreGState()

// --- three isometric slabs
let cx = 512.0, w = 250.0, h = 140.0, t = 38.0
struct Layer { let cy: Double; let top: [UInt32]; let left: UInt32; let right: UInt32; let alpha: CGFloat }
let layers = [
    Layer(cy: 610, top: [0x4f46e5, 0x6d28d9], left: 0x312e81, right: 0x3b0764, alpha: 0.95),
    Layer(cy: 495, top: [0x6366f1, 0x8b5cf6], left: 0x3730a3, right: 0x4c1d95, alpha: 0.97),
    Layer(cy: 380, top: [0x22d3ee, 0x818cf8], left: 0x0e7490, right: 0x4338ca, alpha: 1),
]

for (i, l) in layers.enumerated() {
    let cy = l.cy
    let top = poly([CGPoint(x: cx, y: cy - h), CGPoint(x: cx + w, y: cy), CGPoint(x: cx, y: cy + h), CGPoint(x: cx - w, y: cy)])
    let left = poly([CGPoint(x: cx - w, y: cy), CGPoint(x: cx, y: cy + h), CGPoint(x: cx, y: cy + h + t), CGPoint(x: cx - w, y: cy + t)])
    let right = poly([CGPoint(x: cx, y: cy + h), CGPoint(x: cx + w, y: cy), CGPoint(x: cx + w, y: cy + t), CGPoint(x: cx, y: cy + h + t)])

    // drop shadow for the whole slab
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: 18), blur: 36, color: rgb(0x000000, 0.5))
    ctx.setFillColor(rgb(l.left))
    ctx.addPath(left); ctx.addPath(right); ctx.addPath(top)
    ctx.fillPath()
    ctx.restoreGState()

    ctx.setAlpha(l.alpha)
    ctx.saveGState(); ctx.addPath(left); ctx.clip()
    linear([rgb(l.left), rgb(l.left, 0.85)], CGPoint(x: cx - w, y: cy), CGPoint(x: cx, y: cy + h + t))
    ctx.restoreGState()
    ctx.saveGState(); ctx.addPath(right); ctx.clip()
    linear([rgb(l.right), rgb(l.right, 0.8)], CGPoint(x: cx, y: cy + h), CGPoint(x: cx + w, y: cy + t))
    ctx.restoreGState()
    ctx.saveGState(); ctx.addPath(top); ctx.clip()
    linear([rgb(l.top[0]), rgb(l.top[1])], CGPoint(x: cx - w * 0.6, y: cy - h), CGPoint(x: cx + w * 0.6, y: cy + h))
    // glossy highlight on the upper half of the top face
    radial([rgb(0xffffff, i == 2 ? 0.35 : 0.18), rgb(0xffffff, 0)], CGPoint(x: cx - 40, y: cy - h * 0.55), w * 0.9)
    ctx.restoreGState()
    ctx.setAlpha(1)

    // crisp top edge
    ctx.saveGState()
    ctx.addLines(between: [CGPoint(x: cx - w, y: cy), CGPoint(x: cx, y: cy - h), CGPoint(x: cx + w, y: cy)])
    ctx.setStrokeColor(rgb(0xffffff, i == 2 ? 0.55 : 0.25))
    ctx.setLineWidth(4)
    ctx.setLineJoin(.round)
    ctx.strokePath()
    ctx.restoreGState()
}

// --- "live" status dot
let dot = CGPoint(x: 748, y: 262)
ctx.saveGState()
radial([rgb(0x34d399, 0.55), rgb(0x34d399, 0)], dot, 120)
ctx.restoreGState()
ctx.saveGState()
ctx.setShadow(offset: .zero, blur: 24, color: rgb(0x34d399, 0.9))
ctx.addEllipse(in: CGRect(x: dot.x - 46, y: dot.y - 46, width: 92, height: 92))
ctx.setFillColor(rgb(0x0b1020))
ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addEllipse(in: CGRect(x: dot.x - 34, y: dot.y - 34, width: 68, height: 68))
ctx.clip()
linear([rgb(0x6ee7b7), rgb(0x10b981)], CGPoint(x: dot.x - 34, y: dot.y - 34), CGPoint(x: dot.x + 34, y: dot.y + 34))
ctx.restoreGState()

let img = ctx.makeImage()!
let rep = NSBitmapImageRep(cgImage: img)
let url = URL(fileURLWithPath: out).appendingPathComponent("icon-1024.png")
try! rep.representation(using: .png, properties: [:])!.write(to: url)
print("wrote \(url.path)")
