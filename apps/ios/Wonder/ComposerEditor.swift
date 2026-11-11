import SwiftUI
import UniformTypeIdentifiers
import WonderPairing

/// Dismiss only composer editing. A timeline link, menu or text selection may
/// already own the responder by the time its simultaneous tap finishes.
@MainActor enum ComposerKeyboard {
    static func dismiss() {
        UIApplication.shared.sendAction(#selector(ComposerTextView.dismissComposerKeyboard), to: nil, from: nil, for: nil)
    }
}

/// The conversation's message editor. It observes only its chat's typed text,
/// not the connection model, so a keystroke redraws and re-measures the editor
/// alone; text another owner changes (a send, a restore) arrives the same way.
struct ComposerMessageEditor: View {
    @ObservedObject var draft: ComposerDraftText
    let onType: (String) -> Void
    let maximumLines: Int
    let label: String
    let editable: Bool
    var placeholder: String?
    var acceptSuggestion: (() -> Bool)?
    var dictation: DictationController?
    var conversationID: String?
    var canPasteImages = false
    var pasteImages: (([NSItemProvider]) -> Void)?
    var completion: ComposerCompletionModel?

    var body: some View {
        #if WONDER_DIAGNOSTICS
        let _ = RenderCounts.composerEditor += 1
        #endif
        let typed = self.draft
        BoundedComposerEditor(text: Binding(get: { typed.text }, set: onType),
            maximumLines: maximumLines, label: label, editable: editable, placeholder: placeholder,
            acceptSuggestion: acceptSuggestion, dictation: dictation, conversationID: conversationID,
            canPasteImages: canPasteImages, pasteImages: pasteImages, completion: completion)
    }
}

/// Explicitly bounded UIKit sizing avoids the vertical SwiftUI TextField's
/// intrinsic-size feedback when a full dictation result replaces a short draft.
struct BoundedComposerEditor: UIViewRepresentable {
    @Binding var text: String
    let maximumLines: Int
    let label: String
    let editable: Bool
    var placeholder: String?
    var acceptSuggestion: (() -> Bool)?
    var dictation: DictationController?
    var conversationID: String?
    var didCommitDictation: (() -> Void)?
    var canPasteImages = false
    var pasteImages: (([NSItemProvider]) -> Void)?
    /// The inline `/` and `@` lists, when the provider offers either.
    var completion: ComposerCompletionModel?

    func makeUIView(context: Context) -> ComposerTextView {
        // TextKit 1 avoids the TextKit 2 re-layout path seen in the hang sample.
        let view = ComposerTextView(usingTextLayoutManager: false)
        view.delegate = context.coordinator
        context.coordinator.view = view
        view.backgroundColor = .clear
        view.textColor = context.environment.wonderTheme.textUIColor
        view.accessibilityIdentifier = "message-draft"
        #if WONDER_DIAGNOSTICS
        view.focusWhenAttached = ProcessInfo.processInfo.arguments.contains("-diagnostics-composer-keyboard")
        #endif
        view.textContainerInset = UIEdgeInsets(top: 12, left: 0, bottom: 12, right: 0)
        view.isScrollEnabled = true
        view.adjustsFontForContentSizeCategory = true
        view.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        return view
    }

