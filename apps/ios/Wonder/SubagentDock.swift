import SwiftUI
import WonderPairing

/// Composer rows keep their text capsules. The Duo rail uses native-size icons
/// and separate 44 pt surfaces. Either way the open tool looks the same: accent
/// text, a tinted fill and an accent ring, and VoiceOver reads it as selected.
/// Each button's label owns the surface and its complete hit target.
struct ComposerStatusPill: ViewModifier {
    var isActive = false
    @ScaledMetric(relativeTo: .subheadline) private var height: CGFloat = 36
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.composerPillsInBar) private var inBar
    func body(content: Content) -> some View {
        if inBar {
            content
                .font(.system(size: 20, weight: .medium))
                .foregroundStyle(isActive ? theme.accent : theme.text)
                .frame(width: DuoRailLayout.buttonSide, height: DuoRailLayout.buttonSide)
                .clipped()
                .background(isActive ? theme.accent.opacity(0.14) : theme.surface,
                            in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                .overlay {
                    // The rail's open tool: a light tint and a thin ring, so the column stays calm.
                    if isActive {
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .strokeBorder(theme.accent.opacity(0.7), lineWidth: 1)
                            .allowsHitTesting(false)
                    }
                }
                .contentShape(Rectangle())
                .accessibilityAddTraits(isActive ? .isSelected : [])
        } else {
            capsule(content)
        }
    }
    private func capsule(_ content: Content) -> some View {
        content
            .font(.subheadline.weight(.medium))
            .foregroundStyle(isActive ? theme.accent : theme.text)
            .padding(.horizontal, 12)
            .frame(minWidth: 44, minHeight: height)
            .background(isActive ? theme.accent.opacity(0.22) : theme.surface, in: Capsule())
            .overlay {
                if isActive {
                    Capsule().strokeBorder(theme.accent, lineWidth: 2).allowsHitTesting(false)
                }
            }
            .accessibilityAddTraits(isActive ? .isSelected : [])
            .animation(reduceMotion ? nil : .easeOut(duration: 0.15), value: isActive)
            .frame(minHeight: 44)
            .contentShape(Rectangle())
    }
}

/// A shortened Duo window keeps complete buttons on the rail instead of
/// handing the entire custom group to the system's More menu.
enum DuoRailLayout {
    static let buttonSide: CGFloat = 44
    static let spacing: CGFloat = 6
    /// Keyboard up: rows that fit between the rail's resting top and the keyboard's top.
    /// Unknown geometry (the rail attached after the keyboard) takes the smallest useful rail.
    static func keyboardRows(railTop: CGFloat?, keyboardTop: CGFloat, spacing: CGFloat = DuoRailLayout.spacing) -> Int {
        guard let railTop else { return 2 }
        let room = keyboardTop - railTop - spacing
        return min(4, max(1, Int((room + spacing) / (buttonSide + spacing))))
    }

    static func height(content: CGFloat, window: CGSize?, keyboard: CGFloat, keyboardRows: Int? = nil,
                       spacing: CGFloat = DuoRailLayout.spacing) -> CGFloat {
        let content = max(buttonSide, content)
        // The window fallback also handles attaching after the keyboard appeared.
        let shortened = keyboard > 120 || window.map { $0.height < 10 * buttonSide } == true
        // The cover's larger system status tile takes another button's room.
        let rows = keyboard > 120 ? keyboardRows ?? 4 : shortened ? 4 : window.map { $0.width < $0.height } == true ? 8 : 9
        let available = CGFloat(rows) * buttonSide + CGFloat(rows - 1) * spacing
        guard content > available else { return content }
        let fittingRows = max(1, Int((available + spacing) / (buttonSide + spacing)))
        return CGFloat(fittingRows) * buttonSide + CGFloat(fittingRows - 1) * spacing
    }
}

struct DuoRailControls<Content: View>: View {
    let selection: String?
    let spacing: CGFloat
    @ViewBuilder let content: () -> Content

    init(selection: String?, spacing: CGFloat = DuoRailLayout.spacing, @ViewBuilder content: @escaping () -> Content) {
        self.selection = selection
        self.spacing = spacing
        self.content = content
    }
    @Environment(\.shellWindowSize) private var window
    @State private var contentHeight: CGFloat = 44
    @State private var keyboardHeight: CGFloat = 0
    @State private var keyboardTop: CGFloat = 0
    /// Where the rail starts with no keyboard: UIKit slides a too-tall item up over the buttons above it.
    @State private var restingTop: CGFloat?

    private struct ReadingTarget: Hashable {
        let selection: String?
        let height: CGFloat
    }

    var body: some View {
        let height = DuoRailLayout.height(
            content: contentHeight, window: window, keyboard: keyboardHeight,
            keyboardRows: DuoRailLayout.keyboardRows(railTop: restingTop, keyboardTop: keyboardTop, spacing: spacing),
            spacing: spacing)
        ScrollViewReader { proxy in
            ScrollView(.vertical) {
                content()
                    .fixedSize(horizontal: false, vertical: true)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { contentHeight = $0 }
            }
            .scrollDismissesKeyboard(.never)
            .scrollDisabled(height >= contentHeight)
            .frame(width: 64, height: height)
            .accessibilityLabel("Tools")
            .accessibilityIdentifier("duo-rail-tools")
            .task(id: ReadingTarget(selection: selection, height: height)) {
                await Task.yield()
                guard !Task.isCancelled, let selection else { return }
                // Keep the slots still when the target already fits; reveal a
                // clipped selection without centering or resetting on close.
                proxy.scrollTo(selection)
            }
        }
        .background(alignment: .top) {
            // A toolbar item's SwiftUI global frame is its own host's; the screen's is real and matches the keyboard frame.
            RailToolbarSizing(size: CGSize(width: 64, height: height)) { top in
                if keyboardHeight < 120, restingTop != top { restingTop = top }
            } reportKeyboard: { height, top in
                // A rail built while the keyboard is already up (a fold, an unfold or a new chat with the
                // keyboard open) never saw keyboardWillShow, so it took full rows, and UIKit slid it up
                // over the Chats button. Seed from the window's own keyboard guide; show/hide notifications
                // keep it current afterwards.
                if keyboardHeight < 120, height >= 120 { keyboardHeight = height; keyboardTop = top }
            }
                .frame(width: 0, height: 0).accessibilityHidden(true)
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { note in
            let frame = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect
            keyboardHeight = frame?.height ?? 0
            keyboardTop = frame?.minY ?? 0
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in keyboardHeight = 0 }
        .onReceive(NotificationCenter.default.publisher(for: UIApplication.didEnterBackgroundNotification)) { _ in keyboardHeight = 0 }
    }
}

/// UIKit caches a custom bar item's frame while SwiftUI updates its intrinsic
/// size. Revalidate that same item's layout after a bounded viewport change.
private struct RailToolbarSizing: UIViewRepresentable {
    let size: CGSize
    let reportTop: (CGFloat) -> Void
    let reportKeyboard: (CGFloat, CGFloat) -> Void
    func makeUIView(context: Context) -> SizingView { SizingView() }
    func updateUIView(_ view: SizingView, context: Context) {
        view.reportTop = reportTop
        view.reportKeyboard = reportKeyboard
        view.request(size)
    }
    static func dismantleUIView(_ view: SizingView, coordinator: ()) { view.pending?.cancel() }

