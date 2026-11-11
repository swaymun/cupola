import SwiftUI
import UIKit
import WonderPairing

// MARK: - Provider identity

/// One provider identity for rows and controls; VoiceOver hears its name.
struct ProviderIcon: View {
    let family: AgentFamily
    var size: CGFloat = 16
    @ScaledMetric(relativeTo: .subheadline) private var scale: CGFloat = 1
    var body: some View {
        // The share extension draws the same `ProviderMark`.
        switch ProviderMark(family) {
        case .symbol(let name):
            // No bundled artwork: a neutral glyph in a tile.
            Image(systemName: name)
                .font(.system(size: size * scale * 0.5, weight: .bold))
                .foregroundStyle(.white)
                .frame(width: size * scale, height: size * scale)
                .background(Color.black.opacity(0.85), in: RoundedRectangle(cornerRadius: size * scale * 0.22, style: .continuous))
                .accessibilityLabel(family.title)
        case .artwork(let name):
            Image(name)
                .resizable().interpolation(.high).scaledToFit()
                .frame(width: size * scale, height: size * scale)
                .accessibilityLabel(family.title)
        }
    }
}

// MARK: - Composer controls shared by new chats and threads

extension View {
    /// Attach inside the label so UIKit receives the rendered pill's bounds,
    /// including in a system bar where SwiftUI resolves popover anchors to zero.
    func pillPopover<Content: View>(isPresented: Binding<Bool>, showsSwipeDismiss: Bool = true,
                                   @ViewBuilder content: @escaping () -> Content) -> some View {
        background {
            PillPopoverAnchor(isPresented: isPresented, showsSwipeDismiss: showsSwipeDismiss, content: content())
        }
    }

    func pillPopover<Item: Identifiable, Content: View>(item: Binding<Item?>,
                                                       @ViewBuilder content: @escaping (Item) -> Content) -> some View {
        pillPopover(isPresented: Binding(get: { item.wrappedValue != nil }, set: { if !$0 { item.wrappedValue = nil } })) {
            if let selected = item.wrappedValue { content(selected) }
        }
    }
}

private struct OptionalTopDragDismiss: ViewModifier {
    let enabled: Bool
    let dismiss: () -> Void
    @ViewBuilder func body(content: Content) -> some View {
        if enabled { content.dismissOnTopDrag(dismiss) } else { content }
    }
}

/// A toolbar anchor is only a view. A child controller inside the navigation
/// item can suppress the conversation's appearance callback and hold its
/// initial reading position behind Loading chat. The panel's controller belongs
/// to the window presenter; the source only supplies bounds and lifetime.
private struct PillPopoverAnchor<Content: View>: UIViewRepresentable {
    @Binding var isPresented: Bool
    let showsSwipeDismiss: Bool
    let content: Content

    func makeUIView(context: Context) -> AnchorView { AnchorView() }

    func updateUIView(_ controller: AnchorView, context: Context) {
        let environment = context.environment
        let root = AnyView(VStack(spacing: 0) {
            content
        }
            .modifier(OptionalTopDragDismiss(enabled: showsSwipeDismiss) { isPresented = false })
            .background(environment.wonderTheme.page)
            .accessibilityAction(.escape) { isPresented = false }
            .environment(\.wonderTheme, environment.wonderTheme)
            .environment(\.dynamicTypeSize, environment.dynamicTypeSize)
            .environment(\.colorScheme, environment.colorScheme)
            .environment(\.scenePhase, environment.scenePhase)
            .environment(\.layoutDirection, environment.layoutDirection))
        controller.update(root: root, background: environment.wonderTheme.pageUIColor, presented: isPresented) { isPresented = false }
    }

    static func dismantleUIView(_ controller: AnchorView, coordinator: ()) {
        controller.tearDown()
    }

    @MainActor final class AnchorView: UIView, UIPopoverPresentationControllerDelegate {
        private var root = AnyView(EmptyView())
        private var wantsPresentation = false
        private var panel: PillPopoverController?
        private var dismissed: (() -> Void)?
        private var pending: Task<Void, Never>?
        private var presentationRetries = 0
        private var background = WonderTheme.resolve(id: nil, systemIsDark: false).surfaceUIColor

        override init(frame: CGRect) {
            super.init(frame: frame)
            backgroundColor = .clear
            isUserInteractionEnabled = false
            isAccessibilityElement = false
        }
        required init?(coder: NSCoder) { nil }

        func update(root: AnyView, background: UIColor, presented: Bool, dismissed: @escaping () -> Void) {
            self.root = root
            self.background = background
            if presented != wantsPresentation {
                presentationRetries = presented ? 20 : 0
                if !presented { pending?.cancel(); pending = nil }
            }
            wantsPresentation = presented
            self.dismissed = dismissed
            if presented {
                panel?.rootView = root
                panel?.view.backgroundColor = .clear
                panel?.updateSurface(background)
            }
            schedule()
        }

        override func didMoveToWindow() {
            super.didMoveToWindow()
            if window != nil, wantsPresentation, panel == nil { presentationRetries = 20 }
            schedule()
        }

        override func layoutSubviews() {
            super.layoutSubviews()
            if let popover = panel?.popoverPresentationController, popover.sourceRect != bounds {
                popover.sourceRect = bounds
            }
            schedule()
        }

        /// Presentation and binding changes never run synchronously from layout.
        private func schedule(delayed: Bool = false) {
            guard pending == nil else { return }
            pending = Task { @MainActor [weak self] in
                if delayed { try? await Task.sleep(for: .milliseconds(100)) }
                else { await Task.yield() }
                guard !Task.isCancelled, let self else { return }
                self.pending = nil
                self.reconcile()
            }
        }

        private func reconcile() {
            guard wantsPresentation else {
                let closing = panel
                panel = nil
                closing?.dismiss(animated: true) { [weak self] in self?.schedule() }
                return
            }
            guard panel == nil else { return }
            guard let window,
                  sourceIsVisible,
                  bounds.width > 0, bounds.height > 0,
                  window.bounds.intersects(convert(bounds, to: window)),
                  var presenter = window.rootViewController else { retryPresentation(); return }
            while let presented = presenter.presentedViewController {
                guard !presented.isBeingDismissed, !presented.isBeingPresented else { retryPresentation(); return }
                presenter = presented
            }
            guard presenter.viewIfLoaded?.window != nil else { retryPresentation(); return }
            presentationRetries = 0
            let next = PillPopoverController(rootView: root, surface: background)
            next.modalPresentationStyle = .popover
            next.view.backgroundColor = .clear
            next.preferredContentSize = CGSize(width: 320, height: 280)
            next.sizingOptions = .preferredContentSize
            guard let popover = next.popoverPresentationController else { return }
            popover.sourceView = self
            popover.sourceRect = bounds
            popover.permittedArrowDirections = .any
            popover.backgroundColor = background
            popover.popoverBackgroundViewClass = PillPopoverSurface.self
            popover.delegate = self
            panel = next
            presenter.present(next, animated: true)
        }

        /// A fold can mount the source before its window geometry or the old
        /// presenter's dismissal settles. That readiness change need not lay
        /// out this 44-point source again. Retry only the outstanding request,
        /// coalesced, bounded to two seconds and cancelled with its owner.
        private func retryPresentation() {
            guard wantsPresentation, panel == nil, presentationRetries > 0 else { return }
            presentationRetries -= 1
            schedule(delayed: true)
        }

        /// SwiftUI retains an invisible row while the system bar displays its
        /// items. Both receive binding updates; only the displayed source owns
        /// a presentation. Check public view state, not private host class names.
        private var sourceIsVisible: Bool {
            var ancestor: UIView? = self
            while let current = ancestor {
                guard !current.isHidden, current.alpha > 0.01 else { return false }
                ancestor = current.superview
            }
            return true
        }

        func adaptivePresentationStyle(for controller: UIPresentationController) -> UIModalPresentationStyle { .none }
        func adaptivePresentationStyle(for controller: UIPresentationController,
                                       traitCollection: UITraitCollection) -> UIModalPresentationStyle { .none }

        func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
            closed(presentationController.presentedViewController)
        }

        func popoverPresentationControllerDidDismissPopover(_ popover: UIPopoverPresentationController) {
            closed(popover.presentedViewController)
        }

        private func closed(_ controller: UIViewController) {
            guard controller === panel else { return }
            panel = nil
            wantsPresentation = false
            presentationRetries = 0
            dismissed?()
        }

        func tearDown() {
            pending?.cancel()
            pending = nil
            dismissed = nil
            wantsPresentation = false
            presentationRetries = 0
            let closing = panel
            panel = nil
            closing?.dismiss(animated: false)
        }
    }
}

/// The presenting owner passes its theme directly to its own chrome. Searching
/// only the nearest containing view keeps stacked presentations independent.
@MainActor final class PillPopoverController: UIHostingController<AnyView> {
    private var surfaceColor: UIColor
    private weak var chrome: PillPopoverSurface?

    init(rootView: AnyView, surface: UIColor) {
        surfaceColor = surface
        super.init(rootView: rootView)
    }
    @MainActor required dynamic init?(coder: NSCoder) { nil }

    func updateSurface(_ color: UIColor) {
        surfaceColor = color
        popoverPresentationController?.backgroundColor = color
        chrome?.configure(surface: color)
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        if let chrome, chrome.window != nil {
            chrome.configure(surface: surfaceColor)
            return
        }
        func surface(in view: UIView) -> PillPopoverSurface? {
            if let surface = view as? PillPopoverSurface { return surface }
            for child in view.subviews where child !== self.view {
                if let found = surface(in: child) { return found }
            }
            return nil
        }
        var ancestor = view.superview
        while let current = ancestor {
            if let found = surface(in: current) {
                chrome = found
                found.configure(surface: surfaceColor)
                break
            }
            if current === popoverPresentationController?.containerView { break }
            ancestor = current.superview
        }
    }
}

/// UIKit chooses the direction, offset and source target. The union has one
/// outside contour, so no overlapping subpaths can leave an antialiased seam.
@MainActor final class PillPopoverSurface: UIPopoverBackgroundView {
    private var direction: UIPopoverArrowDirection = .up
    private var offset: CGFloat = 0
    private var surface = WonderTheme.resolve(id: nil, systemIsDark: false).surfaceUIColor
    override class func arrowBase() -> CGFloat { 16 }
    override class func arrowHeight() -> CGFloat { 8 }
    override class func contentViewInsets() -> UIEdgeInsets { UIEdgeInsets(top: 14, left: 14, bottom: 14, right: 14) }
    override var arrowDirection: UIPopoverArrowDirection {
        get { direction }
        set { direction = newValue; setNeedsDisplay() }
    }
    override var arrowOffset: CGFloat {
        get { offset }
        set { offset = newValue; setNeedsDisplay() }
    }
    override init(frame: CGRect) {
        super.init(frame: frame)
        isOpaque = false
        backgroundColor = .clear
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: PillPopoverSurface, _: UITraitCollection) in
            view.setNeedsDisplay()
        }
    }
    required init?(coder: NSCoder) { nil }

    func configure(surface: UIColor) {
        guard self.surface != surface else { return }
        self.surface = surface
        setNeedsDisplay()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        // UIKit installs its default heavy shadow after initialization. Apply
        // our restrained elevation after that layout, for every direction.
        layer.shadowOpacity = 0.12
        layer.shadowRadius = 8
        layer.shadowOffset = CGSize(width: 0, height: 3)
        backgroundColor = .clear
        setNeedsDisplay()
    }

    var silhouette: CGPath {
        var body = bounds
        let height = Self.arrowHeight(), half = Self.arrowBase() / 2
        switch direction {
        case .up: body.origin.y += height; body.size.height -= height
        case .down: body.size.height -= height
        case .left: body.origin.x += height; body.size.width -= height
        case .right: body.size.width -= height
        default: break
        }
        let path = UIBezierPath(roundedRect: body, cornerRadius: 22)
        let triangle = UIBezierPath()
        if direction == .up || direction == .down {
            let x = min(body.maxX - 30, max(body.minX + 30, bounds.midX + offset))
            let edge = direction == .up ? body.minY + 1 : body.maxY - 1
            triangle.move(to: CGPoint(x: x - half, y: edge))
            triangle.addLine(to: CGPoint(x: x, y: direction == .up ? bounds.minY : bounds.maxY))
            triangle.addLine(to: CGPoint(x: x + half, y: edge))
        } else if direction == .left || direction == .right {
            let y = min(body.maxY - 30, max(body.minY + 30, bounds.midY + offset))
            let edge = direction == .left ? body.minX + 1 : body.maxX - 1
            triangle.move(to: CGPoint(x: edge, y: y - half))
            triangle.addLine(to: CGPoint(x: direction == .left ? bounds.minX : bounds.maxX, y: y))
            triangle.addLine(to: CGPoint(x: edge, y: y + half))
        } else { return path.cgPath }
        triangle.close()
        return path.cgPath.union(triangle.cgPath)
    }

    override func draw(_ rect: CGRect) {
        let path = silhouette
        surface.resolvedColor(with: traitCollection).setFill()
        UIBezierPath(cgPath: path).fill()
        layer.shadowPath = path
    }
}

