import SwiftUI

/// Presentation only. The iOS app and Mac snapshot package compile this same file.
/// Owners prepare strings, colours and actions before rendering; these layouts
/// never load history, decode images, parse messages or start work.
struct PreparedChatFonts {
    var body: Font = .body
    var caption: Font = .caption
    var subheadline: Font = .subheadline
    var permissionHeading: Font = .subheadline.weight(.semibold)
    var emptyHeading: Font? = nil
}

private struct PreparedChatFontsKey: EnvironmentKey {
    static let defaultValue = PreparedChatFonts()
}

extension EnvironmentValues {
    var preparedChatFonts: PreparedChatFonts {
        get { self[PreparedChatFontsKey.self] }
        set { self[PreparedChatFontsKey.self] = newValue }
    }
}

/// Line 1: the title. Line 2: a small provider icon, the Project (omitted
/// inside its own Project group), the branch or worktree in the accent colour,
/// then what the thread is doing now when it fits whole.
struct PreparedChatRow<Accessory: View, Provider: View, Status: View>: View {
    let title: String
    var project: String? = nil
    /// The paired computer, only while several are paired.
    var computer: String? = nil
    var location: String? = nil
    var locationTint: Color = .accentColor
    var activity: String? = nil
    let isComputerSession: Bool
    /// A clock when an automation posts into the thread; its symbol carries
    /// an exclamation mark, tinted red, after a failed run. The owner's
    /// accessibility value speaks it.
    var automationSymbol: String? = nil
    var automationFailed = false
    /// A bell when the chat follows or mutes notifications; nil for Default.
    var notificationSymbol: String? = nil
    var notificationLabel: String? = nil
    let leading: CGFloat
    @ViewBuilder var accessory: () -> Accessory
    @ViewBuilder var provider: () -> Provider
    @ViewBuilder var status: () -> Status
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.preparedChatFonts) private var fonts

    var body: some View {
        let detailLines = typeSize.isAccessibilitySize ? 2 : 1
        HStack(spacing: 8) {
            accessory()
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 4) {
                    Text(title).lineLimit(typeSize.isAccessibilitySize ? 3 : 1).truncationMode(.tail)
                    if isComputerSession {
                        Image(systemName: "desktopcomputer")
                            .font(.caption2).foregroundStyle(.secondary)
                            .fixedSize().layoutPriority(1).accessibilityHidden(true)
                    }
                    if let automationSymbol {
                        Image(systemName: automationSymbol)
                            .font(.caption2).foregroundStyle(automationFailed ? Color.red : Color.secondary)
                            .fixedSize().layoutPriority(1).accessibilityHidden(true)
                    }
                    if let notificationSymbol {
                        Image(systemName: notificationSymbol)
                            .font(.caption2).foregroundStyle(.secondary)
                            .fixedSize().layoutPriority(1)
                            .accessibilityLabel(notificationLabel ?? "")
                            .accessibilityIdentifier("sidebar-notification")
                    }
                }
                if let activity = activity.nonEmpty {
                    // The step follows the branch only when the whole line fits.
                    // Otherwise it moves to its own line at accessibility sizes
                    // and is left out at standard sizes; the owner's
                    // accessibility value still speaks it.
                    ViewThatFits(in: .horizontal) {
                        HStack(alignment: .center, spacing: 4) {
                            details(lines: 1)
                            separator(after: project.nonEmpty ?? computer.nonEmpty ?? location.nonEmpty)
                            activityText(activity, lines: 1)
                        }
                        if typeSize.isAccessibilitySize {
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(alignment: .center, spacing: 4) { details(lines: detailLines) }
                                activityText(activity, lines: detailLines)
                            }
                        } else {
                            HStack(alignment: .center, spacing: 4) { details(lines: detailLines) }
                        }
                    }
                } else {
                    HStack(alignment: .center, spacing: 4) { details(lines: detailLines) }
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
            status()
        }
        .padding(.leading, leading).contentShape(Rectangle()).frame(minHeight: 46)
    }

    @ViewBuilder private func details(lines: Int) -> some View {
        provider()
        if let project = project.nonEmpty {
            Text(project).font(fonts.caption).foregroundStyle(.secondary).lineLimit(lines)
        }
        if let computer = computer.nonEmpty {
            separator(after: project)
            HStack(spacing: 3) {
                Image(systemName: "desktopcomputer").font(.caption2).accessibilityHidden(true)
                Text(computer).font(fonts.caption).lineLimit(lines)
            }.foregroundStyle(.secondary).accessibilityElement(children: .combine)
        }
        if let location = location.nonEmpty {
            separator(after: project.nonEmpty ?? computer)
            Text(location).font(fonts.caption).foregroundStyle(locationTint)
                .lineLimit(lines).truncationMode(.middle)
        }
    }

    private func activityText(_ activity: String, lines: Int) -> some View {
        Text(activity).font(fonts.caption).foregroundStyle(.secondary).lineLimit(lines)
    }

    @ViewBuilder private func separator(after earlier: String?) -> some View {
        if earlier.nonEmpty != nil {
            Text("·").font(fonts.caption).foregroundStyle(.secondary).accessibilityHidden(true)
        }
    }
}

private extension Optional where Wrapped == String {
    var nonEmpty: String? { self?.isEmpty == false ? self : nil }
}

/// Bubbles use most of the row: a user bubble keeps a 24-point lead so it
/// still reads as right-aligned, an agent reply only 12 points after it.
struct PreparedMessageLayout<Message: View, Receipt: View>: View {
    let isUser: Bool
    @ViewBuilder var message: () -> Message
    @ViewBuilder var receipt: () -> Receipt

