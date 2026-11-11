import SwiftUI
import UIKit
import WonderComputerView
import WonderPairing

/// The tool shown beside a conversation in a regular-width window (iPad, an
/// unfolded iPhone Duo, a large iPhone on its side). One per conversation;
/// opening another replaces it. Phone-width windows keep their sheets.
enum CompanionTool: Equatable {
    case computer
    case simulators
    case terminal
    case pullRequests
    case files(WorkspaceBrowserRequest)
    case agentTask(ProjectSubagentSummary)
    case details
    /// The files one reply changed, from its "Changed N files" row.
    case replyChanges(turnID: String)

    /// The pill this tool belongs to: tapping it again closes the pane.
    enum Kind: String { case computer, simulators, terminal, pullRequests = "pull-requests", files, agentTask = "agent-task", details, replyChanges = "reply-changes" }

    var kind: Kind {
        switch self {
        case .computer: .computer
        case .simulators: .simulators
        case .terminal: .terminal
        case .pullRequests: .pullRequests
        case .files: .files
        case .agentTask: .agentTask
        case .details: .details
        case .replyChanges: .replyChanges
        }
    }

    /// The existing entry that remains selected while this pane is open.
    var pill: ComposerPill? {
        switch self {
        case .computer: .computer
        case .simulators: .simulators
        case .terminal: .terminal
        case .files: .files
        case .agentTask: .agentTasks
        case .pullRequests, .details, .replyChanges: nil
        }
    }

    /// Identity of what the pane shows: a new Files request or another agent
    /// task is a new view; the same tool keeps its view (and its live state).
    var contentID: String {
        switch self {
        case .files(let request): "files:" + request.id.uuidString
        case .agentTask(let child): "agent-task:" + child.id
        case .replyChanges(let turnID): "reply-changes:" + turnID
        default: kind.rawValue
        }
    }

    /// Spoken for the pane as a whole.
    var label: String {
        switch self {
        case .computer: "Computer"
        case .simulators: "Simulators"
        case .terminal: "Terminal"
        case .pullRequests: "Pull requests"
        case .files: "Files"
        case .agentTask(let child): child.title
        case .details: "Chat details"
        case .replyChanges: "Changed files"
        }
    }

    static func == (lhs: Self, rhs: Self) -> Bool { lhs.contentID == rhs.contentID }
}

/// The shape of the control panel under a tool (the computer bar, the terminal
/// keys): the chat composer's radius and fill, at the composer's one-line
/// height, ending on the composer's bottom edge, so the two panes read as one
/// baseline across the split.
enum ToolPanelMetrics {
    /// `PreparedComposerSurface`'s corner radius.
    static let cornerRadius: CGFloat = 28
    /// The composer's inset around its rows, and the 44 pt control row it holds.
    static let inset: CGFloat = 5
    static let rowHeight: CGFloat = 44
    /// A one-line composer, measured: its text row and control row inside the inset.
    static let height: CGFloat = 100
    /// How far the chat's composer sinks into the home-indicator inset on a
    /// Duo; an unfolded tool's frame reaches that far too, so its panel shares
    /// the composer's bottom edge instead of being clipped above it.
    static let sink: CGFloat = 16.5
    /// How far above the window's bottom edge the composer ends on a fold, so
    /// the panel's margin under it when the pane reaches the window's bottom.
    static let composerRise: CGFloat = 20
    /// The composer's margin above the software keyboard, measured, which the
    /// panel keeps while the keyboard is up.
    static let keyboardRise: CGFloat = 5.8
    /// `PreparedComposerDock`'s margin under the composer.
    static let bottomMargin: CGFloat = 2
}

extension View {
    /// Draws a tool's control panel as the composer's twin: rounded surface,
    /// composer height, the dock's horizontal margin and bottom edge.
    func toolPanelSurface(_ theme: WonderTheme, bottomMargin: CGFloat = ToolPanelMetrics.bottomMargin) -> some View {
        self.padding(ToolPanelMetrics.inset)
            .frame(minHeight: ToolPanelMetrics.height)
            .background(theme.surface, in: RoundedRectangle(cornerRadius: ToolPanelMetrics.cornerRadius))
            .padding(.horizontal)
            .padding(.bottom, bottomMargin)
    }
}

