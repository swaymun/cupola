import Foundation

/// One theme's colours as 0xRRGGBB values. Views map them to SwiftUI colours;
/// keeping them as plain numbers lets tests check contrast without a UI.
public struct ThemePalette: Sendable, Equatable {
    public let background: UInt32
    /// The Chats column, one step off `background`.
    public let sidebar: UInt32
    public let agentBubble: UInt32
    public let userBubble: UInt32
    public let primaryText: UInt32
    public let secondaryText: UInt32
    public let separator: UInt32
    public let accent: UInt32
    public let codeBackground: UInt32
    public let keyword: UInt32
    public let string: UInt32
    public let number: UInt32
    public let comment: UInt32
    public let type: UInt32
    public let function: UInt32
    public let punctuation: UInt32

    /// The syntax colour for a role. Attributes share the function colour.
    public func color(for role: SyntaxRole) -> UInt32 {
        switch role {
        case .keyword: keyword
        case .string: string
        case .number: number
        case .comment: comment
        case .type: type
        case .function, .attribute: function
        case .punctuation: punctuation
        }
    }
}

/// What a coloured status means, for badges, callouts and step markers.
public enum StatusRole: Sendable, CaseIterable {
    case success, warning, error, info
}

extension ThemePalette {
    /// A status colour for this palette: a familiar hue (green, amber, red,
    /// blue) moved toward the text colour until it reads at 4.5:1 on both the
    /// agent bubble and the page, so a status is legible in every theme.
    public func status(_ role: StatusRole) -> UInt32 {
        let dark = Self.luminance(background) < 0.4
        let base: UInt32 = switch role {
        case .success: dark ? 0x4ADE80 : 0x15803D
        case .warning: dark ? 0xFBBF24 : 0xB45309
        case .error: dark ? 0xF87171 : 0xB91C1C
        case .info: dark ? 0x60A5FA : 0x1D4ED8
        }
        return readable(base, minimum: 4.5)
    }

    /// Up to six chart series and sketch areas: the accent first, then the
    /// syntax colours, each at least 3:1 against the agent bubble.
    public var series: [UInt32] {
        [accent, type, string, keyword, number, function].map { readable($0, minimum: 3) }
    }

    /// `color`, mixed toward the primary text in tenths until it reaches
    /// `minimum` contrast on the agent bubble and the background. The primary
    /// text itself always does, so this ends.
    func readable(_ color: UInt32, minimum: Double) -> UInt32 {
        for step in 0...10 {
            let mixed = Self.mix(color, primaryText, Double(step) / 10)
            if Self.contrast(mixed, agentBubble) >= minimum, Self.contrast(mixed, background) >= minimum { return mixed }
        }
        return primaryText
    }

    static func mix(_ a: UInt32, _ b: UInt32, _ amount: Double) -> UInt32 {
        func channel(_ shift: UInt32) -> UInt32 {
            let x = Double((a >> shift) & 0xFF), y = Double((b >> shift) & 0xFF)
            return UInt32((x + (y - x) * amount).rounded()) << shift
        }
        return channel(16) | channel(8) | channel(0)
    }

    /// WCAG 2 relative luminance and contrast ratio.
    public static func luminance(_ hex: UInt32) -> Double {
        func channel(_ value: UInt32) -> Double {
            let c = Double(value & 0xFF) / 255
            return c <= 0.03928 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * channel(hex >> 16) + 0.7152 * channel(hex >> 8) + 0.0722 * channel(hex)
    }

    public static func contrast(_ a: UInt32, _ b: UInt32) -> Double {
        let (high, low) = (max(luminance(a), luminance(b)), min(luminance(a), luminance(b)))
        return (high + 0.05) / (low + 0.05)
    }
}

/// A photo drawn once behind the conversation, under a flat scrim of the
/// theme's `background` so text on it keeps its contrast.
public struct ThemeBackground: Sendable, Equatable {
    public let asset: String
    /// Opacity of the scrim over the image, 0...1.
    public let scrim: Double
    /// Darkest and lightest greyscale values (0...255) measured in the image.
    public let pixelRange: ClosedRange<Double>
}

public enum ThemeScheme: Sendable, Equatable {
    /// Follows the device: both palettes are declared.
    case system, dark, light
}

public struct WonderThemeSpec: Sendable, Equatable, Identifiable {
    public let id: String
    public let name: String
    public let scheme: ThemeScheme
    let light: ThemePalette?
    let dark: ThemePalette?
    public let background: ThemeBackground?

    /// The palette to draw with; `systemIsDark` only matters for `.system`.
    public func palette(systemIsDark: Bool) -> ThemePalette {
        switch scheme {
        case .dark: dark ?? light!
        case .light: light ?? dark!
        case .system: systemIsDark ? dark! : light!
        }
    }

    /// The appearance to force, or nil to follow the device.
    public var forcedDark: Bool? {
        switch scheme {
        case .system: nil
        case .dark: true
        case .light: false
        }
    }
}

/// One section of Settings → Theme.
public struct ThemeGroup: Sendable, Identifiable {
    /// Nil for the untitled first section.
    public let title: String?
    public let themes: [WonderThemeSpec]
    public var id: String { title ?? "" }
}

public enum WonderThemeCatalog {
    public static let defaultID = "wonder"
    /// Cupola's own themes, listed under "Cupola" after the default.
    static let cupolaIDs: Set<String> = ["cupola-light", "cupola-dark"]

    /// The default (iOS's own look, following the device) first, then Cupola's
    /// own themes, classic palettes and photos.
    public static var themes: [WonderThemeSpec] { all }

    /// Settings → Theme sections, in catalog order.
    public static let groups: [ThemeGroup] = [
        ThemeGroup(title: nil, themes: all.filter { $0.id == defaultID }),
        ThemeGroup(title: "Cupola", themes: all.filter { cupolaIDs.contains($0.id) }),
        ThemeGroup(title: "Classic", themes: all.filter { $0.id != defaultID && !cupolaIDs.contains($0.id) && $0.background == nil }),
        ThemeGroup(title: "Photo", themes: all.filter { $0.background != nil }),
    ]

    /// A stored id that no longer exists falls back to the default.
    public static func theme(id: String?) -> WonderThemeSpec {
        all.first { $0.id == id } ?? all[0]
    }
}
