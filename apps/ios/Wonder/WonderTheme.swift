import ImageIO
import SwiftUI
import UIKit
import WonderPairing

/// The one owner of Wonder's colours: a theme from `WonderThemeCatalog`
/// resolved for one appearance. It is a small static value injected through the
/// environment by `WonderThemeHost`; views read tokens and never compute colours
/// per row.
struct WonderTheme: Equatable {
    static let storageKey = "wonderThemeID"

    let spec: WonderThemeSpec
    let isDark: Bool
    let palette: ThemePalette

    /// The tokens native reply components and converted pages draw with,
    /// prepared once per theme (status colours are contrast-checked there).
    let nativeUI: NativeUITheme

    init(spec: WonderThemeSpec, systemIsDark: Bool) {
        self.spec = spec
        let palette = spec.palette(systemIsDark: systemIsDark)
        self.palette = palette
        isDark = spec.forcedDark ?? systemIsDark
        let isDefault = spec.id == WonderThemeCatalog.defaultID
        nativeUI = NativeUITheme(
            surface: isDefault ? Color(uiColor: .tertiarySystemBackground) : Color(hex: palette.agentBubble),
            background: isDefault ? Color(uiColor: .systemBackground) : Color(hex: palette.background),
            separator: isDefault ? Color(uiColor: .separator) : Color(hex: palette.separator),
            secondaryText: isDefault ? Color.secondary : Color(hex: palette.secondaryText),
            accent: Color(hex: palette.accent),
            success: Color(hex: palette.status(.success)), warning: Color(hex: palette.status(.warning)),
            error: Color(hex: palette.status(.error)), info: Color(hex: palette.status(.info)),
            series: palette.series.map { Color(hex: $0) })
    }

    static func resolve(id: String?, systemIsDark: Bool) -> WonderTheme {
        WonderTheme(spec: WonderThemeCatalog.theme(id: id), systemIsDark: systemIsDark)
    }

    var id: String { spec.id }
    /// The default theme keeps iOS's own tint and bubble-palette choices.
    var isDefault: Bool { spec.id == WonderThemeCatalog.defaultID }
    var photo: ThemeBackground? { spec.background }
    var forcedScheme: ColorScheme? { spec.forcedDark.map { $0 ? .dark : .light } }
    var cacheKey: String { "\(spec.id)-\(isDark ? "d" : "l")" }

    var background: Color { Color(hex: palette.background) }
    var sidebar: Color { Color(hex: palette.sidebar) }
    var primaryText: Color { Color(hex: palette.primaryText) }
    var secondaryText: Color { Color(hex: palette.secondaryText) }
    var separator: Color { Color(hex: palette.separator) }
    var accent: Color { Color(hex: palette.accent) }
    var codeBackground: Color { Color(hex: palette.codeBackground) }
    /// A search field on the sidebar: lighter than the sidebar in dark, white-ish in light.
    var field: Color { Color(hex: isDark ? palette.agentBubble : palette.background) }
    /// The composer and navigation bar; slightly see-through over a photo.
    var chrome: Color { photo == nil ? background : background.opacity(0.9) }

    /// Controls that sit on the page, such as the composer and its pills: the system
    /// fill on the default theme, the agent bubble's colour on any other.
    var surface: Color { isDefault ? Color(uiColor: .secondarySystemBackground) : Color(hex: palette.agentBubble) }
    /// A plain screen or sheet: the system background on the default theme.
    var page: Color { isDefault ? Color(uiColor: .systemBackground) : background }
    /// Body text on the page, for SwiftUI and for UIKit text views.
    var text: Color { isDefault ? Color.primary : primaryText }
    var textUIColor: UIColor { isDefault ? .label : UIColor(hex: palette.primaryText) }
    /// `page` and `surface` for UIKit, resolved for this theme's appearance.
    var pageUIColor: UIColor { resolved(isDefault ? .systemBackground : UIColor(hex: palette.background)) }
    var surfaceUIColor: UIColor { resolved(isDefault ? .secondarySystemBackground : UIColor(hex: palette.agentBubble)) }
    private func resolved(_ color: UIColor) -> UIColor {
        color.resolvedColor(with: UITraitCollection(userInterfaceStyle: isDark ? .dark : .light))
    }
    /// Small chips and choices that sit on a surface or the page.
    var chip: Color { isDefault ? Color(uiColor: .tertiarySystemBackground) : surface }
    /// Hairlines between columns, such as a split diff's.
    var rule: Color { isDefault ? Color(uiColor: .separator) : separator }