    func updateUIView(_ view: ComposerTextView, context: Context) {
        if context.coordinator.parent.conversationID != conversationID {
            context.coordinator.parent.dictation?.captureControlsHidden(conversationID: context.coordinator.parent.conversationID ?? "")
        }
        context.coordinator.parent = self
        if let conversationID { dictation?.attachEditor(context.coordinator, conversationID: conversationID) }
        view.canPasteImages = canPasteImages
        view.pasteImages = pasteImages
        if let completion {
            if completion.editor !== context.coordinator { completion.editor = context.coordinator }
            if view.completion !== completion { view.completion = completion }
        } else if view.completion != nil {
            view.completion = nil
        }
        let font = context.environment.wonderTypography.uiFont(.body)
        if view.font != font { view.font = font }
        let textColor = context.environment.wonderTheme.textUIColor
        if view.textColor != textColor { view.textColor = textColor }
        if view.isEditable != editable {
            if !editable {
                // Disabling the editor resigns it, which re-enters the SwiftUI
                // graph mid-update and spins forever (AttributeGraph cycle on send).
                // Not only when focused: UIKit's accessibility resigns it anyway
                // (measured hang on a Duo send, 11-05).
                DispatchQueue.main.async { [weak view, weak coordinator = context.coordinator] in
                    guard let view, let editable = coordinator?.parent.editable, view.isEditable != editable else { return }
                    view.isEditable = editable
                }
            } else {
                view.isEditable = editable
            }
        }
        if view.accessibilityLabel != label { view.accessibilityLabel = label }
        view.placeholderColor = UIColor(context.environment.wonderTheme.secondaryText)
        view.placeholder = placeholder
        view.acceptSuggestion = acceptSuggestion == nil ? nil : { [weak coordinator = context.coordinator] in
            coordinator?.acceptPromptSuggestion() ?? false
        }
        if view.markedTextRange != nil {
            context.coordinator.beginComposition()
            return
        }
        // A disabled model may have rejected an IME commit during upload. Retry
        // after this SwiftUI update, using the latest draft rather than stale text.
        if context.coordinator.pendingCommit != nil || context.coordinator.compositionBase != nil {
            context.coordinator.scheduleCommit(view)
            return
        }
        if let projection = context.coordinator.projection {
            if projection.base == text { return }
            // A send, restoration or external edit replaced the owning draft.
            let controller = dictation, conversation = conversationID
            DispatchQueue.main.async {
                if controller?.nativeConversationID == conversation { controller?.cancelNativeForExternalEdit() }
            }
        }
        let replaced = view.text != text
        context.coordinator.replaceText(text, in: view)
        context.coordinator.lastSynchronizedText = text
        // A send, restore or other outside edit can end or move a trigger.
        if replaced { context.coordinator.notifyCompletion(view) }
    }

    func sizeThatFits(_ proposal: ProposedViewSize, uiView: ComposerTextView, context: Context) -> CGSize? {
        guard let width = proposal.width, width > 0, width.isFinite else { return nil }
        let font = uiView.font ?? UIFont.preferredFont(forTextStyle: .body)
        let insets = uiView.textContainerInset
        let maximum = max(44, font.lineHeight * CGFloat(maximumLines) + insets.top + insets.bottom)
        let measured = context.coordinator.measurer.height(of: ComposerHeightMeasurer.Key(
            text: uiView.text ?? "", font: font, width: width - insets.left - insets.right,
            padding: uiView.textContainer.lineFragmentPadding, maximumLines: maximumLines))
        return CGSize(width: width, height: min(maximum, max(44, ceil(measured + insets.top + insets.bottom))))
    }

