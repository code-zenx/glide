#!/usr/bin/env swift
//
// Glide app icon generator. See contrib/ICON-THEME.md for the rules this
// implements; this file is the reference implementation for that theme.
//
//   swift contrib/icon.swift <output.png> [size]      (size defaults to 1024)
//
// Theme: macOS squircle plate inset in a 1024 canvas, dark tile, and a single
// letter inside a rounded accent frame — a letter mark, legible at any size.
// At 32px and below the frame is dropped and the letter grows to fill the
// plate, because a frame plus a letter is two elements too many at 26 pixels.
//
// ponytail: pure CoreGraphics + ImageIO, no AppKit, so it runs headless.

import CoreGraphics
import CoreText
import Foundation
import ImageIO

// ---------------------------------------------------------------- args

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write(
        Data("usage: swift icon.swift <output.png> [size]\n".utf8))
    exit(2)
}
let outPath = args[1]
let size: Double = args.count >= 3 ? (Double(args[2]) ?? 0) : 1024
// A menu bar image is a template: macOS keeps only its alpha and tints it to
// match the bar, so it is the bare letter with no plate and no colour.
let template = args.count >= 4 && args[3] == "template"
guard size >= 16 else {
    FileHandle.standardError.write(Data("icon.swift: size must be >= 16\n".utf8))
    exit(2)
}

// Everything below is authored in a 1024x1024 design space and scaled.
let s = size / 1024.0
func u(_ v: Double) -> CGFloat { CGFloat(v * s) }

let space = CGColorSpace(name: CGColorSpace.sRGB)!
func hex(_ v: Int) -> CGColor {
    CGColor(
        colorSpace: space,
        components: [
            Double((v >> 16) & 0xFF) / 255.0,
            Double((v >> 8) & 0xFF) / 255.0,
            Double(v & 0xFF) / 255.0,
            1,
        ])!
}

// ---------------------------------------------------------------- theme

// Per-project accent (ICON-THEME.md, "Colour"). Glide owns indigo, hue 250.
let accentTop = hex(0x9B86FF)
let accentBottom = hex(0x6C4FE8)
// The plate is a dark tile, so the accent is carried by the mark rather than
// by the background: one glanceable colour against near-black.
let plateTop = hex(0x2A2A31)
let plateBottom = hex(0x17171B)
// The letter this project is known by.
let letter = "G"

let plateInset = 100.0  // 824/1024 plate: the macOS dock grid
let cornerRatio = 0.2237

// ---------------------------------------------------------------- context

