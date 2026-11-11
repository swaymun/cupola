import SwiftUI
import UIKit
import WonderPairing

/// While Wonder is open, events in other chats (a permission request, a
/// question, a finished or failed reply) show as an in-app banner instead of a
/// system notification. It is fed by the live event stream, the pending-request
/// reads those events trigger, and notifications that arrive in the foreground
/// for chats the stream does not cover. The chats on screen stay quiet.
@MainActor final class InAppBannerCenter: ObservableObject {
    static let shared = InAppBannerCenter()
    /// Newest first.
    @Published private(set) var alerts: [InAppAlert] = []
    private var queue = InAppAlertQueue()
    private var screens: [UUID: InAppAlertAddress] = [:]
    private var holds: Set<String> = []
    private var timer: Task<Void, Never>?

    /// How long a finished or failed reply stays; longer with VoiceOver.
    static var lifetime: TimeInterval {
        #if WONDER_DIAGNOSTICS
        // `-diagnostics-in-app-banner-seconds N` holds banners for a scenario's taps.
        let arguments = ProcessInfo.processInfo.arguments
        if let index = arguments.firstIndex(of: "-diagnostics-in-app-banner-seconds"), arguments.indices.contains(index + 1),
           let seconds = TimeInterval(arguments[index + 1]) { return seconds }
        #endif
        return UIAccessibility.isVoiceOverRunning ? 15 : 6
    }
    private var onScreen: Set<InAppAlertAddress> { Set(screens.values) }

    func receive(_ events: [ReplayEvent], host: String) {
        // Most frames are streamed text; only terminal states can alert.
        guard events.contains(where: { $0.event.type == "message_state" || $0.event.type == "assistant_completed" }) else { return }
        update { $0.receive(events, host: host, onScreen: onScreen, now: Date()) }
    }
    func receiveAttention(_ requests: [AttentionRequest], host: String) {
        update { $0.receiveAttention(requests, host: host, onScreen: onScreen, now: Date()) }
    }
    func receiveNotification(_ address: InAppAlertAddress, body: String?, completed: Bool?) {
        update { $0.receiveNotification(address, body: body, completed: completed, onScreen: onScreen, now: Date()) }
    }
    /// The conversation a window shows (nil for none). Opening a chat clears its alerts.
    func setScreen(_ scene: UUID, _ address: InAppAlertAddress?) {
        screens[scene] = address
        if let address { update { $0.opened(address); return true } }
    }
    func dismiss(_ id: String) { update { $0.dismiss(id); return true } }
    func dismissAll() { update { $0.dismissAll(); return true } }
    func forget(host: String) { update { $0.forget(host: host); return true } }
    /// Nothing expires while the user drags or reads the expanded stack.
    func hold(_ reason: String, _ on: Bool) {
        if on { holds.insert(reason) } else { holds.remove(reason) }
        schedule()
    }

    private func update(_ change: (inout InAppAlertQueue) -> Bool) {
        guard change(&queue) else { return }
        if alerts != queue.alerts { alerts = queue.alerts }
        schedule()
    }
    private func schedule() {
        timer?.cancel()
        timer = nil
        guard holds.isEmpty, let next = queue.nextExpiry(lifetime: Self.lifetime) else { return }
        timer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0.05, next.timeIntervalSinceNow)))
            guard !Task.isCancelled, let self else { return }
            self.timer = nil
            if self.queue.expire(now: Date(), lifetime: Self.lifetime) { self.alerts = self.queue.alerts }
            self.schedule()
        }
    }
}

/// What one banner shows, read from the chat's Mac.
struct InAppBannerContent: Equatable {
    let chat: String
    let line: String
    let family: AgentFamily?

    @MainActor static func make(_ alert: InAppAlert, model: ConnectionModel, macCount: Int) -> Self {
        let id = alert.address.conversation
        let thread = model.projects.pinned.first { $0.thread.conversationId == id }?.thread
            ?? model.projects.threads.values.lazy.flatMap(\.threads).first { $0.conversationId == id }
        let detail = model.projects.details[id]
        let title = [detail?.title, thread?.title].compactMap { $0 }.first { !$0.isEmpty } ?? "Chat"
        let family = detail?.family ?? thread?.family
        var agent = family?.title ?? "Your agent"
        if macCount > 1 { agent += " on \(model.macName)" }
        // A finished reply without its own text reads the chat's latest preview.
        let summary = alert.summary ?? (alert.kind == .finished ? model.snapshots[id]?.assistantMessages.last?.text : nil)
        return Self(chat: title, line: InAppAlertText.line(kind: alert.kind, agent: agent, summary: summary),
                    family: family)
    }
}