/// Provider icon and the model's name, the button that opens the model sheet.
/// A narrow composer (the folded Duo, an iPhone) keeps model and effort on the button row by
/// dropping the provider icon and chevrons, so the names get the room.
private struct ComposerCompactChipsKey: EnvironmentKey { static let defaultValue = false }
extension EnvironmentValues {
    var composerCompactChips: Bool {
        get { self[ComposerCompactChipsKey.self] }
        set { self[ComposerCompactChipsKey.self] = newValue }
    }
}

struct ComposerModelLabel: View {
    let family: AgentFamily?
    let title: String
    /// A faster speed than standard is chosen.
    var fast = false
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.composerCompactChips) private var compact
    var body: some View {
        HStack(spacing: 5) {
            if let family, !compact { ProviderIcon(family: family, size: 16) }
            Text(title).lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
                .fixedSize(horizontal: false, vertical: true)
            if fast { Image(systemName: "bolt.fill").imageScale(.small).foregroundStyle(.secondary) }
            if !compact { Image(systemName: "chevron.down").imageScale(.small).accessibilityHidden(true) }
        }.font(.subheadline)
        .fixedSize(horizontal: false, vertical: true)
        .foregroundStyle(.primary)
        .padding(.horizontal, 4).frame(minWidth: 44, minHeight: 44)
        .contentShape(Rectangle())
    }
}

/// Narrow composers give the selected model and effort their own readable row.
/// Inputs remain in the bottom action row; no selected value becomes an icon.
enum ComposerSettingsPlacement { case inline, choices, inputs }

struct ComposerSettingsRow<Access: View, Model: View, Effort: View>: View {
    var placement: ComposerSettingsPlacement = .inline
    @ViewBuilder let access: () -> Access
    @ViewBuilder let model: () -> Model
    @ViewBuilder let effort: () -> Effort
    @Environment(\.dynamicTypeSize) private var typeSize
    var body: some View {
        if placement == .choices && typeSize.isAccessibilitySize {
            VStack(alignment: .leading, spacing: 0) {
                model()
                effort()
            }.fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            HStack(spacing: 0) {
                if placement != .choices { access() }
                if placement != .inputs {
                    if placement == .inline { Spacer(minLength: 0) }
                    model()
                    effort()
                    if placement == .choices { Spacer(minLength: 0) }
                } else { Spacer(minLength: 0) }
            }
        }
    }
}

/// Conversational voice has a different glyph from the dictation microphone.
/// Keep its touch target the same in a thread and New chat.
struct ComposerVoiceButton: View {
    var unavailable = false
    let start: () -> Void
    var body: some View {
        Button(action: start) {
            // Icon-only like the dictation microphone beside it; the name is the accessibility label, help and long-press menu.
            Image(systemName: "waveform").font(.system(size: 20))
                .foregroundStyle(Color.primary)
                .frame(width: 44, height: 44)
                .contentShape(Rectangle())
                .help("Talk")
        }
        .disabled(unavailable)
        .accessibilityLabel("Talk with voice")
        .accessibilityHint("Opens a two-way voice chat.")
        .accessibilityIdentifier("voice-start")
        .contextMenu {
            Button("Talk with voice", systemImage: "waveform.circle", action: start)
        }
    }
}

/// Icon-only composer actions retain their names for accessibility and help.
struct ComposerVoiceActionLabel: View {
    let title: String
    let systemImage: String
    var loading = false
    var recording = false
    var body: some View {
        Group {
            if loading { ProgressView() }
            else { Image(systemName: systemImage).font(.system(size: 20)) }
        }
        .foregroundStyle(recording ? Color.blue : Color.primary)
        .frame(width: 44, height: 44)
        .contentShape(Rectangle())
        .accessibilityLabel(title)
        .help(title)
    }
}

/// The selected effort stays readable at every text size.
struct ComposerEffortLabel: View {
    let title: String
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.composerCompactChips) private var compact
    var body: some View {
        HStack(spacing: 5) {
            Text(title).lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
                .fixedSize(horizontal: false, vertical: true)
            if !compact { Image(systemName: "chevron.down").imageScale(.small).accessibilityHidden(true) }
        }.font(.subheadline)
        .fixedSize(horizontal: false, vertical: true)
        .foregroundStyle(.primary)
        .padding(.horizontal, 4).frame(minWidth: 44, minHeight: 44)
        .contentShape(Rectangle())
    }
}

/// The reasoning effort beside the model: its level's name, and a slider
/// across exactly the levels this model reports. Absent for a model without
/// effort levels (Claude Haiku).
struct ComposerEffortButton: View {
    let levels: [AgentOptions.Choice]
    let selected: String?
    var isDisabled = false
    let identifier: String
    let choose: (String) -> Void
    @State private var showing = false
    private var current: AgentOptions.Choice? { levels.first { $0.id == selected } }
    /// The name the button shows for the selected level.
    static func title(levels: [AgentOptions.Choice], selected: String?) -> String {
        levels.first { $0.id == selected }.map(ModelDefaults.title(of:)) ?? "Effort"
    }
    var body: some View {
        Button { showing = true } label: {
            ComposerEffortLabel(title: Self.title(levels: levels, selected: selected))
                // A small anchored panel dismisses by tapping away; no grab line.
                .pillPopover(isPresented: $showing, showsSwipeDismiss: false) {
                    EffortSelector(levels: levels, selected: selected, identifier: identifier) { choose($0) }
                }
        }
        .buttonStyle(.plain)
        .disabled(isDisabled)
        .accessibilityLabel("Reasoning effort")
        .accessibilityValue(current.map(ModelDefaults.title(of:)) ?? "Default")
        .accessibilityIdentifier(identifier)
        #if WONDER_DIAGNOSTICS
        .onAppear {
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-effort-open") { showing = true }
        }
        #endif
    }
}

/// A stepped slider over a model's effort levels, from least to most.
struct EffortSelector: View {
    let levels: [AgentOptions.Choice]
    let selected: String?
    let identifier: String
    let choose: (String) -> Void
    @State private var position: Double
    /// A drag the owner started; only its end chooses a level.
    @State private var dragging = false
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize
    init(levels: [AgentOptions.Choice], selected: String?, identifier: String, choose: @escaping (String) -> Void) {
        self.levels = levels; self.selected = selected; self.identifier = identifier; self.choose = choose
        _position = State(initialValue: Double(levels.firstIndex { $0.id == selected } ?? 0))
    }
    private var index: Int { min(max(Int(position.rounded()), 0), max(levels.count - 1, 0)) }
    var body: some View {
        if typeSize.isAccessibilitySize { levelList } else { slider }
    }

    /// Accessibility text sizes: the level names cannot sit under the stops of a
    /// track, so each level is a full-width row instead, the chosen one checked.
    private var levelList: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 4) {
                Text("Reasoning effort").font(.subheadline).foregroundStyle(.secondary)
                    .accessibilityAddTraits(.isHeader)
                ForEach(Array(levels.enumerated()), id: \.element.id) { offset, level in
                    Button {
                        position = Double(offset)
                        if level.id != selected { choose(level.id) }
                    } label: {
                        HStack(spacing: 12) {
                            Text(ModelDefaults.title(of: level)).foregroundStyle(.primary)
                            Spacer(minLength: 8)
                            if offset == index { Image(systemName: "checkmark").foregroundStyle(theme.accent) }
                        }
                        .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(offset == index ? .isSelected : [])
                    .accessibilityIdentifier(identifier + ":" + level.id)
                }
            }
            .padding(16)
        }
        .scrollBounceBehavior(.basedOnSize)
        .frame(minWidth: 300, idealWidth: 340, maxWidth: 400)
    }

    private var slider: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text("Reasoning effort").foregroundStyle(.secondary)
                Spacer(minLength: 16)
                if levels.indices.contains(index) { Text(ModelDefaults.title(of: levels[index])).fontWeight(.semibold) }
            }
            .font(.subheadline)
            if levels.count > 1 {
                Slider(value: $position, in: 0...Double(levels.count - 1), step: 1) { editing in
                    if editing { dragging = true; return }
                    guard dragging else { return }
                    dragging = false
                    if levels.indices.contains(index), levels[index].id != selected { choose(levels[index].id) }
                }
                // VoiceOver adjusts without a drag: choose each step it lands on.
                .onChange(of: position) { _, _ in
                    guard !dragging, levels.indices.contains(index), levels[index].id != selected else { return }
                    choose(levels[index].id)
                }
                .tint(theme.accent)
                .accessibilityLabel("Reasoning effort")
                .accessibilityValue(levels.indices.contains(index) ? ModelDefaults.title(of: levels[index]) : "")
                .accessibilityIdentifier(identifier + "-slider")
                // Each level's name sits under its stop on the track, in a column
                // no wider than the gap between stops, so neighbours never overlap.
                GeometryReader { proxy in
                    let inset: CGFloat = 14
                    let step = (proxy.size.width - inset * 2) / CGFloat(max(levels.count - 1, 1))
                    let column = min(max(step - 2, 44), 96)
                    ForEach(Array(levels.enumerated()), id: \.element.id) { offset, level in
                        Button { position = Double(offset) } label: {
                            Text(ModelDefaults.title(of: level)).font(.caption2)
                                .foregroundStyle(offset == index ? .primary : .secondary)
                                .lineLimit(1).minimumScaleFactor(0.6)
                                .frame(width: column, height: 28,
                                       alignment: offset == 0 ? .leading : offset == levels.count - 1 ? .trailing : .center)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .position(x: min(max(inset + step * CGFloat(offset), column / 2), proxy.size.width - column / 2), y: 14)
                        .accessibilityIdentifier(identifier + ":" + level.id)
                    }
                }
                .frame(height: 28)
                .accessibilityElement(children: .contain)
            }
        }
        .padding(16)
        .frame(minWidth: 300, idealWidth: 380, maxWidth: 420)
    }
}

/// The thread header's context ring: how full this thread's context window is,
/// as the provider last measured it. It is offered only once measured, never an
/// empty ring, and says nothing about the plan: Usage beside Settings owns plan
/// limits. The rendered control anchors its panel in both the horizontal header
/// and the Duo rail, where it is icon-only like the other rail buttons.
struct ContextUsageButton<Panel: View>: View {
    let usage: ContextUsage?
    @Binding var showing: Bool
    var inRail = false
    @ViewBuilder let panel: () -> Panel
    var body: some View {
        Button { showing.toggle() } label: {
            ring
                .padding(.horizontal, inRail ? 0 : 4)
                .frame(minWidth: 44, minHeight: 44)
                .modifier(RailButtonSurface(inBar: inRail, selected: showing))
                .contentShape(Rectangle())
                .pillPopover(isPresented: $showing, showsSwipeDismiss: false, content: panel)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Context window")
        .accessibilityValue(usage.map { ($0.hasWindow ? "\($0.percent) percent used" : "\($0.summary) used") + ($0.age().map { ", last reported \($0)" } ?? "") } ?? "Not reported yet")
        .accessibilityIdentifier("context-usage-ring")
        .accessibilityAddTraits(showing ? .isSelected : [])
        #if WONDER_DIAGNOSTICS
        .onChange(of: usage, initial: true) { _, _ in
            // The button exists only once the fixture's activity read supplied
            // usage or the not-reported note, so a launch capture matches the
            // normal tap-after-load state.
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-context-usage-open") { showing = true }
        }
        #endif
    }

    @ViewBuilder private var ring: some View {
        if let usage {
            TimelineView(.everyMinute) { tick in
                ContextUsageRing(fraction: usage.fraction, known: usage.hasWindow, maximumSize: inRail ? 18 : 30)
                    .opacity(usage.isStale(now: tick.date) ? 0.45 : 1)
            }
        } else {
            // Nothing reported yet: a dashed ring, never a fill that looks measured.
            ContextUsageRing(fraction: 0, known: false, maximumSize: inRail ? 18 : 30).opacity(0.6)
        }
    }
}


/// A one-point strip at the top of the content, just below the navigation bar.
/// The header's popovers and dialogs present from here, so they open below the
/// header instead of covering its title and buttons. `width` narrows it to sit
/// under a trailing header button.
struct HeaderAnchor: View {
    var width: CGFloat? = nil
    var body: some View {
        Color.clear
            .frame(width: width, height: 1)
            .frame(maxWidth: width == nil ? .infinity : nil)
            .allowsHitTesting(false)
    }
}

/// A small ring that fills with the context window over its track.
struct ContextUsageRing: View {
    let fraction: Double
    /// False draws a dashed track: the window size, or the whole reading, is not reported.
    var known = true
    var maximumSize: CGFloat = 30
    @Environment(\.wonderTheme) private var theme
    @ScaledMetric(relativeTo: .body) private var size: CGFloat = 22
    var body: some View {
        ZStack {
            Circle().stroke(Color.secondary.opacity(known ? 0.3 : 0.55),
                            style: StrokeStyle(lineWidth: 3, dash: known ? [] : [3, 3]))
            if known {
                Circle().trim(from: 0, to: max(0.02, fraction))
                    .stroke(fraction >= 0.9 ? Color.orange : theme.accent, style: StrokeStyle(lineWidth: 3, lineCap: .round))
                    .rotationEffect(.degrees(-90))
            }
        }
        .frame(width: min(size, maximumSize), height: min(size, maximumSize))
        .accessibilityHidden(true)
    }
}

/// The thread's context window as the provider last reported it. Only reported
/// values show; nothing is estimated.
struct ContextUsagePanel: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize
    private var usage: ContextUsage? { model.nativeContextUsage[chat.id] }
    private var note: String? { model.nativeContextNote[chat.id] }
    var body: some View {
        ScrollView {
            // The tick can predate this render by up to a minute; an age computed from it reads a unit low.
            TimelineView(.everyMinute) { tick in contextRows(now: max(tick.date, Date())) }
                .padding(16)
        }
        .frame(minWidth: 300, idealWidth: 340, maxWidth: 380)
        .scrollBounceBehavior(.basedOnSize)
    }

