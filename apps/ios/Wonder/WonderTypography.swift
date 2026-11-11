import CoreText
import SwiftUI
import UIKit

/// Settings → Appearance → Font: the typeface for message text (bubbles, the
/// composer and rendered Markdown). Apple's system designs need no bundling; the
/// others ship unmodified in `Fonts/` under the SIL Open Font License.
enum MessageFont: String, CaseIterable, Identifiable, Sendable {
    case system, serif, rounded, monospaced, inter, atkinson

    var id: String { rawValue }
    var name: String {
        switch self {
        case .system: "Default"
        case .serif: "New York"
        case .rounded: "Rounded"
        case .monospaced: "Monospaced"
        case .inter: "Inter"
        case .atkinson: "Atkinson Hyperlegible"
        }
    }
    var detail: String {
        switch self {
        case .system: "San Francisco, Apple's system font"
        case .serif: "Apple's serif"
        case .rounded: "San Francisco Rounded"
        case .monospaced: "SF Mono"
        case .inter: "A neutral sans for screens"
        case .atkinson: "Clear letter shapes for low vision"
        }
    }
    fileprivate var design: Font.Design? {
        switch self {
        case .system: .default
        case .serif: .serif
        case .rounded: .rounded
        case .monospaced: .monospaced
        case .inter, .atkinson: nil
        }
    }
    /// The bundled regular face; bold and italic come from the same family.
    fileprivate var face: String? {
        switch self {
        case .inter: "Inter-Regular"
        case .atkinson: "AtkinsonHyperlegible-Regular"
        default: nil
        }
    }
    /// The license shipped beside a bundled family, in `Fonts/`.
    var licenseFile: String? {
        switch self {
        case .inter: "Inter-OFL"
        case .atkinson: "AtkinsonHyperlegible-OFL"
        default: nil
        }
    }
}

/// The typeface for code blocks, file sources and diffs.
enum CodeFont: String, CaseIterable, Identifiable, Sendable {
    case system, jetBrains, plex

    var id: String { rawValue }
    var name: String {
        switch self {
        case .system: "SF Mono"
        case .jetBrains: "JetBrains Mono"
        case .plex: "IBM Plex Mono"
        }
    }
    fileprivate var face: String? {
        switch self {
        case .system: nil
        case .jetBrains: "JetBrainsMono-Regular"
        case .plex: "IBMPlexMono-Regular"
        }
    }
    var licenseFile: String? {
        switch self {
        case .system: nil
        case .jetBrains: "JetBrainsMono-OFL"
        case .plex: "IBMPlexMono-OFL"
        }
    }
}

/// The one owner of Wonder's text faces, injected through the environment by
/// `WonderThemeHost` next to the theme. Every face scales with Dynamic Type:
/// system faces by text style, bundled ones through `relativeTo:` and
/// `UIFontMetrics`.
struct WonderTypography: Equatable, Sendable {
    static let messageKey = "wonderMessageFont"
    static let codeKey = "wonderCodeFont"

    var message: MessageFont = .system
    var code: CodeFont = .system

    init(message: MessageFont = .system, code: CodeFont = .system) {
        self.message = message
        self.code = code
    }
    init(messageID: String, codeID: String) {
        self.init(message: MessageFont(rawValue: messageID) ?? .system, code: CodeFont(rawValue: codeID) ?? .system)
    }

    /// Part of every cache key for text prepared with these faces.
    var cacheKey: String { "\(message.rawValue)-\(code.rawValue)" }

    /// Message text in a text style. Headline keeps the system's semibold.
    func font(_ style: Font.TextStyle) -> Font {
        if let design = message.design { return .system(style, design: design) }
        BundledFonts.registerIfNeeded()
        let font = Font.custom(message.face!, size: Self.baseSize(style), relativeTo: style)
        return style == .headline ? font.weight(.semibold) : font
    }

    /// Code in a text style.
    func codeFont(_ style: Font.TextStyle) -> Font {
        guard let face = code.face else { return .system(style, design: .monospaced) }
        BundledFonts.registerIfNeeded()
        return .custom(face, size: Self.baseSize(style), relativeTo: style)
    }

    /// Message text for UIKit text views, scaled for the current text size.
    func uiFont(_ style: UIFont.TextStyle) -> UIFont {
        let preferred = UIFont.preferredFont(forTextStyle: style)
        if let design = message.design {
            let system: UIFontDescriptor.SystemDesign = switch design {
            case .serif: .serif
            case .rounded: .rounded
            case .monospaced: .monospaced
            default: .default
            }
            return preferred.fontDescriptor.withDesign(system).map { UIFont(descriptor: $0, size: 0) } ?? preferred
        }
        BundledFonts.registerIfNeeded()
        guard let base = UIFont(name: message.face!, size: Self.baseSize(style)) else { return preferred }
        return UIFontMetrics(forTextStyle: style).scaledFont(for: base)
    }