    static func dismantleUIView(_ view: ComposerTextView, coordinator: Coordinator) {
        coordinator.parent.dictation?.captureControlsHidden(conversationID: coordinator.parent.conversationID ?? "")
    }
    func makeCoordinator() -> Coordinator { Coordinator(self) }
    final class Coordinator: NSObject, UITextViewDelegate, DictationEditor, ComposerCompletionEditor {
        weak var view: UITextView?
        /// Reports the committed draft and caret; never while IME text is
        /// marked, dictation is showing or a commit is still pending.
        func notifyCompletion(_ textView: UITextView) {
            guard let completion = parent.completion, textView.markedTextRange == nil, projection == nil,
                  pendingCommit == nil, compositionBase == nil, textView.text == parent.text else { return }
            completion.editorChanged(text: parent.text, selection: textView.selectedRange)
        }
        func replaceCompletion(_ trigger: ComposerTrigger, with insertion: String) -> Bool {
            guard let view, parent.editable, view.markedTextRange == nil, projection == nil,
                  pendingCommit == nil, compositionBase == nil, view.text == parent.text,
                  ComposerTrigger.detect(in: parent.text, selection: view.selectedRange,
                                         commands: trigger.isCommand, files: !trigger.isCommand) == trigger,
                  let result = trigger.replacing(in: parent.text, with: insertion) else { return false }
            parent.text = result.text
            guard parent.text == result.text else { return false }
            lastSynchronizedText = result.text
            replaceText(result.text, in: view)
            isCommitting = true
            view.selectedRange = NSRange(location: result.caret, length: 0)
            isCommitting = false
            (view as? ComposerTextView)?.updatePlaceholder()
            notifyCompletion(view)
            return true
        }
        func acceptPromptSuggestion() -> Bool {
            guard let view, parent.editable, view.text.isEmpty, parent.text.isEmpty,
                  view.markedTextRange == nil, projection == nil, compositionBase == nil, pendingCommit == nil,
                  parent.acceptSuggestion?() == true else { return false }
            replaceText(parent.text, in: view)
            lastSynchronizedText = parent.text
            view.selectedRange = NSRange(location: (parent.text as NSString).length, length: 0)
            view.becomeFirstResponder()
            return true
        }
        var projection: DictationProjection? {
            didSet { (view as? ComposerTextView)?.isDictating = projection != nil }
        }
        var dictationLocale: String { view?.textInputMode?.primaryLanguage ?? Locale.preferredLanguages.first ?? Locale.current.identifier }
        func beginDictation() -> Bool {
            guard let view, parent.editable, view.markedTextRange == nil, pendingCommit == nil,
                  compositionBase == nil, view.text == parent.text else { return false }
            projection = DictationProjection(base: parent.text, selection: view.selectedRange)
            return true
        }
        func showDictation(_ words: String) -> Bool {
            guard let view, var next = projection, next.base == parent.text, view.markedTextRange == nil else { return false }
            let previous = next
            guard next.update(words) else { return false }
            let selection = next.selection(afterReplacing: previous, selection: view.selectedRange)
            projection = next
            replaceText(next.text, in: view)
            isCommitting = true
            view.selectedRange = selection
            isCommitting = false
            // The binding keeps the draft's base while words stream in, so SwiftUI
            // has no reason to update this view; ask it to measure the new text now.
            view.invalidateIntrinsicContentSize()
            return true
        }
        func endDictation(commitWords: Bool) -> Bool {
            guard let view, let projection else { return false }
            guard projection.base == parent.text else {
                self.projection = nil
                replaceText(parent.text, in: view)
                return false
            }
            let selection = view.selectedRange
            if commitWords {
                parent.text = projection.text
                guard parent.text == projection.text else { return false }
                parent.didCommitDictation?()
            }
            self.projection = nil
            lastSynchronizedText = parent.text
            replaceText(parent.text, in: view)
            if !commitWords {
                let original = DictationProjection(base: projection.base, selection: projection.range)
                let dictatedEnd = projection.range.location + (projection.insertion as NSString).length
                view.selectedRange = !projection.transcript.isEmpty && selection == NSRange(location: dictatedEnd, length: 0)
                    ? projection.range : (projection.transcript.isEmpty ? selection : original.selection(afterReplacing: projection, selection: selection))
            }
            return true
        }
        func textView(_ textView: UITextView, shouldChangeTextIn range: NSRange, replacementText text: String) -> Bool {
            if projection != nil { parent.dictation?.finishNativeImmediately() }
            return projection == nil
        }
        var parent: BoundedComposerEditor
        let measurer = ComposerHeightMeasurer()
        var lastSynchronizedText: String
        var compositionBase: String?
        var pendingCommit: ComposerComposition.PendingCommit?
        private var isCommitting = false
        private var commitScheduled = false
        init(_ parent: BoundedComposerEditor) { self.parent = parent; lastSynchronizedText = parent.text }
        func beginComposition() { if compositionBase == nil { compositionBase = lastSynchronizedText } }
        func textViewDidChange(_ textView: UITextView) {
            (textView as? ComposerTextView)?.updatePlaceholder()
            commit(textView)
        }
        func textViewDidChangeSelection(_ textView: UITextView) {
            if projection != nil, !isCommitting { parent.dictation?.finishNativeImmediately() }
            if compositionBase != nil, textView.markedTextRange == nil { commit(textView) }
            if !isCommitting { notifyCompletion(textView) }
        }
        func textViewDidBeginEditing(_ textView: UITextView) { notifyCompletion(textView) }
        func textViewDidEndEditing(_ textView: UITextView) {
            textView.unmarkText()
            commit(textView)
            parent.completion?.editorEnded()
        }
        func scheduleCommit(_ textView: UITextView) {
            guard parent.editable, !commitScheduled else { return }
            commitScheduled = true
            DispatchQueue.main.async { [weak self, weak textView] in
                guard let self else { return }
                self.commitScheduled = false
                guard let textView, self.parent.editable else { return }
                self.commit(textView)
            }
        }
        private func commit(_ textView: UITextView) {
            guard !isCommitting, projection == nil else { return }
            guard textView.markedTextRange == nil else { beginComposition(); return }
            let committed = textView.text ?? ""
            let pending = pendingCommit?.replacingCommittedText(committed) ?? ComposerComposition.PendingCommit(
                base: compositionBase ?? lastSynchronizedText, committed: committed)
            pendingCommit = pending
            guard parent.editable else { return }
            isCommitting = true
            defer { isCommitting = false }
            guard let merged = pending.apply(to: parent.text, save: { text in
                if self.parent.text != text { self.parent.text = text }
                return self.parent.text
            }) else { return }
            // Clear only after the binding acknowledges the text. Programmatic
            // text/selection delegate callbacks cannot recursively merge it again.
            pendingCommit = nil; compositionBase = nil; lastSynchronizedText = merged
            replaceText(merged, in: textView)
            notifyCompletion(textView)
        }
        func replaceText(_ text: String, in view: UITextView) {
            guard view.text != text else { return }
            let wasCommitting = isCommitting
            isCommitting = true
            defer { isCommitting = wasCommitting }
            let selection = view.selectedRange
            view.text = text
            let count = (text as NSString).length
            let location = min(selection.location, count)
            view.selectedRange = NSRange(location: location, length: min(selection.length, count - location))
            #if WONDER_DIAGNOSTICS
            // A seeded preview models typing at the end. Ordinary draft restores
            // retain their selection, including when the fixture is not active.
            if DiagnosticSubagentFixture.composerPreviewText == text {
                view.selectedRange = NSRange(location: count, length: 0)
            }
            #endif
        }
    }
}

/// The editor's height lays out at most one line past its cap, in its own
/// TextKit stack, so a long draft is not measured in full on every layout.
/// The editor's own text container stays unbounded and scrolls. The last
/// result is reused only for the same text, font, width and line limit.
final class ComposerHeightMeasurer {
    struct Key: Equatable {
        let text: String
        let font: UIFont
        let width: CGFloat
        let padding: CGFloat
        let maximumLines: Int
    }
    private let storage = NSTextStorage()
    private let layout = NSLayoutManager()
    private let container = NSTextContainer(size: .zero)
    private var last: (key: Key, height: CGFloat)?
    /// Characters laid out by the last measurement, for tests.
    private(set) var laidOutCharacters = 0

