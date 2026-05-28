// Build a macOS-style app icon from a square-ish source image.
//
// Crops the source to a centred square, scales it to the standard inner
// content rect (824×824 on a 1024 canvas), masks it to a squircle with a
// radial vignette darkening the edges, and adds a subtle drop shadow.
//
// Run via `assets/make-icon.sh` (which also generates the iconset and
// `.icns`). Standalone usage:
//
//     swift assets/make-icon.swift <source> <output-1024.png>

import AppKit
import CoreGraphics
import Foundation

guard CommandLine.arguments.count == 3 else {
    print("usage: make-icon <source> <output-1024.png>")
    exit(1)
}
let srcURL = URL(fileURLWithPath: CommandLine.arguments[1])
let dstURL = URL(fileURLWithPath: CommandLine.arguments[2])

guard let srcImage = NSImage(contentsOf: srcURL),
      let srcCG = srcImage.cgImage(forProposedRect: nil, context: nil, hints: nil)
else {
    print("failed to load source image at \(srcURL.path)")
    exit(1)
}

let srcW = srcCG.width
let srcH = srcCG.height
let side = min(srcW, srcH)
let cropOriginX = (srcW - side) / 2
let cropOriginY = (srcH - side) / 2
guard let cropped = srcCG.cropping(to: CGRect(
    x: cropOriginX,
    y: cropOriginY,
    width: side,
    height: side
)) else {
    print("crop failed")
    exit(1)
}

let canvas: CGFloat = 1024
let inner: CGFloat = 824
let inset = (canvas - inner) / 2
// 22.37% is Apple's continuous-corner (squircle) approximation.
let cornerRadius = inner * 0.2237

guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
      let ctx = CGContext(
          data: nil,
          width: Int(canvas),
          height: Int(canvas),
          bitsPerComponent: 8,
          bytesPerRow: 0,
          space: colorSpace,
          bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
      )
else {
    print("CGContext init failed")
    exit(1)
}

// Drop shadow on the squircle itself.
ctx.setShadow(
    offset: CGSize(width: 0, height: -8),
    blur: 24,
    color: CGColor(red: 0, green: 0, blue: 0, alpha: 0.45)
)

let innerRect = CGRect(x: inset, y: inset, width: inner, height: inner)
let path = CGPath(
    roundedRect: innerRect,
    cornerWidth: cornerRadius,
    cornerHeight: cornerRadius,
    transform: nil
)

ctx.saveGState()
ctx.addPath(path)
ctx.clip()

// 1. Draw the source image filling the squircle.
ctx.draw(cropped, in: innerRect)

// 2. Vignette: radial gradient from transparent at the centre to a
//    semi-opaque black at the edges so the glowing W reads as the
//    focal point. Disable the shadow first so the vignette overlay
//    doesn't cast one of its own.
ctx.setShadow(offset: .zero, blur: 0, color: nil)
let centre = CGPoint(x: innerRect.midX, y: innerRect.midY)
let endRadius = inner * 0.72
let colors = [
    CGColor(red: 0, green: 0, blue: 0, alpha: 0.0),
    CGColor(red: 0, green: 0, blue: 0, alpha: 0.0),
    CGColor(red: 0, green: 0, blue: 0, alpha: 0.55),
] as CFArray
let locations: [CGFloat] = [0.0, 0.55, 1.0]
if let gradient = CGGradient(colorsSpace: colorSpace, colors: colors, locations: locations) {
    ctx.drawRadialGradient(
        gradient,
        startCenter: centre, startRadius: 0,
        endCenter: centre, endRadius: endRadius,
        options: []
    )
}

ctx.restoreGState()

guard let outCG = ctx.makeImage() else {
    print("makeImage failed")
    exit(1)
}
let rep = NSBitmapImageRep(cgImage: outCG)
guard let png = rep.representation(using: .png, properties: [:]) else {
    print("png encode failed")
    exit(1)
}
try png.write(to: dstURL)
print("wrote \(dstURL.path)")