    /// Code for UIKit text views: `size` at the default text size, scaled like `style`.
    func codeUIFont(_ style: UIFont.TextStyle, size: CGFloat) -> UIFont {
        let system = UIFont.monospacedSystemFont(ofSize: size, weight: .regular)
        guard let face = code.face else { return UIFontMetrics(forTextStyle: style).scaledFont(for: system) }
        BundledFonts.registerIfNeeded()
        return UIFontMetrics(forTextStyle: style).scaledFont(for: UIFont(name: face, size: size) ?? system)
    }

    /// The point size of a text style at the default (Large) text size; the scaling
    /// for the reader's text size is applied on top. Looked up once, not per row.
    private static func baseSize(_ style: Font.TextStyle) -> CGFloat { baseSize(style.uiStyle) }
    private static func baseSize(_ style: UIFont.TextStyle) -> CGFloat {
        baseSizes[style] ?? 17
    }
    private static let baseSizes: [UIFont.TextStyle: CGFloat] = {
        let large = UITraitCollection(preferredContentSizeCategory: .large)
        let styles: [UIFont.TextStyle] = [.largeTitle, .title1, .title2, .title3, .headline, .subheadline,
                                          .body, .callout, .footnote, .caption1, .caption2]
        return Dictionary(uniqueKeysWithValues: styles.map {
            ($0, UIFont.preferredFont(forTextStyle: $0, compatibleWith: large).pointSize)
        })
    }()
}

private extension Font.TextStyle {
    var uiStyle: UIFont.TextStyle {
        switch self {
        case .largeTitle: .largeTitle
        case .title: .title1
        case .title2: .title2
        case .title3: .title3
        case .headline: .headline
        case .subheadline: .subheadline
        case .callout: .callout
        case .footnote: .footnote
        case .caption: .caption1
        case .caption2: .caption2
        default: .body
        }
    }
}

/// Registers the bundled fonts for this process once, on first use.
enum BundledFonts {
    private static let registered: Void = {
        let urls = Bundle.main.urls(forResourcesWithExtension: "ttf", subdirectory: "Fonts") ?? []
        CTFontManagerRegisterFontURLs(urls as CFArray, .process, false, nil)
        #if WONDER_DIAGNOSTICS
        // A renamed or missing file would silently fall back to the system font;
        // fail loudly in Diagnostics builds so scenarios catch it.
        let faces = MessageFont.allCases.compactMap(\.face) + CodeFont.allCases.compactMap(\.face)
        for face in faces where UIFont(name: face, size: 12) == nil {
            preconditionFailure("Bundled font \(face) did not register")
        }
        // Every bundled family ships its license (OFL requires it) and shows it in the app.
        let bundled = MessageFont.allCases.filter { $0.face != nil }.map { ($0.name, $0.licenseFile) }
            + CodeFont.allCases.filter { $0.face != nil }.map { ($0.name, $0.licenseFile) }
        for (family, file) in bundled where file.flatMap(licenseText) == nil {
            preconditionFailure("Bundled font \(family) has no license text; add its license file to Fonts/ and licenseFile")
        }
        #endif
    }()
    static func registerIfNeeded() { _ = registered }

    /// Each bundled family and its license, for Settings → Acknowledgements and
    /// a font's long-press menu. Derived from the fonts, so none is left out.
    static var licenses: [(family: String, file: String)] {
        MessageFont.allCases.compactMap { font in font.licenseFile.map { (font.name, $0) } }
            + CodeFont.allCases.compactMap { font in font.licenseFile.map { (font.name, $0) } }
    }
    static func licenseText(_ file: String) -> String? {
        Bundle.main.url(forResource: file, withExtension: "txt", subdirectory: "Fonts")
            .flatMap { try? String(contentsOf: $0, encoding: .utf8) }
    }
}

private struct WonderTypographyKey: EnvironmentKey {
    static let defaultValue = WonderTypography()
}

extension EnvironmentValues {
    var wonderTypography: WonderTypography {
        get { self[WonderTypographyKey.self] }
        set { self[WonderTypographyKey.self] = newValue }
    }
}

// MARK: Settings

/// Settings → Appearance → Font: one row per face, drawn in that face.
struct FontPickerView: View {
    @AppStorage(WonderTypography.messageKey) private var messageID = MessageFont.system.rawValue
    @AppStorage(WonderTypography.codeKey) private var codeID = CodeFont.system.rawValue