    func bubble(isUser: Bool) -> Color {
        Color(hex: isUser ? palette.userBubble : palette.agentBubble).opacity(photo == nil ? 1 : 0.92)
    }
    func syntax(_ role: SyntaxRole) -> Color { Color(hex: palette.color(for: role)) }
}

extension Color {
    init(hex: UInt32) {
        self.init(.sRGB, red: Double((hex >> 16) & 0xFF) / 255, green: Double((hex >> 8) & 0xFF) / 255,
                  blue: Double(hex & 0xFF) / 255, opacity: 1)
    }
}

private struct WonderThemeKey: EnvironmentKey {
    static let defaultValue = WonderTheme.resolve(id: nil, systemIsDark: false)
}

extension EnvironmentValues {
    var wonderTheme: WonderTheme {
        get { self[WonderThemeKey.self] }
        set { self[WonderThemeKey.self] = newValue }
    }
}

/// Injects the chosen theme and applies what must follow it app-wide: the
/// forced appearance, the tint, the navigation background and solid headers.
struct WonderThemeHost: ViewModifier {
    @AppStorage(WonderTheme.storageKey) private var themeID = WonderThemeCatalog.defaultID
    @AppStorage(WonderTypography.messageKey) private var messageFontID = MessageFont.system.rawValue
    @AppStorage(WonderTypography.codeKey) private var codeFontID = CodeFont.system.rawValue
    @Environment(\.colorScheme) private var systemScheme

    func body(content: Content) -> some View {
        let theme = WonderTheme.resolve(id: ProcessInfo.processInfo.diagnosticThemeOverride ?? themeID,
                                        systemIsDark: systemScheme == .dark)
        return content
            .onChange(of: theme.spec, initial: true) { _, spec in SyntaxColors.activate(spec) }
            // The share extension cannot read the app's preferences; it reads this.
            .onChange(of: themeID, initial: true) { _, id in try? PhoneIdentity().save(Data(id.utf8), account: ShareComposition.themeKeychainAccount) }
            .environment(\.wonderTheme, theme)
            .environment(\.wonderTypography, WonderTypography(
                messageID: ProcessInfo.processInfo.diagnosticFontOverride ?? messageFontID,
                codeID: ProcessInfo.processInfo.diagnosticCodeFontOverride ?? codeFontID))
            .preferredColorScheme(theme.forcedScheme)
            // Actions read as plain text, not blue links; the default theme keeps `.primary`.
            .tint(theme.isDefault ? Color.primary : theme.primaryText)
            .containerBackground(theme.isDefault ? Color(uiColor: .systemBackground) : theme.background, for: .navigation)
            // The soft default let text scrolled under a page's title show through it.
            .scrollEdgeEffectStyle(.hard, for: .top)
    }
}

/// A native filled button whose label follows the theme and enabled state.
struct WonderProminentButtonStyle: PrimitiveButtonStyle {
    @Environment(\.wonderTheme) private var theme
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        Button(role: configuration.role, action: configuration.trigger) {
            configuration.label.foregroundStyle(isEnabled ? theme.page : theme.secondaryText)
        }
        .buttonStyle(.borderedProminent) // theme-exempt: the one owner of the prominent style
    }
}

extension PrimitiveButtonStyle where Self == WonderProminentButtonStyle {
    static var wonderProminent: WonderProminentButtonStyle { WonderProminentButtonStyle() }
}

/// Answer fields use the same surface as their choices, including in photo themes.
struct WonderTextFieldStyle: TextFieldStyle {
    @Environment(\.wonderTheme) private var theme

    func _body(configuration: TextField<Self._Label>) -> some View {
        configuration
            .textFieldStyle(.plain)
            .foregroundStyle(theme.text)
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .frame(minHeight: 44)
            .background(theme.surface, in: RoundedRectangle(cornerRadius: 8))
    }
}

extension TextFieldStyle where Self == WonderTextFieldStyle {
    static var wonder: WonderTextFieldStyle { WonderTextFieldStyle() }
}