    final class SizingView: UIView {
        var pending: Task<Void, Never>?
        private var desired = CGSize.zero
        private var applied: CGSize?
        override init(frame: CGRect) {
            super.init(frame: frame)
            isUserInteractionEnabled = false
            isAccessibilityElement = false
        }
        required init?(coder: NSCoder) { nil }
        var reportTop: ((CGFloat) -> Void)?
        var reportKeyboard: ((CGFloat, CGFloat) -> Void)?
        private var reportedTop: CGFloat?
        private var reportedKeyboard: CGFloat?
        func request(_ size: CGSize) { desired = size; schedule(); announceTop() }
        override func didMoveToWindow() { super.didMoveToWindow(); applied = nil; schedule(); announceTop() }
        override func layoutSubviews() { super.layoutSubviews(); schedule(); announceTop() }
        /// Where the rail starts on the screen (the keyboard's coordinate space), after layout (never from inside it).
        private func announceTop() {
            guard let window, bounds.height >= 0 else { return }
            announceKeyboard(in: window)
            let top = window.screen.coordinateSpace.convert(convert(bounds, to: window), from: window).minY
            guard top != reportedTop else { return }
            reportedTop = top
            DispatchQueue.main.async { [weak self] in self?.reportTop?(top) }
        }
        /// The keyboard as the window sees it now, in screen coordinates (the guide sits at the window's bottom with none).
        private func announceKeyboard(in window: UIWindow) {
            let screen = window.screen.coordinateSpace
            let frame = window.keyboardLayoutGuide.layoutFrame
            // Before its first layout the guide is a zero rect at the origin, which reads as a keyboard
            // filling the screen and collapses the rail to two rows.
            guard frame.width > 0 else { return }
            let guide = screen.convert(frame, from: window)
            let height = max(0, screen.bounds.maxY - guide.minY)
            guard height != reportedKeyboard else { return }
            reportedKeyboard = height
            let top = guide.minY
            DispatchQueue.main.async { [weak self] in self?.reportKeyboard?(height, top) }
        }
        private func schedule() {
            guard window != nil, desired != applied, pending == nil else { return }
            let request = desired
            pending = Task { @MainActor [weak self] in
                await Task.yield()
                guard let self else { return }
                defer {
                    self.pending = nil
                    if self.desired != request { self.schedule() }
                }
                guard !Task.isCancelled, self.desired == request, let window = self.window else { return }
                var queue = window.rootViewController.map { [$0] } ?? []
                var visited = Set<ObjectIdentifier>()
                while !queue.isEmpty, visited.count < 200 {
                    let owner = queue.removeFirst()
                    guard visited.insert(ObjectIdentifier(owner)).inserted else { continue }
                    queue += owner.children
                    if let presented = owner.presentedViewController { queue.append(presented) }
                    let navigation = owner.navigationItem
                    let groups = navigation.leadingItemGroups + navigation.centerItemGroups + navigation.trailingItemGroups
                    guard let item = groups.flatMap(\.barButtonItems).first(where: {
                        $0.customView.map { self.isDescendant(of: $0) } == true
                    }), let custom = item.customView else { continue }
                    self.applied = request
                    custom.invalidateIntrinsicContentSize()
                    custom.superview?.invalidateIntrinsicContentSize()
                    custom.superview?.setNeedsLayout()
                    // Keep the same groups/items and their reference-model owners.
                    let leading = navigation.leadingItemGroups, trailing = navigation.trailingItemGroups
                    navigation.leadingItemGroups = leading
                    navigation.trailingItemGroups = trailing
                    #if canImport(SwiftUI, _version: 8.0.85)
                    if #available(iOS 27.1, *) { owner.setNeedsUpdateOfVerticalBarConfiguration() }
                    #endif
                    owner.viewIfLoaded?.setNeedsLayout()
                    window.setNeedsLayout()
                    return
                }
            }
        }
    }
}

struct RailButtonSurface: ViewModifier {
    let inBar: Bool
    let selected: Bool
    func body(content: Content) -> some View {
        if inBar { content.modifier(ComposerStatusPill(isActive: selected)).environment(\.composerPillsInBar, true) }
        else { content }
    }
}

/// Sheets opened from the composer's pills: bottom sheets on a phone-width
/// window (an iPhone, a folded iPhone Duo), centred cards on a wide one. iPad
/// already shows a sheet in a wide window as a centred form sheet; an iPhone-class
/// device (an unfolded iPhone Duo) keeps sheets full width at every size, so
/// there the content is presented as a card over a dimmed window instead.
/// `wide` is the presenting window's, since a bottom sheet's own content reads compact.
///
/// The style is chosen when the sheet opens and kept until it closes. Switching
/// it while open (folding or unfolding a Duo) would end one presentation and
/// start another: the content lost its state (a chosen scope, expanded
/// activity), and a sheet could fail to come back at all. A card opened
/// unfolded instead narrows with the folded window; a sheet stays a sheet.
extension View {
    func pillSheet<Content: View>(isPresented: Binding<Bool>, wide: Bool, onDismiss: (() -> Void)? = nil,
                                  @ViewBuilder content: @escaping () -> Content) -> some View {
        modifier(PillPresentation(item: Binding(get: { isPresented.wrappedValue ? PillPresented.shown : nil },
                                                set: { isPresented.wrappedValue = $0 != nil }),
                                  wide: wide, onDismiss: onDismiss) { _ in content() })
    }

    func pillSheet<Item: Identifiable, Content: View>(item: Binding<Item?>, wide: Bool, onDismiss: (() -> Void)? = nil,
                                                      @ViewBuilder content: @escaping (Item) -> Content) -> some View {
        modifier(PillPresentation(item: item, wide: wide, onDismiss: onDismiss, sheet: content))
    }
}

extension ToolbarContent {
    /// The same control stays in the system rail across pane changes. Hide the
    /// shared glass envelope there: each label draws its own 44 pt surface.
    @ToolbarContentBuilder func duoRail(_ enabled: Bool) -> some ToolbarContent {
        #if canImport(SwiftUI, _version: 8.0.85)
        if #available(iOS 27.1, *) {
            self.axisBehavior(enabled ? .verticalPreferred : .automatic)
                .sharedBackgroundVisibility(enabled ? .hidden : .automatic)
        } else {
            self
        }
        #else
        self
        #endif
    }
}

private enum PillPresented: Identifiable { case shown; var id: Int { 0 } }

private struct PillPresentation<Item: Identifiable, Sheet: View>: ViewModifier {
    @Binding var item: Item?
    let wide: Bool
    let onDismiss: (() -> Void)?
    @ViewBuilder let sheet: (Item) -> Sheet
    /// The open sheet's style; nil while none is open.
    @State private var openAsCard: Bool?

    private var cardNow: Bool { wide && UIDevice.current.userInterfaceIdiom == .phone }

    func body(content: Content) -> some View {
        let card = openAsCard ?? cardNow
        content
            .sheet(item: card ? .constant(nil) : $item, onDismiss: closed) { value in
                sheet(value).environment(\.toolHasGestureDismissal, true)
                    .presentationDragIndicator(.hidden)
            }
            .fullScreenCover(item: card ? $item : .constant(nil), onDismiss: closed) { value in PillCard { sheet(value).environment(\.toolHasGestureDismissal, true) } }
            .onChange(of: item == nil, initial: true) { _, none in
                if none { openAsCard = nil } else if openAsCard == nil { openAsCard = cardNow }
            }
    }

    private func closed() {
        openAsCard = nil
        onDismiss?()
    }
}

/// A centred card, up to 720 × 900 points, over the dimmed window; tapping
/// outside it or the escape gesture closes it like a sheet.
private struct PillCard<Content: View>: View {
    @ViewBuilder let content: () -> Content
    @Environment(\.dismiss) private var dismiss
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        GeometryReader { geometry in
            ZStack {
                Color.black.opacity(0.35).ignoresSafeArea()
                    .onTapGesture { dismiss() }
                    .accessibilityHidden(true)
                content()
                    .dismissOnTopDrag { dismiss() }
                    .frame(width: min(720, geometry.size.width - 64), height: min(900, geometry.size.height - 32))
                    .background(theme.page)
                    .clipShape(RoundedRectangle(cornerRadius: 28, style: .continuous))
                    .shadow(color: .black.opacity(0.25), radius: 30, y: 10)
                    .accessibilityAction(.escape) { dismiss() }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .presentationBackground(.clear)
    }
}

/// A pill sheet's height on a phone-width window: half first, then full.
struct PillSheet: ViewModifier {
    let wide: Bool
    var detent: Binding<PresentationDetent>?
    func body(content: Content) -> some View {
        if wide {
            content
        } else if let detent {
            content.presentationDetents([.medium, .large], selection: detent).presentationDragIndicator(.hidden)
        } else {
            content.presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
        }
    }
}

extension EnvironmentValues {
    /// Composer pills shown as items in the system's vertical bar (a folded iPhone Duo).
    @Entry var composerPillsInBar = false
    /// The folded display shows Chats as its own page, including hiding the
    /// conversation's system-managed toolbar outside the navigation stack.
    @Entry var shellChatsCoverConversation = false
}

extension View {
    /// Tracks `VerticalBarReader` for this view. The reader is a zero-size overlay
    /// hidden from accessibility, so an identifier set on this view stays on its
    /// content (a full-size background reader took the identifier and read as
    /// covering every row). Systems without the bar install nothing.
    func readsVerticalBar(_ present: Binding<Bool>) -> some View {
        overlay(alignment: .topLeading) {
            if #available(iOS 27.1, *) {
                VerticalBarReader(present: present)
                    .frame(width: 0, height: 0)
                    .accessibilityHidden(true)
            }
        }
    }
}

