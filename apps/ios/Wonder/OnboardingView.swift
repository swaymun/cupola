import SwiftUI

/// A short paged carousel: the welcome at first launch, and "What's new"
/// after an update with release notes. Both read their pages from `ReleaseNotes`.
struct CarouselView<Finish: View>: View {
    let pages: [CarouselPage]
    /// Shown under the last page (sign-in, Skip, Continue).
    @ViewBuilder let finish: () -> Finish
    let close: () -> Void
    /// Opens the demo from every page, so someone with no computer finds it at once.
    var tryDemo: (() -> Void)? = nil
    @State private var index = 0
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(spacing: 0) {
            // The last page has its own actions; earlier pages can skip ahead.
            HStack {
                Spacer()
                Button("Skip", action: close)
                    .padding()
                    .opacity(isLast ? 0 : 1)
                    .disabled(isLast)
                    .accessibilityHidden(isLast)
                    .accessibilityIdentifier("carousel-close")
            }
            TabView(selection: $index) {
                ForEach(Array(pages.enumerated()), id: \.element.id) { offset, page in
                    PageView(page: page).tag(offset)
                }
            }
            .tabViewStyle(.page(indexDisplayMode: .always))
            .indexViewStyle(.page(backgroundDisplayMode: .always))
            .accessibilityIdentifier("carousel-pages")
            VStack(spacing: 12) {
                if isLast { finish() }
                else {
                    Button {
                        if reduceMotion { index += 1 } else { withAnimation { index += 1 } }
                    } label: { Text("Continue").frame(maxWidth: .infinity, minHeight: 44) }
                        .buttonStyle(.wonderProminent).controlSize(.large)
                        .accessibilityIdentifier("carousel-next")
                    if let tryDemo {
                        Button("No computer? Try the demo", action: tryDemo)
                            .accessibilityHint("Shows sample chats on a demo computer. Nothing is sent anywhere.")
                            .accessibilityIdentifier("carousel-try-demo")
                    }
                }
            }
            .padding(.horizontal, 24)
            .padding(.bottom, 24)
            .frame(minHeight: 120, alignment: .top)
        }
    }
    private var isLast: Bool { index >= pages.count - 1 }

    private struct PageView: View {
        let page: CarouselPage
        var body: some View {
            ScrollView {
                VStack(spacing: 20) {
                    Image(systemName: page.symbol)
                        .font(.system(size: 64, weight: .regular))
                        .foregroundStyle(.tint)
                        .accessibilityHidden(true)
                    Text(page.title)
                        .font(.largeTitle.bold())
                        .multilineTextAlignment(.center)
                        .accessibilityAddTraits(.isHeader)
                    Text(page.body)
                        .font(.body)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                .padding(.horizontal, 32)
                .padding(.top, 48)
                .padding(.bottom, 56)
                .frame(maxWidth: 560)
                .frame(maxWidth: .infinity)
            }
            .accessibilityElement(children: .combine)
        }
    }
}

/// When to show the welcome or What's new, saved per installation.
@MainActor enum Onboarding {
    enum Sheet: String, Identifiable { case welcome, whatsNew; var id: String { rawValue } }
    private static let welcomedKey = "onboarding-welcomed-v1", notesKey = "onboarding-notes-version-v1"
    static var version: String { Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "" }
    /// The notes Settings > About replays; empty hides the row.
    static let replayNotes = ReleaseNotes.latest(for: version)

    static func pending(defaults: UserDefaults = .standard, arguments: [String] = ProcessInfo.processInfo.arguments) -> Sheet? {
        #if WONDER_DIAGNOSTICS
        // Scenarios never see either sheet unless they select one, whatever an
        // earlier run saved.
        if arguments.contains("-diagnostics-onboarding") { return .welcome }
        if arguments.contains("-diagnostics-whats-new") { return ReleaseNotes.whatsNew[version] == nil ? nil : .whatsNew }
        return nil
        #else
        return saved(defaults: defaults, version: version)
        #endif
    }
    /// Release: the welcome once per installation, then each version's notes once.
    static func saved(defaults: UserDefaults, version: String, notes: [String: [CarouselPage]] = ReleaseNotes.whatsNew) -> Sheet? {
        if !defaults.bool(forKey: welcomedKey) { return .welcome }
        if defaults.string(forKey: notesKey) != version, notes[version] != nil { return .whatsNew }
        return nil
    }
    /// The welcome covers this version's notes too: a new install sees no What's new.
    static func finished(defaults: UserDefaults = .standard, version: String = Onboarding.version) {
        defaults.set(true, forKey: welcomedKey)
        defaults.set(version, forKey: notesKey)
    }
}