/// What a tool inside the pane can ask of it.
struct CompanionPaneChrome {
    var isExpanded: Bool
    /// A pane with no corresponding pill retains its own close control.
    /// Tools with pills keep their selected toggle visible while open.
    var showsClose: Bool
    var toggleExpanded: () -> Void
    var close: () -> Void
    /// The two navigation stacks share the Duo's window bar in either inner
    /// orientation, including the one whose bar is horizontal.
    var onDisplayWithFold = false
    var swap: (() -> Void)?
    var swapVertically = false
    /// The swap control sits on the pane divider (a seamless fold), so the
    /// tool's header does not repeat it.
    var swapOnDivider = false
    /// The pane reaches the window's bottom (`ToolPanelMetrics.sink`): the
    /// tool ends its control panel at the composer's bottom edge, not the pane's.
    var alignsPanelToComposer = false
    /// The margin under the tool's control panel so it ends where the composer
    /// beside it does: the home-indicator rise, the keyboard's, or the dock's.
    var panelBottomMargin = ToolPanelMetrics.bottomMargin
    /// The slot, rather than the window, owns these interior boundaries.
    /// A nested NavigationStack must not reserve the window rail there again.
    var interiorHorizontalEdges: Edge.Set = []

    static func interiorEdges(of frame: CGRect, in size: CGSize) -> Edge.Set {
        var edges: Edge.Set = []
        if frame.minX > 0.5 { edges.insert(.leading) }
        if frame.maxX < size.width - 0.5 { edges.insert(.trailing) }
        return edges
    }

    /// Whether a tool draws `CompanionInlineHeader` instead of toolbar items:
    /// in the pane on the Duo (a vertical bar, or the pane split on the fold).
    static func usesInlineHeader(_ pane: CompanionPaneChrome?, verticalBar: Bool) -> Bool {
        guard let pane else { return false }
        return verticalBar || pane.onDisplayWithFold
    }
}

extension EnvironmentValues {
    /// Set inside the companion pane; nil in a sheet or full-screen cover.
    @Entry var companionPane: CompanionPaneChrome?
    /// Native sheets and outside-tap cards already provide an exit.
    @Entry var toolHasGestureDismissal = false
    /// The whole window (with any sidebar), which picks the pane's direction.
    @Entry var shellWindowSize: CGSize?
    /// Counts the times the shell's height collapsed to nothing (an unfold with the keyboard still up) and
    /// recovered. The vertical bar can come back from that with empty item hosts, so the rail's items take
    /// this in their ids and are built fresh once the window is back.
    @Entry var shellBarEpoch = 0
    /// UIKit's scene bounds stay stable while a software keyboard changes the
    /// SwiftUI safe area. Presentation chooses by this size, layout by the inset size.
    @Entry var shellWindowBounds: CGSize?
    /// Chats temporarily occupies the other display without closing its tool.
    @Entry var shellChatsReplaceTool = false
    /// An unfolded Duo's physical fold divides the window; known from the shell's
    /// first layout so the conversation never re-lays out a frame later.
    @Entry var shellDisplayFold = false
}

/// One probe per scene. Never use keyboard-reduced content geometry to move a
/// live tool between a sheet and a split; doing that dismisses its presentation.
struct ShellWindowBoundsReader: UIViewRepresentable {
    // A layout change schedules updateUIView; the reported size still comes
    // from UIWindow, never from this keyboard-sensitive geometry.
    let layoutSize: CGSize
    @Binding var size: CGSize?
    func makeUIView(context: Context) -> Probe { Probe() }
    func updateUIView(_ view: Probe, context: Context) {
        view.changed = { if size != $0 { size = $0 } }
        view.report()
    }
    final class Probe: UIView {
        var changed: (CGSize) -> Void = { _ in }
        private var reported: CGSize?
        override func didMoveToWindow() { super.didMoveToWindow(); report() }
        override func layoutSubviews() { super.layoutSubviews(); report() }
        func report() {
            guard let window, window.bounds.size != reported else { return }
            let next = window.bounds.size
            reported = next
            DispatchQueue.main.async { [weak self, weak window] in
                guard let self, let window, self.window === window, self.reported == next else { return }
                self.changed(next)
            }
        }
    }
}

/// A tool's Done/Close: closes the companion pane when the tool is in one,
/// otherwise dismisses its sheet as before. (`dismiss` inside the pane would
/// act on the conversation's own navigation instead.)
@MainActor struct ToolDismiss: DynamicProperty {
    @Environment(\.dismiss) private var system
    @Environment(\.companionPane) private var pane
    func callAsFunction() {
        if let pane { pane.close() } else { system() }
    }
}