/// Reports whether the system shows a vertical bar for toolbar items: the
/// iPhone Duo's displays (iOS 27.1 `verticalBarEdge`). Earlier systems and
/// SDKs have none. The value is published after layout, never during it.
struct VerticalBarReader: UIViewRepresentable {
    @Binding var present: Bool

    func makeUIView(context: Context) -> ReaderView {
        let view = ReaderView()
        view.report = { value in if present != value { present = value } }
        return view
    }
    func updateUIView(_ view: ReaderView, context: Context) {
        view.report = { value in if present != value { present = value } }
    }

    final class ReaderView: UIView {
        var report: ((Bool) -> Void)?
        override init(frame: CGRect) {
            super.init(frame: frame)
            isUserInteractionEnabled = false
            isAccessibilityElement = false
            #if canImport(SwiftUI, _version: 8.0.85)
            if #available(iOS 27.1, *) {
                registerForTraitChanges(UITraitCollection.systemTraitsAffectingVerticalBarEdge) { (view: ReaderView, _: UITraitCollection) in
                    view.publish()
                }
            }
            #endif
        }
        required init?(coder: NSCoder) { nil }
        override func didMoveToWindow() {
            super.didMoveToWindow()
            publish()
            // A posture change while the app is in the background delivers no trait change
            // to a view that is not on screen; read the trait again when its scene returns.
            sceneObserver = window?.windowScene.map { scene in
                NotificationCenter.default.addObserver(forName: UIScene.didActivateNotification, object: scene, queue: .main) { [weak self] _ in
                    self?.publish()
                }
            }
        }
        private var sceneObserver: NSObjectProtocol? {
            didSet { if let old = oldValue { NotificationCenter.default.removeObserver(old) } }
        }
        private var hasBar: Bool {
            #if canImport(SwiftUI, _version: 8.0.85)
            if #available(iOS 27.1, *) { return window != nil && traitCollection.verticalBarEdge != .unspecified }
            #endif
            return false
        }
        private func publish() {
            let value = hasBar
            Task { @MainActor [weak self] in self?.report?(value) }
        }
    }
}

/// Project helpers are provider threads, not Wonder conversations. Keep their
/// roster and history read-only under the parent Project thread; the one action
/// is stopping a running task: a Claude task or a command Codex left running.
/// Codex helpers have names and keep their roster; tasks and commands have only
/// a description, so they show a status glyph instead of an avatar (grouped into
/// Running and Completed when nothing else is listed).
struct ProjectSubagentDock: View {
    let agents: [ProjectSubagentSummary]
    /// `agents` grouped Running / Completed, prepared when the list changes.
    let sections: ProjectSubagentSections?
    let available: Bool
    let freshIDs: Set<String>
    let detail: String?
    let stoppingIDs: Set<String>
    let stopError: String?
    /// The chat's provider ("Codex"), named in the Stop confirmation.
    let provider: String?
    let hasOlder: Bool
    let loadingOlder: Bool
    @Binding var isPresented: Bool
    var paneIsPresented = false
    var closePane: () -> Void = {}
    let loadOlder: () -> Void
    let stop: (ProjectSubagentSummary) -> Void
    let open: (ProjectSubagentSummary) -> Void
    @State private var confirmingStop: ProjectSubagentSummary?
    @Environment(\.composerPillsInBar) private var inBar
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var tasksOnly: Bool { !agents.isEmpty && !agents.contains(where: \.usesAvatar) }