/// The banner stack at the top of a window: the newest alert, with "N updates"
/// to expand the rest. Every banner is a short heads-up that hides by itself.
/// Tapping anywhere on it opens the chat (with its request in front); a swipe
/// up or sideways dismisses it. There is no close button to mis-tap.
struct InAppBannerOverlay: View {
    @ObservedObject private var center = InAppBannerCenter.shared
    @ObservedObject var library: ConnectionLibrary
    /// This window's open conversation, which never shows a banner.
    let current: InAppAlertAddress?
    let open: (InAppAlert) -> Void
    @State private var expanded = false
    @State private var drag: CGSize = .zero
    @State private var dragged: String?
    @State private var announced: String?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize

    private var visible: [InAppAlert] { center.alerts.filter { $0.address != current } }

    var body: some View {
        let alerts = visible
        VStack(spacing: 6) {
            if expanded && alerts.count > 1 {
                // At accessibility sizes the count takes its own line above the buttons,
                // so none of the three breaks inside a word.
                let large = typeSize.isAccessibilitySize
                let layout = large ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4)) : AnyLayout(HStackLayout())
                layout {
                    Text("\(alerts.count) updates").font(.subheadline.weight(.semibold))
                    if !large { Spacer() }
                    HStack {
                        Button("Clear all") { center.dismissAll() }
                            .accessibilityIdentifier("in-app-banner-clear")
                        if large { Spacer() }
                        Button("Show less") { expanded = false }
                            .accessibilityIdentifier("in-app-banner-collapse")
                    }
                }
                .font(.subheadline)
                .padding(.horizontal, 14).padding(.vertical, 8)
                .frame(maxWidth: large ? .infinity : nil, alignment: .leading)
                .background(InAppBannerCard.fill(theme), in: RoundedRectangle(cornerRadius: large ? 16 : 100, style: .continuous))
                .shadow(color: .black.opacity(theme.isDark ? 0.4 : 0.12), radius: 8, y: 2)
                ForEach(alerts.prefix(5)) { alert in
                    card(alert, stacked: 0)
                        .offset(offset(alert.id))
                        .highPriorityGesture(swipe(alert.id, dismissing: [alert.id]))
                }
            } else if let top = alerts.first {
                // A swipe on the collapsed stack dismisses all of it, as on the Lock Screen.
                card(top, stacked: min(alerts.count - 1, 2))
                    .offset(offset(top.id))
                    .highPriorityGesture(swipe(top.id, dismissing: alerts.map(\.id)))
                if alerts.count > 1 {
                    Button { expanded = true } label: {
                        Text("\(alerts.count) updates")
                            .font(.footnote.weight(.semibold))
                            .foregroundStyle(theme.text)
                            .padding(.horizontal, 12).padding(.vertical, 6)
                            .background(InAppBannerCard.fill(theme), in: Capsule())
                            .shadow(color: .black.opacity(theme.isDark ? 0.4 : 0.12), radius: 8, y: 2)
                    }
                    .buttonStyle(.plain)
                    .accessibilityHint("Shows every update")
                    .accessibilityIdentifier("in-app-banner-count")
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 4)
        .frame(maxWidth: 560)
        .animation(reduceMotion ? nil : .spring(response: 0.35, dampingFraction: 0.86), value: alerts.map(\.id))
        .animation(reduceMotion ? nil : .spring(response: 0.3, dampingFraction: 0.9), value: expanded)
        .onChange(of: alerts.count < 2) { _, single in if single { expanded = false } }
        .onChange(of: expanded) { _, value in center.hold("expanded", value) }
        .onChange(of: alerts.first?.id, initial: true) { _, id in announce(alerts.first, id: id) }
        .onDisappear { center.hold("expanded", false); center.hold("drag", false) }
    }