    /// Label and value side by side, or stacked at accessibility text sizes,
    /// where side by side breaks words like "Context" across lines.
    private var pair: AnyLayout {
        typeSize.isAccessibilitySize ? AnyLayout(VStackLayout(alignment: .leading, spacing: 2))
            : AnyLayout(HStackLayout(alignment: .firstTextBaseline, spacing: 8))
    }

    private func contextRows(now: Date) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            pair {
                Text("Context window").foregroundStyle(.secondary)
                if !typeSize.isAccessibilitySize { Spacer(minLength: 4) }
                if let usage {
                    Text(usage.hasWindow ? "\(usage.summary.replacingOccurrences(of: " of ", with: " / ")) (\(usage.percent)%)" : usage.summary)
                        .monospacedDigit().opacity(usage.isStale(now: now) ? 0.6 : 1)
                } else {
                    Text("Not reported").foregroundStyle(.secondary)
                }
            }
            if let usage {
                if usage.hasWindow {
                    UsageBar(fraction: usage.fraction, tint: usage.fraction >= 0.9 ? .orange : theme.accent, height: 6)
                        .opacity(usage.isStale(now: now) ? 0.45 : 1)
                    if usage.windowIsTypical {
                        Text("Typical window for this model; the provider did not report one.").font(.footnote).foregroundStyle(.secondary)
                    }
                } else {
                    Text("This model's window size is not known, so there is no percentage.").font(.footnote).foregroundStyle(.secondary)
                }
                if let age = usage.age(now: now) {
                    Text("Last reported \(age); updates after the next reply.").font(.footnote).foregroundStyle(.secondary)
                }
            } else if let note {
                Text(note).font(.footnote).foregroundStyle(.secondary)
            }
        }
        .font(.subheadline)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Context window")
        .accessibilityValue(usage?.accessibilityValue ?? note ?? "Not reported yet")
        .accessibilityIdentifier("context-usage-window")
    }
}

/// A thin rounded progress bar.
struct UsageBar: View {
    let fraction: Double
    let tint: Color
    var height: CGFloat = 4
    var body: some View {
        GeometryReader { proxy in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.secondary.opacity(0.2))
                Capsule().fill(tint).frame(width: fraction > 0 ? max(height, proxy.size.width * min(1, fraction)) : 0)
            }
        }
        .frame(height: height)
        .accessibilityHidden(true)
    }
}

/// Shows that plan mode is on; tapping turns it off.
struct PlanModeChip: View {
    var isDisabled = false
    let turnOff: () -> Void
    var body: some View {
        ComposerModeChip(title: "Plan", systemImage: "list.bullet.clipboard", label: "Plan mode on",
                         hint: "Turns plan mode off", id: "plan-mode-chip", isDisabled: isDisabled, turnOff: turnOff)
    }
}

/// A choice the next message goes with (Plan mode, Start in T3 Code), shown
/// above the draft; tapping turns it off.
struct ComposerModeChip: View {
    @Environment(\.wonderTheme) private var theme
    let title: String
    let systemImage: String
    let label: String
    let hint: String
    let id: String
    var isDisabled = false
    let turnOff: () -> Void
    var body: some View {
        Button(action: turnOff) {
            HStack(spacing: 4) {
                Image(systemName: systemImage).imageScale(.small)
                Text(title)
                Image(systemName: "xmark").font(.caption2.weight(.bold))
            }
            .font(.subheadline.weight(.medium))
            .foregroundStyle(theme.accent)
            .padding(.horizontal, 10).frame(minHeight: 28)
            .background(theme.accent.opacity(0.12), in: Capsule())
            .padding(.vertical, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(isDisabled)
        .accessibilityLabel(label)
        .accessibilityHint(hint)
        .accessibilityIdentifier(id)
    }
}

/// Message and sidebar chrome share the palette's secondary text. SwiftUI's
/// default hierarchical secondary style can wash out on a colored page.
struct ConversationTextColors: ViewModifier {
    @Environment(\.wonderTheme) private var theme
    func body(content: Content) -> some View {
        content.foregroundStyle(theme.text, theme.isDefault ? Color.secondary : theme.secondaryText)
    }
}

/// The shield menu for a project thread or draft. Rows and labels follow the
/// provider; `choose` receives the row the owner picked.
struct ProjectAccessMenu: View {
    let family: AgentFamily
    let capabilities: ProviderCapabilities
    let supportsModes: Bool
    let access: ProjectAccess
    var isDisabled = false
    let identifier: String
    /// False for a row that cannot be chosen yet: an approval mode of the
    /// harness the next message moves the thread to.
    var isAvailable: (ProjectAccessChoice) -> Bool = { _ in true }
    let choose: (ProjectAccessChoice) -> Void
    var body: some View {
        let selected = ProjectAccessChoice.selected(for: access, capabilities: capabilities, supportsModes: supportsModes)
        Menu {
            ForEach(ProjectAccessChoice.choices(capabilities: capabilities, supportsModes: supportsModes, current: access)) { choice in
                if choice == .providerSettings {
                    Text(choice.title(for: family, supportsModes: supportsModes))
                } else {
                    row(choice, isSelected: choice == selected)
                }
            }
            if supportsModes, capabilities.plan == .toggle {
                Section {
                    row(.plan, isSelected: access.planMode)
                }
            }
        } label: {
            ComposerPermissionsLabel()
                .foregroundStyle(selected.isElevated ? Color.orange : Color.primary)
                .contentShape(Rectangle())
        }
        .menuOrder(.fixed)
        .disabled(isDisabled)
        .accessibilityLabel("Permissions")
        .accessibilityValue(selected.title(for: family, supportsModes: supportsModes))
        .accessibilityIdentifier(identifier)
    }
    private func row(_ choice: ProjectAccessChoice, isSelected: Bool) -> some View {
        let available = isAvailable(choice)
        return Button { choose(choice) } label: {
            Text(choice.title(for: family, supportsModes: supportsModes))
            Text(available ? choice.detail(for: family) : "Available after your next message")
            if isSelected { Image(systemName: "checkmark") }
        }
        .disabled(!available)
        .tint(choice.isElevated ? Color.orange : nil)
        .accessibilityIdentifier("access-choice-" + choice.rawValue)
    }
}

/// Shared by native and T3-held threads, as well as New chat.
/// The menu owns its full accessibility name and current permission value.
struct ComposerPermissionsLabel: View {
    var body: some View {
        Image(systemName: "shield").font(.system(size: 16))
            .frame(width: 44, height: 44)
            .accessibilityLabel("Permissions")
            .help("Permissions")
    }
}

// MARK: - Conversation chrome

/// Keep the current step in the existing subtitle while work runs. The
/// Project and provider remain named for VoiceOver and in conversation details.
struct ProjectConversationHeader: View {
    let detail: ProjectConversationDetail
    var currentStep: String? = nil
    /// The computer the thread runs on. VoiceOver always names it; the line
    /// under the title only when more than one computer is paired.
    var computer: String? = nil
    var pairedComputers = 1
    /// A thread on the demo computer, badged so it never passes for a real one.
    var isDemo = false
    /// This chat's notification choice; Follow and Mute show a small bell.
    var notification: ChatNotificationLevel = .default
    var notificationSaving = false
    /// The room the navigation bar leaves between its buttons (`BarGeometry`).
    var room: CGFloat = .infinity
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    var body: some View {
        // One layout whether or not work runs: the title keeps its font and
        // width, and the line under it swaps the Project for the running step,
        // which cannot then push a long target into the toolbar buttons.
        VStack(spacing: 1) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if isDemo { DemoBadge() }
                Text(detail.title).font(.headline).lineLimit(1)
                if notificationSaving {
                    ProgressView().controlSize(.mini).fixedSize().layoutPriority(1)
                } else if let symbol = notification.badgeSymbolName {
                    Image(systemName: symbol).font(.caption2).foregroundStyle(.secondary)
                        .fixedSize().layoutPriority(1)
                        .accessibilityIdentifier("header-notification-" + notification.rawValue)
                }
            }
            if currentStep != nil || detail.worktreeLabel != nil || !dynamicTypeSize.isAccessibilitySize {
                caption
            }
        }
        // Room for a long thread name between an iPad's toolbar buttons.
        .frame(maxWidth: min(room, horizontalSizeClass == .regular ? 420 : 240))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel((isDemo ? "Demo, " : "") + "\(detail.title), \(detail.projectName) project, \(detail.family.title)"
                            + (detail.worktreeLabel.map { ", worktree \($0)" } ?? "")
                            + (computer.map { ", on \($0)" } ?? "")
                            + (notificationSaving ? ", saving notification choice" : notification.badgeLabel.map { ", " + $0.lowercased() } ?? ""))
        .accessibilityValue(currentStep ?? "")
        .accessibilityIdentifier("project-conversation-header")
    }
}

/// Reports a navigation bar item's horizontal extent in its bar's coordinates.
/// SwiftUI resolves no coordinate space of the screen inside a toolbar item, and
/// window coordinates move every frame while the phone sidebar drags the
/// conversation aside; the bar's own coordinates only change when the bar lays
/// its items out again. Reports only changes, after layout.
struct NavigationBarItemExtent: ViewModifier {
    let report: (ClosedRange<CGFloat>?) -> Void
    @State private var reader = Reader()

    func body(content: Content) -> some View {
        content
            .background { Probe(reader: reader).accessibilityHidden(true) }
            // A move in the window can be the bar laying out again; re-read.
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { _ in reader.read() }
            .onAppear { reader.report = report }
            .onDisappear { reader.report = nil; report(nil) }
    }

    @MainActor final class Reader {
        weak var view: UIView?
        var report: ((ClosedRange<CGFloat>?) -> Void)?
        private var last: ClosedRange<CGFloat>?

        func read() {
            guard let view else { return }
            var ancestor = view.superview
            while let next = ancestor, !(next is UINavigationBar) { ancestor = next.superview }
            guard let bar = ancestor else { return }
            let frame = view.convert(view.bounds, to: bar)
            let extent = frame.minX...max(frame.minX, frame.maxX)
            guard extent != last else { return }
            last = extent
            // Not from inside layout: the change lands on the next turn.
            Task { @MainActor [weak self] in self?.report?(extent) }
        }
    }

    private struct Probe: UIViewRepresentable {
        let reader: Reader
        func makeUIView(context: Context) -> ProbeView {
            let view = ProbeView()
            view.reader = reader
            reader.view = view
            return view
        }
        func updateUIView(_ view: ProbeView, context: Context) {
            view.reader = reader
            reader.view = view
        }
    }

    final class ProbeView: UIView {
        weak var reader: Reader?
        override init(frame: CGRect) {
            super.init(frame: frame)
            isUserInteractionEnabled = false
            isAccessibilityElement = false
        }
        required init?(coder: NSCoder) { nil }
        override func didMoveToWindow() { super.didMoveToWindow(); reader?.read() }
        override func layoutSubviews() { super.layoutSubviews(); reader?.read() }
    }
}

extension ProjectConversationHeader {
    /// The navigation bar's extent and its trailing button, in the bar's
    /// coordinates. The bar centres the title without shrinking it to the room
    /// between its buttons, so a long thread name drew under the sidebar and
    /// context buttons (measured on the unfolded Duo, whose vertical system bar
    /// also ends the bar's trailing side early). Only items the title does not
    /// move are measured, so its width never feeds back into its own layout.
    struct BarGeometry: Equatable {
        /// The conversation's width; the bar spans it.
        var bar: ClosedRange<CGFloat>?
        /// The context button, when the bar shows it: the bar ends one margin after it.
        var trailingItem: ClosedRange<CGFloat>?

        /// Kept clear at the leading end for a button: a 40–44 pt glass button
        /// inside the bar's 16–24 pt margin, plus a gap.
        static let sideClearance: CGFloat = 72
        static let barMargin: CGFloat = 20
        static let itemGap: CGFloat = 8
        /// What the title menu adds around the header: its chevron, spacing and
        /// the button's padding (measured: 26 pt on the unfolded Duo).
        static let menuChrome: CGFloat = 28