    var body: some View {
        if !agents.isEmpty || hasOlder || paneIsPresented {
            let pill = ProjectSubagentPill(agents: agents, sections: sections, available: available, fresh: freshIDs)
            Button {
                if paneIsPresented { closePane() } else { isPresented.toggle() }
            } label: {
                let icon = Group {
                    if pill.spins && !reduceMotion {
                        Image(systemName: pill.symbolName)
                            .symbolEffect(.rotate, options: .repeat(.continuous), isActive: true)
                    } else {
                        // Ending an indefinite symbol effect lets its current
                        // rotation finish. A separate still image makes the
                        // completed checkmark upright immediately instead.
                        Image(systemName: pill.symbolName)
                    }
                }
                Group {
                    if inBar {
                        // A folded Duo's vertical bar is one icon wide: the icon over
                        // its count (the label still says what is running).
                        VStack(spacing: 1) {
                            icon.frame(width: 22, height: 22)
                            Text(pill.count.formatted())
                                .font(.system(size: 11, weight: .semibold)).monospacedDigit()
                                .lineLimit(1).fixedSize()
                        }
                        .modifier(ComposerStatusPill(isActive: isPresented || paneIsPresented))
                    } else {
                        HStack(spacing: 6) {
                            icon
                            // Accessibility sizes keep the count alone; the label says what it counts.
                            Text(typeSize.isAccessibilitySize ? pill.count.formatted() : pill.text).monospacedDigit()
                        }
                        .modifier(ComposerStatusPill(isActive: isPresented || paneIsPresented))
                    }
                }
                .pillPopover(isPresented: $isPresented) { roster }
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("project-subagent-status-pill")
            .accessibilityLabel(pill.accessibilityLabel)
            .accessibilityValue(paneIsPresented ? "Open" : isPresented ? "Expanded" : "Collapsed")
            .accessibilityAddTraits(isPresented || paneIsPresented ? .isSelected : [])
            .accessibilityHint(tasksOnly ? "Show running and completed agent tasks" : "Show agent tasks in this chat")
            #if WONDER_DIAGNOSTICS
            .task(id: inBar) {
                let arguments = ProcessInfo.processInfo.arguments
                guard arguments.contains("-diagnostics-task-roster-open"),
                      !arguments.contains("-diagnostics-task-roster-in-bar") || inBar else { return }
                // A Duo first lays out the row, then moves its pills into the
                // system bar. Open only the fixture's chosen presentation owner.
                isPresented = true
            }
            #endif
        }
    }

    private var roster: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 8) {
                Text("Agent tasks").font(.headline).accessibilityAddTraits(.isHeader)
                // A Claude session can list hundreds of runs; a refused Stop is explained
                // up top, where it stays in view.
                if let stopError {
                    Text(stopError).font(.caption).foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("project-subagent-stop-error")
                }
                if tasksOnly {
                    let sections = sections ?? agents.sectioned()
                    taskSection("Running", sections.running)
                    taskSection("Completed", sections.completed)
                } else {
                    // A command Codex left running sits among its named helpers
                    // with its own Stop.
                    ForEach(agents) { agent in
                        if agent.usesAvatar { rosterButton(agent) } else { taskRow(agent) }
                    }
                }
                if hasOlder {
                    Button(action: loadOlder) {
                        if loadingOlder { ProgressView() }
                        else { Text("Load older agent tasks") }
                    }
                    .disabled(!available || loadingOlder)
                    .frame(minHeight: 44)
                    .accessibilityIdentifier("project-subagent-roster-load-older")
                }
                if let detail {
                    Text(detail).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("project-subagent-roster-detail")
                }
            }
            .padding()
        }
        .accessibilityIdentifier("project-subagent-roster-list")
        .frame(idealWidth: 320, maxWidth: 360, idealHeight: typeSize.isAccessibilitySize ? 640 : 280,
               maxHeight: typeSize.isAccessibilitySize ? 640 : 360)
        .confirmationDialog(confirmingStop?.stopConfirmation(provider: provider).title ?? "Stop this task?",
            isPresented: Binding(get: { confirmingStop != nil }, set: { if !$0 { confirmingStop = nil } }),
            titleVisibility: .visible, presenting: confirmingStop) { task in
            Button(task.stopConfirmation(provider: provider).action, role: .destructive) { stop(task) }
                .accessibilityIdentifier("project-subagent-stop-confirm")
            Button("Keep running", role: .cancel) {}
        } message: { task in
            Text(task.stopConfirmation(provider: provider).message)
        }
    }

    @ViewBuilder private func taskSection(_ title: String, _ tasks: [ProjectSubagentSummary]) -> some View {
        if !tasks.isEmpty {
            Text("\(title) \u{00B7} \(tasks.count.formatted())").font(.subheadline.weight(.semibold))
                .foregroundStyle(.secondary).padding(.top, 4)
                .accessibilityAddTraits(.isHeader)
                .accessibilityIdentifier("project-subagent-section:" + title.lowercased())
            // A task keeps its row identity within a state; moving to Completed is a new row.
            ForEach(tasks) { agent in taskRow(agent).id(agent.threadId + "|" + agent.status) }
        }
    }

    private func taskRow(_ agent: ProjectSubagentSummary) -> some View {
        let fresh = available && freshIDs.contains(agent.threadId)
        let stopping = stoppingIDs.contains(agent.threadId)
        let status = agent.isRunningElsewhere ? "Running on your computer" : agent.statusLabel(available: fresh)
        // The glyph shows the status; the words stay in the accessibility label.
        let step = fresh ? agent.currentStep : nil
        let captionParts: [String?] = [step ?? agent.kindLabel, agent.durationLabel, agent.isRunningElsewhere ? status : nil]
        let caption = captionParts.compactMap { $0 }.joined(separator: " \u{00B7} ")
        let labelParts: [String?] = [agent.title, agent.kindLabel, step ?? status, agent.durationAccessibilityLabel]
        let label = labelParts.compactMap { $0 }.joined(separator: ", ")
        // Accessibility sizes put Stop under the title, which keeps the row's width.
        let layout = typeSize.isAccessibilitySize ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4))
            : AnyLayout(HStackLayout(spacing: 8))
        let summary = HStack(spacing: 10) {
            Image(systemName: agent.statusSymbol)
                .foregroundStyle(agent.isRunning ? Color.accentColor : .secondary)
                .frame(width: 22)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(agent.title).foregroundStyle(.primary).lineLimit(typeSize.isAccessibilitySize ? 5 : 2)
                    .multilineTextAlignment(.leading)
                if !caption.isEmpty {
                    Text(caption).font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 8)
        }
        .frame(minHeight: 44).contentShape(Rectangle())
        return layout {
            if agent.opensHistory {
                Button { open(agent) } label: { summary }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("project-subagent-roster:" + agent.threadId)
                    .accessibilityLabel(label)
                    .accessibilityHint("Open read-only agent task")
            } else {
                // A command Codex left running has no history to open; Stop is its action.
                summary
                    .accessibilityElement(children: .ignore)
                    .accessibilityIdentifier("project-subagent-roster:" + agent.threadId)
                    .accessibilityLabel(label)
            }
            if stopping {
                ProgressView().frame(width: 56, height: 44)
                    .accessibilityLabel("Stopping")
                    .accessibilityIdentifier("project-subagent-stopping:" + agent.threadId)
            } else if agent.isStoppable && fresh {
                Button("Stop", role: .destructive) { confirmingStop = agent }
                    .font(.subheadline.weight(.medium))
                    .buttonStyle(.bordered)
                    .frame(minWidth: 44, minHeight: 44)
                    .accessibilityLabel("Stop " + agent.title)
                    .accessibilityIdentifier("project-subagent-stop:" + agent.threadId)
            } else if agent.opensHistory && !typeSize.isAccessibilitySize {
                Image(systemName: "chevron.right").font(.caption).foregroundStyle(.secondary)
                    .accessibilityHidden(true)
            }
        }
    }

    private func rosterButton(_ agent: ProjectSubagentSummary) -> some View {
        let fresh = available && freshIDs.contains(agent.threadId)
        return Button { open(agent) } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(agent.title).foregroundStyle(.primary)
                    Text((fresh ? agent.currentStep : nil) ?? agent.statusLabel(available: fresh)).font(.caption).foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)
                Image(systemName: "chevron.right").font(.caption).foregroundStyle(.secondary)
            }
            .frame(minHeight: 44).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("project-subagent-roster:" + agent.threadId)
        .accessibilityLabel(agent.title + ", " + ((fresh ? agent.currentStep : nil) ?? agent.statusLabel(available: fresh)))
        .accessibilityHint("Open read-only agent task")
    }
}