    init() {
        layout.addTextContainer(container)
        storage.addLayoutManager(layout)
        container.lineBreakMode = .byWordWrapping
    }

    /// The text's height, without insets, up to `maximumLines + 1` lines.
    func height(of key: Key) -> CGFloat {
        if let last, last.key == key { return last.height }
        container.size = CGSize(width: max(0, key.width), height: .greatestFiniteMagnitude)
        container.lineFragmentPadding = key.padding
        container.maximumNumberOfLines = key.maximumLines + 1
        // An empty draft is one line at TextKit's line height, as on screen.
        storage.setAttributedString(NSAttributedString(string: key.text.isEmpty ? " " : key.text, attributes: [.font: key.font]))
        layout.ensureLayout(for: container)
        laidOutCharacters = layout.characterRange(forGlyphRange: layout.glyphRange(for: container), actualGlyphRange: nil).length
        // A trailing newline's empty last line counts, as it does on screen.
        let extra = layout.extraLineFragmentTextContainer === container ? layout.extraLineFragmentUsedRect.maxY : 0
        let height = max(key.font.lineHeight, layout.usedRect(for: container).maxY, extra)
        last = (key, height)
        return height
    }
}

/// Keep Paste in the system edit menu, including for an image-only clipboard.
/// Inspect type metadata while building the menu; read contents only on Paste.
/// The keyboard's paste suggestion (iOS 27) and drops paste item providers
/// instead of calling paste(_:), so images are routed from that path too.
final class ComposerTextView: UITextView, UITextPasteDelegate {
    @objc func dismissComposerKeyboard() { resignFirstResponder() }
    #if WONDER_DIAGNOSTICS
    var focusWhenAttached = false
    override func didMoveToWindow() {
        super.didMoveToWindow()
        guard focusWhenAttached, let attached = window else { return }
        focusWhenAttached = false
        Task { @MainActor [weak self] in
            await Task.yield()
            guard let self, self.window === attached else { return }
            self.becomeFirstResponder()
        }
    }
    #endif
    // UIKit's usingTextLayoutManager factory bypasses Swift property
    // initializers, so stored state here must be valid when zero-filled.
    var placeholder: String? { didSet { if placeholder != oldValue { updatePlaceholder() } } }
    var placeholderColor: UIColor? { didSet { if placeholderColor != oldValue { updatePlaceholder() } } }
    var isDictating = false { didSet { updatePlaceholder() } }
    var acceptSuggestion: (() -> Bool)? { didSet { updatePlaceholder() } }
    private var placeholderLabel: UILabel?
    override var text: String! { didSet { updatePlaceholder() } }
    override var attributedText: NSAttributedString! { didSet { updatePlaceholder() } }