        /// The widest the centred title (with its menu chevron) may be so it
        /// truncates between the leading and trailing buttons. Unbounded until
        /// the bar has been measured.
        var titleRoom: CGFloat {
            guard let bar, bar.upperBound > bar.lowerBound else { return .infinity }
            let end = trailingItem.map { $0.upperBound + Self.barMargin } ?? bar.upperBound
            let center = (bar.lowerBound + end) / 2
            let leading = bar.lowerBound + Self.sideClearance
            let trailing = trailingItem.map { $0.lowerBound - Self.itemGap } ?? end - Self.sideClearance
            return max(0, trailing - leading)
        }
    }

    /// One way to write the line under the title: the Project, its branch in
    /// the accent colour with no separator, and the computer when it is named.
    struct Caption: Hashable {
        var text: String
        var branch: String? = nil
        var computer: String? = nil
    }

    /// The line under the title: the running step, or the Project, branch and
    /// computer, dropping the computer when it does not fit.
    @ViewBuilder private var caption: some View {
        let lines = Self.captions(project: detail.projectName, branch: detail.worktreeLabel, step: currentStep,
                                  computer: computer, pairedComputers: pairedComputers)
        // At most two lines, built here rather than in a ForEach: ViewThatFits can ask a ForEach for its
        // rows off the main thread, and this view's row closure is MainActor-isolated (a SIGTRAP crash).
        let texts = lines.map { text($0) }
        let last = texts.last ?? text(Caption(text: detail.projectName))
        ViewThatFits(in: .horizontal) {
            if let first = texts.first { first.lineLimit(1).fixedSize() }
            if texts.count > 1 { texts[1].lineLimit(1).fixedSize() }
            last.lineLimit(1)
        }
    }

    private func text(_ line: Caption) -> Text {
        let branch = line.branch.map { Text(" \($0)").foregroundStyle(theme.accent) } ?? Text("")
        let computer = line.computer.map { Text(" · \($0)") } ?? Text("")
        return Text("\(line.text)\(branch)\(computer)").font(.caption).foregroundStyle(theme.isDefault ? Color.secondary : theme.secondaryText)
    }

    /// Longest first: "Wonder agent/fix · MacBook" when several computers are
    /// paired, then without the computer. A running step keeps the branch in
    /// front of it.
    static func captions(project: String, branch: String?, step: String?, computer: String?, pairedComputers: Int) -> [Caption] {
        if let step {
            let text = branch.map { $0 + " · " + step } ?? step
            return [Caption(text: text)]
        }
        var lines = [Caption(text: project, branch: branch)]
        if let computer, pairedComputers > 1 { lines.insert(Caption(text: project, branch: branch, computer: computer), at: 0) }
        return lines
    }
}

/// Turns a project thread's plan mode on or off. Sending waits for the save,
/// and a failure is reported in the composer.
@MainActor @discardableResult func setPlanMode(_ enabled: Bool, model: ConnectionModel, library: ProjectLibrary, chat: ChatSummary) async -> Bool {
    guard library.supportsModes, !model.savingComposerSettings.contains(chat.id),
          !model.approvalSettingsBlockSending(chat.id), !model.accessEnded else { return false }
    let scope = model.assignmentScope
    model.savingComposerSettings.insert(chat.id); model.controlErrors[chat.id] = nil
    defer { if scope == model.assignmentScope { model.savingComposerSettings.remove(chat.id) } }
    do {
        try await library.updateConversation(chat.id, fields: ["planMode": enabled])
        return !Task.isCancelled && scope == model.assignmentScope && library.details[chat.id]?.planMode == enabled
    } catch {
        if scope == model.assignmentScope { model.controlErrors[chat.id] = "Plan mode wasn’t changed. " + managementError(error) }
        return false
    }
}

/// The model sheet's lists, shared by New chat and a thread's composer: the
/// harness first, then that harness's models, then their speed. `middle`
/// follows the harnesses (New chat's account); `trailing` ends the sheet.
struct HarnessModelPicker<Middle: View, Trailing: View>: View {
    let family: AgentFamily?
    /// Harnesses listed here: the computer offers them and did not find them
    /// uninstalled. The chosen harness is always listed.
    let offered: (AgentFamily) -> Bool
    /// Harnesses that can be chosen; the others are listed as unavailable.
    let canChoose: (AgentFamily) -> Bool
    /// The chosen harness's visible models, already narrowed by any search.
    let models: [AgentOptions.Model]
    /// False when the harness lists no models at all, not only none matching.
    let hasModels: Bool
    let query: String
    let selectedModel: String?
    let speeds: [AgentOptions.Choice]
    let speed: String
    let computerName: String
    /// Prefix of the rows' accessibility identifiers ("project", "new-chat").
    let identifier: String
    let chooseFamily: (AgentFamily) -> Void
    let chooseModel: (AgentOptions.Model) -> Void
    let chooseSpeed: (String) -> Void
    @ViewBuilder var middle: () -> Middle
    @ViewBuilder var trailing: () -> Trailing
    var body: some View {
        Section {
            ForEach(AgentFamily.allCases.filter { $0 == family || offered($0) }) { option in
                Button { chooseFamily(option) } label: {
                    HStack(spacing: 10) {
                        ProviderIcon(family: option, size: 22)
                        Text(canChoose(option) ? option.title : option.title + " (unavailable)")
                            .lineLimit(canChoose(option) ? 1 : nil).minimumScaleFactor(0.75)
                            .layoutPriority(1)
                        Spacer()
                        if option == family { Image(systemName: "checkmark").font(.system(size: 16, weight: .semibold)) }
                    }
                }
                .foregroundStyle(.primary)
                .disabled(!canChoose(option))
                .accessibilityIdentifier("\(identifier)-agent:\(option.rawValue)")
            }
        } header: {
            // The sheet's first element: the anchor a dismissing swipe starts from.
            Text("Agent").accessibilityIdentifier("sheet-top")
        }
        middle()
        Section("Model") {
            if !hasModels { Text("Models are unavailable. Check \(computerName).").foregroundStyle(.secondary) }
            else if models.isEmpty { Text("No models match “\(query)”.").foregroundStyle(.secondary) }
            ForEach(models) { option in
                Button { chooseModel(option) } label: {
                    HStack { Text(option.displayName); Spacer(); if option.id == selectedModel { Image(systemName: "checkmark").font(.system(size: 16, weight: .semibold)) } }
                }
                .foregroundStyle(.primary)
                .accessibilityIdentifier("\(identifier)-model:\(option.id)")
            }
        }
        if speeds.count > 1 {
            Section("Speed") {
                ForEach(speeds) { option in
                    Button { chooseSpeed(option.id) } label: {
                        HStack(alignment: .firstTextBaseline) {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(option.label)
                                    .lineLimit(1).minimumScaleFactor(0.75)
                                if let description = option.description {
                                    Text(description).font(.footnote).foregroundStyle(.secondary)
                                }
                            }
                            Spacer()
                            if option.id == speed { Image(systemName: "checkmark").font(.system(size: 16, weight: .semibold)) }
                        }
                    }
                    .foregroundStyle(.primary)
                    .accessibilityIdentifier("\(identifier)-speed:\(option.id)")
                }
            }
        }
        trailing()
    }
}

/// Model, effort and access for one project thread. The model sheet is New
/// chat's: the harness, then its models. A model of the thread's own harness
/// is saved on the thread; another harness's is carried by the next message,
/// and the Mac moves the thread to it when the message is delivered. The
/// access menu is already that harness's.
/// The conversation owns this presentation across compact/wide composer layouts.
struct ComposerModelPresentation {
    var showing = false
    var search = ""
    var failure: String?
}

struct ProjectComposerSettings<Input: View>: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    let chat: ChatSummary
    var placement: ComposerSettingsPlacement = .inline
    @Binding var presentation: ComposerModelPresentation
    @ViewBuilder var input: () -> Input
    private var detail: ProjectConversationDetail? { library.details[chat.id] }
    private var saving: Bool { model.savingComposerSettings.contains(chat.id) || model.approvalSettingsBlockSending(chat.id) }
    private var accessChange: ProjectAccessChange? { model.projectAccessChanges[chat.id] }
    private var displayedAccess: ProjectAccess {
        // A failed save shows the Mac's confirmed choice while Retry keeps the intent.
        if let change = accessChange, change.saving || !change.reconciled { return change.desired }
        return detail?.access ?? ProjectAccess()
    }
    private var pending: ProjectMessageModel? { model.projectNextModels[chat.id] }
    private var allModels: [AgentOptions.Model] { (library.options?.models ?? []).filter { !$0.hidden } }
    /// The provider the next message goes to.
    private var targetFamily: AgentFamily? { pending?.family ?? detail?.family }
    private var models: [AgentOptions.Model] { allModels.filter { $0.family == targetFamily } }
    private var searchingModels: Bool { models.count > ProjectModelPicker.searchThreshold }
    /// A thread that never chose a model uses its provider's default.
    private var selected: AgentOptions.Model? {
        guard let detail else { return nil }
        if let pending { return models.first { $0.id == pending.model } }
        if let stored = detail.model { return models.first { $0.id == stored } }
        return ModelDefaults.defaultModel(in: models)
    }
    private var effort: String? { selected.flatMap { ModelDefaults.effort(pending?.effort ?? detail?.effort, for: $0) } }
    private var serviceTier: String? { pending == nil ? detail?.serviceTier : pending?.serviceTier }
    private var title: String { ProjectModelPicker.title(model: selected, family: targetFamily) }
    /// A chosen speed other than standard, shown as a bolt beside the model.
    private var speed: AgentOptions.Choice? {
        selected?.serviceTiers?.first { $0.id == serviceTier && $0.id != "default" }
    }
    private var modelLabel: ComposerModelLabel { ComposerModelLabel(family: targetFamily, title: title, fast: speed != nil) }
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ComposerSettingsRow(placement: placement) {
                HStack(spacing: 0) {
                    accessMenu
                        .fixedSize(horizontal: true, vertical: false)
                    input()
                }
            } model: {
                Button { presentation.showing = true } label: { modelLabel }
                    .disabled(saving || detail == nil)
                    .accessibilityLabel("Model").accessibilityValue([title, speed?.label].compactMap { $0 }.joined(separator: ", "))
                    .accessibilityIdentifier("project-composer-model")
                    #if WONDER_DIAGNOSTICS
                    .onAppear {
                        if ProcessInfo.processInfo.arguments.contains("-diagnostics-project-model-open") { presentation.showing = true }
                    }
                    #endif
            } effort: {
                if let selected, !selected.reasoningEfforts.isEmpty {
                    ComposerEffortButton(levels: selected.reasoningEfforts, selected: effort,
                                         isDisabled: saving || detail == nil || model.accessEnded,
                                         identifier: "project-composer-effort") { level in
                        Task { await choose(selected, effort: level) }
                    }
                }
            }
            if placement != .choices, model.savingComposerSettings.contains(chat.id) { ProgressView("Saving…").font(.caption).padding(.horizontal, 12) }
            if placement != .choices, let failure = presentation.failure { FailureDetails("Settings not saved", message: failure).padding(.horizontal, 12) }
            if placement != .choices, let change = accessChange, let failure = change.failure {
                HStack {
                    Text(failure).font(.caption)
                    Button("Try again") { model.setProjectAccess(change.desired, conversationID: chat.id) }
                }.padding(.horizontal, 12)
                    .accessibilityIdentifier("project-access-error")
            }
        }
        .task(id: model.assignmentScope + ":" + String(model.macConnected == true)) { if placement != .choices { await library.loadOptions() } }
        // The choice ends once the Mac has moved the thread to it.
        .onChange(of: detail) { _, latest in
            if let pending, ProjectModelPicker.isApplied(pending, to: latest) { model.projectNextModels[chat.id] = nil }
        }
        .sheet(isPresented: Binding(get: { placement != .choices && presentation.showing }, set: { presentation.showing = $0 })) {
            NavigationStack {
                WonderForm {
                    // A harness the computer lists models for can take the next message.
                    let listed = { (family: AgentFamily) in allModels.contains { $0.family == family } }
                    HarnessModelPicker(
                        family: targetFamily, offered: { library.families.lists($0) || listed($0) },
                        canChoose: { $0 == detail?.family || listed($0) },
                        models: ProjectModelPicker.matching(models, query: searchingModels ? presentation.search : ""),
                        hasModels: !models.isEmpty, query: presentation.search, selectedModel: selected?.id,
                        speeds: selected?.serviceTiers ?? [], speed: serviceTier ?? "default",
                        computerName: model.macName, identifier: "project",
                        chooseFamily: chooseFamily,
                        chooseModel: { option in
                            Task {
                                await choose(option)
                                // A saved choice ends the sheet unless the model has speeds to choose next;
                                // a failed one stays to show why.
                                if presentation.failure == nil, (option.serviceTiers ?? []).count <= 1 { presentation.showing = false }
                            }
                        },
                        chooseSpeed: { tier in
                            guard let selected else { return }
                            Task { await chooseSpeed(tier, model: selected) }
                        },
                        middle: { EmptyView() },
                        trailing: { if let failure = presentation.failure { FailureDetails("Settings not saved", message: failure) } })
                }
                .disabled(saving || model.accessEnded)
                .pinnedSheetHeader("", search: searchingModels ? SheetSearch(text: $presentation.search, prompt: "Search models", identifier: "project-model-search") : nil) { presentation.showing = false }
            }.presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
        }
        .onChange(of: presentation.showing) { _, showing in if !showing { presentation.search = "" } }
    }
    /// The access menu of the harness the next message goes to. Before the
    /// thread moves, a choice saves only what the move keeps.
    @ViewBuilder private var accessMenu: some View {
        let current = detail?.family ?? .codex // provider identity: shown before the thread loads
        let family = targetFamily ?? current
        let capabilities = library.capabilities(family), modes = library.supportsModes
        let moving = family != current
        let shown = moving ? ProjectModelPicker.access(displayedAccess, movingTo: capabilities) : displayedAccess
        let permissions = detail?.t3CodePermissions
        if !model.nativeRunsWithoutT3Code.contains(chat.id), permissions != nil || model.nativeHolders[chat.id]?.app == "T3 Code" {
            Menu {
                Text((permissions?.title ?? "Mode unavailable") + " · T3 Code")
                Text("Change permissions in T3 Code on \(model.macName).")
            } label: {
                ComposerPermissionsLabel()
                    .foregroundStyle(permissions?.runtimeMode == "full-access" ? Color.orange : Color.primary)
                    .contentShape(Rectangle())
            }
            .menuOrder(.fixed)
            .accessibilityLabel("Permissions")
            .accessibilityValue((permissions?.title ?? "Mode unavailable") + " in T3 Code")
            .accessibilityIdentifier("project-composer-access")
        } else {
            ProjectAccessMenu(family: family, capabilities: capabilities, supportsModes: modes, access: shown,
                                 isDisabled: saving || detail == nil || model.accessEnded, identifier: "project-composer-access",
                                 isAvailable: { choice in
            !moving || ProjectModelPicker.canSaveBeforeMove(choice.result(from: shown, capabilities: capabilities, supportsModes: modes), shown: shown,
                                                            current: library.capabilities(current), target: capabilities, supportsModes: modes)
        }) { choice in
            guard detail != nil else { return }
            let next = choice.result(from: shown, capabilities: capabilities, supportsModes: modes)
            guard next != shown else { return }
            model.setProjectAccess(moving ? ProjectModelPicker.savedBeforeMove(next, thread: displayedAccess) : next, conversationID: chat.id)
        }
        }
    }
    /// Another harness carries the Project's last model for it, or its
    /// default, with the next message; the thread's own drops that choice.
    private func chooseFamily(_ family: AgentFamily) {
        guard let detail, family != targetFamily else { return }
        presentation.failure = nil; presentation.search = ""
        let project = library.projects.first(where: { $0.id == detail.projectId })
        model.projectNextModels[chat.id] = ProjectModelPicker.carried(choosing: family, thread: detail.family,
                                                                      models: allModels, remembered: project?.lastModel(for: family))
    }
    /// Picking a model starts from that model's default effort.
    private func choose(_ option: AgentOptions.Model, effort: String? = nil) async {
        guard let detail else { return }
        // Choosing the current model again keeps the effort already chosen.
        guard effort != nil || option.id != selected?.id || option.family != targetFamily else { return }
        let chosen = effort ?? ModelDefaults.effort(for: option)
        let carriedTier = serviceTier.flatMap { tier in
            (option.serviceTiers ?? []).contains(where: { $0.id == tier }) || option.defaultServiceTier == tier ? tier : nil
        }
        switch ProjectModelPicker.choice(current: detail.family, option: option, effort: chosen, serviceTier: option.family == targetFamily ? carriedTier : nil) {
        case .sendWithNextMessage(let carried):
            // Nothing changes on the Mac yet; the next message carries this choice.
            presentation.failure = nil
            model.projectNextModels[chat.id] = carried
        case .saveToThread:
            let fields: [String: Any] = ["model": option.id, "effort": chosen as Any? ?? NSNull(),
                                         "serviceTier": carriedTier as Any? ?? NSNull()]
            if await save(fields) {
                model.projectNextModels[chat.id] = nil
                // The Mac records this as the Project's last model too.
                library.recordModelChoice(projectID: detail.projectId, family: detail.family, model: option.id, effort: chosen)
            }
        }
    }
    private func chooseSpeed(_ tier: String, model selected: AgentOptions.Model) async {
        if var carried = pending {
            carried.serviceTier = tier
            model.projectNextModels[chat.id] = carried
            return
        }
        var fields: [String: Any] = ["serviceTier": tier]
        if detail?.model == nil {
            fields["model"] = selected.id
            fields["effort"] = effort as Any? ?? NSNull()
        }
        _ = await save(fields)
    }
    @discardableResult private func save(_ fields: [String: Any]) async -> Bool {
        guard !fields.isEmpty, !saving else { return false }
        let scope = model.assignmentScope
        model.savingComposerSettings.insert(chat.id); presentation.failure = nil
        defer { if scope == model.assignmentScope { model.savingComposerSettings.remove(chat.id) } }
        do { try await library.updateConversation(chat.id, fields: fields); return true }
        catch { presentation.failure = managementError(error); return false }
    }
}