struct ProjectSubagentTranscriptView: View {
    @ObservedObject var model: ConnectionModel
    let parent: ChatSummary
    let child: ProjectSubagentSummary
    private let dismiss = ToolDismiss()
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.companionPane) private var pane
    @Environment(\.toolHasGestureDismissal) private var gestureDismissal
    @State private var verticalBar = false
    private var inlineHeader: Bool { CompanionPaneChrome.usesInlineHeader(pane, verticalBar: verticalBar) }
    /// The newest page and the older pages loaded under it.
    @State private var pages: SubagentTranscriptPages?
    @State private var chatDetail: ChatDetailSettings?
    private var snapshot: ConversationSnapshot? { pages?.shown }
    @State private var entries: [ChatFeedEntry] = []
    @State private var unsaved: SubagentTranscriptNode.Unsaved = [:]
    @State private var turns: [String: ReadTurn] = [:]
    @State private var lastActivityEntries: Set<String> = []
    @State private var expanded: Set<String> = []
    /// Tool-call rows the reader opened. Rows start collapsed, as in the conversation.
    @State private var expandedDetails: Set<String> = []
    @State private var error: String?
    @State private var loading = true
    @State private var loadingOlder = false
    @State private var olderTask: Task<Void, Never>?
    @State private var loadToken = UUID()

    init(model: ConnectionModel, parent: ChatSummary, child: ProjectSubagentSummary) {
        self.model = model
        self.parent = parent
        self.child = child
        _chatDetail = State(initialValue: model.projects.chatDetail)
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    if let cursor = snapshot?.thread.nextCursor {
                        Button {
                            olderTask?.cancel()
                            olderTask = Task { await loadOlder(cursor: cursor) }
                        } label: {
                            if loadingOlder { ProgressView() }
                            else { Text("Load older activity") }
                        }
                        .disabled(loadingOlder)
                        .frame(maxWidth: .infinity, minHeight: 44)
                        .accessibilityIdentifier("project-subagent-load-older")
                    }
                    // Each visible row is a peer in this one lazy stack (no nested
                    // stacks); IDs come from the history so state survives reloads.
                    ForEach(SubagentTranscriptNode.visible(entries, expanded: expanded, unsaved: unsaved,
                                                           detail: chatDetail?.detail(for: model.agentFamily(parent), chat: parent.id) ?? .everything)) { node in
                        switch node {
                        case .feed(let node): transcriptNode(node)
                        case .unsaved(let row, let media): ProjectSubagentFileNotice(unsaved: media, rowID: row)
                        }
                    }
                    if !loading, entries.isEmpty, error == nil {
                        ContentUnavailableView("No activity yet", systemImage: "text.bubble")
                    }
                    if loading { ProgressView("Loading agent task…").frame(maxWidth: .infinity) }
                    if let error {
                        VStack(spacing: 8) {
                            Text(error).font(.footnote).foregroundStyle(.secondary)
                            Button("Try again") { Task { await loadInitial() } }
                                .accessibilityIdentifier("project-subagent-retry")
                        }
                        .frame(maxWidth: .infinity)
                    }
                }
                .padding(16)
            }
            .wonderPage()
            // Commands and their output read top to bottom on a phone.
            .environment(\.codeBlocksWrap, true)
            .accessibilityIdentifier("project-subagent-transcript")
            .navigationTitle(child.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if !inlineHeader && (pane != nil || !gestureDismissal) {
                    ToolbarItem(placement: .topBarTrailing) {
                        HStack {
                            doneButton
                            CompanionExpandButton()
                        }
                    }
                }
            }
            .modifier(CompanionPaneNavigationHeader(enabled: inlineHeader, title: child.title) { doneButton })
        }
        .readsVerticalBar($verticalBar)
        .task(id: transcriptKey) { await loadInitial() }
        .onReceive(model.projects.$chatDetail) { chatDetail = $0 }
        // A running task keeps working: while Cupola is in front, re-read its
        // newest page so new tool calls appear. Row and group disclosure is
        // keyed by history IDs, so what the reader opened stays open across
        // the refresh. In the background the reads stop and resume on return.
        .task(id: transcriptKey + (scenePhase == .active ? ":active" : ":inactive")) {
            while SubagentTranscriptPages.polls(active: scenePhase == .active, running: child.isRunning, shown: snapshot),
                  !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(4)) } catch { return }
                await refreshNewest()
            }
        }
        .onDisappear {
            loadToken = UUID()
            olderTask?.cancel()
        }
    }

    private var doneButton: some View {
        ToolCloseControl {
            Button { dismiss() } label: {
                Text("Close").frame(minWidth: inlineHeader ? 44 : nil, minHeight: inlineHeader ? 44 : nil)
                    .contentShape(Rectangle())
            }.accessibilityIdentifier("project-subagent-sheet-done")
        }
    }

    private var transcriptKey: String { child.threadId + ":" + model.assignmentScope }

    @ViewBuilder private func transcriptNode(_ node: ChatFeedNode) -> some View {
        switch node.content {
        case .entry(let entry):
            if entry.isActivity {
                ActivityGroupView(rows: entry.rows,
                    turn: entry.rows.first?.turnId.flatMap { turns[$0] },
                    isLatestSegmentForTurn: lastActivityEntries.contains(entry.id),
                    isLatestActiveSegment: false,
                    expanded: expanded.contains(entry.id)) {
                        if !expanded.insert(entry.id).inserted { expanded.remove(entry.id) }
                    }
            } else if let row = entry.rows.first {
                MessageRow(row: row)
                    .accessibilityIdentifier("project-subagent-row:" + row.id)
            }
        case .activity(let row):
            if row.isCommentary {
                AgentMessageText(text: row.text)
                    .modifier(ChatBubbleSurface())
                    .accessibilityLabel(row.author + ": " + ChatPlainText.of(row.text))
                    .padding(.leading, 12)
            } else {
                ActivityItemView(row: row, expanded: expandedDetails.contains(row.id)) {
                    if !expandedDetails.insert(row.id).inserted { expandedDetails.remove(row.id) }
                }
                .padding(.leading, 12)
            }
        case .thinking(let row):
            ThinkingActivityView(row: row, isRunning: row.activitySummary?.isRunning == true, summary: row.thinkingSummary)
                .padding(.leading, 12)
        case .commands(let run):
            CommandRunView(run: run, expanded: expanded.contains(node.id)) {
                if !expanded.insert(node.id).inserted { expanded.remove(node.id) }
            }
            .padding(.leading, 12)
        case .compaction(let row):
            ContextCompactionMarker(row: row, isProjectConversation: true)
        case .file(let file):
            ProjectSubagentFileNotice(files: [file])
        case .gallery(let files):
            ProjectSubagentFileNotice(files: files)
        case .updatedPage(let file):
            // An agent task's files have no route to open from here (the
            // chat's own Updated notice opens the parent chat's files).
            ProjectSubagentFileNotice(files: [file])
        case .outcome, .question:
            EmptyView()
        }
    }

    private func display(_ value: ConversationSnapshot) {
        let rows = value.rows(author: child.title)
        entries = ChatFeedEntry.grouping(rows)
        unsaved = SubagentTranscriptNode.unsavedMedia(entries)
        turns = Dictionary((value.thread.turns ?? []).map { ($0.id, $0) }, uniquingKeysWith: { _, newer in newer })
        lastActivityEntries = ChatFeedEntry.latestActivityEntryIDs(entries)
    }

    private func loadInitial() async {
        olderTask?.cancel()
        olderTask = nil
        let token = UUID()
        loadToken = token
        let requestedChild = child
        let scope = model.assignmentScope
        pages = nil
        entries = []
        unsaved = [:]
        turns = [:]
        lastActivityEntries = []
        expanded = []
        expandedDetails = []
        loading = true
        loadingOlder = false
        error = nil
        do {
            let response = try await model.projectSubagentTranscript(parent: parent, child: requestedChild)
            guard !Task.isCancelled, loadToken == token, child.threadId == requestedChild.threadId,
                  model.assignmentScope == scope else { return }
            pages = SubagentTranscriptPages(response.snapshot)
            display(response.snapshot)
        } catch {
            guard !Task.isCancelled, loadToken == token, child.threadId == requestedChild.threadId,
                  model.assignmentScope == scope else { return }
            if case PairingFailure.hostMessage(_, let reason) = error {
                self.error = reason
            } else {
                self.error = "This agent task could not be opened. Refresh the chat and try again."
            }
        }
        loading = false
    }

    private func refreshNewest() async {
        guard !loading, !loadingOlder, error == nil else { return }
        let token = loadToken
        let requestedChild = child
        let scope = model.assignmentScope
        guard let response = try? await model.projectSubagentTranscript(parent: parent, child: requestedChild),
              !Task.isCancelled, loadToken == token, child.threadId == requestedChild.threadId,
              model.assignmentScope == scope, !loadingOlder, var current = pages else { return }
        // Nothing changed on the newest page: keep the prepared rows as they are.
        guard let shown = current.refreshed(response.snapshot) else { return }
        pages = current
        display(shown)
    }

    private func loadOlder(cursor: String) async {
        guard var current = pages else { return }
        let token = loadToken
        let requestedChild = child
        let scope = model.assignmentScope
        loadingOlder = true
        error = nil
        do {
            let response = try await model.projectSubagentTranscript(parent: parent, child: requestedChild, cursor: cursor)
            guard !Task.isCancelled, loadToken == token, child.threadId == requestedChild.threadId,
                  model.assignmentScope == scope else { return }
            let shown = try current.older(response.snapshot)
            pages = current
            display(shown)
        } catch {
            guard !Task.isCancelled, loadToken == token, child.threadId == requestedChild.threadId,
                  model.assignmentScope == scope else { return }
            self.error = "Older activity could not be loaded. Try again."
        }
        loadingOlder = false
    }
}

/// The pages an agent task's transcript shows: the newest page as last read
/// and, once older activity is loaded, everything merged under it.
struct SubagentTranscriptPages: Equatable {
    private(set) var shown: ConversationSnapshot
    private(set) var newest: ConversationSnapshot
    private(set) var loadedOlder = false

    init(_ newest: ConversationSnapshot) {
        shown = newest
        self.newest = newest
    }

    /// A re-read of the newest page. Nil when it is unchanged (compared
    /// whole: an item's output, state or text changes in place without
    /// moving the sequence or the active turn), so the prepared rows stay.
    /// Otherwise what to show: once older pages are loaded, the newest page
    /// merges into them by turn and item ID, so older rows and what the
    /// reader opened stay.
    mutating func refreshed(_ page: ConversationSnapshot) -> ConversationSnapshot? {
        guard page != newest else { return nil }
        let next: ConversationSnapshot
        if loadedOlder {
            guard let merged = try? page.mergingOlder(shown) else { return nil }
            next = merged
        } else { next = page }
        newest = page
        shown = next
        return next
    }

    mutating func older(_ page: ConversationSnapshot) throws -> ConversationSnapshot {
        shown = try shown.mergingOlder(page)
        loadedOlder = true
        return shown
    }

    /// A running task's newest page is re-read only while Cupola is in front
    /// and the task still has a turn under way; in the background the reads
    /// stop and resume on return.
    static func polls(active: Bool, running: Bool, shown: ConversationSnapshot?) -> Bool {
        active && running && (shown.map { $0.activeTurnID != nil } ?? true)
    }
}

/// One row of an agent task's transcript: a conversation node, or a picture
/// or recording a tool showed or made that the provider recorded without a
/// file the Mac saved (`ReadRow.unsavedMedia`). Such media is not dropped:
/// it shows where a verified picture would, visible without opening the
/// Working group, as a notice that says where to see it.
enum SubagentTranscriptNode: Identifiable {
    case feed(ChatFeedNode)
    case unsaved(rowID: String, UnsavedToolMedia)

    var id: String {
        switch self {
        case .feed(let node): node.id
        case .unsaved(let row, _): "unsaved:" + row
        }
    }