    // The displayed text and its placeholder share one rendering owner. A
    // cleared SwiftUI draft must not reveal a hint over the old UIKit text.
    func updatePlaceholder() {
        guard placeholder != nil || placeholderLabel != nil else { return }
        if placeholderLabel == nil {
            let label = UILabel()
            label.numberOfLines = 0
            label.isUserInteractionEnabled = false
            label.isAccessibilityElement = false
            label.addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(usePromptSuggestion)))
            addSubview(label)
            placeholderLabel = label
        }
        guard let label = placeholderLabel else { return }
        if label.text != placeholder { label.text = placeholder }
        if label.font != font { label.font = font }
        if label.textColor != placeholderColor { label.textColor = placeholderColor }
        if label.textAlignment != textAlignment { label.textAlignment = textAlignment }
        label.isHidden = hasText || markedTextRange != nil || isDictating || placeholder == nil
        let suggested = !label.isHidden && acceptSuggestion != nil && isEditable
        label.numberOfLines = suggested ? 1 : 0
        label.lineBreakMode = .byTruncatingTail
        label.isUserInteractionEnabled = suggested
        label.isAccessibilityElement = suggested
        label.accessibilityIdentifier = suggested ? "composer-prompt-suggestion" : nil
        label.accessibilityTraits = suggested ? .button : .staticText
        label.accessibilityLabel = suggested ? "Suggested next message: \(placeholder ?? "")" : nil
        label.accessibilityHint = suggested ? "Fills your draft without sending." : nil
        accessibilityCustomActions = suggested
            ? [UIAccessibilityCustomAction(name: "Use suggested message: \(placeholder ?? "")", target: self, selector: #selector(usePromptSuggestion))] : nil
        guard !label.isHidden else { return }
        let padding = textContainer.lineFragmentPadding
        let width = max(0, bounds.width - textContainerInset.left - textContainerInset.right - padding * 2)
        let height = label.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude)).height
        label.frame = CGRect(x: textContainerInset.left + padding, y: textContainerInset.top,
                             width: width, height: height)
    }

    @objc @discardableResult func usePromptSuggestion() -> Bool {
        guard !hasText, markedTextRange == nil, !isDictating, isEditable else { return false }
        return acceptSuggestion?() ?? false
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        updatePlaceholder()
    }

    /// Hardware keyboard control of the inline suggestion list while it shows.
    weak var completion: ComposerCompletionModel?
    override var keyCommands: [UIKeyCommand]? {
        guard let completion, completion.acceptsKeys else { return super.keyCommands }
        let keys = [
            UIKeyCommand(input: UIKeyCommand.inputUpArrow, modifierFlags: [], action: #selector(suggestionUp)),
            UIKeyCommand(input: UIKeyCommand.inputDownArrow, modifierFlags: [], action: #selector(suggestionDown)),
            UIKeyCommand(input: "\r", modifierFlags: [], action: #selector(suggestionReturn)),
            UIKeyCommand(input: "\t", modifierFlags: [], action: #selector(suggestionTab)),
            UIKeyCommand(input: UIKeyCommand.inputEscape, modifierFlags: [], action: #selector(suggestionDismiss)),
        ]
        keys.forEach { $0.wantsPriorityOverSystemBehavior = true }
        return (super.keyCommands ?? []) + keys
    }
    @objc private func suggestionUp() { completion?.moveHighlight(-1) }
    @objc private func suggestionDown() { completion?.moveHighlight(1) }
    // With nothing to choose, Return and Tab type as usual.
    @objc private func suggestionReturn() { if completion?.chooseHighlighted() != true { insertText("\n") } }
    @objc private func suggestionTab() { if completion?.chooseHighlighted() != true { insertText("\t") } }
    @objc private func suggestionDismiss() { completion?.dismiss() }

    var canPasteImages = false { didSet { if canPasteImages != oldValue { updatePasteConfiguration() } } }
    var pasteImages: (([NSItemProvider]) -> Void)? { didSet { if (pasteImages == nil) != (oldValue == nil) { updatePasteConfiguration() } } }
    private var capturedTextPasteConfiguration = false
    private var textPasteConfiguration: UIPasteConfiguration?
    private var pendingImageProviders: [NSItemProvider]?
    private var acceptsImagePaste: Bool { isEditable && canPasteImages && pasteImages != nil }
    private static let imageTypes = [UTType.image, .png, .jpeg, .heic, .tiff, .gif].map(\.identifier)

    private static func isImage(_ provider: NSItemProvider) -> Bool {
        provider.hasItemConformingToTypeIdentifier(UTType.image.identifier)
    }

    private func updatePasteConfiguration() {
        if !capturedTextPasteConfiguration {
            capturedTextPasteConfiguration = true
            textPasteConfiguration = pasteConfiguration
        }
        let base = textPasteConfiguration?.acceptableTypeIdentifiers ?? []
        pasteConfiguration = acceptsImagePaste
            ? UIPasteConfiguration(acceptableTypeIdentifiers: base + Self.imageTypes.filter { !base.contains($0) })
            : textPasteConfiguration
        if pasteDelegate == nil { pasteDelegate = self }
    }

    /// Item-provider pastes run through UIKit's text paste pipeline. Accepting
    /// image types above lets them reach it; each image then becomes a staged
    /// attachment rather than being dropped by this plain-text editor.
    func textPasteConfigurationSupporting(_ textPasteConfigurationSupporting: any UITextPasteConfigurationSupporting,
                                          transform item: any UITextPasteItem) {
        guard acceptsImagePaste, Self.isImage(item.itemProvider) else { item.setDefaultResult(); return }
        item.setNoResult()
        // Items in one paste are transformed together; stage them as one batch.
        if pendingImageProviders == nil {
            DispatchQueue.main.async { [weak self] in
                guard let self, let providers = self.pendingImageProviders else { return }
                self.pendingImageProviders = nil
                if self.acceptsImagePaste { self.pasteImages?(providers) }
            }
        }
        pendingImageProviders = (pendingImageProviders ?? []) + [item.itemProvider]
    }

    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        // Only the detection properties are read here: reading the items
        // would ask for paste permission every time the menu is built.
        if action == #selector(paste(_:)), UIPasteboard.general.hasImages {
            return isEditable && (acceptsImagePaste || UIPasteboard.general.hasStrings || UIPasteboard.general.hasURLs)
        }
        return super.canPerformAction(action, withSender: sender)
    }

    /// Text and images copied together paste together: the text is typed and
    /// each image is attached once. Without attachments the text still pastes.
    override func paste(_ sender: Any?) {
        let board = UIPasteboard.general
        guard board.hasImages, isEditable else { super.paste(sender); return }
        let parts = Self.splitPaste(board.items)
        if let text = parts.text { insertText(text) }
        guard acceptsImagePaste, let pasteImages else { return }
        let providers = board.itemProviders
        let images = parts.images.compactMap { providers.indices.contains($0) ? providers[$0] : nil }
        if !images.isEmpty { pasteImages(images) }
    }

    /// Splits pasteboard items into the text to type and the indices of the
    /// items to attach. An item carrying an image is only attached: its other
    /// forms (a name or link for the same picture) are not typed as well.
    nonisolated static func splitPaste(_ items: [[String: Any]]) -> (text: String?, images: [Int]) {
        var texts: [String] = []
        var images: [Int] = []
        for (index, item) in items.enumerated() {
            if item.keys.contains(where: { UTType($0)?.conforms(to: .image) == true }) {
                images.append(index)
                continue
            }
            if let text = plainText(item), !text.isEmpty { texts.append(text) }
        }
        return (texts.isEmpty ? nil : texts.joined(separator: "\n"), images)
    }

    /// The item's plain text, whether the pasteboard gives it as a `String`
    /// or as encoded bytes (UTF-8, or UTF-16 for the UTF-16 type), then its
    /// link. Rich text types are skipped: their plain form sits beside them.
    private nonisolated static func plainText(_ item: [String: Any]) -> String? {
        let known = [UTType.utf8PlainText, .utf16PlainText, .plainText, .text].map(\.identifier)
        let others = item.keys.filter { !known.contains($0) && UTType($0)?.conforms(to: .plainText) == true }.sorted()
        for key in known + others {
            guard let value = item[key], let type = UTType(key) else { continue }
            if let text = value as? String { return text }
            if let data = value as? Data, let text = decode(data, utf16: type.conforms(to: .utf16PlainText)) { return text }
        }
        if let url = item[UTType.url.identifier] as? URL { return url.absoluteString }
        if let data = item[UTType.url.identifier] as? Data, let text = decode(data, utf16: false),
           let url = URL(string: text.trimmingCharacters(in: .whitespacesAndNewlines)) { return url.absoluteString }
        return nil
    }

    private nonisolated static func decode(_ data: Data, utf16: Bool) -> String? {
        if utf16 {
            // A byte-order mark decides; without one iOS writes little-endian.
            let bom = data.starts(with: [0xFE, 0xFF]) || data.starts(with: [0xFF, 0xFE])
            return String(data: data, encoding: bom ? .utf16 : .utf16LittleEndian)
        }
        return String(data: data, encoding: .utf8)
    }
}