/// Row backgrounds must be applied inside the native container. Applying the same
/// modifier to the List or Form itself leaves its rows in the system colour.
///
/// `plainRows` draws plain rows on the page colour instead of grouped cards, for a
/// list that already sits in its own panel (Files in the companion pane).
struct WonderList<Content: View>: View {
    @Environment(\.wonderTheme) private var theme
    let overBackdrop: Bool
    let plainRows: Bool
    let content: Content

    init(overBackdrop: Bool = false, plainRows: Bool = false, @ViewBuilder content: () -> Content) {
        self.overBackdrop = overBackdrop
        self.plainRows = plainRows
        self.content = content()
    }

    var body: some View {
        if plainRows {
            List { content.listRowBackground(theme.page) }
                .listStyle(.plain)
                .scrollContentBackground(.hidden)
                .background(theme.page)
                .modifier(WonderGroupedStyle(overBackdrop: true))
        } else {
            List { content.modifier(WonderGroupedRows()) }
                .modifier(WonderGroupedStyle(overBackdrop: overBackdrop))
        }
    }
}

struct WonderForm<Content: View>: View {
    let content: Content

    init(@ViewBuilder content: () -> Content) { self.content = content() }

    var body: some View {
        Form { content.modifier(WonderGroupedRows()) }
            .modifier(WonderGroupedStyle(overBackdrop: false))
    }
}

private struct WonderGroupedRows: ViewModifier {
    @Environment(\.wonderTheme) private var theme

    @ViewBuilder func body(content: Content) -> some View {
        if theme.isDefault { content }
        else { content.listRowBackground(theme.surface) }
    }
}

/// The page behind grouped controls. Wonder keeps the system look; Files can
/// leave the page clear over the conversation's existing backdrop.
///
/// Beside the iPhone Duo's vertical bar the system leaves grouped cards almost no
/// trailing margin, so they run into the bar; there they keep `barMargin`. Without
/// a bar (iPhone, iPad, earlier systems) the system margins are unchanged.
private struct WonderGroupedStyle: ViewModifier {
    private static let barMargin: CGFloat = 16

    @Environment(\.wonderTheme) private var theme
    @State private var verticalBar = false
    let overBackdrop: Bool

    func body(content: Content) -> some View {
        page(content)
            // Constrain the whole list, including native row backgrounds and
            // separators. Scroll-content margins alone leave the separator's
            // full-width layout running underneath the Duo rail.
            .safeAreaPadding(.trailing, verticalBar ? Self.barMargin : 0)
            .readsVerticalBar($verticalBar)
    }

    @ViewBuilder private func page(_ content: Content) -> some View {
        if theme.isDefault {
            content
        } else {
            content
                .scrollContentBackground(.hidden)
                .background(overBackdrop ? Color.clear : theme.background)
        }
    }
}

/// A screen or sheet that is not a grouped List: the theme's page (with its photo)
/// behind it, under the navigation bar and behind a presented sheet, and the theme's
/// text colour. The default System theme keeps the system look.
private struct WonderPage: ViewModifier {
    @Environment(\.wonderTheme) private var theme

    @ViewBuilder func body(content: Content) -> some View {
        if theme.isDefault {
            content
        } else {
            content
                .foregroundStyle(theme.primaryText)
                .containerBackground(for: .navigation) { ThemeBackdrop() }
                .presentationBackground { ThemeBackdrop() }
        }
    }
}

/// A sheet's title and optional Done button that stay put. On a short screen (the iPhone Duo) the
/// navigation bar of a scrolled sheet slides away with its Done button, so the header is
/// a fixed inset instead. Inside a NavigationStack; the system bar is hidden.
private struct PinnedSheetHeader: ViewModifier {
    @Environment(\.wonderTheme) private var theme
    let title: String
    var search: SheetSearch?
    let onDone: (() -> Void)?

    /// A sheet whose content already names itself passes an empty title; with
    /// no Done button and no search it then has no header band at all.
    private var hasHeader: Bool { !title.isEmpty || onDone != nil || search != nil }
    /// With no title band the search field is the sheet's first row: it sits below the
    /// grab indicator, whose tap target would otherwise dismiss the sheet.
    private var hasTitleBand: Bool { !title.isEmpty || onDone != nil }