/// A tool's own Done/Close. A pane's selected pill closes it, so only panes
/// without a corresponding pill keep this control. Gesture-dismissed sheets omit it;
/// VoiceOver's Close and escape stay on the pane either way.
struct ToolCloseControl<Content: View>: View {
    @Environment(\.companionPane) private var pane
    @Environment(\.toolHasGestureDismissal) private var gestureDismissal
    @ViewBuilder let content: Content
    var body: some View {
        if pane?.showsClose ?? !gestureDismissal { content }
    }
}

/// On a fold, Swap belongs to the tool header instead of the occluded gutter.
struct CompanionSwapButton: View {
    @Environment(\.companionPane) private var pane
    var body: some View {
        if let swap = pane?.swap, pane?.swapOnDivider != true {
            Button(action: swap) {
                Label("Swap panels", systemImage: pane?.swapVertically == true ? "arrow.up.arrow.down" : "arrow.left.arrow.right")
                    .labelStyle(.iconOnly).frame(minWidth: 44, minHeight: 44)
            }
            .accessibilityIdentifier("companion-pane-flip")
        }
    }
}

/// iPad can fill its continuous screen; a Duo tool already fills one display.
struct CompanionExpandButton: View {
    var inHeader = false
    @Environment(\.companionPane) private var pane
    var body: some View {
        if let pane {
            if pane.onDisplayWithFold {
                CompanionSwapButton()
            } else {
                Button {
                    pane.toggleExpanded()
                } label: {
                    Label(pane.isExpanded ? "Exit full screen" : "Fill screen",
                          systemImage: pane.isExpanded ? "arrow.down.right.and.arrow.up.left" : "arrow.up.left.and.arrow.down.right")
                        .labelStyle(.iconOnly)
                        .frame(minWidth: 44, minHeight: 44)
                        .contentShape(Rectangle())
                }
                .accessibilityIdentifier("companion-pane-expand")
            }
        }
    }
}

/// A tool's own header row inside the companion pane: its title, ⤢ and its
/// controls. The pane is drawn outside the chat's navigation stack; on the Duo
/// (a vertical bar, or a pane split on the fold) the tool's toolbar otherwise
/// shares the window's bar and its items end up under the conversation header.
struct CompanionInlineHeader<Title: View, Controls: View>: View {
    @ViewBuilder let title: Title
    @ViewBuilder let controls: Controls
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        HStack(spacing: 8) {
            title
            Spacer(minLength: 8)
            controls
            CompanionExpandButton(inHeader: true)
        }
        .padding(.horizontal, 16)
        .modifier(DisplayHeaderCorners(enabled: true))
        .background(theme.page)
    }
}

/// Apply to each page inside the tool's NavigationStack. That page owns the
/// safe area beside the system rail; an inset outside the stack can stretch
/// underneath it when expanded. Pushed pages retain explicit Back/close/expand
/// routes while the same navigation stack stays mounted across posture changes.
struct CompanionPaneNavigationHeader<Controls: View>: ViewModifier {
    let enabled: Bool
    let title: String
    var showsBack = false
    var back: () -> Void = {}
    /// A second line under the title in the same header row (a file's language and counts).
    var subtitle: AnyView? = nil
    @ViewBuilder let controls: Controls
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.wonderTheme) private var theme

    func body(content: Content) -> some View {
        content
            .navigationTitle(enabled ? "" : title)
            .toolbar {
                if !enabled, let subtitle {
                    ToolbarItem(placement: .principal) { titleStack(subtitle) }
                }
            }
            .toolbar(enabled ? .hidden : .automatic, for: .navigationBar)
            .safeAreaInset(edge: .top, spacing: 0) {
                if enabled {
                    VStack(spacing: 0) {
                        CompanionInlineHeader { heading } controls: {
                            if !typeSize.isAccessibilitySize { controls }
                        }
                        // Large action labels must not squeeze Back or expand
                        // out of a half-width pane. This row belongs to the tool.
                        if typeSize.isAccessibilitySize {
                            HStack(spacing: 8) { Spacer(minLength: 0); controls }
                                .padding(.horizontal, 16)
                        }
                    }
                    .background(theme.page)
                    .buttonStyle(.borderless)
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier("companion-pane-header")
                }
            }
    }

    @ViewBuilder private var heading: some View {
        if showsBack {
            Button(action: back) {
                Label("Back", systemImage: "chevron.left").labelStyle(.iconOnly)
                    .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
            }
            .accessibilityIdentifier("companion-pane-back")
        }
        if let subtitle {
            // Takes the row's spare width, so the path wraps only when the controls really leave no room.
            titleStack(subtitle).frame(maxWidth: .infinity, alignment: .leading).layoutPriority(1)
        } else {
            Text(title).font(.headline)
                .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                .fixedSize(horizontal: false, vertical: true)
                .layoutPriority(1)
        }
    }

    /// Title and subtitle as one header block, read as one element by the caller's label.
    private func titleStack(_ subtitle: AnyView) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            // A path wraps to a second line before it is cut: the file is named in full where it can be.
            Text(title).font(.footnote.weight(.semibold))
                .lineLimit(typeSize.isAccessibilitySize ? nil : 2).truncationMode(.middle)
                .fixedSize(horizontal: false, vertical: true)
            subtitle
        }
    }
}

