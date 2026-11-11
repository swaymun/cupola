import XCTest
@testable import WonderPairing

final class WonderThemeTests: XCTestCase {
    private func palettes(_ spec: WonderThemeSpec) -> [(String, ThemePalette)] {
        [("light", spec.palette(systemIsDark: false)), ("dark", spec.palette(systemIsDark: true))]
            .filter { spec.scheme == .system || $0.0 == (spec.forcedDark == true ? "dark" : "light") }
    }

    func testUnknownStoredThemeFallsBackToWonder() {
        XCTAssertEqual(WonderThemeCatalog.theme(id: "retired-theme").id, "wonder")
        XCTAssertEqual(WonderThemeCatalog.theme(id: nil).id, "wonder")
        XCTAssertEqual(WonderThemeCatalog.theme(id: "nord").name, "Nord")
    }

    private static func hsl(_ hex: UInt32) -> (hue: Int, saturation: Double, lightness: Double) {
        let r = Double((hex >> 16) & 0xFF) / 255, g = Double((hex >> 8) & 0xFF) / 255, b = Double(hex & 0xFF) / 255
        let high = max(r, g, b), low = min(r, g, b), delta = high - low
        let lightness = (high + low) / 2
        guard delta > 0 else { return (0, 0, lightness) }
        let saturation = delta / (1 - abs(2 * lightness - 1))
        let hue: Double = switch high {
        case r: 60 * ((g - b) / delta).truncatingRemainder(dividingBy: 6)
        case g: 60 * ((b - r) / delta + 2)
        default: 60 * ((r - g) / delta + 4)
        }
        return (Int((hue < 0 ? hue + 360 : hue).rounded()), saturation, lightness)
    }

    func testTextAndSyntaxKeepWCAGContrastOnEverySurface() {
        for spec in WonderThemeCatalog.themes {
            for (tag, p) in palettes(spec) {
                for (surface, color) in [("background", p.background), ("sidebar", p.sidebar), ("agent", p.agentBubble),
                                         ("user", p.userBubble), ("code", p.codeBackground)] {
                    XCTAssertGreaterThanOrEqual(ThemeContrast.ratio(p.primaryText, color), 4.5, "\(spec.id) \(tag) text on \(surface)")
                }
                for (surface, color) in [("background", p.background), ("sidebar", p.sidebar), ("agent", p.agentBubble), ("code", p.codeBackground)] {
                    XCTAssertGreaterThanOrEqual(ThemeContrast.ratio(p.secondaryText, color), 4.5, "\(spec.id) \(tag) secondary on \(surface)")
                }
                for role in [SyntaxRole.keyword, .string, .number, .comment, .type, .function, .punctuation] {
                    XCTAssertGreaterThanOrEqual(ThemeContrast.ratio(p.color(for: role), p.codeBackground), 4.5, "\(spec.id) \(tag) \(role) on code (scripts/theme-contrast --fix repairs it)")
                }
            }
        }
    }

    func testContrastRatioMatchesKnownValues() {
        XCTAssertEqual(ThemeContrast.ratio(0x000000, 0xFFFFFF), 21, accuracy: 0.01)
        XCTAssertEqual(ThemeContrast.ratio(0x777777, 0xFFFFFF), 4.48, accuracy: 0.02)
    }
}

/// WCAG 2 relative luminance and contrast ratio for 0xRRGGBB colours.
private enum ThemeContrast {
    static func luminance(_ hex: UInt32) -> Double {
        func channel(_ value: UInt32) -> Double {
            let c = Double(value & 0xFF) / 255
            return c <= 0.03928 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * channel(hex >> 16) + 0.7152 * channel(hex >> 8) + 0.0722 * channel(hex)
    }

    static func ratio(_ a: UInt32, _ b: UInt32) -> Double {
        let (high, low) = (max(luminance(a), luminance(b)), min(luminance(a), luminance(b)))
        return (high + 0.05) / (low + 0.05)
    }

    /// Contrast of `foreground` over `background` shown through a scrim of
    /// `scrim` opacity on a pixel of the given 0...255 brightness.
    static func worstCaseRatio(foreground: UInt32, scrimColor: UInt32, scrim: Double, pixelValue: Double) -> Double {
        func blend(_ shift: UInt32) -> Double {
            let s = Double((scrimColor >> shift) & 0xFF)
            return s * scrim + pixelValue * (1 - scrim)
        }
        let r = UInt32(blend(16).rounded()), g = UInt32(blend(8).rounded()), b = UInt32(blend(0).rounded())
        return ratio(foreground, (r << 16) | (g << 8) | b)
    }
}