    func body(content: Content) -> some View {
        content
            .toolbar(.hidden, for: .navigationBar)
            .safeAreaInset(edge: .top, spacing: 0) {
                if hasHeader {
                    VStack(spacing: 8) {
                        if !title.isEmpty || onDone != nil {
                            HStack {
                                if !title.isEmpty {
                                    Text(title).font(.headline).accessibilityAddTraits(.isHeader)
                                        .accessibilityIdentifier("sheet-title")
                                }
                                Spacer()
                                if let onDone {
                                    Button("Done", action: onDone).fontWeight(.semibold).frame(minHeight: 44)
                                        .accessibilityIdentifier("sheet-done")
                                }
                            }
                            .frame(minHeight: 44)
                        }
                        if let search { SheetSearchField(search: search) }
                    }
                    .padding(.horizontal, 20).padding(.top, hasTitleBand ? 8 : 24).padding(.bottom, search == nil ? 0 : 8)
                    .background(theme.isDefault ? AnyShapeStyle(.bar) : AnyShapeStyle(theme.background))
                }
            }
    }
}

/// A search field pinned under a sheet's header. The navigation bar these
/// sheets hide is where `.searchable` would place its field.
struct SheetSearch {
    var text: Binding<String>
    var prompt: String
    var identifier: String
}

private struct SheetSearchField: View {
    @Environment(\.wonderTheme) private var theme
    let search: SheetSearch
    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary).accessibilityHidden(true)
            TextField(search.prompt, text: search.text)
                .textInputAutocapitalization(.never).autocorrectionDisabled().submitLabel(.search)
                .accessibilityIdentifier(search.identifier)
            if !search.text.wrappedValue.isEmpty {
                Button { search.text.wrappedValue = "" } label: { Image(systemName: "xmark.circle.fill") }
                    .buttonStyle(.plain).foregroundStyle(.secondary)
                    .accessibilityLabel("Clear search").accessibilityIdentifier(search.identifier + "-clear")
            }
        }
        .padding(.horizontal, 10).frame(minHeight: 36)
        .background(Color(uiColor: .tertiarySystemFill), in: RoundedRectangle(cornerRadius: 10))
        .foregroundStyle(theme.isDefault ? Color.primary : theme.text)
    }
}

extension View {
    func pinnedSheetHeader(_ title: String, search: SheetSearch? = nil, onDone: (() -> Void)? = nil) -> some View {
        modifier(PinnedSheetHeader(title: title, search: search, onDone: onDone))
    }
    func wonderPage() -> some View { modifier(WonderPage()) }
}

extension ProcessInfo {
    /// `-diagnostics-theme <id>` forces a theme so screens can be captured.
    var diagnosticThemeOverride: String? {
        #if WONDER_DIAGNOSTICS
        DiagnosticTheme.forcedID
        #else
        nil
        #endif
    }
    /// `-diagnostics-font MESSAGE[+CODE]` forces the faces, for example `serif+jetBrains`.
    var diagnosticFontOverride: String? { diagnosticFonts?.first }
    var diagnosticCodeFontOverride: String? { diagnosticFonts.flatMap { $0.count > 1 ? $0[1] : nil } }
    private var diagnosticFonts: [String]? {
        #if WONDER_DIAGNOSTICS
        DiagnosticTheme.value(after: "-diagnostics-font").map { $0.split(separator: "+").map(String.init) }
        #else
        nil
        #endif
    }
}

/// Syntax colours for diffs, whose attributed text is prepared once off the
/// main thread. The colours resolve at draw time from the active theme, so a
/// theme change recolours them without re-preparing the diff.
enum SyntaxColors {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var active = WonderThemeCatalog.theme(id: nil)

    static func activate(_ spec: WonderThemeSpec) {
        lock.lock(); defer { lock.unlock() }
        if active != spec { active = spec }
    }

    static func color(_ role: SyntaxRole) -> Color {
        Color(uiColor: UIColor { traits in
            lock.lock(); let spec = active; lock.unlock()
            let hex = spec.palette(systemIsDark: traits.userInterfaceStyle == .dark).color(for: role)
            return UIColor(red: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
                           blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
        })
    }
}

// MARK: Background photo

/// Draws a photo theme's picture once behind the conversation, under a flat
/// scrim. It is a background, so it never affects layout, and it draws only the
/// scrim colour when Reduce Transparency is on or the theme has no photo.
struct ThemeBackdrop: View {
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency

    var body: some View {
        if theme.isDefault {
            Color.clear
        } else {
            ZStack {
                theme.background
                if let photo = theme.photo, !reduceTransparency {
                    ThemePhoto(asset: photo.asset)
                    theme.background.opacity(photo.scrim)
                }
            }
            .ignoresSafeArea()
            .accessibilityHidden(true)
        }
    }
}

/// The composer and its pills belong to the chat: they draw nothing of their own, so the
/// theme's background or photo shows through. With Reduce Transparency on they take the
/// solid chat background instead, so a photo never sits behind their text.
struct ChatBottomUnit: ViewModifier {
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency

    func body(content: Content) -> some View {
        content.background(reduceTransparency
            ? (theme.isDefault ? Color(uiColor: .systemBackground) : theme.background)
            : Color.clear)
    }
}

/// The send circle: solid when it responds, a quiet tint of the text colour with
/// a readable arrow when it cannot, so it never reads as missing on any theme.
struct SendCircleStyle: ViewModifier {
    let responds: Bool
    @Environment(\.wonderTheme) private var theme
    func body(content: Content) -> some View {
        content
            .foregroundStyle(responds ? theme.page : theme.text.opacity(0.7))
            .background(responds ? theme.text : theme.text.opacity(0.16), in: Circle())
    }
}

extension View {
    func sendCircle(responds: Bool) -> some View { modifier(SendCircleStyle(responds: responds)) }
}

/// Fade scrolled content below the floating title and above the composer with
/// one mask, leaving layout, the backdrop and overlaid controls untouched.
/// The top is clear at the boundary so a clipped action row cannot peek out.
struct TimelineScrollEdgeFade: ViewModifier {
    /// Height of the fade at the bottom edge: longer while the scroll-to-bottom
    /// strip is open, so a cut through a bubble reads as a fade, not a glitch.
    var bottomFade: CGFloat = 14
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @State private var contentAbove = false

    func body(content: Content) -> some View {
        content
        // Publish only this threshold, never a per-pixel offset to the chat.
        .onScrollGeometryChange(for: Bool.self) { geometry in
            geometry.contentOffset.y + geometry.contentInsets.top > 1
        } action: { _, above in contentAbove = above }
        // Always masked, so scrolling and accessibility changes preserve identity.
        .mask {
            VStack(spacing: 0) {
                LinearGradient(stops: [.init(color: .clear, location: 0.55),
                                       .init(color: .black, location: 1)],
                               startPoint: .top, endPoint: .bottom)
                    .frame(height: contentAbove && !reduceTransparency ? 48 : 0)
                Rectangle()
                LinearGradient(colors: [.black, .clear], startPoint: .top, endPoint: .bottom)
                    .frame(height: reduceTransparency ? 0 : bottomFade)
            }
        }
    }
}

private struct ThemePhoto: View {
    let asset: String
    @State private var image: UIImage?

    var body: some View {
        Color.clear
            .overlay {
                if let image { Image(uiImage: image).resizable().scaledToFill() }
            }
            .clipped()
            .task(id: asset) {
                let decoded = await ThemePhotoCache.shared.image(asset)
                guard !Task.isCancelled else { return }
                image = decoded
            }
            .onDisappear { image = nil }
    }
}

/// Shares cold requests across surfaces and serializes different asset decodes.
/// Only cache bookkeeping runs on the main actor; decoding stays on its worker.
@MainActor final class ThemePhotoCache {
    static let shared = ThemePhotoCache()
    private let cache: NSCache<NSString, UIImage> = {
        let cache = NSCache<NSString, UIImage>()
        cache.countLimit = 2
        cache.totalCostLimit = 16 * 1024 * 1024
        return cache
    }()
    private var pending: [String: Task<UIImage?, Never>] = [:]
    private var lastDecode: Task<UIImage?, Never>?