/// The settings the pane keeps on this device: which side the tool is on and
/// how much room it takes in landscape and portrait windows.
enum CompanionPanePreferences {
    static let flippedKey = "wonder.companionPane.flipped"
    static let landscapeFractionKey = "wonder.companionPane.fraction.landscape"
    static let portraitFractionKey = "wonder.companionPane.fraction.portrait"
}

/// The conversation with its companion pane. The chat (with its composer)
/// always stays in the first slot of one custom layout; the tool's slot is
/// published to `CompanionPaneOverlayHost`, which draws the tool in one stable
/// place at the app's root. Neither view is rebuilt when the window resizes or
/// rotates, the panels swap, the direction changes or the tool fills the
/// window: a web page, a shell and a computer stream stay connected.
struct CompanionPaneContainer<Chat: View, Pane: View>: View {
    let tool: CompanionTool?
    @Binding var expanded: Bool
    /// The tool's own close control shows (its pill is turned off).
    var showsClose = false
    let close: () -> Void
    @ViewBuilder let chat: Chat
    @ViewBuilder let pane: Pane

    @Environment(\.wonderTheme) private var theme
    @Environment(\.shellWindowSize) private var shellWindow
    @Environment(\.shellChatsReplaceTool) private var chatsReplaceTool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AppStorage(CompanionPanePreferences.flippedKey) private var flipped = false
    @AppStorage(CompanionPanePreferences.landscapeFractionKey) private var landscapeFraction = Double(CompanionPaneLayout.defaultPaneFraction)
    @AppStorage(CompanionPanePreferences.portraitFractionKey) private var portraitFraction = Double(CompanionPaneLayout.defaultPaneFraction)
    /// The divider's share while a finger drags it; stored when it lifts.
    @State private var dragFraction: CGFloat?
    @State private var keyboardVisible = false
    /// The window's shape before the keyboard came up. The keyboard shortens the
    /// window; on the Duo simulator that turned a stacked split side by side.
    @State private var windowBeforeKeyboard: CGSize?
    @State private var containerSize: CGSize = .zero