/// The thread name typed in Thread details. The conversation owns it, so it
/// outlives the details view: closing the pane or switching tools saves it
/// first and stays on details when the name is refused or the save fails.
/// Observable, so only the details controls that read it redraw as you type,
/// not the conversation that owns it.
@MainActor @Observable final class ThreadTitleDraft {
    enum Problem: Equatable {
        case blank, tooLong
        var message: String {
            switch self {
            case .blank: "Enter a name for this chat."
            case .tooLong: "Use a shorter name."
            }
        }
    }
    /// The computer's limit on a thread name, in UTF-8 bytes.
    static let maxBytes = 400
    static func problem(_ name: String) -> Problem? {
        if name.isEmpty { return .blank }
        if name.utf8.count > maxBytes { return .tooLong }
        return nil
    }
    static func trimmed(_ text: String) -> String { text.trimmingCharacters(in: .whitespacesAndNewlines) }

    private(set) var conversationID: String?
    /// What the field shows.
    var text = ""
    /// The name the computer last confirmed.
    private(set) var saved: String?
    private(set) var saving = false
    private(set) var failure: String?

    var hasUnsavedChange: Bool { saved != nil && Self.trimmed(text) != saved }
    /// Shown under the field once the name was changed.
    var problem: Problem? { hasUnsavedChange ? Self.problem(Self.trimmed(text)) : nil }

    /// Details opened or the computer answered with this name. A change typed
    /// for the same thread stays; another thread starts from its own name.
    func show(_ conversationID: String, title: String) {
        if conversationID != self.conversationID {
            self.conversationID = conversationID
            text = title; failure = nil
        } else if !hasUnsavedChange {
            text = title
        }
        saved = title
    }

    func discard() {
        if let saved { text = saved }
        failure = nil
    }

    /// Leaving details (closing the pane, escape, switching tools): a changed
    /// name is saved first, as Done does. `leave` runs once nothing is left
    /// unsaved and details are still showing; a refused name or a failed save
    /// keeps details with the typed name and the reason.
    func leave(_ library: ProjectLibrary, showing: () -> Bool, then leave: () -> Void) async {
        guard showing(), hasUnsavedChange else { leave(); return }
        // Another chat or tool replaced details meanwhile: nothing left to leave.
        guard await commit(library), showing() else { return }
        leave()
    }

    /// Saves a changed name. True when nothing is left unsaved; false keeps
    /// the typed name with its problem or the save's failure.
    func commit(_ library: ProjectLibrary) async -> Bool {
        guard hasUnsavedChange else { return true }
        guard let conversationID, !saving else { return false }
        let name = Self.trimmed(text)
        guard Self.problem(name) == nil else { return false }
        let typed = text
        saving = true; failure = nil
        defer { saving = false }
        do {
            try await library.updateConversation(conversationID, fields: ["title": name])
            guard conversationID == self.conversationID else { return false }
            saved = name
            if text == typed { text = name }
            return true
        } catch is CancellationError {
            return false
        } catch {
            if conversationID == self.conversationID { failure = managementError(error) }
            return false
        }
    }
}

/// Details for a project thread, including exact-ID continuation on the Mac.
struct ProjectConversationDetailsView: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    let chat: ChatSummary
    /// Owned by the conversation, so a typed name survives switching tools.
    @Bindable var title: ThreadTitleDraft
    private let dismiss = ToolDismiss()
    @Environment(\.companionPane) private var pane
    @State private var verticalBar = false
    private var inlineHeader: Bool { CompanionPaneChrome.usesInlineHeader(pane, verticalBar: verticalBar) }
    @State private var continuation: DesktopContinuation?
    @State private var continuationFailure: String?
    @State private var copied = false
    @State private var editingName = false
    private var detail: ProjectConversationDetail? { library.details[chat.id] }
    var body: some View {
        NavigationStack {
            WonderForm {
                if let detail {
                    Section {
                        if editingName || title.hasUnsavedChange {
                            TextField("Chat name", text: $title.text).submitLabel(.done)
                                .onSubmit { Task { if await title.commit(library) { editingName = false } } }
                                .accessibilityIdentifier("thread-details-name")
                        } else {
                            Button("Rename chat", systemImage: "pencil") { editingName = true }
                                .accessibilityIdentifier("thread-details-rename")
                        }
                        if let problem = title.problem {
                            Text(problem.message).font(.footnote).foregroundStyle(.red)
                                .accessibilityIdentifier("thread-details-name-problem")
                        }
                        if let failure = title.failure { FailureDetails("The name wasn’t saved", message: failure) }
                    }
                    permissionDetails(detail)
                    Section {
                        LabeledContent("Project", value: detail.projectName)
                        LabeledContent("Agent") {
                            HStack(spacing: 6) { ProviderIcon(family: detail.family, size: 16); Text(detail.family.title) }
                        }
                        VStack(alignment: .leading, spacing: 4) {
                            Text(inT3Code(detail) ? "Worktree" : "Working folder").font(.subheadline)
                            Text(detail.workingFolder).font(.footnote.monospaced()).foregroundStyle(.secondary)
                                .lineLimit(2).truncationMode(.middle).textSelection(.enabled)
                        }
                        if !detail.folderInProject {
                            Text("This folder is no longer part of the Project. Add it back in Project settings to continue here.")
                                .font(.footnote).foregroundStyle(.secondary)
                        }
                        if let notice = detail.notice { Text(notice).font(.footnote).foregroundStyle(.secondary) }
                        if let reason = continuation?.unavailableText ?? continuationFailure {
                            Text(reason).font(.footnote).foregroundStyle(.secondary)
                                .accessibilityIdentifier("continuation-unavailable")
                        }
                    }
                    if let option = continuation?.options.first {
                        Section("Continue on computer") {
                            HStack(spacing: 12) {
                                Text(option.command).font(.footnote.monospaced()).lineLimit(1).truncationMode(.middle)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                Button(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc") {
                                    UIPasteboard.general.string = option.command
                                    copied = true
                                }
                                .buttonStyle(.borderless).labelStyle(.titleAndIcon).font(.subheadline)
                                .accessibilityIdentifier("copy-continuation-" + option.id)
                            }
                            .task(id: copied) {
                                guard copied else { return }
                                try? await Task.sleep(for: .seconds(2))
                                if !Task.isCancelled { copied = false }
                            }
                        }
                    }
                } else {
                    ProgressView("Loading chat…")
                }
            }
            .disabled(title.saving || model.accessEnded)
            .navigationTitle("Chat details").navigationBarTitleDisplayMode(.inline)
            // A swipe would drop a changed name; Save commits it and Cancel discards it.
            .interactiveDismissDisabled(title.hasUnsavedChange)
            .toolbar {
                if !inlineHeader {
                    if title.hasUnsavedChange {
                        ToolbarItem(placement: .cancellationAction) { cancelButton }
                    }
                    ToolbarItemGroup(placement: .topBarTrailing) {
                        doneButton
                        CompanionExpandButton()
                    }
                }
            }
            .modifier(CompanionPaneNavigationHeader(enabled: inlineHeader, title: "Chat details") {
                if title.hasUnsavedChange { cancelButton }
                doneButton
            })
            .onChange(of: detail?.title) { _, saved in
                if let saved { title.show(chat.id, title: saved) }
            }
            .task {
                _ = try? await library.loadDetail(chat.id)
                title.show(chat.id, title: detail?.title ?? chat.title)
                do { continuation = try await library.continuation(chat.id) }
                catch PairingFailure.response(409) { continuationFailure = "Send the first message before continuing on your computer." }
                catch { continuationFailure = "Continuation details are unavailable. Check \(model.macName)." }
            }
        }
        .readsVerticalBar($verticalBar)
    }

    private func inT3Code(_ detail: ProjectConversationDetail) -> Bool {
        !model.nativeRunsWithoutT3Code.contains(chat.id) && (detail.t3CodePermissions != nil || model.nativeHolders[chat.id]?.app == "T3 Code")
    }

    private func permissionDetails(_ detail: ProjectConversationDetail) -> some View {
        let t3 = detail.t3CodePermissions
        let inT3 = inT3Code(detail)
        let choice = ProjectAccessChoice.selected(for: detail.access, capabilities: library.capabilities(detail.family), supportsModes: library.supportsModes)
        let ownDetail = choice == .providerSettings ? "Uses \(detail.family.title)’s permissions on \(model.macName)." : choice.detail(for: detail.family)
        let ownLocation = model.nativeHolders[chat.id].map {
            "Approve permission requests in \($0.app) on \(model.macName)."
        } ?? "Answer permission requests here in Cupola."
        return Section("Permissions") {
            VStack(alignment: .leading, spacing: 4) {
                Text(inT3 ? (t3?.title ?? "Mode unavailable") + " · T3 Code" : choice.title(for: detail.family, supportsModes: library.supportsModes))
                    .font(.subheadline.weight(.medium))
                    .accessibilityIdentifier("thread-permission-mode")
                Text(inT3 ? t3?.detail ?? "Check this chat’s permission mode in T3 Code on \(model.macName)." : ownDetail)
                    .font(.footnote).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("thread-permission-description")
                Text(inT3 ? T3CodePermissions.approvalLocation(computer: model.macName) : ownLocation)
                    .font(.footnote).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("thread-approval-location")
            }
        }
    }

    private var cancelButton: some View {
        Button { title.discard(); dismiss() } label: {
            Text("Cancel").frame(minWidth: inlineHeader ? 44 : nil, minHeight: inlineHeader ? 44 : nil)
                .contentShape(Rectangle())
        }.disabled(title.saving)
            .accessibilityIdentifier("thread-details-cancel")
    }

    @ViewBuilder private var doneButton: some View {
        if title.hasUnsavedChange {
            detailsCommitButton
        } else {
            ToolCloseControl { detailsCommitButton }
        }
    }

    private var detailsCommitButton: some View {
        Button {
            Task {
                if await title.commit(library) { dismiss() }
            }
        } label: {
            Text(title.hasUnsavedChange ? "Save" : "Close").frame(minWidth: inlineHeader ? 44 : nil, minHeight: inlineHeader ? 44 : nil)
                .contentShape(Rectangle())
        }.disabled(title.saving)
            .accessibilityIdentifier("thread-details-done")
    }
}