    typealias Unsaved = [String: [(row: String, media: UnsavedToolMedia)]]

    /// The unsaved media of each Working group, by entry ID. Prepared when
    /// the entries change, not on every disclosure.
    static func unsavedMedia(_ entries: [ChatFeedEntry]) -> Unsaved {
        var unsaved: Unsaved = [:]
        for entry in entries where entry.isActivity {
            let media = entry.rows.compactMap { row in row.unsavedMedia.map { (row: row.id, media: $0) } }
            if !media.isEmpty { unsaved[entry.id] = media }
        }
        return unsaved
    }

    /// After its tool row when that row is shown (an open Working group),
    /// otherwise after the last node of its entry.
    static func visible(_ entries: [ChatFeedEntry], expanded: Set<String>, unsaved: Unsaved,
                        detail: ChatDetailLevel) -> [Self] {
        var unsaved = unsaved
        let nodes = ChatFeedNode.visible(entries, expanded: expanded, detail: detail)
        guard !unsaved.isEmpty else { return nodes.map(Self.feed) }
        var result: [Self] = []
        result.reserveCapacity(nodes.count + unsaved.count)
        func flush(_ entryID: String?) {
            guard let entryID, let pending = unsaved.removeValue(forKey: entryID) else { return }
            result += pending.map { Self.unsaved(rowID: $0.row, $0.media) }
        }
        var next = nodes.startIndex
        for entry in entries {
            while next < nodes.endIndex, nodes[next].entryID == entry.id {
                let node = nodes[next]
                result.append(.feed(node))
                if case .activity(let row) = node.content,
                   let index = unsaved[entry.id]?.firstIndex(where: { $0.row == row.id }),
                   let media = unsaved[entry.id]?.remove(at: index) {
                    result.append(.unsaved(rowID: media.row, media.media))
                }
                next += 1
            }
            // At Conversation the entire Working group can be hidden, but
            // its media notice still precedes the following reply.
            flush(entry.id)
        }
        return result
    }
}

/// A file, picture or page an agent task made. The Mac serves a file only
/// from the chat it was saved to, and an agent task's history is read from
/// its provider rather than saved into the parent chat, so the phone has no
/// verified place to download it from; asking the parent chat would be a
/// guess. Say where to see it instead of leaving a gap.
struct ProjectSubagentFileNotice: View {
    private let title: String
    private let detail: String
    private let symbol: String
    private let identifier: String
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography

    init(files: [ConversationFile]) {
        title = Self.title(files)
        detail = Self.detail(files)
        symbol = Self.symbol(files)
        identifier = "project-subagent-file:" + (files.first?.id ?? "")
    }

    /// Media the provider recorded without a saved file: its name when the
    /// provider gives one, else what it was.
    init(unsaved media: UnsavedToolMedia, rowID: String) {
        let picture = media.kind == .picture
        title = media.name ?? (media.count == 1 ? (picture ? "A picture" : "A recording")
            : "\(media.count) " + (picture ? "pictures" : "recordings"))
        detail = media.count == 1 ? "Open it on your computer to \(picture ? "see" : "hear") it."
            : "Open them on your computer to \(picture ? "see" : "hear") them."
        symbol = picture ? (media.count == 1 ? "photo" : "photo.on.rectangle") : "waveform"
        identifier = "project-subagent-media:" + rowID
    }

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text(title).fontWeight(.medium).lineLimit(2)
                Text(detail).foregroundStyle(theme.secondaryText)
            }
        } icon: {
            Image(systemName: symbol)
        }
        .font(typography.font(.footnote))
        .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(identifier)
    }

    static func title(_ files: [ConversationFile]) -> String {
        guard files.count == 1, let file = files.first else { return "\(files.count) pictures" }
        return isPage(file) ? PublishedPageView.title(of: file) : file.name
    }

    static func detail(_ files: [ConversationFile]) -> String {
        files.count == 1 ? "Open it on your computer to see it." : "Open them on your computer to see them."
    }

    static func symbol(_ files: [ConversationFile]) -> String {
        guard files.count == 1, let file = files.first else { return "photo.on.rectangle" }
        let mime = (file.mimeType ?? "").lowercased()
        if isPage(file) { return "doc.richtext" }
        if mime.hasPrefix("image/") { return "photo" }
        if mime.hasPrefix("video/") { return "play.rectangle" }
        if mime.hasPrefix("audio/") { return "waveform" }
        return "doc"
    }

    private static func isPage(_ file: ConversationFile) -> Bool {
        ConversationFile.publishedPageTypes.contains(file.mimeType?.lowercased() ?? "")
    }
}

/// One compact entry point for the Mac and the windows the agent is using.
struct ComputerDock: View {
    var pendingDialogs = 0
    var dialogSummary: String? = nil
    var isPresented = false
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            Image(systemName: ConceptSymbol.computer)
                .modifier(ComposerStatusPill(isActive: isPresented))
                .modifier(ComputerDialogBadge(count: pendingDialogs))
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("computer-status-pill")
        .accessibilityLabel("View computer")
        .accessibilityValue(accessibilityState)
        .accessibilityAddTraits(isPresented ? .isSelected : [])
        .accessibilityHint(isPresented ? "Return to the chat" : "Open the viewer and choose a window or screen")
    }

    private var accessibilityState: String {
        let summary = dialogSummary ?? MacDialogNotice.countLabel(pendingDialogs)
        return (isPresented ? "Open" : "Closed") + (summary.isEmpty ? "" : ", " + summary)
    }
}

/// A corner badge leaves the computer icon and its hit target in the same place
/// as attention arrives. The parent speaks the full count and descriptions.
private struct ComputerDialogBadge: ViewModifier {
    let count: Int
    @Environment(\.wonderTheme) private var theme
    func body(content: Content) -> some View {
        content.overlay(alignment: .topTrailing) {
            if count > 0 {
                Text("\(count)")
                    .font(.caption2.weight(.semibold)).monospacedDigit()
                    .padding(.horizontal, 4)
                    .frame(minWidth: 16, minHeight: 16)
                    .foregroundStyle(theme.page)
                    .background(theme.text, in: Capsule())
                    .fixedSize()
                    .dynamicTypeSize(...DynamicTypeSize.xxxLarge)
                    .accessibilityHidden(true)
                    .allowsHitTesting(false)
            }
        }
    }
}

/// A host-scoped leaf keeps notification state out of the conversation and
/// new-chat view dependencies. Re-create its subscription when pairing changes.
struct MacDialogComputerEntry: View {
    let host: String
    let name: String
    var dock = false
    var isPresented = false
    let open: () -> Void
    var body: some View {
        MacDialogComputerControl(host: host, name: name, dock: dock, isPresented: isPresented, open: open).id(host)
    }
}
private struct MacDialogComputerControl: View {
    let host: String
    let name: String
    let dock: Bool
    let isPresented: Bool
    let open: () -> Void
    @StateObject private var dialogs: MacDialogCount
    init(host: String, name: String, dock: Bool, isPresented: Bool, open: @escaping () -> Void) {
        self.host = host; self.name = name; self.dock = dock; self.isPresented = isPresented; self.open = open
        _dialogs = StateObject(wrappedValue: MacDialogCount(host: host))
    }
    var body: some View {
        if dock {
            ComputerDock(pendingDialogs: dialogs.count, dialogSummary: dialogs.accessibilityValue, isPresented: isPresented, open: open)
        } else {
            Button(action: open) {
                HStack(spacing: 8) {
                    Image(systemName: ConceptSymbol.computer)
                        .foregroundStyle(.secondary)
                        .dynamicTypeSize(...DynamicTypeSize.xxxLarge)
                        .frame(width: 28, height: 28)
                        .modifier(ComputerDialogBadge(count: dialogs.count))
                    Text("View computer").foregroundStyle(.primary)
                }
                .font(.callout)
                .frame(minHeight: 44)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityValue(dialogs.accessibilityValue)
            .accessibilityHint("Shows the screen of \(name)")
            .accessibilityIdentifier("view-computer")
        }
    }
}

/// A dismissed banner stays reachable without redirecting the Computer entry.
/// Its subscription belongs to this viewer's current computer, not its chat.
/// One compact badge, never a row: it floats beside the computer's name and
/// jumps straight to the waiting dialog.
struct MacDialogViewerChip: View {
    @StateObject private var dialogs: MacDialogCount
    let enabled: Bool
    let open: () -> Void
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    init(host: String, enabled: Bool, open: @escaping () -> Void) {
        _dialogs = StateObject(wrappedValue: MacDialogCount(host: host))
        self.enabled = enabled
        self.open = open
    }
    var body: some View {
        if dialogs.count > 0 {
            Button(action: open) {
                HStack(spacing: 6) {
                    Image(systemName: "exclamationmark.bubble.fill").foregroundStyle(.orange)
                    if !dynamicTypeSize.isAccessibilitySize {
                        Text(dialogs.count == 1 ? "1 dialog" : "\(dialogs.count) dialogs").lineLimit(1)
                    }
                }
                .font(.footnote.weight(.semibold))
                .padding(.horizontal, 12).padding(.vertical, 7)
                .background(.regularMaterial, in: Capsule())
                .frame(minHeight: 44)
                .contentShape(Capsule())
            }
            .buttonStyle(.plain)
            .dynamicTypeSize(...DynamicTypeSize.accessibility2)
            .disabled(!enabled)
            .accessibilityLabel("Mac dialog waiting")
            .accessibilityValue(dialogs.accessibilityValue)
            .accessibilityHint("Show a current dialog window on this computer")
            .accessibilityAction(.default, open)
            .accessibilityIdentifier("computer-session-dialogs")
        }
    }
}

struct FilesDock: View {
    let isPresented: Bool
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            Label("Files", systemImage: "folder")
                .labelStyle(.iconOnly)
                .modifier(ComposerStatusPill(isActive: isPresented))
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("conversation-files-pill")
        .accessibilityLabel("Files")
        .accessibilityValue(isPresented ? "Open" : "Closed")
        .accessibilityAddTraits(isPresented ? .isSelected : [])
        .accessibilityHint(isPresented ? "Return to the chat" : "Browse this chat’s files and changes")
    }
}