    var body: some View {
        GeometryReader { geometry in
            let measured = shellWindow ?? geometry.size
            let window = keyboardVisible ? (windowBeforeKeyboard ?? measured) : measured
            let landscape = window.width > window.height
            let stored = CGFloat(landscape ? landscapeFraction : portraitFraction)
            let displayFold = DuoFold.region(in: geometry)
            let layout = CompanionPaneLayout.resolve(
                size: geometry.size, window: window, fold: displayFold,
                paneFraction: dragFraction ?? stored, flipped: flipped,
                expanded: expanded, keyboardVisible: keyboardVisible)
            // Only the tools with a composer-shaped panel reach the window's bottom.
            let hasPanel = tool?.kind == .computer || tool?.kind == .terminal
            let sinks = layout.onFold && layout.pane.maxY >= geometry.size.height - 0.5 && hasPanel && !keyboardVisible
            CompanionSplit(layout: chatsReplaceTool || tool == nil ? nil : layout, parksTool: chatsReplaceTool) {
                // Off screen while the tool fills the window (see `CompanionSplit`).
                chat
                    .accessibilityHidden(tool != nil && expanded && !layout.onFold && !chatsReplaceTool)
                    .allowsHitTesting(tool == nil || !expanded || layout.onFold || chatsReplaceTool)
                if let tool {
                    // The slots `CompanionPaneOverlayHost` draws the tool and the divider into.
                    Color.clear
                        .anchorPreference(key: CompanionPanePlacementKey.self, value: .bounds) { anchor in
                            CompanionPanePlacement(pane: .init(anchor: anchor, label: tool.label, close: close, content: AnyView(
                                pane.environment(\.companionPane, CompanionPaneChrome(
                                    isExpanded: expanded, showsClose: showsClose, toggleExpanded: toggleExpanded, close: close,
                                    onDisplayWithFold: layout.onFold, swap: layout.onFold ? { animate { flipped.toggle() } } : nil,
                                    swapVertically: layout.direction == .stacked,
                                    swapOnDivider: layout.swapsOnDivider,
                                    alignsPanelToComposer: sinks,
                                    panelBottomMargin: sinks ? ToolPanelMetrics.composerRise
                                        : keyboardVisible && layout.onFold && hasPanel ? ToolPanelMetrics.keyboardRise
                                        : ToolPanelMetrics.bottomMargin,
                                    interiorHorizontalEdges: CompanionPaneChrome.interiorEdges(of: layout.pane, in: geometry.size)))),
                                bottomExtension: sinks ? ToolPanelMetrics.sink : 0,
                                ignoresTopInset: layout.swapsOnDivider))
                        }
                        .accessibilityHidden(true)
                    if !expanded && !layout.onFold && !chatsReplaceTool {
                        let divider = CompanionDivider(
                            layout: layout, size: geometry.size, origin: geometry.frame(in: .global).origin, flipped: flipped,
                            dragFraction: $dragFraction,
                            commit: { fraction in
                                if landscape { landscapeFraction = Double(fraction) } else { portraitFraction = Double(fraction) }
                                dragFraction = nil
                            },
                            step: { delta in
                                let next = CompanionPaneLayout.clampedFraction(stored + delta, size: geometry.size,
                                                                               direction: layout.direction)
                                if landscape { landscapeFraction = Double(next) } else { portraitFraction = Double(next) }
                            },
                            flip: { animate { flipped.toggle() } },
                            expand: toggleExpanded)
                        Color.clear
                            .anchorPreference(key: CompanionPanePlacementKey.self, value: .bounds) { anchor in
                                CompanionPanePlacement(divider: .init(anchor: anchor, content: AnyView(divider)))
                            }
                            .accessibilityHidden(true)
                    } else if layout.swapsOnDivider && !chatsReplaceTool {
                        // A seamless fold has no gap to keep clear: Swap stays
                        // where the owner reached for it, on the divider.
                        Color.clear
                            .anchorPreference(key: CompanionPanePlacementKey.self, value: .bounds) { anchor in
                                CompanionPanePlacement(divider: .init(anchor: anchor, content: AnyView(
                                    CompanionFoldSwap(flip: { animate { flipped.toggle() } }))))
                            }
                            .accessibilityHidden(true)
                    }
                }
            }
        }
        // Paint through the reserved fold band; only control placement avoids
        // it. A transparent slot used to show a contrasting empty hinge strip.
        .background(theme.page)
        .onGeometryChange(for: CGSize.self) { $0.size } action: { containerSize = $0 }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { note in
            let frame = (note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect) ?? .zero
            // A hardware keyboard's shortcut bar is short; only a real keyboard shrinks the pane.
            let visible = frame.height > 120
            if visible, !keyboardVisible { windowBeforeKeyboard = shellWindow ?? containerSize }
            if keyboardVisible != visible { animate { keyboardVisible = visible } }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
            if keyboardVisible { animate { keyboardVisible = false } }
        }
        .onChange(of: tool == nil) { _, closed in
            if closed, expanded { expanded = false }
        }
    }

    private func toggleExpanded() { animate { expanded.toggle() } }

    private func animate(_ change: () -> Void) {
        if reduceMotion { change() } else { withAnimation(.snappy(duration: 0.25), change) }
    }
}

/// Where the companion pane goes and what it shows, published by the
/// conversation for `CompanionPaneOverlayHost`.
struct CompanionPanePlacement {
    struct Pane {
        let anchor: Anchor<CGRect>
        let label: String
        let close: () -> Void
        let content: AnyView
        var clipsContent = true
        /// The pane reaches this far below its slot (see `ToolPanelMetrics.sink`).
        var bottomExtension: CGFloat = 0
        /// The tool's own top inset is dropped, so its header centres on the
        /// same line as the chat's title (see `CompanionFoldSwap.headerCenter`).
        var ignoresTopInset = false
    }
    struct Divider {
        let anchor: Anchor<CGRect>
        let content: AnyView
    }
    var pane: Pane?
    var divider: Divider?
}

struct CompanionPanePlacementKey: PreferenceKey {
    static var defaultValue: CompanionPanePlacement { .init() }
    static func reduce(value: inout CompanionPanePlacement, nextValue: () -> CompanionPanePlacement) {
        let next = nextValue()
        if value.pane == nil { value.pane = next.pane }
        if value.divider == nil { value.divider = next.divider }
    }
}

private extension CompanionPanePlacement.Pane {
    var ignoredEdges: Edge.Set {
        var edges: Edge.Set = []
        if bottomExtension > 0 { edges.insert(.bottom) }
        if ignoresTopInset { edges.insert(.top) }
        return edges
    }
}