// MARK: - Archived threads

/// A Project's archived threads with Restore. Codex threads are archived in
/// Codex itself (from Wonder or the Codex app); Claude Code threads are hidden
/// in Wonder only, and their sessions stay in Claude Code on the Mac.
struct ArchivedThreadsView: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    let projectID: String
    let projectName: String
    /// A page in the chat column, which owns the navigation stack and Back.
    var embedded = false
    @Environment(\.dismiss) private var dismiss
    @State private var page: ArchivedProjectThreadsPage?
    @State private var loadFailure: String?
    @State private var restoring: String?
    @State private var failure: String?

    var body: some View {
        if embedded { list } else { NavigationStack { list.toolbar { Button("Done") { dismiss() }.accessibilityIdentifier("archived-threads-done") } } }
    }

    private var list: some View {
            WonderList {
                if let page {
                    if page.threads.isEmpty {
                        Text("No archived chats in \(projectName).").foregroundStyle(.secondary)
                            .accessibilityIdentifier("archived-threads-empty")
                    }
                    ForEach(page.threads) { thread in row(thread) }
                    if !page.partial.isEmpty {
                        Text(page.partial.map(\.detail).joined(separator: " "))
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    Section {
                    } footer: {
                        Text(footer(page))
                    }
                } else if computerIsOffline(model) {
                    ComputerOfflineRow(name: model.macName)
                } else if let loadFailure {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(loadFailure).foregroundStyle(.secondary)
                        Button("Try again") { Task { await load() } }
                            .accessibilityIdentifier("archived-threads-retry")
                    }
                } else {
                    ProgressView("Loading archived chats…").frame(maxWidth: .infinity)
                }
                if let failure { FailureDetails(message: failure) }
            }
            .navigationTitle("Archived chats").navigationBarTitleDisplayMode(.inline)
            .task { await load() }
            .onChange(of: model.macConnected) { _, connected in
                if connected == true, page == nil { Task { await load() } }
            }
            .refreshable { await load() }
    }

    private func row(_ thread: ProjectThreadSummary) -> some View {
        HStack(spacing: 10) {
            ProviderIcon(family: thread.family, size: 18)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(thread.title).lineLimit(2)
                Text(archivedWhere(thread))
                    .font(.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("archived-where:" + thread.reference)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if restoring == thread.reference {
                ProgressView().accessibilityLabel("Restoring")
            } else {
                Button(restoreLabel(thread)) { restore(thread) }
                    .buttonStyle(.bordered)
                    .disabled(restoring != nil || model.macConnected != true)
                    .accessibilityLabel(restoreLabel(thread) + ": " + thread.title)
                    .accessibilityIdentifier("restore-thread:" + thread.reference)
            }
        }
        .frame(minHeight: 44)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("archived-thread:" + thread.reference)
    }

    /// What Restore does, for the kinds of thread listed.
    private func footer(_ page: ArchivedProjectThreadsPage) -> String {
        var lines = ["Restoring a Codex chat restores it in Codex too. Claude Code chats are archived in Cupola only; Claude Code on your computer still shows them."]
        if page.threads.contains(where: { $0.reopensThere == true }) {
            lines.append("Chats settled, snoozed or archived in T3 Code reopen there too when \(model.macName) is connected to T3 Code; otherwise they show again in Cupola only.")
        }
        if page.threads.contains(where: { $0.closedIn != nil && $0.reopensThere != true }) {
            lines.append("Chats archived in the Claude app or deleted in T3 Code show again in Cupola only; that app keeps them closed.")
        }
        return lines.joined(separator: " ")
    }

    /// A thread its own app keeps closed (archived in the Claude app, deleted in
    /// T3 Code) only shows again in Cupola, and the button says so.
    static func restoreLabel(closedIn: String?, reopensThere: Bool?) -> String {
        closedIn != nil && reopensThere != true ? "Show in Cupola" : "Restore"
    }
    private func restoreLabel(_ thread: ProjectThreadSummary) -> String {
        Self.restoreLabel(closedIn: thread.closedIn, reopensThere: thread.reopensThere)
    }

    private func archivedWhere(_ thread: ProjectThreadSummary) -> String {
        if let closed = thread.closedLabel() { return closed }
        return library.capabilities(thread.family).archive == .provider ? "Archived in \(thread.family.title)" : "Archived in Cupola"
    }

    private func load() async {
        loadFailure = nil
        do { page = try await library.archivedThreads(projectID) }
        catch is CancellationError {}
        catch { if page == nil { loadFailure = "Archived chats couldn’t be loaded. Check \(model.macName) and try again." } }
    }

    private func restore(_ thread: ProjectThreadSummary) {
        restoring = thread.reference; failure = nil
        Task {
            defer { restoring = nil }
            do {
                try await library.unarchive(projectID, thread: thread)
                if let current = page {
                    page = ArchivedProjectThreadsPage(threads: current.threads.filter { $0.reference != thread.reference },
                                                      partial: current.partial)
                }
            } catch is CancellationError {
            } catch {
                failure = "The chat couldn’t be restored. " + managementError(error)
            }
        }
    }
}

// MARK: - Project library

/// An Existing folder create whose answer must be known before its form is
/// editable again. A create the computer didn't confirm may still have made the
/// Project, so its request is kept and reused rather than forgotten; a definite
/// refusal made nothing and frees the form.
@MainActor enum PendingProjectCreate {
    /// The form's unanswered create: its request ID and input.
    static let formKey = "project.new"
    /// Creates set aside with Start over, input to request ID, so entering the
    /// same Project again resends that request instead of making a second one.
    static let setAsideKey = "project.new.set-aside"
    static let setAsideLimit = 8

    struct Input: Equatable {
        var name: String
        var folders: [String]
        var primaryIndex: Int
    }
    enum Outcome {
        case created(ProjectSummary)
        /// The computer made nothing; the form can change and try again.
        case refused(String)
        /// Kept for Retry or Start over.
        case unconfirmed(String)
    }

    static func payload(_ input: Input) -> String {
        let value: [String: Any] = ["name": input.name, "folders": input.folders, "primaryIndex": input.primaryIndex]
        return (try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])).map { String(decoding: $0, as: UTF8.self) } ?? ""
    }
    static func input(payload: String) -> Input? {
        guard let data = payload.data(using: .utf8),
              let value = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let name = value["name"] as? String, let folders = value["folders"] as? [String] else { return nil }
        return Input(name: name, folders: folders, primaryIndex: value["primaryIndex"] as? Int ?? 0)
    }
    static func pending(_ drafts: ManagementDraftStore) -> Input? {
        drafts.load(formKey)?.values["payload"].flatMap(input(payload:))
    }

    /// The computer answered and made nothing for this request. Anything else
    /// (no answer, a dropped connection, a changed pairing) may have created it.
    static func isRefusal(_ error: Error) -> Bool {
        switch error {
        case PairingFailure.response(let status), PairingFailure.hostMessage(let status, _): return [400, 409, 422].contains(status)
        default: return false
        }
    }

    static func create(_ input: Input, library: ProjectLibrary, drafts: ManagementDraftStore) async -> Outcome {
        let payload = Self.payload(input)
        var draft = drafts.load(formKey) ?? ManagementDraft()
        if draft.values["payload"].flatMap(Self.input(payload:)) != input {
            draft = ManagementDraft()
            if let earlier = drafts.load(setAsideKey)?.values[payload] { draft.requestId = earlier }
        }
        draft.values["payload"] = payload
        do { try drafts.save(draft, key: formKey) } catch { return .refused("Your choices couldn’t be saved on this device. Try again.") }
        do {
            let project = try await library.createProject(requestID: draft.requestId, name: input.name,
                                                          folders: input.folders, primaryIndex: input.primaryIndex)
            settle(payload, drafts: drafts)
            return .created(project)
        } catch let error where isRefusal(error) {
            settle(payload, drafts: drafts)
            switch error {
            case PairingFailure.hostMessage(409, _), PairingFailure.response(409):
                // The request already made a Project from an earlier try.
                await library.refresh()
                return .refused("This Project may already have been added on an earlier try. Check the sidebar before adding it again.")
            case PairingFailure.hostMessage(_, let message): return .refused(message)
            case PairingFailure.response(422):
                return .refused("One of these folders can’t be used. Choose existing folders that don’t contain protected computer settings.")
            default: return .refused(managementError(error))
            }
        } catch {
            return .unconfirmed(unconfirmedMessage(error))
        }
    }

    static func unconfirmedMessage(_ error: Error?) -> String {
        if error is URLError { return "Your computer can’t be reached, so this Project wasn’t confirmed. Retry once it’s connected, or start over." }
        return "Your computer didn’t confirm this Project. Retry to finish adding it, or start over."
    }

    /// Start over: the form is free again while its request stays known, so a
    /// Project the computer did make is not added twice.
    static func setAside(_ drafts: ManagementDraftStore) {
        guard let draft = drafts.load(formKey) else { return }
        if let payload = draft.values["payload"] {
            var aside = drafts.load(setAsideKey) ?? ManagementDraft()
            aside.values[payload] = draft.requestId
            // Bounded; dropping one only risks a duplicate Project, never a lost one.
            if aside.values.count > setAsideLimit, let drop = aside.values.keys.first(where: { $0 != payload }) { aside.values[drop] = nil }
            try? drafts.save(aside, key: setAsideKey)
        }
        drafts.remove(formKey)
    }

    /// The pending create already shows in the computer's Projects.
    static func reconciled(_ input: Input, in projects: [ProjectSummary]) -> ProjectSummary? {
        projects.first { $0.name == input.name && Set($0.folders.map(\.path)) == Set(input.folders) }
    }

    /// Forgets an answered create, and anything set aside with the same input.
    static func settle(_ payload: String, drafts: ManagementDraftStore) {
        drafts.remove(formKey)
        if var aside = drafts.load(setAsideKey), aside.values[payload] != nil {
            aside.values[payload] = nil
            if aside.values.isEmpty { drafts.remove(setAsideKey) } else { try? drafts.save(aside, key: setAsideKey) }
        }
    }
}