    @ViewBuilder private func card(_ alert: InAppAlert, stacked: Int) -> some View {
        if let saved = library.saved.connections.first(where: { $0.credential.hostInstallationId == alert.address.host }) {
            InAppBannerCard(model: library.model(for: saved), projects: library.model(for: saved).projects,
                            alert: alert, macCount: library.saved.connections.count,
                            stacked: stacked, open: { expanded = false; open(alert) },
                            dismiss: { center.dismiss(alert.id) })
                .opacity(dragged == alert.id ? max(0.3, 1 - Double(max(abs(drag.width), -drag.height)) / 240) : 1)
                .transition(reduceMotion ? .opacity : .move(edge: .top).combined(with: .opacity))
        }
    }

    /// Up or sideways follows the finger; down does not move the banner.
    private func offset(_ id: String) -> CGSize {
        guard dragged == id else { return .zero }
        return abs(drag.width) > abs(drag.height) ? CGSize(width: drag.width, height: 0) : CGSize(width: 0, height: min(drag.height, 0))
    }
    private func swipe(_ id: String, dismissing ids: [String]) -> some Gesture {
        DragGesture(minimumDistance: 12)
            .onChanged { value in
                center.hold("drag", true)
                dragged = id
                drag = value.translation
            }
            .onEnded { value in
                center.hold("drag", false)
                let sideways = abs(value.translation.width) > abs(value.translation.height)
                let gone = sideways
                    ? abs(value.translation.width) > 60 || abs(value.predictedEndTranslation.width) > 160
                    // A banner is short: a flick of a fingertip's height dismisses it.
                    : value.translation.height < -24 || value.predictedEndTranslation.height < -60
                withAnimation(reduceMotion ? nil : .spring(response: 0.3, dampingFraction: 0.9)) {
                    if gone { for id in ids { center.dismiss(id) } }
                    drag = .zero
                    dragged = nil
                }
            }
    }

    private func announce(_ alert: InAppAlert?, id: String?) {
        guard let alert, let id, id != announced,
              let saved = library.saved.connections.first(where: { $0.credential.hostInstallationId == alert.address.host }) else { return }
        announced = id
        let content = InAppBannerContent.make(alert, model: library.model(for: saved), macCount: library.saved.connections.count)
        UIAccessibility.post(notification: .announcement, argument: "In \(content.chat): \(content.line)")
    }
}

/// Dismissing a notice hides its descriptions, not the Mac's pending windows.
/// The Computer badge remains the entry point until a different dialog appears.
struct MacDialogNotificationBanner: View {
    @ObservedObject private var dialogs = PushNotifications.shared
    @ObservedObject private var viewers = ComputerViewerPresence.shared
    @ObservedObject var library: ConnectionLibrary
    var body: some View {
        VStack(spacing: 6) {
            ForEach(dialogs.macDialogs.keys.sorted(), id: \.self) { host in
                if dialogs.macDialogBannerIsVisible(host: host), !viewers.hosts.contains(host), let notice = dialogs.macDialogs[host],
                   let saved = library.paired.first(where: { $0.credential.hostInstallationId == host }) {
                    MacDialogBannerCard(name: library.model(for: saved).macName, summary: notice.summary,
                        open: { dialogs.openMacDialogs(host: host) },
                        dismiss: { dialogs.dismissMacDialogBanner(host: host) })
                        .accessibilityIdentifier("mac-dialog-notification:" + host)
                }
            }
        }
        .padding(.horizontal, 12)
        .frame(maxWidth: 560)
    }
}

