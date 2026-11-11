// Theme palette data. Classic palettes use the published colours of widely used
// editor themes (VS Code's Dark/Light Modern, One Dark Pro, Dracula, Monokai,
// Solarized, Nord, Gruvbox, Tokyo Night, Catppuccin, GitHub, Night Owl, Rosé
// Pine) and the MIT-licensed Apothecary Diary VS Code theme by Matheus
// Montemurro (Maomao palette as "Herbalist"; notice in ThemeCredits);
// a few secondary-text and comment colours are adjusted
// so every theme keeps WCAG AA. Cupola Light (sky blue), Cupola Dark (night
// sky) and the photo themes are Cupola's own. Keep the WonderThemeTests contrast checks green when
// editing a palette.
import Foundation

extension WonderThemeCatalog {
    static let all: [WonderThemeSpec] = [
        // iOS's own look, following the device. The id predates the rename.
        WonderThemeSpec(id: "wonder", name: "System", scheme: .system,
            light: ThemePalette(
                background: 0xFFFFFF, sidebar: 0xEFEFF2, agentBubble: 0xF2F2F7, userBubble: 0xD1D1D6,
                primaryText: 0x000000, secondaryText: 0x6C6C70, separator: 0xC6C6C8, accent: 0x9A5A00,
                codeBackground: 0xF2F2F7, keyword: 0x9B2393, string: 0xC41A16, number: 0x1C00CF,
                comment: 0x5D6C79, type: 0x0B4F79, function: 0x326D74, punctuation: 0x000000),
            dark: ThemePalette(
                background: 0x000000, sidebar: 0x121214, agentBubble: 0x1C1C1E, userBubble: 0x3A3A3C,
                primaryText: 0xFFFFFF, secondaryText: 0x98989F, separator: 0x38383A, accent: 0xF2B84B,
                codeBackground: 0x1C1C1E, keyword: 0xFF7AB2, string: 0xFF8170, number: 0xD9C97C,
                comment: 0x7F8C98, type: 0x6BDFFF, function: 0x4EB0CC, punctuation: 0xFFFFFF),
            background: nil),
        // Soft cloud bubbles on the icon's muted sky, deep-sky accent.
        WonderThemeSpec(id: "cupola-light", name: "Cupola Light", scheme: .light,
            light: ThemePalette(
                background: 0xB9D3E8, sidebar: 0xA8C7E0, agentBubble: 0xEEF4F9, userBubble: 0x9BBEDB,
                primaryText: 0x0B2540, secondaryText: 0x2B4760, separator: 0x8FB4D2, accent: 0x02507F,
                codeBackground: 0xE6EEF5, keyword: 0x8A2F9E, string: 0xA8321E, number: 0x1D4FB8,
                comment: 0x556D84, type: 0x0B6B7D, function: 0x2B58A8, punctuation: 0x0B2540),
            dark: nil,
            background: nil),
        // Midnight blue with starlight text, star-gold strings and a pale-sky accent.
        WonderThemeSpec(id: "cupola-dark", name: "Cupola Dark", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x0D1330, sidebar: 0x02040E, agentBubble: 0x182041, userBubble: 0x283766,
                primaryText: 0xE8ECFA, secondaryText: 0xA3ADCF, separator: 0x252E52, accent: 0x9CC8FF,
                codeBackground: 0x080C20, keyword: 0xC7A4FF, string: 0xF3D58A, number: 0xFFB38A,
                comment: 0x8A94BB, type: 0x7FDBE8, function: 0x9CC8FF, punctuation: 0xC6CDE8),
            background: nil),
        WonderThemeSpec(id: "dark-modern", name: "Dark Modern", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x1F1F1F, sidebar: 0x141414, agentBubble: 0x262626, userBubble: 0x313131,
                primaryText: 0xCCCCCC, secondaryText: 0x9D9D9D, separator: 0x2B2B2B, accent: 0x0078D4,
                codeBackground: 0x181818, keyword: 0x569CD6, string: 0xCE9178, number: 0xB5CEA8,
                comment: 0x6A9955, type: 0x4EC9B0, function: 0xDCDCAA, punctuation: 0xD4D4D4),
            background: nil),
        WonderThemeSpec(id: "light-modern", name: "Light Modern", scheme: .light,
            light: ThemePalette(
                background: 0xFFFFFF, sidebar: 0xF3F3F3, agentBubble: 0xF3F3F3, userBubble: 0xE1ECF8,
                primaryText: 0x3B3B3B, secondaryText: 0x616161, separator: 0xE5E5E5, accent: 0x005FB8,
                codeBackground: 0xF3F3F3, keyword: 0x0000FF, string: 0xA31515, number: 0x0E7E55,
                comment: 0x008000, type: 0x29778E, function: 0x795E26, punctuation: 0x3B3B3B),
            dark: nil,
            background: nil),
        WonderThemeSpec(id: "one-dark-pro", name: "One Dark Pro", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x282C34, sidebar: 0x21242A, agentBubble: 0x2C313A, userBubble: 0x3E4451,
                primaryText: 0xABB2BF, secondaryText: 0x9AA1AE, separator: 0x181A1F, accent: 0x61AFEF,
                codeBackground: 0x21252B, keyword: 0xB896CE, string: 0x98C379, number: 0xD19A66,
                comment: 0x878C97, type: 0xE5C07B, function: 0x61AFEF, punctuation: 0xABB2BF),
            background: nil),
        WonderThemeSpec(id: "dracula", name: "Dracula", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x282A36, sidebar: 0x21222C, agentBubble: 0x343746, userBubble: 0x44475A,
                primaryText: 0xF8F8F2, secondaryText: 0xA4B0DA, separator: 0x191A21, accent: 0xBD93F9,
                codeBackground: 0x21222C, keyword: 0xFF79C6, string: 0xF1FA8C, number: 0xBD93F9,
                comment: 0x8A97C8, type: 0x8BE9FD, function: 0x50FA7B, punctuation: 0xF8F8F2),
            background: nil),
        WonderThemeSpec(id: "monokai", name: "Monokai", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x272822, sidebar: 0x1E1F1C, agentBubble: 0x3E3D32, userBubble: 0x49483E,
                primaryText: 0xF8F8F2, secondaryText: 0xB4B0A0, separator: 0x3B3A32, accent: 0xA6E22E,
                codeBackground: 0x1E1F1C, keyword: 0xF96D9E, string: 0xE6DB74, number: 0xAE81FF,
                comment: 0x8F8B73, type: 0x66D9EF, function: 0xA6E22E, punctuation: 0xF8F8F2),
            background: nil),
        WonderThemeSpec(id: "solarized-dark", name: "Solarized Dark", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x002B36, sidebar: 0x00212B, agentBubble: 0x073642, userBubble: 0x063A47,
                primaryText: 0x93A1A1, secondaryText: 0x93A1A1, separator: 0x073642, accent: 0x268BD2,
                codeBackground: 0x00212B, keyword: 0x8F9F6F, string: 0x2AA198, number: 0xAF7394,
                comment: 0x75898F, type: 0xB58900, function: 0x268BD2, punctuation: 0x839496),
            background: nil),
        WonderThemeSpec(id: "solarized-light", name: "Solarized Light", scheme: .light,
            light: ThemePalette(
                background: 0xFDF6E3, sidebar: 0xEEE8D5, agentBubble: 0xEEE8D5, userBubble: 0xE1DAC2,
                primaryText: 0x4A5E65, secondaryText: 0x4F656C, separator: 0xDDD6C1, accent: 0x268BD2,
                codeBackground: 0xEEE8D5, keyword: 0x52654F, string: 0x2F7271, number: 0xAA4279,
                comment: 0x586B71, type: 0x786726, function: 0x306D95, punctuation: 0x566B72),
            dark: nil,
            background: nil),
        WonderThemeSpec(id: "nord", name: "Nord", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x2E3440, sidebar: 0x282C37, agentBubble: 0x3B4252, userBubble: 0x434C5E,
                primaryText: 0xD8DEE9, secondaryText: 0xB0B8C7, separator: 0x3B4252, accent: 0x88C0D0,
                codeBackground: 0x292E39, keyword: 0x9CC0F5, string: 0xA3BE8C, number: 0xB48EAD,
                comment: 0x8A96AD, type: 0x8FBCBB, function: 0x88C0D0, punctuation: 0xD8DEE9),
            background: nil),
        WonderThemeSpec(id: "gruvbox-dark", name: "Gruvbox Dark", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x282828, sidebar: 0x1D2021, agentBubble: 0x3C3836, userBubble: 0x504945,
                primaryText: 0xEBDBB2, secondaryText: 0xB0A18C, separator: 0x3C3836, accent: 0xD79921,
                codeBackground: 0x1D2021, keyword: 0xF6785C, string: 0xB8BB26, number: 0xD3869B,
                comment: 0x948575, type: 0xFABD2F, function: 0x8EC07C, punctuation: 0xEBDBB2),
            background: nil),
        WonderThemeSpec(id: "tokyo-night", name: "Tokyo Night", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x1A1B26, sidebar: 0x101016, agentBubble: 0x24283B, userBubble: 0x292E42,
                primaryText: 0xC0CAF5, secondaryText: 0x8F96BC, separator: 0x101014, accent: 0x7AA2F7,
                codeBackground: 0x16161E, keyword: 0xBB9AF7, string: 0x9ECE6A, number: 0xFF9E64,
                comment: 0x7580AB, type: 0x2AC3DE, function: 0x7AA2F7, punctuation: 0x89DDFF),
            background: nil),
        WonderThemeSpec(id: "catppuccin-mocha", name: "Catppuccin Mocha", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x1E1E2E, sidebar: 0x14141F, agentBubble: 0x313244, userBubble: 0x45475A,
                primaryText: 0xCDD6F4, secondaryText: 0xA6ADC8, separator: 0x313244, accent: 0x89B4FA,
                codeBackground: 0x181825, keyword: 0xCBA6F7, string: 0xA6E3A1, number: 0xFAB387,
                comment: 0x7F849C, type: 0xF9E2AF, function: 0x89B4FA, punctuation: 0x9399B2),
            background: nil),
        WonderThemeSpec(id: "catppuccin-latte", name: "Catppuccin Latte", scheme: .light,
            light: ThemePalette(
                background: 0xEFF1F5, sidebar: 0xE1E4EA, agentBubble: 0xDCE0E8, userBubble: 0xCCD0DA,
                primaryText: 0x4C4F69, secondaryText: 0x5C5F77, separator: 0xCCD0DA, accent: 0x1E66F5,
                codeBackground: 0xDCE0E8, keyword: 0x5F4894, string: 0x336F2F, number: 0x9C4C22,
                comment: 0x5E6179, type: 0x835A30, function: 0x2F5DC1, punctuation: 0x5E6179),
            dark: nil,
            background: nil),
        WonderThemeSpec(id: "github-dark", name: "GitHub Dark", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x0D1117, sidebar: 0x000001, agentBubble: 0x161B22, userBubble: 0x21262D,
                primaryText: 0xE6EDF3, secondaryText: 0x8B949E, separator: 0x30363D, accent: 0x2F81F7,
                codeBackground: 0x161B22, keyword: 0xFF7B72, string: 0xA5D6FF, number: 0x79C0FF,
                comment: 0x8B949E, type: 0xFFA657, function: 0xD2A8FF, punctuation: 0xE6EDF3),
            background: nil),
        WonderThemeSpec(id: "github-light", name: "GitHub Light", scheme: .light,
            light: ThemePalette(
                background: 0xFFFFFF, sidebar: 0xF1F3F5, agentBubble: 0xF6F8FA, userBubble: 0xDDF4FF,
                primaryText: 0x1F2328, secondaryText: 0x59636E, separator: 0xD0D7DE, accent: 0x0969DA,
                codeBackground: 0xF6F8FA, keyword: 0xB6222D, string: 0x0A3069, number: 0x0550AE,
                comment: 0x69727C, type: 0x953800, function: 0x8250DF, punctuation: 0x1F2328),
            dark: nil,
            background: nil),
        WonderThemeSpec(id: "night-owl", name: "Night Owl", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x011627, sidebar: 0x00060B, agentBubble: 0x0B2942, userBubble: 0x1D3B53,
                primaryText: 0xD6DEEB, secondaryText: 0x8CA3B8, separator: 0x0B253A, accent: 0x82AAFF,
                codeBackground: 0x01111D, keyword: 0xC792EA, string: 0xECC48D, number: 0xF78C6C,
                comment: 0x6D8081, type: 0xFFCB8B, function: 0x82AAFF, punctuation: 0x89DDFF),
            background: nil),
        WonderThemeSpec(id: "rose-pine", name: "Rosé Pine", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x191724, sidebar: 0x0C0B12, agentBubble: 0x1F1D2E, userBubble: 0x26233A,
                primaryText: 0xE0DEF4, secondaryText: 0x908CAA, separator: 0x26233A, accent: 0xC4A7E7,
                codeBackground: 0x16141F, keyword: 0x5B9DBC, string: 0xF6C177, number: 0xEBBCBA,
                comment: 0x807D98, type: 0x9CCFD8, function: 0xEBBCBA, punctuation: 0x908CAA),
            background: nil),
        WonderThemeSpec(id: "herbalist-dark", name: "Herbalist Dark", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x22322E, sidebar: 0x18201F, agentBubble: 0x2C3F3A, userBubble: 0x34524A,
                primaryText: 0xF8F7F3, secondaryText: 0xB8C6BF, separator: 0x454E4E, accent: 0x7AAE8F,
                codeBackground: 0x1A2323, keyword: 0x8CBCCF, string: 0xF4E5B0, number: 0xF2856B,
                comment: 0x7AAE8F, type: 0xACDFB2, function: 0xAEE4CE, punctuation: 0x89DDFF),
            background: nil),
        WonderThemeSpec(id: "herbalist-light", name: "Herbalist Light", scheme: .light,
            light: ThemePalette(
                background: 0xF8F7F3, sidebar: 0xE8E6DF, agentBubble: 0xFFFFFF, userBubble: 0xDCEBE1,
                primaryText: 0x22322E, secondaryText: 0x4F5F59, separator: 0xDDD9D0, accent: 0x1A5D4A,
                codeBackground: 0xF0EFEB, keyword: 0x3A5E70, string: 0x84672C, number: 0xA45731,
                comment: 0x4F7460, type: 0x1A5D4A, function: 0x1A5D4A, punctuation: 0x22322E),
            dark: nil,
            background: nil),
        WonderThemeSpec(id: "misty-forest", name: "Misty Forest", scheme: .light,
            light: ThemePalette(
                background: 0xE9EEEA, sidebar: 0xDBE3DD, agentBubble: 0xF6F8F6, userBubble: 0xDCE8DF,
                primaryText: 0x1B2A22, secondaryText: 0x3F5247, separator: 0xB9C8BE, accent: 0x3E6B52,
                codeBackground: 0xEEF2EF, keyword: 0x8A3F7A, string: 0x9A3412, number: 0x1F4E9F,
                comment: 0x5D6E64, type: 0x1B6B6B, function: 0x3B5F9A, punctuation: 0x1B2A22),
            dark: nil,
            background: ThemeBackground(asset: "ThemeBackgroundForest", scrim: 0.72, pixelRange: 85...251)),
        WonderThemeSpec(id: "ocean-waves", name: "Ocean Waves", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x0B1F2A, sidebar: 0x07131B, agentBubble: 0x12303F, userBubble: 0x1B4457,
                primaryText: 0xE6F1F5, secondaryText: 0x9FB9C4, separator: 0x1C3A49, accent: 0x5CC8D8,
                codeBackground: 0x08161E, keyword: 0xC792EA, string: 0xA3E4B0, number: 0xF2B880,
                comment: 0x7F9BA6, type: 0x6FD3E0, function: 0x8CB4F5, punctuation: 0xE6F1F5),
            background: ThemeBackground(asset: "ThemeBackgroundOcean", scrim: 0.7, pixelRange: 21...168)),
        WonderThemeSpec(id: "desert-dunes", name: "Desert Dunes", scheme: .light,
            light: ThemePalette(
                background: 0xF6EBDD, sidebar: 0xEBDCC7, agentBubble: 0xFFF9F1, userBubble: 0xF6DCC4,
                primaryText: 0x3A2418, secondaryText: 0x6A4B3A, separator: 0xD8C3AA, accent: 0xB4540A,
                codeBackground: 0xFBF3E8, keyword: 0xA3236B, string: 0x8A3B12, number: 0x7A3E00,
                comment: 0x7A6454, type: 0x2E6E73, function: 0x4A5BA0, punctuation: 0x3A2418),
            dark: nil,
            background: ThemeBackground(asset: "ThemeBackgroundDunes", scrim: 0.74, pixelRange: 99...245)),
        WonderThemeSpec(id: "desert-dunes-night", name: "Desert Dunes Night", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x1B1622, sidebar: 0x0B080F, agentBubble: 0x272031, userBubble: 0x3B2E2B,
                primaryText: 0xF3E9DC, secondaryText: 0xC4B5A5, separator: 0x35293B, accent: 0xF0A35E,
                codeBackground: 0x15111B, keyword: 0xF28FB8, string: 0xF2B880, number: 0xE6C07B,
                comment: 0x9F918A, type: 0x7FC8C8, function: 0x9AB0F0, punctuation: 0xF3E9DC),
            background: ThemeBackground(asset: "ThemeBackgroundDunesNight", scrim: 0.8, pixelRange: 4...238)),
        WonderThemeSpec(id: "geometric", name: "Geometric", scheme: .dark,
            light: nil,
            dark: ThemePalette(
                background: 0x14152B, sidebar: 0x080812, agentBubble: 0x1E2040, userBubble: 0x2B2E5A,
                primaryText: 0xE8E9F8, secondaryText: 0xA9ACD0, separator: 0x2B2E58, accent: 0x9AA2FF,
                codeBackground: 0x0F1022, keyword: 0xC4A1FF, string: 0x9EE6B8, number: 0xF5B98A,
                comment: 0x8F93BD, type: 0x7ED6F0, function: 0x9AB4FF, punctuation: 0xE8E9F8),
            background: ThemeBackground(asset: "ThemeBackgroundGeometric", scrim: 0.74, pixelRange: 61...139)),
    ]
}

/// Notices for palettes derived from others' work, shown in Settings →
/// Acknowledgements. A derived palette keeps its source's notice here.
public enum ThemeCredits {
    public struct Credit: Sendable {
        public let title: String
        public let detail: String
        public let themeIDs: [String]
        public let license: String
    }
    /// Notices for palettes derived from others' work.
    public static let all: [Credit] = [
        Credit(title: "Apothecary Diary theme", detail: "Colors of Herbalist · MIT",
               themeIDs: ["herbalist-dark", "herbalist-light"],
               license: """
            The Herbalist themes adapt colours from the Apothecary Diary \
            theme for Visual Studio Code (github.com/montemurro19/apothecary-diary-theme).

            MIT License

            Copyright (c) 2025 Matheus Montemurro

            Permission is hereby granted, free of charge, to any person obtaining a copy
            of this software and associated documentation files (the "Software"), to deal
            in the Software without restriction, including without limitation the rights
            to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
            copies of the Software, and to permit persons to whom the Software is
            furnished to do so, subject to the following conditions:

            The above copyright notice and this permission notice shall be included in all
            copies or substantial portions of the Software.

            THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
            IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
            FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
            AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
            LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
            OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
            SOFTWARE.
            """),
    ]
}