/// Create or edit a named group of folders on one Mac. Saving is metadata only.
struct ProjectEditorView: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    let project: ProjectSummary?
    /// Pushed inside Add Project or the chat column, which own the navigation stack and dismissal.
    var embedded = false
    var onSaved: (ProjectSummary) -> Void = { _ in }
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var folders: [String] = []
    @State private var primary = 0
    @State private var browsing = false
    @State private var saving = false
    @State private var failure: String?
    /// A create the computer didn't confirm: its form waits for Retry or Start over.
    @State private var unconfirmed = false
    /// A pending create found among the computer's Projects.
    @State private var notice: String?
    private var frozen: Bool { project == nil && unconfirmed }
    @State private var nameEdited = false
    /// This Project's own auto-archive rule; nil follows the computer's.
    @State private var autoArchive: AutoArchiveRule?
    private var changed: Bool {
        guard let project else { return true }
        return name != project.name || folders != project.folders.map(\.path)
            || primary != (project.folders.firstIndex(where: \.isPrimary) ?? 0)
            || autoArchive != project.autoArchive
    }
    private var canSave: Bool {
        !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && name.count <= 120 && !folders.isEmpty && folders.count <= 16 && changed && !saving && !model.accessEnded
    }
    var body: some View {
        if embedded { form } else { NavigationStack { form } }
    }
    private var form: some View {
            WonderForm {
                if project != nil, computerIsOffline(model) {
                    Section { ComputerOfflineRow(name: model.macName) }
                }
                Group { fields }.disabled(saving || frozen)
                if let notice {
                    Section { Text(notice).foregroundStyle(.secondary).accessibilityIdentifier("project-create-notice") }
                }
                if let failure, !computerIsOffline(model) { FailureDetails(message: failure) }
                if frozen {
                    Section {
                        Button("Start over") { startOver() }.disabled(saving).accessibilityIdentifier("project-start-over")
                    } footer: {
                        Text("If your computer did add this Project, it appears in the sidebar and isn’t added a second time.")
                    }
                }
            }
            .navigationTitle(project == nil ? "Existing folder" : "Edit Project").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if !embedded { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
                ToolbarItem(placement: .confirmationAction) {
                    if saving { ProgressView() }
                    else { Button(frozen ? "Try again" : project == nil ? "Create" : "Save") { Task { await save() } }.disabled(!canSave).accessibilityIdentifier("project-save") }
                }
            }
            .sheet(isPresented: $browsing) {
                MacLocationBrowser(model: model, title: "Add folder", foldersOnly: true) { path, isDirectory in
                    guard isDirectory, !folders.contains(path) else { return }
                    folders.append(path)
                    if project == nil, !nameEdited || name.isEmpty, folders.count == 1 {
                        name = URL(fileURLWithPath: path).lastPathComponent
                        nameEdited = false
                    }
                }
            }
            .onAppear {
                guard folders.isEmpty else { return }
                guard let project else {
                    restorePending()
                    return
                }
                name = project.name
                folders = project.folders.map(\.path)
                primary = project.folders.firstIndex(where: \.isPrimary) ?? 0
                autoArchive = project.autoArchive
                nameEdited = true
            }
            .task(id: model.assignmentScope) {
                guard project != nil else { return }
                if library.autoArchive == nil || library.continuation == nil { await library.refreshWorktreeSettings() }
                if library.options == nil { await library.loadOptions() }
            }
            .task {
                // Adding an existing folder starts by choosing it.
                if embedded, project == nil, folders.isEmpty { browsing = true }
                // A pending create may have been made: read the computer's Projects first.
                if frozen {
                    await library.refresh()
                    guard !Task.isCancelled, frozen, !saving else { return }
                    reconcilePending()
                }
            }
    }
    @ViewBuilder private var fields: some View {
        Section {
            LabeledContent("Computer", value: model.macName)
            TextField("Project name", text: $name)
                .onChange(of: name) { _, _ in nameEdited = true }
                .accessibilityIdentifier("project-name")
        }
        Section {
            ForEach(Array(folders.enumerated()), id: \.element) { index, path in
                HStack(spacing: 12) {
                    Image(systemName: "folder").foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(URL(fileURLWithPath: path).lastPathComponent).lineLimit(1)
                        Text(path).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    }
                    Spacer()
                    if index == primary {
                        Text("Primary").font(.caption.weight(.medium)).foregroundStyle(.secondary)
                    } else {
                        Button("Make primary") { primary = index }.font(.caption).buttonStyle(.borderless)
                    }
                }
                .accessibilityElement(children: .combine)
                .accessibilityValue(index == primary ? "Primary folder" : "")
                .swipeActions {
                    Button("Remove", role: .destructive) { remove(at: index) }.disabled(folders.count == 1)
                }
            }
            Button("Add folder", systemImage: "folder.badge.plus") { browsing = true }
                .accessibilityIdentifier("project-add-folder")
        } header: { Text("Folders") } footer: {
            Text("New chats start in the primary folder. Other folders are available to the agent in this Project.")
        }
        // Hosts without auto-archive report no computer rule.
        if project != nil, let computer = library.autoArchive {
            Section {
                Toggle("Own rule for this Project", isOn: Binding(get: { autoArchive != nil }, set: {
                    autoArchive = $0 ? (autoArchive ?? computer) : nil
                }))
                .systemSwitch()
                .accessibilityIdentifier("project-auto-archive-own")
                if let rule = autoArchive {
                    AutoArchiveRuleFields(rule: rule, identifier: "project-auto-archive") { autoArchive = $0 }
                }
            } header: { Text("Auto-archive") } footer: {
                if autoArchive == nil {
                    Text("Follows \(model.macName): \(AutoArchiveRule.daysLabel(computer.idleDays).lowercased())\(computer.onMerge ? ", and when the pull request merges" : "").")
                }
            }
        }
    }
    /// Shows a create the computer didn't confirm, as it was sent.
    private func restorePending() {
        guard let drafts = model.managementDrafts, let input = PendingProjectCreate.pending(drafts) else { return }
        name = input.name; folders = input.folders; primary = input.primaryIndex
        nameEdited = true; unconfirmed = true
        failure = PendingProjectCreate.unconfirmedMessage(nil)
        reconcilePending()
    }
    /// The pending create is among the computer's Projects: it was made.
    private func reconcilePending() {
        guard let drafts = model.managementDrafts, let input = PendingProjectCreate.pending(drafts),
              let made = PendingProjectCreate.reconciled(input, in: library.projects) else { return }
        PendingProjectCreate.settle(PendingProjectCreate.payload(input), drafts: drafts)
        unconfirmed = false; failure = nil
        name = ""; folders = []; primary = 0; nameEdited = false
        notice = "“\(made.name)” was added. Choose folders for another Project."
    }
    /// Frees the form; the unanswered request is kept in case it was made.
    private func startOver() {
        if let drafts = model.managementDrafts { PendingProjectCreate.setAside(drafts) }
        unconfirmed = false; failure = nil; notice = nil
        name = ""; folders = []; primary = 0; nameEdited = false
        browsing = true
    }
    private func remove(at index: Int) {
        folders.remove(at: index)
        if primary >= folders.count || primary == index { primary = 0 }
        else if primary > index { primary -= 1 }
    }
    private func save() async {
        saving = true; failure = nil
        defer { saving = false }
        do {
            let saved: ProjectSummary
            if let project {
                var fields: [String: Any] = ["name": name.trimmingCharacters(in: .whitespacesAndNewlines)]
                if folders != project.folders.map(\.path) || primary != (project.folders.firstIndex(where: \.isPrimary) ?? 0) {
                    fields["folders"] = folders; fields["primaryIndex"] = primary; fields["rootsRevision"] = project.rootsRevision
                }
                if autoArchive != project.autoArchive { fields["autoArchive"] = AutoArchiveRule.projectField(autoArchive) }
                saved = try await library.update(project.id, fields: fields)
            } else {
                guard let drafts = model.managementDrafts else { throw ReadFailure.resync }
                notice = nil
                let input = PendingProjectCreate.Input(name: name.trimmingCharacters(in: .whitespacesAndNewlines), folders: folders, primaryIndex: primary)
                switch await PendingProjectCreate.create(input, library: library, drafts: drafts) {
                case .created(let created): unconfirmed = false; saved = created
                case .refused(let message): unconfirmed = false; failure = message; return
                case .unconfirmed(let message): unconfirmed = true; failure = message; return
                }
            }
            onSaved(saved)
            if !embedded { dismiss() }
        } catch PairingFailure.response(409) {
            failure = "These folders changed on another device. Close and reopen the Project to see the latest folders."
        } catch PairingFailure.response(422) {
            failure = "One of these folders can’t be used. Choose existing folders that don’t contain protected computer settings."
        } catch { failure = managementError(error) }
    }
}