    func image(_ asset: String) async -> UIImage? {
        guard !Task.isCancelled else { return nil }
        if let hit = cache.object(forKey: asset as NSString) { return hit }
        if let shared = pending[asset] {
            let image = await shared.value
            return Task.isCancelled ? nil : image
        }
        let previous = lastDecode
        let work = Task.detached(priority: .userInitiated) { () -> UIImage? in
            // A second theme waits without occupying another decode worker.
            _ = await previous?.value
            #if WONDER_DIAGNOSTICS
            let started = ProcessInfo.processInfo.systemUptime
            #endif
            guard let data = NSDataAsset(name: asset)?.data,
                  let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
            let options: [CFString: Any] = [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceShouldCacheImmediately: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: 1536,
            ]
            let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
            #if WONDER_DIAGNOSTICS
            DiagnosticJournal.shared.record(DiagnosticEvent(operation: "theme.image.decode",
                durationMs: (ProcessInfo.processInfo.systemUptime - started) * 1000,
                bytes: UInt64(image.map { $0.bytesPerRow * $0.height } ?? 0)))
            #endif
            return image.map { UIImage(cgImage: $0) }
        }
        pending[asset] = work
        lastDecode = work
        let decoded = await work.value
        pending[asset] = nil
        if pending.isEmpty { lastDecode = nil }
        if let decoded, let image = decoded.cgImage {
            cache.setObject(decoded, forKey: asset as NSString, cost: image.bytesPerRow * image.height)
        }
        return Task.isCancelled ? nil : decoded
    }
}

// MARK: Settings

/// Settings → Appearance → Theme: one row per theme with a live swatch.
struct ThemePickerView: View {
    @AppStorage(WonderTheme.storageKey) private var themeID = WonderThemeCatalog.defaultID
    @Environment(\.colorScheme) private var systemScheme

    var body: some View {
        WonderList {
            ForEach(WonderThemeCatalog.groups) { group in
                Section {
                    ForEach(group.themes, id: \.settingsSearchID) { spec in row(spec) }
                } header: {
                    if let title = group.title { SettingsScopeHeader(title: title) }
                }.id(SettingsSearch.themeSectionID(group))
            }
        }
        .accessibilityIdentifier("theme-picker")
        .settingsNavigationTitle("Theme")
        .navigationBarTitleDisplayMode(.inline)
    }

    private func row(_ spec: WonderThemeSpec) -> some View {
        // A Diagnostics build can force a theme; show the one actually in use.
        let selected = WonderThemeCatalog.theme(id: ProcessInfo.processInfo.diagnosticThemeOverride ?? themeID).id == spec.id
        return Button { themeID = spec.id } label: {
            HStack(spacing: 12) {
                ThemeSwatch(theme: WonderTheme(spec: spec, systemIsDark: systemScheme == .dark))
                VStack(alignment: .leading, spacing: 2) {
                    Text(spec.name).foregroundStyle(.primary)
                    Text(detail(spec)).font(.caption).foregroundStyle(.secondary)
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
        .accessibilityIdentifier("theme-option:\(spec.id)").settingsRow("theme-option:\(spec.id)")
    }

    private func detail(_ spec: WonderThemeSpec) -> String {
        switch spec.scheme {
        case .system: "Follows your device"
        case .dark: spec.background == nil ? "Dark" : "Dark photo"
        case .light: spec.background == nil ? "Light" : "Light photo"
        }
    }
}

/// A miniature conversation in a theme's colours: surface, sidebar edge, two
/// bubbles and the syntax palette.
struct ThemeSwatch: View {
    let theme: WonderTheme

    var body: some View {
        HStack(spacing: 0) {
            theme.sidebar.frame(width: 10)
            VStack(alignment: .leading, spacing: 4) {
                bubble(theme.bubble(isUser: false), width: 34, alignment: .leading)
                bubble(theme.bubble(isUser: true), width: 26, alignment: .trailing)
                HStack(spacing: 2) {
                    ForEach([SyntaxRole.keyword, .string, .number, .type, .function], id: \.self) { role in
                        Circle().fill(theme.syntax(role)).frame(width: 5, height: 5)
                    }
                }
            }
            .padding(.horizontal, 6).padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(theme.background)
        }
        .frame(width: 64, height: 44)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).stroke(theme.separator, lineWidth: 1))
        .accessibilityHidden(true)
    }

    private func bubble(_ color: Color, width: CGFloat, alignment: Alignment) -> some View {
        RoundedRectangle(cornerRadius: 3).fill(color).frame(width: width, height: 7)
            .frame(maxWidth: .infinity, alignment: alignment)
    }
}