/// Presents the welcome or What's new over the app once, as `Onboarding` decides.
struct OnboardingHost: ViewModifier {
    @State private var sheet = Onboarding.pending()
    /// Opens the demo computer; nil hides "Try the demo".
    var tryDemo: (() -> Void)?
    func body(content: Content) -> some View {
        content.fullScreenCover(item: $sheet) { sheet in
            switch sheet {
            case .welcome:
                let openDemo = tryDemo.map { open in { done(); open() } }
                CarouselView(pages: ReleaseNotes.welcome, finish: { WelcomeFinish(done: done, tryDemo: openDemo) },
                             close: { done() }, tryDemo: openDemo)
                    // A container keeps its children's identifiers.
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier("onboarding-welcome")
            case .whatsNew:
                WhatsNewCarousel(pages: ReleaseNotes.whatsNew[Onboarding.version] ?? [], done: done)
            }
        }
    }
    private func done() { Onboarding.finished(); sheet = nil }
}

/// "What's new" after an update, and again from Settings > About.
struct WhatsNewCarousel: View {
    let pages: [CarouselPage]
    let done: () -> Void
    var body: some View {
        CarouselView(pages: pages) {
            Button { done() } label: { Text("Continue").frame(maxWidth: .infinity, minHeight: 44) }
                .buttonStyle(.wonderProminent).controlSize(.large)
                .accessibilityIdentifier("whats-new-done")
        } close: { done() }
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("onboarding-whats-new")
    }
}

/// The installed version and the app-wide pages Settings > About links to.
enum AppInfo {
    /// The app's privacy policy, as listed on the App Store. The notification
    /// service's own page covers only notifications.
    static let privacyURL = URL(string: "https://getcupola.app/privacy")!
    /// The playbook a coding agent follows to find out why Cupola isn't working.
    static let triageURL = URL(string: "https://github.com/swaymun/cupola/blob/main/TRIAGE.md")!
    /// "1.0 (412)": the marketing version, then the build testers report.
    static func label(version: String, build: String) -> String {
        build.isEmpty || build == version ? version : "\(version) (\(build))"
    }
    static var versionLabel: String {
        let info = Bundle.main.infoDictionary ?? [:]
        return label(version: info["CFBundleShortVersionString"] as? String ?? "",
                     build: info["CFBundleVersion"] as? String ?? "")
    }
}

/// Last welcome page: Sign in with Apple (optional), Try the demo, Skip.
private struct WelcomeFinish: View {
    let done: () -> Void
    let tryDemo: (() -> Void)?
    @ObservedObject private var account = WonderAccount.shared
    var body: some View {
        VStack(spacing: 12) {
            if account.isSignedIn {
                Label("Signed in for notifications", systemImage: "checkmark.circle.fill")
                    .accessibilityIdentifier("onboarding-signed-in")
                Button { done() } label: { Text("Get started").frame(maxWidth: .infinity, minHeight: 44) }
                    .buttonStyle(.wonderProminent).controlSize(.large)
                    .accessibilityIdentifier("onboarding-done")
            } else {
                Text(WonderAccount.consent)
                    .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("account-consent")
                AccountSignInButton()
                if let error = account.error {
                    Text(error).font(.footnote).foregroundStyle(.red).accessibilityIdentifier("account-error")
                }
                if let tryDemo {
                    Button("Try the demo", action: tryDemo).accessibilityIdentifier("onboarding-try-demo")
                }
                Button("Skip", action: done).accessibilityIdentifier("onboarding-skip")
            }
        }
    }
}

extension View {
    func onboarding(tryDemo: (() -> Void)? = nil) -> some View { modifier(OnboardingHost(tryDemo: tryDemo)) }
}