/// Pending permission requests and questions. Like Files, it replaces the
/// conversation with their review and returns to it.
struct AttentionDock: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let isPresented: Bool
    let toggle: () -> Void
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        // Only a pending question's answer window needs the clock, and only
        // at its deadline; otherwise the pill changes with the model alone.
        TimelineView(AnswerDeadlineSchedule(questions: model.asyncQuestions[chat.id] ?? [])) { context in
            pill(at: AnswerDeadlineSchedule.now(context.date))
        }
    }

    @ViewBuilder private func pill(at now: Date) -> some View {
        let pending = PendingAttention(model: model, chat: chat, now: now)
        if pending.count > 0 {
            Button(action: toggle) {
                // At accessibility sizes the pill row has no room for the word: the
                // icon stays, like Files and the computer, and VoiceOver reads the label.
                Group {
                    if typeSize.isAccessibilitySize {
                        Label(pending.label, systemImage: pending.symbol).labelStyle(.iconOnly)
                    } else {
                        Label(pending.label, systemImage: pending.symbol)
                    }
                }
                .modifier(ComposerStatusPill(isActive: isPresented))
            }
            .buttonStyle(.plain)
            .accessibilityLabel(pending.label)
            .accessibilityIdentifier("question-dock-open")
            .accessibilityValue(isPresented ? "Open" : "Closed")
            .accessibilityAddTraits(isPresented ? .isSelected : [])
            .accessibilityHint(isPresented ? "Return to the chat" : "Answer what the agent is waiting for")
        }
    }
}

/// A goal thread with no messages yet: the agent works toward the goal by
/// itself, so the empty conversation says so instead of looking idle.
struct GoalRunningNotice: View {
    let objective: String
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Group {
                if reduceMotion { Image(systemName: "hourglass") } else { ProgressView() }
            }
            .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text("Goal running").font(.subheadline.weight(.semibold))
                Text(objective).font(.subheadline).foregroundStyle(theme.secondaryText).lineLimit(3)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 8)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("goal-running")
    }
}

/// A compact status control. The sheet owns only presentation and draft state;
/// the connection model remains the source of truth for Goal mutations.
struct GoalDock: View {
    let goal: ConversationGoal
    let error: String?
    @Binding var isPresented: Bool
    let save: (String, Int?, Int?) async -> Bool
    let pause: () async -> Void
    let resume: () async -> Void
    let clear: () async -> Void
    var contentTypeSize: DynamicTypeSize? = nil
    @Environment(\.horizontalSizeClass) private var sizeClass
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.composerPillsInBar) private var inBar
    @Environment(\.shellWindowSize) private var window
    @State private var popoverContentHeight: CGFloat = 320
    @ScaledMetric(relativeTo: .subheadline) private var iconSize: CGFloat = 20

    private var presentsPopover: Bool { inBar || sizeClass == .regular }
    /// Accessibility text wraps a 320-point body into a tall column; a wider
    /// body keeps the controls visible without scrolling, inside the display.
    private var popoverWidth: CGFloat {
        (contentTypeSize ?? typeSize).isAccessibilitySize ? min(480, max(320, (window?.width ?? 400) - 48)) : 320
    }

    private var statusLabel: String {
        switch goal.status {
        case "active": "active"
        case "paused": "paused"
        case "blocked": "needs attention"
        case "usageLimited": "usage limited"
        case "budgetLimited": "budget reached"
        case "complete": "complete"
        default: "updated"
        }
    }

    private var statusSymbol: String {
        switch goal.status {
        case "active": "hourglass"
        case "paused", "usageLimited", "budgetLimited": "pause.fill"
        case "complete": "checkmark"
        case "blocked": "exclamationmark.circle"
        default: "questionmark.circle"
        }
    }

    @ViewBuilder private var statusIcon: some View {
        Image(systemName: statusSymbol)
    }

    var body: some View {
        Button { isPresented.toggle() } label: {
            Group {
                if inBar {
                    VStack(spacing: 1) {
                        Image(systemName: "scope").resizable().scaledToFit().frame(width: 20, height: 20)
                        Image(systemName: statusSymbol).resizable().scaledToFit().frame(width: 10, height: 10)
                    }
                } else {
                    HStack(spacing: 6) {
                        Image(systemName: "scope")
                            .frame(width: iconSize, height: iconSize)
                        statusIcon
                            .frame(width: iconSize, height: iconSize)
                    }
                }
            }
            .accessibilityHidden(true)
            .modifier(ComposerStatusPill(isActive: isPresented))
            .pillPopover(isPresented: presentsPopover ? $isPresented : .constant(false)) {
                details(wide: true)
                    .frame(width: popoverWidth)
                    .frame(idealHeight: min(popoverContentHeight + 44, max(200, (window?.height ?? 640) - 100)),
                           maxHeight: min(popoverContentHeight + 44, max(200, (window?.height ?? 640) - 100)))
            }
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("goal-status-pill")
        .accessibilityLabel("Goal \(statusLabel)")
        .accessibilityValue(isPresented ? "Expanded" : "Collapsed")
        .accessibilityAddTraits(isPresented ? .isSelected : [])
        .accessibilityHint("Show goal details and controls")
        .pillSheet(isPresented: presentsPopover ? .constant(false) : $isPresented, wide: false) {
            details(wide: sizeClass == .regular)
        }
    }

    private func details(wide: Bool) -> some View {
        GoalDetailsSheet(goal: goal, error: error, wide: wide, save: save, pause: pause, resume: resume, clear: clear,
                         close: { isPresented = false },
                         contentHeightChanged: { height in if presentsPopover { popoverContentHeight = height } })
            .environment(\.dynamicTypeSize, contentTypeSize ?? typeSize)
    }
}

private struct GoalDetailsSheet: View {
    @Environment(\.wonderTheme) private var theme
    let goal: ConversationGoal
    let error: String?
    let wide: Bool
    let save: (String, Int?, Int?) async -> Bool
    let pause: () async -> Void
    let resume: () async -> Void
    let clear: () async -> Void
    let close: () -> Void
    let contentHeightChanged: (CGFloat) -> Void

    @State private var editing = false
    @State private var objectiveDraft = ""
    @State private var budgetDraft = ""
    @State private var timeBudgetDraft = ""
    @State private var originalTimeBudgetDraft = ""
    @State private var validationError: String?
    @State private var saving = false
    @State private var showingRemoveConfirmation = false
    @State private var detent: PresentationDetent = .medium