/// Included and hidden Projects of every paired computer, one section each
/// (no section titles with one computer). Hiding never deletes history.
struct ManageProjectsView: View {
    let models: [ConnectionModel]
    /// Inside Settings the view is a navigation destination, not a sheet.
    var embedded = false
    @Environment(\.dismiss) private var dismiss
    @State private var search = ""
    @State private var editing: ManageProjectsTarget?
    /// The Project a swipe asked to delete; the one confirmation lives here, outside the list rows, so a
    /// row redrawing under it (a refresh, the swipe closing) cannot drop it.
    @State private var deleting: ManageProjectsTarget?
    @State private var deleteFailure: String?
    /// Projects whose delete is running; the row shows progress and takes no second delete.
    @State private var deletingIDs: Set<String> = []
    /// Beside the Duo's vertical bar the cards run to the bar's margin, so the scroll indicator
    /// would cut along their edge; a trailing gutter keeps the cards clear and the indicator
    /// stays visible in it (iPhone and iPad keep it in the screen margin).
    @State private var verticalBar = false
    var body: some View {
        if embedded { content } else { NavigationStack { content.toolbar { Button("Done") { dismiss() } } } }
    }
    private var content: some View {
        WonderList {
            ForEach(Array(models.enumerated()), id: \.element.connection?.credential.hostInstallationId) { index, model in
                ManageProjectsSection(model: model, library: model.projects, search: search,
                                      title: models.count > 1 ? model.macName : nil,
                                      showsFooter: index == models.count - 1,
                                      deleting: deletingIDs,
                                      edit: { editing = ManageProjectsTarget(model: model, project: $0) },
                                      requestDelete: { deleting = ManageProjectsTarget(model: model, project: $0) })
            }
        }
        .contentMargins(.trailing, verticalBar ? 12 : 0, for: .scrollContent)
        .readsVerticalBar($verticalBar)
        .searchable(text: $search, prompt: "Search Projects")
        .navigationTitle("Projects").navigationBarTitleDisplayMode(.inline)
        .refreshable { await withTaskGroup(of: Void.self) { group in models.forEach { m in group.addTask { await m.projects.refresh() } } } }
        .navigationDestination(item: $editing) { target in
            ProjectEditorView(model: target.model, library: target.model.projects, project: target.project, embedded: true) { _ in editing = nil }
        }
        .confirmationDialog(deleting.map { "Delete “\($0.project.name)”?" } ?? "", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }),
                            titleVisibility: .visible, presenting: deleting) { target in
            Button("Delete", role: .destructive) {
                Task {
                    let key = target.id
                    deletingIDs.insert(key); defer { deletingIDs.remove(key) }
                    do { try await target.model.projects.delete(project: target.project) }
                    // A changed computer or pairing after the Mac answered: the refresh shows what is left.
                    catch is CancellationError { await target.model.projects.refresh() }
                    catch { deleteFailure = managementError(error) }
                }
            }
            .accessibilityIdentifier("project-delete-confirm")
            Button("Cancel", role: .cancel) {}
        } message: { target in
            Text("Its Cupola chats are deleted. Files and agent history on \(target.model.macName) stay. This can’t be undone.")
        }
        .alert("Couldn’t delete the Project", isPresented: Binding(get: { deleteFailure != nil }, set: { if !$0 { deleteFailure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: { Text(deleteFailure ?? "") }
    }
}

struct ManageProjectsTarget: Hashable, Identifiable {
    let model: ConnectionModel
    let project: ProjectSummary
    var id: String { "\(ObjectIdentifier(model).hashValue)/" + project.id }
    static func == (lhs: Self, rhs: Self) -> Bool { lhs.id == rhs.id }
    func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

/// One computer's Projects in Manage Projects.
private struct ManageProjectsSection: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    let search: String
    let title: String?
    let showsFooter: Bool
    let deleting: Set<String>
    let edit: (ProjectSummary) -> Void
    let requestDelete: (ProjectSummary) -> Void
    @State private var busy: Set<String> = []
    @State private var failure: String?
    private var filtered: [ProjectSummary] {
        let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
        let all = SidebarProjection.sorted(library.projects)
        return query.isEmpty ? all : all.filter { $0.name.localizedCaseInsensitiveContains(query) }
    }
    var body: some View {
        Section {
            if computerIsOffline(model) { ComputerOfflineRow(name: model.macName) }
            ForEach(filtered) { project in
                HStack(spacing: 12) {
                    // Only the name opens the editor; a row-wide tap
                    // gesture swallowed taps meant for the switch.
                    Button { edit(project) } label: {
                        HStack(spacing: 12) {
                            Image(systemName: project.isNoProject ? "square.dashed" : "folder").foregroundStyle(.secondary).frame(width: 28)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(project.name).foregroundStyle(.primary)
                                let folders = project.folders.map(\.name).joined(separator: ", ")
                                if !folders.isEmpty { Text(folders).font(.caption).foregroundStyle(.secondary).lineLimit(1) }
                                if !project.isIncluded { Text("Hidden").font(.caption).foregroundStyle(.secondary) }
                            }
                            Spacer(minLength: 0)
                        }.contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityHint("Edits this Project")
                    if busy.contains(project.id) || isDeleting(project) { ProgressView() }
                    Toggle("Show in sidebar", isOn: Binding(get: { project.isIncluded }, set: { value in Task { await set(project, ["isIncluded": value]) } })).systemSwitch()
                        .labelsHidden()
                        .disabled(busy.contains(project.id) || isDeleting(project) || computerIsOffline(model))
                        .accessibilityLabel("Show \(project.name) in sidebar")
                        .accessibilityIdentifier("project-include:\(model.connection?.credential.hostInstallationId ?? model.macName):" + project.id)
                }
                .padding(.vertical, 2)
                .swipeActions(edge: .leading) {
                    Button(project.isPinned ? "Unpin" : "Pin", systemImage: project.isPinned ? "pin.slash" : "pin") {
                        Task { await set(project, ["isPinned": !project.isPinned]) }
                    }.tint(.orange)
                }
                // The Mac must offer deleting a Project; without it nothing here pretends to.
                .swipeActions(edge: .trailing, allowsFullSwipe: false) {
                    if library.supportsProjectDelete && !project.isNoProject && !computerIsOffline(model) && !isDeleting(project) {
                        Button("Delete", systemImage: "trash", role: .destructive) {
                            requestDelete(project)
                        }
                            .accessibilityIdentifier("project-delete:" + project.id)
                    }
                }
            }
            if filtered.isEmpty && library.loadingProjects { ProgressView() }
            else if filtered.isEmpty && !library.projects.isEmpty { Text("No matching Projects").foregroundStyle(.secondary) }
            if let failure { FailureDetails(message: failure) }
        } header: {
            if let title { Text(title).accessibilityIdentifier("manage-projects-computer:" + model.macName) }
        } footer: {
            if showsFooter {
                Text("Switch a Project on to show it in the sidebar again. Hidden Projects keep their chats on your computer. Add Projects from the sidebar.")
            }
        }
        .task { await library.refresh() }
        .onChange(of: model.macConnected) { _, connected in
            if connected == true { Task { await library.refresh() } }
        }
    }
    private func isDeleting(_ project: ProjectSummary) -> Bool {
        deleting.contains(ManageProjectsTarget(model: model, project: project).id)
    }
    private func set(_ project: ProjectSummary, _ fields: [String: Any]) async {
        busy.insert(project.id); failure = nil
        defer { busy.remove(project.id) }
        do { _ = try await library.update(project.id, fields: fields) }
        catch { failure = managementError(error) }
    }
}

/// The one way to add a project on a Mac: create a new folder, choose an
/// existing one, or add a folder Codex or Claude Code already works in.
/// Suggestions are never added until the owner taps Add.
struct AddProjectView: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    /// A page in the chat column, which owns the navigation stack and Back.
    var embedded = false
    /// Each project added, including suggestions added in place.
    var onAdded: (ProjectSummary) -> Void = { _ in }
    /// A project made from New folder or Existing folder, which ends adding;
    /// without it the sheet closes.
    var onFinished: ((ProjectSummary) -> Void)?
    @Environment(\.dismiss) private var dismiss
    @State private var candidates: [ProjectCandidate] = []
    @State private var partial: [ProjectPartialFailure] = []
    @State private var loading = true
    @State private var loadFailure: String?
    @State private var adding: String?
    @State private var added: Set<String> = []
    @State private var addFailure: String?
    @State private var requestIDs: [String: String] = [:]

    var body: some View {
        if embedded {
            choices
        } else {
            NavigationStack {
                choices.toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button(added.isEmpty ? "Cancel" : "Done") { dismiss() }.accessibilityIdentifier("add-project-close")
                    }
                }
            }
        }
    }
    private var choices: some View {
            WonderList {
                Section {
                    // Destination links, not value links: embedded, this page was pushed
                    // by the shell's item destination, and pushing a value rebuilt the
                    // page (losing Added) instead of opening the form.
                    NavigationLink {
                        NewFolderProjectForm(model: model, library: library) { finish($0) }
                    } label: {
                        choice("New folder", detail: nil, icon: "folder.badge.plus")
                    }
                    .accessibilityIdentifier("add-project-new-folder")
                    NavigationLink {
                        ProjectEditorView(model: model, library: library, project: nil, embedded: true) { finish($0) }
                    } label: {
                        choice("Existing folder", detail: nil, icon: "folder")
                    }
                    .accessibilityIdentifier("add-project-existing-folder")
                }
                Section {
                    ForEach(candidates.filter { !$0.isIncluded || added.contains($0.id) }) { candidate in
                        suggestion(candidate)
                    }
                    if loading { ProgressView("Looking for Projects…") }
                    else if computerIsOffline(model) { ComputerOfflineRow(name: model.macName) }
                    else if let loadFailure {
                        Text(loadFailure).foregroundStyle(.secondary)
                        Button("Try again") { Task { await load() } }
                    } else if candidates.allSatisfy(\.isIncluded) && added.isEmpty {
                        Text("No other folders found.").foregroundStyle(.secondary)
                            .accessibilityIdentifier("add-project-no-suggestions")
                    }
                    ForEach(partial, id: \.self) { item in Text(item.detail).font(.footnote).foregroundStyle(.secondary) }
                    if let addFailure {
                        Text(addFailure).font(.footnote).foregroundStyle(.secondary).accessibilityIdentifier("add-project-failure")
                    }
                } header: {
                    Text("Used by Codex or Claude Code")
                }
            }
            .navigationTitle("Add Project").navigationBarTitleDisplayMode(.inline)
            .task { await load() }
    }
    private func choice(_ title: String, detail: String?, icon: String) -> some View {
        HStack(spacing: 12) {
            Image(systemName: icon).foregroundStyle(theme.accent).frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).foregroundStyle(.primary)
                if let detail { Text(detail).font(.caption).foregroundStyle(.secondary) }
            }
        }
        .padding(.vertical, 2)
    }
    private func suggestion(_ candidate: ProjectCandidate) -> some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(candidate.name).foregroundStyle(.primary)
                Text(candidate.folders.joined(separator: ", ")).font(.caption).foregroundStyle(.secondary)
                    .lineLimit(1).truncationMode(.middle)
                Text(candidate.sources.map(\.title).joined(separator: ", ")).font(.caption2).foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            if added.contains(candidate.id) {
                Label("Added", systemImage: "checkmark").labelStyle(.iconOnly).foregroundStyle(theme.accent)
            } else if adding == candidate.id {
                ProgressView().accessibilityLabel("Adding")
            } else {
                Button("Add") { Task { await add(candidate) } }
                    .buttonStyle(.bordered).disabled(adding != nil)
                    .accessibilityLabel("Add \(candidate.name)")
                    .accessibilityIdentifier("add-project-suggestion:" + candidate.name)
            }
        }
        .accessibilityElement(children: .contain)
    }
    private func finish(_ project: ProjectSummary) {
        onAdded(project)
        if let onFinished { onFinished(project) } else { dismiss() }
    }
    private func load() async {
        loading = true; loadFailure = nil
        defer { loading = false }
        do {
            let response = try await library.candidates()
            candidates = response.candidates; partial = response.partial
        } catch is CancellationError {
            return
        } catch { loadFailure = "Suggestions couldn’t be loaded. You can still choose a folder." }
    }
    private func add(_ candidate: ProjectCandidate) async {
        adding = candidate.id; addFailure = nil
        defer { adding = nil }
        let request = requestIDs[candidate.id] ?? UUID().uuidString.lowercased()
        requestIDs[candidate.id] = request
        do {
            let project = try await library.createProject(requestID: request, name: candidate.name, folders: candidate.folders, primaryIndex: 0)
            added.insert(candidate.id)
            onAdded(project)
        } catch PairingFailure.hostMessage(_, let message) {
            addFailure = message
        } catch {
            addFailure = "\(candidate.name) couldn’t be added. \(managementError(error))"
        }
    }
}

/// A project in a folder created for it. The folder takes the project's name.
struct NewFolderProjectForm: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    var onCreated: (ProjectSummary) -> Void
    @State private var name = ""
    /// nil is the Mac's home folder.
    @State private var parent: String?
    @State private var parentChosen = false
    @State private var browsing = false
    @State private var saving = false
    @State private var failure: String?
    @State private var requestID = UUID().uuidString.lowercased()
    @FocusState private var nameFocused: Bool
    private var trimmed: String { name.trimmingCharacters(in: .whitespacesAndNewlines) }
    private var nameProblem: String? {
        if trimmed.hasPrefix(".") { return "Folder names can’t start with a period." }
        if trimmed.contains("/") || trimmed.contains(":") { return "Folder names can’t contain / or :." }
        if trimmed.count > 120 { return "Use a shorter name." }
        return nil
    }
    private var canCreate: Bool { !trimmed.isEmpty && nameProblem == nil && !saving && !model.accessEnded }
    private var locationName: String { parent.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "Home folder" }
    var body: some View {
        WonderForm {
            Section {
                TextField("Project name", text: $name)
                    .focused($nameFocused)
                    .submitLabel(.done)
                    .onSubmit { if canCreate { Task { await create() } } }
                    .accessibilityIdentifier("new-folder-name")
            } footer: {
                if let nameProblem { Text(nameProblem).foregroundStyle(.red) }
                else { Text("The folder gets the same name.") }
            }
            Section {
                Button { browsing = true } label: {
                    HStack(spacing: 12) {
                        Image(systemName: "folder").foregroundStyle(.secondary)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(locationName).foregroundStyle(.primary)
                            if let parent {
                                Text(parent).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                            }
                        }
                        Spacer()
                        Text("Change").font(.subheadline).foregroundStyle(.secondary)
                    }
                }
                .accessibilityLabel("Location, \(locationName)")
                .accessibilityHint("Chooses where the folder is created")
                .accessibilityIdentifier("new-folder-location")
            } header: { Text("Location on \(model.macName)") }
            if let failure {
                Section { Text(failure).foregroundStyle(.secondary).accessibilityIdentifier("new-folder-failure") }
            }
        }
        .disabled(saving)
        .navigationTitle("New folder").navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .confirmationAction) {
                if saving { ProgressView() }
                else { Button("Create") { Task { await create() } }.disabled(!canCreate).accessibilityIdentifier("new-folder-create") }
            }
        }
        .sheet(isPresented: $browsing) {
            MacLocationBrowser(model: model, title: "Location", foldersOnly: true) { path, isDirectory in
                guard isDirectory else { return }
                parent = path; parentChosen = true
            }
        }
        // A changed name or location is a different request.
        .onChange(of: trimmed) { _, _ in requestID = UUID().uuidString.lowercased() }
        .onChange(of: parent) { _, _ in requestID = UUID().uuidString.lowercased() }
        .onAppear {
            // New projects usually sit beside the most recent one.
            if !parentChosen, let recent = SidebarProjection.sorted(library.projects).first,
               let primary = recent.folders.first(where: \.isPrimary) ?? recent.folders.first {
                parent = URL(fileURLWithPath: primary.path).deletingLastPathComponent().path
            }
            nameFocused = true
        }
    }
    private func create() async {
        saving = true; failure = nil
        defer { saving = false }
        do {
            onCreated(try await library.createProject(requestID: requestID, name: trimmed, newFolderParent: parent))
        } catch PairingFailure.hostMessage(_, let message) {
            failure = message
        } catch is CancellationError {
            return
        } catch { failure = managementError(error) }
    }
}

// MARK: - Sidebar reordering

/// Section-aware VoiceOver alternatives to the List's native long-press move.
struct SidebarMoveActions: ViewModifier {
    let row: SidebarRow
    @ObservedObject var presenter: SidebarPresenter
    let enabled: Bool
    func body(content: Content) -> some View {
        if enabled, row.orderItem != nil {
            content.accessibilityActions {
                if let previous = presenter.neighbor(of: row, down: false), let source = row.orderItem {
                    Button("Move up") { presenter.move(source, to: previous, after: false) }
                }
                if let next = presenter.neighbor(of: row, down: true), let source = row.orderItem {
                    Button("Move down") { presenter.move(source, to: next, after: true) }
                }
            }
        } else {
            content
        }
    }
}

/// "Show": how much of the agent's work one chat shows, in a chat's long-press
/// and header menus. With no override nothing is checked and the chat follows
/// its provider's choice from Settings; choosing a level checks it, and
/// choosing the checked level again clears it.
struct ChatDetailMenu: View {
    let override: ChatDetailLevel?
    let choose: (ChatDetailLevel?) -> Void

    var body: some View {
        Menu {
            ForEach(ChatDetailLevel.allCases) { level in
                Toggle(isOn: Binding(get: { override == level }, set: { _ in choose(override == level ? nil : level) })) {
                    Text(level.title)
                    Text(level.summary)
                }
                // A menu's check mark isn't in the accessibility tree, so the
                // identifier says which level is checked.
                .accessibilityIdentifier("chat-detail:" + level.rawValue + (override == level ? ":checked" : ""))
            }
        } label: {
            Label("Show", systemImage: "eye")
        }
        .accessibilityIdentifier("chat-detail-menu")
    }
}