    var body: some View {
        WonderList {
            Section {
                ForEach(MessageFont.allCases, id: \.settingsSearchID) { option in
                    row(id: "message-font:\(option.rawValue)", title: option.name, detail: option.detail,
                        sample: WonderTypography(message: option).font(.body), licenseFile: option.licenseFile,
                        selected: (MessageFont(rawValue: messageID) ?? .system) == option) { messageID = option.rawValue }
                }
            } header: {
                SettingsScopeHeader(title: "Messages")
            } footer: {
                Text("Used for messages, the composer and Markdown files. Every font follows your text size setting.")
            }.id(SettingsSearch.Anchor.messageFonts.id)
            Section {
                ForEach(CodeFont.allCases, id: \.settingsSearchID) { option in
                    row(id: "code-font:\(option.rawValue)", title: option.name, detail: nil,
                        sample: WonderTypography(code: option).codeFont(.body), licenseFile: option.licenseFile,
                        selected: (CodeFont(rawValue: codeID) ?? .system) == option) { codeID = option.rawValue }
                }
            } header: {
                SettingsScopeHeader(title: "Code")
            } footer: {
                Text("Touch and hold a font to read its license.")
            }.id(SettingsSearch.Anchor.codeFonts.id)
        }
        .settingsNavigationTitle("Font")
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("font-picker")
        .sheet(item: $license) { license in
            NavigationStack {
                LicenseTextView(title: license.family, text: BundledFonts.licenseText(license.file))
                    .toolbar { Button("Done") { self.license = nil }.accessibilityIdentifier("font-license-done") }
            }
        }
    }

    private struct LicenseTarget: Identifiable {
        let family: String
        let file: String
        var id: String { file }
    }
    @State private var license: LicenseTarget?

    private func row(id: String, title: String, detail: String?, sample: Font, licenseFile: String?, selected: Bool,
                     choose: @escaping () -> Void) -> some View {
        let show = { if let licenseFile { license = LicenseTarget(family: title, file: licenseFile) } }
        return Button(action: choose) {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).font(sample).foregroundStyle(.primary)
                    if let detail { Text(detail).font(.caption).foregroundStyle(.secondary) }
                }
                Spacer(minLength: 8)
                Image(systemName: "checkmark").foregroundStyle(.tint).opacity(selected ? 1 : 0)
            }
            .frame(minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? [.isSelected] : [])
        .accessibilityIdentifier(id).settingsRow(id)
        .contextMenu {
            if licenseFile != nil {
                Button("License", systemImage: "doc.text", action: show).accessibilityIdentifier("font-license")
            } else {
                Text("Built into iOS")
            }
        }
        .accessibilityActions { if licenseFile != nil { Button("License", action: show) } }
    }
}

/// One license as shipped in the app, selectable for copying.
struct LicenseTextView: View {
    let title: String
    let text: String?
    @State private var chunks: [String] = []
    @State private var copied = false

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(chunks.indices, id: \.self) { index in
                    Text(verbatim: chunks[index])
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .accessibilityIdentifier("license-chunk:\(index)")
                }
            }
            .font(.footnote.monospaced())
            .textSelection(.enabled)
            .padding()
        }
        .wonderPage()
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("license-text")
        .toolbar {
            Button(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc") {
                UIPasteboard.general.string = text ?? "License text unavailable."
                copied = true
            }
            .accessibilityIdentifier("copy-license")
        }
        .task(id: text) {
            copied = false
            let source = text ?? "License text unavailable."
            let work = Task.detached(priority: .userInitiated) {
                let raw = Self.makeChunks(source)
                return raw.enumerated().map { index, chunk in
                    // The next stack row supplies this line break. Removing it
                    // here keeps the original line spacing across chunk boundaries.
                    index < raw.count - 1 && chunk.last?.isNewline == true
                        ? String(chunk.dropLast()) : chunk
                }
            }
            let prepared = await withTaskCancellationHandler {
                await work.value
            } onCancel: {
                work.cancel()
            }
            guard !Task.isCancelled else { return }
            chunks = prepared
        }
    }

    /// Bound every Text, including notices without blank lines. Keep all original
    /// characters so selection and the full-document Copy action retain the notice.
    nonisolated static func makeChunks(_ text: String) -> [String] {
        var chunks: [String] = []
        var start = text.startIndex
        var cursor = start
        var lastNewline: String.Index?
        var characters = 0
        var lines = 0
        while cursor < text.endIndex {
            guard !Task.isCancelled else { return [] }
            let character = text[cursor]
            cursor = text.index(after: cursor)
            characters += 1
            if character.isNewline {
                lastNewline = cursor
                lines += 1
            }
            if characters >= 768 || lines >= 16 {
                let end = lastNewline ?? cursor
                chunks.append(String(text[start..<end]))
                start = end
                cursor = end
                lastNewline = nil
                characters = 0
                lines = 0
            }
        }
        if start < text.endIndex { chunks.append(String(text[start...])) }
        return chunks.isEmpty ? [text] : chunks
    }
}

extension WonderTypography {
    /// The reader's fonts for native reply components and converted pages.
    var nativeUI: NativeUIFonts {
        NativeUIFonts(body: font(.body), heading: font(.headline), caption: font(.footnote),
                      title: font(.title2), section: font(.title3), table: font(.subheadline))
    }
}