/// Draws the companion pane at the app's root, over the slot the conversation
/// left for it. Each tool is a NavigationStack with its own toolbar and pushes;
/// drawn inside the conversation's stack, its items merged into the
/// conversation's header (measured on the iPad simulator, also through a
/// separate hosting controller). Here it is a sibling of every stack.
struct CompanionPaneOverlayHost: ViewModifier {
    func body(content: Content) -> some View {
        content.overlayPreferenceValue(CompanionPanePlacementKey.self) { placement in
            GeometryReader { proxy in
                if let pane = placement.pane {
                    let frame = proxy[pane.anchor]
                    pane.content
                        .accessibilityAction(.escape, pane.close)
                        .frame(width: frame.width, height: frame.height + pane.bottomExtension)
                        // The extension is the point: the content may use it.
                        .ignoresSafeArea(.container, edges: pane.ignoredEdges)
                        .modifier(CompanionPaneClipping(enabled: pane.clipsContent))
                        // The pane as one element behind the tool (name, frame and
                        // Close). A container around the tool instead took over the
                        // tool's own root identifier.
                        .background {
                            Color.clear
                                .accessibilityElement()
                                .accessibilityLabel(pane.label)
                                .accessibilityAction(named: "Close", pane.close)
                                .accessibilityIdentifier("companion-pane")
                        }
                        .position(x: frame.midX, y: frame.midY + pane.bottomExtension / 2)
                }
                // Above the tool, so the swap button can overhang the strip.
                if placement.pane != nil, let divider = placement.divider {
                    let frame = proxy[divider.anchor]
                    divider.content
                        .frame(width: frame.width, height: frame.height)
                        .position(x: frame.midX, y: frame.midY)
                }
            }
        }
    }
}

private struct CompanionPaneClipping: ViewModifier {
    let enabled: Bool
    @ViewBuilder func body(content: Content) -> some View {
        // Clipping only bounds pixels. The overlaid NavigationStack can still
        // intercept the sibling Duo rail outside its frame unless hit testing
        // is bounded too. Settings owns a rail Back and opts out of both.
        if enabled { content.contentShape(Rectangle()).clipped() } else { content }
    }
}

/// Places the chat, the pane and the divider at the frames `CompanionPaneLayout`
/// decided; with no pane the chat fills the space.
private struct CompanionSplit: Layout {
    let layout: CompanionPaneLayout?
    var parksTool = false

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        proposal.replacingUnspecifiedDimensions()
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        guard let layout else {
            for (index, subview) in subviews.enumerated() {
                let origin = parksTool && index > 0 ? CGPoint(x: bounds.maxX + bounds.width * 2, y: bounds.minY) : bounds.origin
                subview.place(at: origin, proposal: ProposedViewSize(bounds.size))
            }
            return
        }
        // While the tool fills the window the chat keeps its size (no new layout) but
        // moves off screen: its UIKit text view ignored `accessibilityHidden` and
        // opacity, and stayed in the accessibility tree under the tool.
        let chat = layout.divider.isNull ? layout.chat.offsetBy(dx: bounds.width * 2, dy: 0) : layout.chat
        let frames = [chat, layout.pane, layout.divider]
        for (index, subview) in subviews.enumerated() {
            let frame = index < frames.count ? frames[index] : .null
            guard !frame.isNull else {
                subview.place(at: bounds.origin, proposal: .zero)
                continue
            }
            subview.place(at: CGPoint(x: bounds.minX + frame.minX, y: bounds.minY + frame.minY),
                          proposal: ProposedViewSize(frame.size))
        }
    }
}

/// The strip between the chat and the tool: drag to resize (not on a fold),
/// double-tap to fill the window, and the swap button in its middle.
struct CompanionDivider: View {
    let layout: CompanionPaneLayout
    let size: CGSize
    /// The split container's origin on screen; the divider draws at the app's root.
    let origin: CGPoint
    let flipped: Bool
    @Binding var dragFraction: CGFloat?
    let commit: (CGFloat) -> Void
    let step: (CGFloat) -> Void
    let flip: () -> Void
    let expand: () -> Void
    @Environment(\.wonderTheme) private var theme
    @ScaledMetric(relativeTo: .body) private var buttonSize: CGFloat = 32

    /// The swap button's circle grows with text size up to this, so large text
    /// does not paint it far over the tools; its 44 pt target stays fixed.
    static let maximumButtonPaint: CGFloat = 40
    static let buttonTarget: CGFloat = 44

