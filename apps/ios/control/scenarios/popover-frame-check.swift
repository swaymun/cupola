// Read the connected popover fill from the original PNG, then compare its
// arrow region with the native AX control. No screenshots are altered.
import AppKit
import Foundation
let a = CommandLine.arguments
let values = a.dropFirst(2).map { Double($0)! }
let data = try Data(contentsOf: URL(fileURLWithPath: a[1]))
guard let bitmap = NSBitmapImageRep(data: data), let pixels = bitmap.bitmapData,
      bitmap.bitsPerSample == 8, !bitmap.isPlanar else { fatalError("Capture an 8-bit PNG for the arrow-frame check") }
let sx = Double(bitmap.pixelsWide) / values[0], sy = Double(bitmap.pixelsHigh) / values[1]
let body = CGRect(x: values[2], y: values[3], width: values[4], height: values[5])
let control = CGRect(x: values[6], y: values[7], width: values[8], height: values[9])
let source = CGRect(x: values[10], y: values[11], width: values[12], height: values[13])
let direction = Int(values[14])
let channels = bitmap.bitsPerPixel / 8, stride = bitmap.bytesPerRow
let x0 = max(0, Int((body.minX - 24) * sx)), y0 = max(0, Int((body.minY - 24) * sy))
let x1 = min(bitmap.pixelsWide - 1, Int((body.maxX + 24) * sx)), y1 = min(bitmap.pixelsHigh - 1, Int((body.maxY + 24) * sy))
let width = x1 - x0 + 1, height = y1 - y0 + 1
let seedX = min(x1, max(x0, Int((body.minX - 7) * sx)))
let seedY = min(y1, max(y0, Int(body.midY * sy)))
let color = (0..<channels).map { Int(pixels[seedY * stride + seedX * channels + $0]) }
var seen = [Bool](repeating: false, count: width * height)
var queue = [(seedY - y0) * width + seedX - x0], index = 0
var best = Double.infinity, bestPoint = CGPoint.zero
while index < queue.count {
    let p = queue[index]; index += 1
    if seen[p] { continue }; seen[p] = true
    let x = p % width + x0, y = p / width + y0
    let difference = (0..<channels).reduce(0) { $0 + abs(Int(pixels[y * stride + x * channels + $1]) - color[$1]) }
    if difference > 8 { continue }
    let point = CGPoint(x: (Double(x) + 0.5) / sx, y: (Double(y) + 0.5) / sy)
    let inArrowRegion: Bool
    switch direction {
    case 1: inArrowRegion = point.y >= source.maxY && point.y <= body.minY
    case 2: inArrowRegion = point.y <= source.minY && point.y >= body.maxY
    case 4: inArrowRegion = point.x >= source.maxX && point.x <= body.minX
    case 8: inArrowRegion = point.x <= source.minX && point.x >= body.maxX
    default: inArrowRegion = false
    }
    if inArrowRegion {
        let dx = max(control.minX - point.x, 0, point.x - control.maxX)
        let dy = max(control.minY - point.y, 0, point.y - control.maxY)
        let distance = hypot(dx, dy)
        if distance < best { best = distance; bestPoint = point }
    }
    if x > x0 { queue.append(p - 1) }; if x < x1 { queue.append(p + 1) }
    if y > y0 { queue.append(p - width) }; if y < y1 { queue.append(p + width) }
}
guard best.isFinite else { fatalError("No connected arrow fill found; inspect the original capture and its AX/source frames") }
let result: [String: Any] = ["arrowFillDistance": best, "arrowFillPoint": ["x": bestPoint.x, "y": bestPoint.y], "pixelScale": [sx, sy], "fillChannels": color]
let json = try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys])
print(String(decoding: json, as: UTF8.self))