private struct MacDialogBannerCard: View {
    let name: String
    let summary: String
    let open: () -> Void
    let dismiss: () -> Void
    @Environment(\.wonderTheme) private var theme
    @GestureState private var drag: CGSize = .zero

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: ConceptSymbol.computer).font(.title3)
            VStack(alignment: .leading, spacing: 2) {
                Text(name).font(.subheadline.weight(.semibold))
                Text(summary).font(.subheadline).lineLimit(3)
            }
            Spacer(minLength: 4)
            Image(systemName: "chevron.right").font(.caption.weight(.semibold))
        }
        .foregroundStyle(theme.text)
        .padding(.horizontal, 14).padding(.vertical, 10)
        .background(InAppBannerCard.fill(theme), in: RoundedRectangle(cornerRadius: 16))
        .shadow(color: .black.opacity(theme.isDark ? 0.4 : 0.12), radius: 8, y: 2)
        .frame(minHeight: 44)
        .contentShape(Rectangle())
        .offset(drag)
        .onTapGesture(perform: open)
        .gesture(DragGesture(minimumDistance: 12)
            .updating($drag) { value, state, _ in
                state = abs(value.translation.width) > abs(value.translation.height)
                    ? CGSize(width: value.translation.width, height: 0)
                    : CGSize(width: 0, height: min(value.translation.height, 0))
            }
            .onEnded { value in
                let sideways = abs(value.translation.width) > abs(value.translation.height)
                if sideways ? abs(value.translation.width) > 60 || abs(value.predictedEndTranslation.width) > 160
                    : value.translation.height < -24 || value.predictedEndTranslation.height < -60 {
                    dismiss()
                }
            })
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction(.default, open)
        .accessibilityAction(named: "Dismiss", dismiss)
        .accessibilityLabel(name + ", " + summary)
        .accessibilityHint("Opens the dialog in Computer")
    }
}

/// One alert: provider, chat name, and who did what in one line.
private struct InAppBannerCard: View {
    @ObservedObject var model: ConnectionModel
    // Thread names and provider changes arrive through the Project library.
    @ObservedObject var projects: ProjectLibrary
    let alert: InAppAlert
    let macCount: Int
    let stacked: Int
    let open: () -> Void
    let dismiss: () -> Void
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        let content = InAppBannerContent.make(alert, model: model, macCount: macCount)
        HStack(alignment: .center, spacing: 10) {
            if let family = content.family {
                ProviderIcon(family: family, size: 32).accessibilityHidden(true)
            } else {
                Image(systemName: ConceptSymbol.thread).font(.title2)
                    .frame(width: 32, height: 32).accessibilityHidden(true)
            }
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 5) {
                    Text(content.chat)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(theme.text)
                        .lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
                    if let symbol {
                        Image(systemName: symbol)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(tint)
                            .accessibilityHidden(true)
                    }
                }
                Text(content.line)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(typeSize.isAccessibilitySize ? 3 : 1)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        // The whole card is the tap target.
        .padding(.horizontal, 12).padding(.vertical, 10)
        .contentShape(Rectangle())
        // A tap, not a Button: a Button took the start of a swipe as a tap.
        .onTapGesture(perform: open)
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction(.default, open)
        .accessibilityLabel("\(content.chat). \(content.line)")
        .accessibilityHint(alert.kind.isRequest ? "Opens the request" : "Opens the chat")
        .accessibilityAction(named: "Dismiss", dismiss)
        .accessibilityIdentifier("in-app-banner")
        .background {
            ZStack {
                // Edges of the alerts behind this one.
                ForEach(Array((0..<stacked).reversed()), id: \.self) { index in
                    RoundedRectangle(cornerRadius: 16, style: .continuous)
                        .fill(fill)
                        .shadow(color: .black.opacity(0.08), radius: 4, y: 1)
                        .scaleEffect(x: 1 - CGFloat(index + 1) * 0.05, y: 1, anchor: .bottom)
                        .offset(y: CGFloat(index + 1) * 6)
                }
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .fill(fill)
                    .shadow(color: .black.opacity(theme.isDark ? 0.4 : 0.14), radius: 12, y: 4)
            }
        }
        .overlay {
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(theme.rule.opacity(0.5), lineWidth: 0.5)
        }
        .padding(.bottom, CGFloat(stacked) * 6)
    }

    /// Lifted off the page: the page colour in light themes, the raised
    /// surface in dark ones, where a shadow does not show.
    static func fill(_ theme: WonderTheme) -> Color { theme.isDark ? theme.surface : theme.page }
    private var fill: Color { Self.fill(theme) }
    private var symbol: String? {
        switch alert.kind {
        case .approval: "hand.raised.fill"
        case .question: "questionmark.bubble.fill"
        case .failed: "exclamationmark.triangle.fill"
        case .finished: "checkmark.circle.fill"
        case .update: nil
        }
    }
    private var tint: Color {
        switch alert.kind {
        case .failed: .red
        case .finished: .green
        default: theme.accent
        }
    }
}