    private var sideBySide: Bool { layout.direction == .sideBySide }

    var body: some View {
        // Only the strip takes layout. The swap button is an overlay: as a ZStack
        // sibling its 44 pt target sized the stack, and the strip's background
        // painted 14 pt over each tool (Terminal's key row, its first columns).
        ZStack {
            // The fold needs no line: the hardware already shows it.
            if !layout.onFold {
                theme.page
                Rectangle().fill(theme.rule)
                    .frame(width: sideBySide ? 1 : nil, height: sideBySide ? nil : 1)
                Capsule().fill(theme.rule)
                    .frame(width: sideBySide ? 4 : 36, height: sideBySide ? 36 : 4)
                    .offset(x: sideBySide ? 0 : -48, y: sideBySide ? -48 : 0)
                    .accessibilityHidden(true)
            }
            handle
        }
        .overlay { flipButton }
    }

    /// The draggable area, wider than the visible strip. A separate element from
    /// the swap button, so VoiceOver reaches both.
    private var handle: some View {
        Color.clear
            .contentShape(Rectangle().inset(by: layout.resizable ? -10 : 0))
            .gesture(layout.resizable ? drag : nil)
            .onTapGesture(count: 2, perform: expand)
            .accessibilityElement()
            .accessibilityLabel("Resize")
            .accessibilityValue(layout.resizable ? "Tool \(Int((sideBySide ? layout.pane.width / max(1, size.width) : layout.pane.height / max(1, size.height)) * 100)) percent" : "On the fold")
            .accessibilityHint(layout.resizable ? "Swipe up or down to give the tool more or less room" : "")
            .accessibilityAdjustableAction { direction in
                guard layout.resizable else { return }
                switch direction {
                case .increment: step(0.1)
                case .decrement: step(-0.1)
                @unknown default: break
                }
            }
            .accessibilityAction(named: "Fill screen", expand)
            .accessibilityIdentifier("companion-pane-divider")
    }

    private var drag: some Gesture {
        DragGesture(minimumDistance: 2, coordinateSpace: .global)
            .onChanged { value in
                let position = sideBySide ? value.location.x - origin.x : value.location.y - origin.y
                dragFraction = CompanionPaneLayout.fraction(dividerAt: position, size: size,
                                                            direction: layout.direction, flipped: flipped)
            }
            .onEnded { _ in
                if let dragFraction { commit(dragFraction) }
            }
    }

    private var flipButton: some View {
        CompanionSwapHandle(sideBySide: sideBySide, flip: flip)
    }
}

/// The round Swap panels control: on the divider, or on a seamless fold.
struct CompanionSwapHandle: View {
    let sideBySide: Bool
    let flip: () -> Void
    @Environment(\.wonderTheme) private var theme
    @ScaledMetric(relativeTo: .body) private var buttonSize: CGFloat = 32

    var body: some View {
        Button(action: flip) {
            let paint = min(buttonSize, CompanionDivider.maximumButtonPaint)
            Image(systemName: sideBySide ? "arrow.left.arrow.right" : "arrow.up.arrow.down")
                .font(.footnote.weight(.semibold))
                .dynamicTypeSize(...DynamicTypeSize.accessibility1)
                .foregroundStyle(theme.text)
                .frame(width: paint, height: paint)
                .background(theme.surface, in: Circle())
                .overlay(Circle().strokeBorder(theme.rule, lineWidth: 1))
                .frame(width: CompanionDivider.buttonTarget, height: CompanionDivider.buttonTarget)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Swap panels")
        .accessibilityIdentifier("companion-pane-flip")
    }
}

/// Swap on a seamless fold: centered on the split at the tool header's height,
/// over the page colour and nothing else.
struct CompanionFoldSwap: View {
    let flip: () -> Void
    /// The distance from the split's top (below the status bar) to the headers' centre line.
    static let headerCenter: CGFloat = 22