guard let ctx = CGContext(
    data: nil,
    width: Int(size.rounded()),
    height: Int(size.rounded()),
    bitsPerComponent: 8,
    bytesPerRow: 0,
    space: space,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else {
    FileHandle.standardError.write(Data("icon.swift: could not create context\n".utf8))
    exit(1)
}
ctx.setAllowsAntialiasing(true)
ctx.setShouldAntialias(true)
ctx.interpolationQuality = .high

// ---------------------------------------------------------------- plate

let plateSide = 1024 - 2 * plateInset
let plate = CGPath(
    roundedRect: CGRect(
        x: u(plateInset), y: u(plateInset), width: u(plateSide), height: u(plateSide)),
    cornerWidth: u(plateSide * cornerRatio), cornerHeight: u(plateSide * cornerRatio),
    transform: nil)

if !template {
ctx.saveGState()
ctx.addPath(plate)
ctx.clip()
// Vertical light falloff (light at the top), not a diagonal wash: that is what
// reads as a macOS icon rather than a generic gradient square.
let gradient = CGGradient(
    colorsSpace: space, colors: [plateTop, plateBottom] as CFArray,
    locations: [0.0, 1.0])!
ctx.drawLinearGradient(
    gradient,
    start: CGPoint(x: 0, y: u(1024 - plateInset)),
    end: CGPoint(x: 0, y: u(plateInset)),
    options: [])
ctx.restoreGState()
}

// ---------------------------------------------------------------- mark

// One shape, everywhere: a filled rounded square with the letter knocked out
// of it. The app icon fills that square with the accent and lets the plate's
// dark show through the letter; the menu bar template is the same silhouette
// in plain alpha, which macOS tints to match the bar.

/// The letter as a path, centred on its own ink rather than its text box.
func letterPath(points: Double, weight: String) -> CGPath {
    let font = CTFontCreateWithName(weight as CFString, u(points), nil)
    let attributed = NSAttributedString(
        string: letter,
        attributes: [kCTFontAttributeName as NSAttributedString.Key: font])
    let line = CTLineCreateWithAttributedString(attributed)
    let glyphs = CGMutablePath()
    for run in CTLineGetGlyphRuns(line) as! [CTRun] {
        let count = CTRunGetGlyphCount(run)
        var ids = [CGGlyph](repeating: 0, count: count)
        var origins = [CGPoint](repeating: .zero, count: count)
        CTRunGetGlyphs(run, CFRangeMake(0, count), &ids)
        CTRunGetPositions(run, CFRangeMake(0, count), &origins)
        let attributes = CTRunGetAttributes(run) as! [String: Any]
        let runFont = attributes[kCTFontAttributeName as String] as! CTFont
        for i in 0..<count {
            if let glyph = CTFontCreatePathForGlyph(runFont, ids[i], nil) {
                glyphs.addPath(
                    glyph, transform: CGAffineTransform(translationX: origins[i].x, y: origins[i].y))
            }
        }
    }
    let ink = glyphs.boundingBox
    let centred = CGMutablePath()
    centred.addPath(
        glyphs,
        transform: CGAffineTransform(
            translationX: (u(1024) - ink.width) / 2 - ink.minX,
            y: (u(1024) - ink.height) / 2 - ink.minY))
    return centred
}

// The square: the plate itself on the app icon, a smaller badge in the bar.
let badgeInset = template ? 40.0 : 236.0
let badgeSide = 1024 - 2 * badgeInset
let badge = CGPath(
    roundedRect: CGRect(
        x: u(badgeInset), y: u(badgeInset), width: u(badgeSide), height: u(badgeSide)),
    cornerWidth: u(badgeSide * 0.26), cornerHeight: u(badgeSide * 0.26),
    transform: nil)

// Square plus letter, filled even-odd: the letter becomes a hole.
let mark = CGMutablePath()
mark.addPath(badge)
mark.addPath(letterPath(points: template ? 580.0 : 400.0, weight: "SFProRounded-Bold"))

if template {
    // Alpha only. macOS paints it white or black to match the menu bar.
    ctx.setFillColor(CGColor(gray: 0, alpha: 1))
    ctx.addPath(mark)
    ctx.fillPath(using: .evenOdd)
} else {
    ctx.saveGState()
    ctx.addPath(mark)
    ctx.clip(using: .evenOdd)
    let accent = CGGradient(
        colorsSpace: space, colors: [accentTop, accentBottom] as CFArray,
        locations: [0.0, 1.0])!
    ctx.drawLinearGradient(
        accent,
        start: CGPoint(x: 0, y: u(1024 - badgeInset)),
        end: CGPoint(x: 0, y: u(badgeInset)),
        options: [])
    ctx.restoreGState()
}

// ---------------------------------------------------------------- write

guard let image = ctx.makeImage() else {
    FileHandle.standardError.write(Data("icon.swift: could not render image\n".utf8))
    exit(1)
}
let url = URL(fileURLWithPath: outPath)
guard let dest = CGImageDestinationCreateWithURL(
    url as CFURL, "public.png" as CFString, 1, nil) else {
    FileHandle.standardError.write(Data("icon.swift: could not open \(outPath)\n".utf8))
    exit(1)
}
CGImageDestinationAddImage(dest, image, nil)
guard CGImageDestinationFinalize(dest) else {
    FileHandle.standardError.write(Data("icon.swift: could not write \(outPath)\n".utf8))
    exit(1)
}
print("wrote \(outPath) (\(Int(size))x\(Int(size)))")