    private var statusTitle: String {
        switch goal.status {
        case "active": "Active"
        case "paused": "Paused"
        case "blocked": "Needs attention"
        case "usageLimited": "Usage limit reached"
        case "budgetLimited": "Budget reached"
        case "complete": "Complete"
        default: "Status unavailable"
        }
    }

    private var elapsedText: String {
        let seconds = goal.createdAt.map { max(0, Int(Date().timeIntervalSince($0))) } ?? goal.timeUsedSeconds
        return seconds.map(durationText) ?? "Unavailable"
    }

    private func durationText(_ seconds: Int) -> String {
        let minutes = max(0, seconds) / 60
        let hours = minutes / 60
        return hours > 0 ? "\(hours) hr \(minutes % 60) min" : "\(minutes) min"
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    if editing {
                        Text("Objective").font(.headline)
                        TextEditor(text: $objectiveDraft)
                            .frame(minHeight: 100)
                            .padding(6)
                            .background(theme.surface, in: RoundedRectangle(cornerRadius: 12))
                            .accessibilityIdentifier("goal-objective-editor")
                        TextField("Token budget (optional)", text: $budgetDraft)
                            .keyboardType(.numberPad)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityIdentifier("goal-budget-editor")
                        TextField("Time limit in minutes (optional)", text: $timeBudgetDraft)
                            .keyboardType(.numberPad)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityIdentifier("goal-time-budget-editor")
                        Text("Only active work counts toward the time limit.")
                            .font(.footnote).foregroundStyle(.secondary)
                        if let validationError {
                            Text(validationError).font(.footnote).foregroundStyle(.red)
                        }
                    } else {
                        Text(goal.objective)
                            .font(.body)
                            .textSelection(.enabled)
                            .accessibilityIdentifier("goal-objective")
                        actionRow
                    }

                    VStack(alignment: .leading, spacing: 12) {
                        metadataRow {
                            metadata(statusTitle, icon: statusSymbol, meaning: "Status, " + statusTitle, id: "status")
                            metadata(elapsedText, icon: "clock", meaning: "Elapsed, " + elapsedText, id: "elapsed")
                        }
                        metadataRow {
                        if let tokens = goal.tokensUsed {
                            let usage = tokens.formatted()
                            let value = goal.tokenBudget.map { usage + " / " + $0.formatted() + " tokens" } ?? usage + " tokens"
                            let meaning = "Tokens used, " + usage + (goal.tokenBudget.map { ". Token budget, " + $0.formatted() } ?? "")
                            metadata(value, icon: "number.circle", meaning: meaning, id: "tokens")
                        } else {
                            let budget = goal.tokenBudget.map { " · " + $0.formatted() + " budget" } ?? ""
                            metadata("Usage unavailable" + budget, icon: "number.circle",
                                     meaning: "Token usage unavailable" + (goal.tokenBudget.map { ". Token budget, " + $0.formatted() } ?? ""),
                                     id: "tokens")
                        }
                        if let seconds = goal.timeBudgetSeconds {
                            let minutes = max(1, (seconds + 59) / 60)
                            let used = goal.timeUsedSeconds.map { "\(max(0, $0) / 60)" } ?? "—"
                            let meaning = goal.timeUsedSeconds.map { "Active time, " + durationText($0) } ?? "Active time unavailable"
                            metadata("\(used) / \(minutes) min", icon: "timer",
                                     meaning: meaning + ". Time limit, \(minutes) min",
                                     id: "time")
                        }
                        }
                    }
                    .font(.subheadline)
                    .foregroundStyle(.secondary)

                    if let error {
                        Text(error)
                            .font(.footnote)
                            .foregroundStyle(.red)
                            .accessibilityIdentifier("goal-error")
                    }

                    if saving { ProgressView("Updating goal…") }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding()
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { contentHeightChanged($0) }
            }
            .wonderPage()
            .accessibilityIdentifier("goal-details-scroll")
            .navigationTitle("Goal")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if editing {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel") { editing = false; validationError = nil }
                            .disabled(saving)
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Save") { saveDraft() }
                            .disabled(saving || objectiveDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                            .accessibilityIdentifier("goal-save")
                    }
                }
            }
            .modifier(PillSheet(wide: wide, detent: $detent))
            .confirmationDialog("Remove this goal?", isPresented: $showingRemoveConfirmation, titleVisibility: .visible) {
                Button("Remove goal", role: .destructive) {
                    perform(clear)
                }
            } message: {
                Text("This removes the goal from this chat.")
            }
        }
    }

    private var statusSymbol: String {
        switch goal.status {
        case "active": "play.circle"
        case "paused", "usageLimited", "budgetLimited": "pause.circle"
        case "complete": "checkmark.circle"
        case "blocked": "exclamationmark.circle"
        default: "questionmark.circle"
        }
    }

    private func metadataRow<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .top, spacing: 20) { content() }
                .fixedSize(horizontal: true, vertical: false)
            VStack(alignment: .leading, spacing: 12) { content() }
        }
    }

    private func metadata(_ value: String, icon: String, meaning: String, id: String) -> some View {
        Label(value, systemImage: icon)
            .fixedSize(horizontal: false, vertical: true)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(meaning)
            .accessibilityIdentifier("goal-metadata-" + id)
    }

    private var actionRow: some View {
        HStack(spacing: 16) {
            Button {
                objectiveDraft = goal.objective
                budgetDraft = goal.tokenBudget.map(String.init) ?? ""
                timeBudgetDraft = goal.timeBudgetSeconds.map { String(($0 + 59) / 60) } ?? ""
                originalTimeBudgetDraft = timeBudgetDraft
                editing = true
                detent = .large
            } label: {
                Label("Edit goal", systemImage: "pencil").frame(minWidth: 48, minHeight: 48)
                    .contentShape(Rectangle())
            }
            .accessibilityIdentifier("goal-edit")
            if goal.status == "active" {
                Button { perform(pause) } label: {
                    Label("Pause goal", systemImage: "pause.fill").frame(minWidth: 48, minHeight: 48)
                    .contentShape(Rectangle())
                }
                .accessibilityIdentifier("goal-pause")
            } else if goal.status != "complete" {
                Button { perform(resume) } label: {
                    Label("Resume goal", systemImage: "play.fill").frame(minWidth: 48, minHeight: 48)
                    .contentShape(Rectangle())
                }
                .accessibilityIdentifier("goal-resume")
            }
            Spacer(minLength: 0)
            Button(role: .destructive) { showingRemoveConfirmation = true } label: {
                Label("Remove goal", systemImage: "trash").frame(minWidth: 48, minHeight: 48)
                    .contentShape(Rectangle())
            }
            .foregroundStyle(.red)
            .accessibilityIdentifier("goal-remove")
        }
        .labelStyle(.iconOnly)
        .font(.title3)
        .buttonStyle(.plain)
        .foregroundStyle(.tint)
        .disabled(saving)
    }

    private func saveDraft() {
        let objective = objectiveDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !objective.isEmpty else { validationError = "Enter a goal."; return }
        let budgetText = budgetDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        let budget: Int?
        if budgetText.isEmpty {
            budget = nil
        } else if let value = Int(budgetText), value > 0 {
            budget = value
        } else {
            validationError = "Enter a positive token budget."; return
        }
        let timeText = timeBudgetDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        let timeBudgetSeconds: Int?
        if timeText == originalTimeBudgetDraft {
            timeBudgetSeconds = goal.timeBudgetSeconds
        } else if timeText.isEmpty {
            timeBudgetSeconds = nil
        } else if let minutes = Int(timeText), minutes > 0, minutes <= Int.max / 60 {
            timeBudgetSeconds = minutes * 60
        } else {
            validationError = "Enter a positive time limit in minutes."; return
        }
        validationError = nil
        guard !saving else { return }
        saving = true
        Task { @MainActor in
            if await save(objective, budget, timeBudgetSeconds) { editing = false }
            saving = false
        }
    }

    private func perform(_ action: @escaping () async -> Void) {
        guard !saving else { return }
        saving = true
        Task { @MainActor in
            await action()
            saving = false
        }
    }
}