    var body: some View {
        Color.clear.allowsHitTesting(false)
            .overlay(alignment: .top) {
                CompanionSwapHandle(sideBySide: true, flip: flip)
                    .padding(.top, Self.headerCenter - CompanionDivider.buttonTarget / 2)
            }
    }
}

/// The fold across an iPhone Duo's inner display (iOS 27.1 `.division`
/// reserved region), in the reading view's coordinates. Inactive while the
/// phone lies flat; absent on the cover display and other devices.
enum DuoFold {
    static func region(in geometry: GeometryProxy) -> ComputerFoldLayout.Fold? {
        #if WONDER_DIAGNOSTICS
        // A simulated fold down the middle, so the companion pane's fold split
        // can be checked on any simulator. The Duo simulator reports the real one.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-companion-fold") {
            let size = geometry.size
            return size.width > size.height
                ? .init(frame: CGRect(x: (size.width - 40) / 2, y: 0, width: 40, height: size.height), isActive: false)
                : .init(frame: CGRect(x: 0, y: (size.height - 40) / 2, width: size.width, height: 40), isActive: false)
        }
        #endif
        #if canImport(SwiftUI, _version: 8.0.85)
        if #available(iOS 27.1, *),
           let region = geometry.reservedRegions(kind: .division, options: .includeInactive).first {
            return .init(frame: region.frame, isActive: region.isActive)
        }
        #endif
        return nil
    }
}

/// Keeps a control that spans the window (a centred title menu) on the leading
/// display when the hinge runs down the middle; message text may still span it.
struct AvoidsDisplayFold: ViewModifier {
    /// Off for a view that owns the whole screen across the hinge (the
    /// computer's full-screen cover): its header and controls centre on it.
    var enabled = true
    @State private var leadingWidth: CGFloat?

    func body(content: Content) -> some View {
        content
            .frame(maxWidth: enabled ? leadingWidth ?? .infinity : .infinity)
            .frame(maxWidth: .infinity, alignment: .leading)
            .onGeometryChange(for: CGFloat?.self) { proxy in
                guard let frame = DuoFold.region(in: proxy)?.frame, frame.height > frame.width,
                      frame.minX > 0, frame.minX < proxy.size.width else { return nil }
                return frame.minX
            } action: { leadingWidth = $0 }
    }
}

/// Follows the iPhone Duo's hinge (iOS 27.1 `onHingeChange`). Other devices
/// and earlier systems never report one.
struct DuoHingeReader: ViewModifier {
    @Binding var hinge: ComputerFoldLayout.Hinge?

    func body(content: Content) -> some View {
        #if canImport(SwiftUI, _version: 8.0.85)
        if #available(iOS 27.1, *) {
            content.onHingeChange { _, context in
                let next = Self.hinge(context.hinge)
                if hinge != next { hinge = next }
            }
        } else {
            content
        }
        #else
        content
        #endif
    }

    #if canImport(SwiftUI, _version: 8.0.85)
    @available(iOS 27.1, *)
    private static func hinge(_ hinge: DeviceHinge?) -> ComputerFoldLayout.Hinge? {
        guard let status = hinge?.status else { return nil }
        if status == .partiallyOpen { return .partiallyOpen }
        if status == .fullyOpen { return .fullyOpen }
        if status == .closed { return .closed }
        return nil
    }
    #endif
}

#if WONDER_DIAGNOSTICS
/// Reports the system division in screen coordinates for native AX assertions.
/// This reads the same GeometryProxy as layout; it never drives production frames.
struct DuoGeometryEvidence: View {
    let geometry: GeometryProxy
    var priority = 0
    var ownsControls = false
    @State private var hinge: ComputerFoldLayout.Hinge?
    var body: some View {
        let origin = geometry.frame(in: .global).origin
        let fold = DuoFold.region(in: geometry)?.frame.offsetBy(dx: origin.x, dy: origin.y)
        DiagnosticDriverSurface(surface: .init(priority: priority, ownsControls: ownsControls, state: {
            ["displayDivision": fold.map { ["x": $0.minX, "y": $0.minY, "width": $0.width, "height": $0.height] } as Any? ?? NSNull(),
             "displayDivisionActive": DuoFold.region(in: geometry)?.isActive as Any? ?? NSNull(),
             "hinge": hinge.map { String(describing: $0) } as Any? ?? NSNull()]
        }))
        .modifier(DuoHingeReader(hinge: $hinge))
        .frame(width: 0, height: 0).accessibilityHidden(true)
    }
}
#endif
/// Swipe-down dismissal for custom popovers/cards, which have no sheet grabber: a drag
/// that starts in the top band and ends lower closes them. It runs beside the content's
/// own gestures, so taps and scrolling are untouched; outside-tap and escape also dismiss.
private struct DismissOnTopDrag: ViewModifier {
    let dismiss: () -> Void
    func body(content: Content) -> some View {
        content.simultaneousGesture(DragGesture(minimumDistance: 10).onEnded { value in
            if value.startLocation.y < 56, value.translation.height > 20 { dismiss() }
        })
    }
}

extension View {
    func dismissOnTopDrag(_ dismiss: @escaping () -> Void) -> some View { modifier(DismissOnTopDrag(dismiss: dismiss)) }
}