    var body: some View {
        HStack(alignment: .bottom, spacing: 4) {
            if isUser { Spacer(minLength: 24) }
            HStack(alignment: .top, spacing: 8) { message() }
                .frame(maxWidth: 640, alignment: isUser ? .trailing : .leading)
            receipt()
            if !isUser { Spacer(minLength: 12) }
        }
        .frame(maxWidth: .infinity, alignment: isUser ? .trailing : .leading)
        .accessibilityElement(children: .contain)
    }
}

struct PreparedBubbleSurface: ViewModifier {
    let foreground: Color
    let background: Color
    func body(content: Content) -> some View {
        content.padding(.horizontal, 14).padding(.vertical, 10)
            .foregroundStyle(foreground)
            .background(background, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
    }
}

/// Timeline consumes the remaining height; composer remains the bottom sibling.
struct PreparedConversationLayout<Timeline: View, Bottom: View>: View {
    @ViewBuilder var timeline: () -> Timeline
    @ViewBuilder var bottom: () -> Bottom
    var body: some View { VStack(spacing: 0) { timeline(); bottom() } }
}

struct PreparedComposerSurface<Content: View>: View {
    let background: Color
    @ViewBuilder var content: () -> Content
    var body: some View {
        content().padding(5).foregroundStyle(.primary)
            .background(background, in: RoundedRectangle(cornerRadius: 28))
    }
}

struct PreparedComposerControls<Controls: View>: View {
    @ViewBuilder var controls: () -> Controls
    var body: some View { HStack(alignment: .center, spacing: 0) { controls() } }
}

struct PreparedSendSymbol: View {
    let preparing: Bool
    var queues = false
    let progressTint: Color
    var body: some View {
        if preparing {
            ProgressView().tint(progressTint).frame(width: 44, height: 44)
        } else {
            // The queue glyph matches the hold menu's Queue item.
            Image(systemName: queues ? "text.line.last.and.arrowtriangle.forward" : "arrow.up").font(.system(size: 20, weight: .semibold)).frame(width: 44, height: 44)
        }
    }
}

struct PreparedComposerDock: ViewModifier {
    func body(content: Content) -> some View {
        content.frame(maxWidth: 768).padding(.horizontal).padding(.top, 4).padding(.bottom, 2)
            .frame(maxWidth: .infinity)
    }
}

struct PreparedPermissionDetails: View {
    var detail: String? = nil
    var reason: String? = nil
    var command: String? = nil
    var path: String? = nil
    var changedPaths: [String] = []
    var message: String? = nil
    var permissionDetails: [String] = []
    @Environment(\.preparedChatFonts) private var fonts

    var body: some View {
        Group {
            Text("Permission requested").font(fonts.permissionHeading)
            if let detail {
                ScrollView { Text(detail).frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled) }
                    .frame(maxHeight: 180).fixedSize(horizontal: false, vertical: true)
            }
            if let reason { Text(reason) }
            if let command { Text(command).font(fonts.subheadline.monospaced()).textSelection(.enabled) }
            if let path { Text(path).font(fonts.caption).textSelection(.enabled) }
            ForEach(changedPaths, id: \.self) { path in Text(path).font(fonts.caption).textSelection(.enabled) }
            if let message { Text(message) }
            ForEach(permissionDetails, id: \.self) { detail in
                Text(detail).font(fonts.subheadline).textSelection(.enabled)
            }
        }
    }
}

/// A question's option (with its radio) or a permission request's one-tap
/// choice (no radio until it is sent). Both read at full contrast: a pale
/// radio made available options look disabled.
struct PreparedChoiceLabel: View {
    let title: String
    var detail: String? = nil
    var selected: Bool? = nil
    let accent: Color
    let background: Color
    /// The one choice a request leads with (Allow): solid `emphasisFill` (the theme's text colour, like
    /// Send) with text in `onAccent`, so contrast holds on every theme.
    var emphasized = false
    var emphasisFill: Color = .primary
    var onAccent: Color = .white
    @Environment(\.preparedChatFonts) private var fonts

    var body: some View {
        HStack(spacing: 10) {
            if let selected {
                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected ? accent : Color.primary).accessibilityHidden(true)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(emphasized ? fonts.subheadline.weight(.semibold) : fonts.subheadline)
                    .foregroundStyle(emphasized ? onAccent : Color.primary)
                if let detail { Text(detail).font(fonts.caption).foregroundStyle(emphasized ? onAccent.opacity(0.85) : .secondary) }
            }.fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }.padding(.horizontal, 10).padding(.vertical, 8)
            .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
            .background(emphasized ? emphasisFill : selected == true ? accent.opacity(0.10) : background, in: RoundedRectangle(cornerRadius: 8))
            .contentShape(Rectangle())
    }
}

/// Stop, apart from a permission request's choices: a red button of its own
/// below them, so it is never read as one of the options.
struct PreparedStopLabel: View {
    let title: String
    @Environment(\.preparedChatFonts) private var fonts

    var body: some View {
        Label(title, systemImage: "stop.fill")
            .font(fonts.subheadline.weight(.semibold))
            .foregroundStyle(Color.red)
            .padding(.horizontal, 10).padding(.vertical, 8)
            .frame(maxWidth: .infinity, minHeight: 44)
            .background(Color.red.opacity(0.14), in: RoundedRectangle(cornerRadius: 8))
            .contentShape(Rectangle())
    }
}
