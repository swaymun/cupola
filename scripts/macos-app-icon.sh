#!/usr/bin/env bash
# Build the Mac app icon from the iOS app icon, so Finder shows the same picture.
# scripts/macos-app-icon.sh OUT.icns [APPICONSET]   (default: the production iOS icon)
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${1:?Usage: macos-app-icon.sh OUT.icns [APPICONSET]}
SET=${2:-apps/ios/Wonder/Assets.xcassets/AppIcon.appiconset}
SOURCE="$SET/$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["images"][0]["filename"])' "$SET/Contents.json")"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
# iOS masks its square icon itself; macOS shows the file as drawn. Draw it on
# Apple's macOS icon grid: an 824-point rounded square centred on 1024.
swift - "$SOURCE" "$WORK/master.png" <<'SWIFT'
import AppKit
let args = CommandLine.arguments
guard let image = NSImage(contentsOfFile: args[1]),
      let source = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
    fatalError("Unreadable icon \(args[1])")
}
let size = 1024, inset = 100.0, radius = 185.4
let context = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
    space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
let tile = CGRect(x: inset, y: inset, width: Double(size) - 2 * inset, height: Double(size) - 2 * inset)
context.addPath(CGPath(roundedRect: tile, cornerWidth: radius, cornerHeight: radius, transform: nil))
context.clip()
context.interpolationQuality = .high
context.draw(source, in: tile)
let png = NSBitmapImageRep(cgImage: context.makeImage()!).representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: args[2]))
SWIFT
ICONSET="$WORK/Cupola.iconset"
mkdir "$ICONSET"
for points in 16 32 128 256 512; do
  sips -z "$points" "$points" "$WORK/master.png" --out "$ICONSET/icon_${points}x${points}.png" >/dev/null
  sips -z $((points * 2)) $((points * 2)) "$WORK/master.png" --out "$ICONSET/icon_${points}x${points}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$OUT"
