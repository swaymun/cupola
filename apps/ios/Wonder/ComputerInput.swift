import SwiftUI
import UIKit
import UIKit.UIGestureRecognizerSubclass
import WonderComputerView
import WonderPairing

enum ComputerInputMode: String, CaseIterable, Identifiable {
    case trackpad
    case directTouch
    var id: String { rawValue }
    var title: String { self == .trackpad ? "Trackpad" : "Direct touch" }
}

@MainActor
final class ComputerPointerState: ObservableObject {
    @Published private(set) var position = CGPoint(x: 0.5, y: 0.5)

    func move(to position: CGPoint) {
        guard self.position != position else { return }
        self.position = position
    }
}

/// The renderer and every input mode use the same aspect-fit, zoom and pan transform.
/// Zoom 1 fits the whole picture; `fillZoom` covers the viewport.
struct ComputerViewportTransform {
    let size: CGSize
    let content: CGRect
    let center: CGPoint
    /// A picture shorter than the viewport sits at its top instead of its
    /// middle. Only the spare height moves; width, zoom and pan are unchanged.
    let anchorsTop: Bool
    private let aspect: CGFloat
    private let zoom: CGFloat

    /// The zoom at which the picture covers the whole viewport, cropping its longer side.
    static func fillZoom(size: CGSize, aspect: CGFloat) -> CGFloat {
        let fit = fitSize(size: size, aspect: aspect)
        guard size.width.isFinite, size.height.isFinite else { return 1 }
        return max(1, size.width / fit.width, size.height / fit.height)
    }

    private static func fitSize(size: CGSize, aspect: CGFloat) -> CGSize {
        let safeAspect = aspect.isFinite && aspect > 0 ? aspect : 16 / 9
        let width = max(1, min(size.width, size.height * safeAspect))
        return CGSize(width: width, height: width / safeAspect)
    }

    init(size: CGSize, aspect: CGFloat, zoom: CGFloat, center: CGPoint = CGPoint(x: 0.5, y: 0.5),
         anchorsTop: Bool = false) {
        self.size = size
        self.aspect = aspect
        self.anchorsTop = anchorsTop
        let fit = Self.fitSize(size: size, aspect: aspect)
        let width = fit.width
        let height = fit.height
        let scale = zoom.isFinite ? min(max(zoom, 1), max(3, Self.fillZoom(size: size, aspect: aspect))) : 1
        self.zoom = scale
        let halfX = min(max(size.width, 0) / (width * scale) / 2, 0.5)
        let halfY = min(max(size.height, 0) / (height * scale) / 2, 0.5)
        let x = center.x.isFinite ? min(max(center.x, halfX), 1 - halfX) : 0.5
        let y = center.y.isFinite ? min(max(center.y, halfY), 1 - halfY) : 0.5
        self.center = CGPoint(x: x, y: y)
        let spareHeight = anchorsTop ? max(0, size.height - height * scale) : 0
        content = CGRect(x: size.width / 2 - x * width * scale,
                         y: size.height / 2 - y * height * scale - spareHeight / 2,
                         width: width * scale, height: height * scale)
    }

    /// The middle of the part of the picture inside the viewport: where an
    /// accessibility Click lands, so it hits the picture wherever it is anchored.
    var visiblePictureCenter: CGPoint {
        let visible = content.intersection(CGRect(origin: .zero, size: size))
        guard !visible.isNull, !visible.isEmpty else { return CGPoint(x: content.midX, y: content.midY) }
        return CGPoint(x: visible.midX, y: visible.midY)
    }

    func location(for point: CGPoint) -> CGPoint {
        CGPoint(x: content.minX + point.x * content.width,
                y: content.minY + point.y * content.height)
    }

    // The host centers the source in its encoded frame. Scale that frame so
    // the source, rather than its padding, fills the shared content rectangle.
    func videoFrame(encodedAspect: CGFloat) -> CGRect {
        let aspect = encodedAspect.isFinite && encodedAspect > 0
            ? encodedAspect : content.width / content.height
        let width = max(content.width, content.height * aspect)
        let height = width / aspect
        return CGRect(x: content.midX - width / 2, y: content.midY - height / 2,
                      width: width, height: height)
    }

    func point(_ location: CGPoint, clamped: Bool = false) -> CGPoint? {
        guard size.width > 0, size.height > 0, location.x.isFinite, location.y.isFinite,
              clamped || content.contains(location) else { return nil }
        return CGPoint(x: min(max((location.x - content.minX) / content.width, 0), 1),
                       y: min(max((location.y - content.minY) / content.height, 0), 1))
    }

    func moving(_ point: CGPoint, by delta: CGSize) -> CGPoint {
        guard delta.width.isFinite, delta.height.isFinite else { return point }
        return CGPoint(x: min(max(point.x + delta.width / content.width, 0), 1),
                       y: min(max(point.y + delta.height / content.height, 0), 1))
    }

    func panning(by delta: CGSize) -> CGPoint {
        moving(center, by: CGSize(width: -delta.width, height: -delta.height))
    }

    /// The center that keeps `point` at least `margin` inside the viewport, moving no more than needed.
    func centerKeeping(_ point: CGPoint, margin: CGFloat) -> CGPoint {
        let location = location(for: point)
        let insetX = min(margin, size.width / 2), insetY = min(margin, size.height / 2)
        var shift = CGSize.zero
        if location.x < insetX { shift.width = location.x - insetX }
        else if location.x > size.width - insetX { shift.width = location.x - (size.width - insetX) }
        if location.y < insetY { shift.height = location.y - insetY }
        else if location.y > size.height - insetY { shift.height = location.y - (size.height - insetY) }
        guard shift != .zero else { return center }
        let moved = CGPoint(x: center.x + shift.width / content.width, y: center.y + shift.height / content.height)
        return ComputerViewportTransform(size: size, aspect: aspect, zoom: zoom, center: moved, anchorsTop: anchorsTop).center
    }
}

/// Only consecutive motion can be replaced. Clicks, keys, clipboard and release
/// form ordering barriers. Overflow fails control closed rather than dropping text.
/// A held key's repeat is the one key traffic that merges: one unsent repeat
/// stands for all of them, so a slow connection never builds a backlog and the
/// key-up is at most one request behind the finger lifting.
struct ComputerInputBuffer {
    private(set) var batches: [[ComputerInputAction]] = []
    /// The deepest the queue has been, for the 30 s hold check and Diagnostics.
    private(set) var maximumDepth = 0
    /// Keys whose down has left the queue and whose up has not been queued yet.
    private var keysDown: Set<String> = []
    private let maximumActions = 64
    private let maximumBytes = 65_536

    mutating func append(_ actions: [ComputerInputAction]) -> Bool {
        guard !actions.isEmpty, actions.count <= 32 else { return false }
        if actions.count == 1, case let .key(key, "down", modifiers) = actions[0],
           let last = batches.last, last == [.key(key: key, phase: "down", modifiers: modifiers)] { return true }
        // A key already down on the computer (no up queued for it) makes an
        // unsent repeat only delay before the key lets go. The up may share its
        // batch with latched modifiers' ups, so look inside every batch.
        for case let .key(key, "up", modifiers) in actions where keysDown.contains(key) {
            let upQueued = batches.contains { $0.contains(.key(key: key, phase: "up", modifiers: modifiers)) }
            let repeatDown = [ComputerInputAction.key(key: key, phase: "down", modifiers: modifiers)]
            if !upQueued { batches.removeAll { $0 == repeatDown } }
        }
        if actions.count == 1, let last = batches.last, last.count == 1 {
            switch (last[0], actions[0]) {
            case let (.pointer(_, _, "move", previousButton), .pointer(_, _, "move", button)) where previousButton == button:
                batches[batches.count - 1] = actions
                return true
            // A simulator finger's newest position replaces an unsent one; the
            // host draws the path between the points it receives.
            case (.touch(_, _, "move", nil), .touch(_, _, "move", nil)):
                batches[batches.count - 1] = actions
                return true
            case let (.scroll(x, y), .scroll(nextX, nextY)) where abs(x + nextX) <= 4_096 && abs(y + nextY) <= 4_096:
                batches[batches.count - 1] = [.scroll(deltaX: x + nextX, deltaY: y + nextY)]
                return true
            case let (.text(previous), .text(next)) where previous.unicodeScalars.count + next.unicodeScalars.count <= 4_096:
                guard batches.joined().reduce(0, { $0 + Self.cost($1) }) + next.utf8.count <= maximumBytes else { return false }
                batches[batches.count - 1] = [.text(previous + next)]
                return true
            default: break
            }
        }
        guard batches.reduce(0, { $0 + $1.count }) + actions.count <= maximumActions,
              batches.joined().reduce(0, { $0 + Self.cost($1) }) + actions.reduce(0, { $0 + Self.cost($1) }) <= maximumBytes else { return false }
        batches.append(actions)
        maximumDepth = max(maximumDepth, batches.count)
        return true
    }

    mutating func popFirst() -> [ComputerInputAction]? {
        guard !batches.isEmpty else { return nil }
        let batch = batches.removeFirst()
        for case let .key(key, phase, _) in batch {
            if phase == "down" { keysDown.insert(key) } else if phase == "up" || phase == "press" { keysDown.remove(key) }
        }
        return batch
    }

    mutating func removeAll() {
        batches.removeAll(keepingCapacity: false)
        keysDown.removeAll()
    }

    private static func cost(_ action: ComputerInputAction) -> Int {
        switch action {
        case .text(let text), .clipboard(_, .some(let text)): return 96 + text.utf8.count
        default: return 96
        }
    }
}

/// UITextView owns marked text. Nothing is sent until composition commits. A
/// bounded shadow of recently committed text makes replacement and deletion exact.
struct ComputerKeyboardComposition {
    private(set) var committed = ""
    mutating func reset() { committed = "" }

    mutating func update(_ text: String, hasMarkedText: Bool) -> [ComputerInputAction] {
        guard !hasMarkedText else { return [] }
        let before = Array(committed), after = Array(text)
        let prefix = zip(before, after).prefix(while: { $0.0 == $0.1 }).count
        var actions = Array(repeating: ComputerInputAction.key(key: "delete", phase: "press", modifiers: 0), count: before.count - prefix)
        var inserted = ""
        var insertedScalars = 0
        func flush() {
            if !inserted.isEmpty { actions.append(.text(inserted)); inserted = ""; insertedScalars = 0 }
        }
        for character in after.dropFirst(prefix) {
            if character == "\n" || character == "\t" {
                flush()
                actions.append(.key(key: character == "\n" ? "return" : "tab", phase: "press", modifiers: 0))
            } else {
                let scalarCount = character.unicodeScalars.count
                if insertedScalars + scalarCount > 4_096 { flush() }
                inserted.append(character)
                insertedScalars += scalarCount
            }
        }
        flush()
        committed = String(text.suffix(128))
        return actions
    }
}

struct ComputerNativeKeyboard: UIViewRepresentable {
    let presented: Bool
    let resetID: UInt64
    let hostReference: String
    let onActions: ([ComputerInputAction]) -> Void
    let onDismiss: () -> Void

    #if WONDER_DIAGNOSTICS
    /// How many keyboard text views were made: a pose change must not make another.
    @MainActor static var made = 0
    #endif

    func makeUIView(context: Context) -> InputView {
        #if WONDER_DIAGNOSTICS
        Self.made += 1
        #endif
        return InputView()
    }
    func updateUIView(_ view: InputView, context: Context) {
        view.accessibilityLabel = "Keyboard for \(hostReference)"
        view.onActions = onActions
        view.onDismiss = onDismiss
        if view.resetID != resetID {
            view.resetID = resetID
            view.clearContext()
        }
        view.wantsKeyboard = presented
        view.updateFocus()
    }
    static func dismantleUIView(_ view: InputView, coordinator: ()) {
        view.onActions = nil
        view.onDismiss = nil
        view.wantsKeyboard = false
        view.resignFirstResponder()
        view.clearContext()
    }

    final class InputView: UITextView, UITextViewDelegate {
        var wantsKeyboard = false
        var resetID: UInt64 = 0
        var onActions: (([ComputerInputAction]) -> Void)?
        var onDismiss: (() -> Void)?
        private var composition = ComputerKeyboardComposition()
        private var editingInternally = false
        private var updatingMarkedText = false
        private var remotePresses = Set<UIPress>()

        init() {
            super.init(frame: .zero, textContainer: nil)
            delegate = self
            backgroundColor = .clear
            textColor = .clear
            tintColor = .clear
            isScrollEnabled = false
            autocapitalizationType = .none
            autocorrectionType = .no
            spellCheckingType = .no
            smartQuotesType = .no
            smartDashesType = .no
            smartInsertDeleteType = .no
            inputAssistantItem.leadingBarButtonGroups = []
            inputAssistantItem.trailingBarButtonGroups = []
            accessibilityIdentifier = "computer-session-native-keyboard"
        }
        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

        override func didMoveToWindow() {
            super.didMoveToWindow()
            updateFocus()
        }
        func updateFocus() {
            if wantsKeyboard {
                guard window != nil, !isFirstResponder else { return }
                // SwiftUI may call updateUIView during a view transaction.
                DispatchQueue.main.async { [weak self] in
                    guard let self, self.wantsKeyboard, self.window != nil, !self.isFirstResponder else { return }
                    self.becomeFirstResponder()
                }
            } else if isFirstResponder {
                resignFirstResponder()
                clearContext()
            }
        }
        func clearContext() {
            editingInternally = true
            super.unmarkText()
            text = ""
            composition.reset()
            editingInternally = false
        }
        override func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
            updatingMarkedText = true
            super.setMarkedText(markedText, selectedRange: selectedRange)
            updatingMarkedText = false
        }
        override func unmarkText() {
            super.unmarkText()
            textViewDidChange(self)
        }
        func textViewDidChange(_ textView: UITextView) {
            guard !editingInternally, !updatingMarkedText, wantsKeyboard, isFirstResponder else { return }
            let actions = composition.update(text ?? "", hasMarkedText: markedTextRange != nil)
            guard markedTextRange == nil else { return }
            if text != composition.committed {
                editingInternally = true
                text = composition.committed
                selectedRange = NSRange(location: (text as NSString).length, length: 0)
                editingInternally = false
            }
            if !actions.isEmpty { onActions?(actions) }
        }
        func textViewDidEndEditing(_ textView: UITextView) {
            guard wantsKeyboard else { return }
            wantsKeyboard = false
            clearContext()
            DispatchQueue.main.async { [weak self] in
                guard let self, !self.wantsKeyboard else { return }
                self.onDismiss?()
            }
        }
        /// The hidden text view has nothing to copy, paste or undo. Declining
        /// the edit actions lets a hardware keyboard's ⌘C, ⌘V, ⌘Z and ⌘A reach
        /// pressesBegan and go to the Mac instead of being handled here.
        override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
            wantsKeyboard ? false : super.canPerformAction(action, withSender: sender)
        }
        override func deleteBackward() {
            guard wantsKeyboard else { return }
            if markedTextRange == nil && (text ?? "").isEmpty {
                onActions?([.key(key: "delete", phase: "press", modifiers: 0)])
            } else { super.deleteBackward() }
        }
        override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
            if markedTextRange != nil {
                super.pressesBegan(presses, with: event)
                return
            }
            var remaining = Set<UIPress>()
            for press in presses {
                if wantsKeyboard, let key = press.key, let action = Self.remoteKey(key) {
                    clearContext()
                    remotePresses.insert(press)
                    onActions?([action])
                } else { remaining.insert(press) }
            }
            if !remaining.isEmpty { super.pressesBegan(remaining, with: event) }
        }
        override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
            let remaining = Set(presses.filter { remotePresses.remove($0) == nil })
            if !remaining.isEmpty { super.pressesEnded(remaining, with: event) }
        }
        override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
            let remaining = Set(presses.filter { remotePresses.remove($0) == nil })
            if !remaining.isEmpty { super.pressesCancelled(remaining, with: event) }
        }
        private static func remoteKey(_ key: UIKey) -> ComputerInputAction? {
            let names: [Int: String] = [40: "return", 41: "escape", 42: "delete", 43: "tab", 74: "home", 75: "pageup", 76: "forwarddelete", 77: "end", 78: "pagedown", 79: "right", 80: "left", 81: "down", 82: "up"]
            let code = Int(key.keyCode.rawValue)
            let flags = key.modifierFlags
            var modifiers: UInt32 = 0
            if flags.contains(.shift) { modifiers |= 1 }
            if flags.contains(.control) { modifiers |= 2 }
            if flags.contains(.alternate) { modifiers |= 4 }
            if flags.contains(.command) { modifiers |= 8 }
            if flags.contains(.alphaShift) { modifiers |= 16 }
            let name: String
            if let known = names[code] { name = known }
            else if (58...69).contains(code) { name = "f\(code - 57)" }
            else if flags.contains(.command) || flags.contains(.control) {
                let value = key.charactersIgnoringModifiers.lowercased()
                guard value.count == 1, value.unicodeScalars.allSatisfy({ $0.isASCII && !$0.properties.isWhitespace }) else { return nil }
                name = value
            } else { return nil }
            return .key(key: name, phase: "press", modifiers: modifiers)
        }
    }
}

/// UIKit owns recognition; the session model is the only owner of remote input.
struct ComputerGestureSurface: UIViewRepresentable {
    /// Only a press carrying the secondary (right) button right-clicks; a
    /// finger tap carries no button.
    static func isSecondaryClick(_ buttons: UIEvent.ButtonMask) -> Bool { buttons.contains(.secondary) }

    let allowsInput: Bool
    let mode: ComputerInputMode
    let zoomScale: CGFloat
    /// Location, size, button and the modifier keys held on a hardware keyboard.
    let onTap: (CGPoint, CGSize, String, [String]) -> Void
    let onMove: (CGSize, CGSize) -> Void
    let onDragBegan: (CGPoint, CGSize, [String]) -> Void
    let onDragMoved: (CGPoint, CGSize) -> Void
    let onDragEnded: (CGPoint?, CGSize) -> Void
    let onScroll: (CGSize) -> Void
    let onPan: (CGSize, CGSize) -> Void
    let onPinch: (CGFloat) -> Void
    /// An iPad pointer (trackpad or mouse) hovering at a location.
    let onHover: (CGPoint, CGSize) -> Void
    /// A tap while input is off (you are only watching): the viewer shows or hides its buttons.
    var onWatchTap: () -> Void = {}

    func makeUIView(context: Context) -> InputView { InputView() }
    func updateUIView(_ view: InputView, context: Context) {
        if view.mode != mode || (view.allowsInput && !allowsInput) { view.cancelDrag(); view.stopMomentum() }
        view.allowsInput = allowsInput
        view.mode = mode
        view.zoomScale = zoomScale
        view.onTap = onTap
        view.onMove = onMove
        view.onDragBegan = onDragBegan
        view.onDragMoved = onDragMoved
        view.onDragEnded = onDragEnded
        view.onScroll = onScroll
        view.onPan = onPan
        view.onPinch = onPinch
        view.onHover = onHover
        view.onWatchTap = onWatchTap
    }
    static func dismantleUIView(_ view: InputView, coordinator: ()) {
        view.cancelDrag()
        view.stopMomentum()
        view.allowsInput = false
        view.onTap = nil; view.onMove = nil; view.onDragBegan = nil
        view.onDragMoved = nil; view.onDragEnded = nil
        view.onScroll = nil; view.onPan = nil; view.onPinch = nil; view.onHover = nil; view.onWatchTap = nil
    }

    final class InputView: UIView, UIGestureRecognizerDelegate, UIPointerInteractionDelegate {
        var allowsInput = false
        var mode: ComputerInputMode = .trackpad
        var zoomScale: CGFloat = 1
        var onTap: ((CGPoint, CGSize, String, [String]) -> Void)?
        var onMove: ((CGSize, CGSize) -> Void)?
        var onDragBegan: ((CGPoint, CGSize, [String]) -> Void)?
        var onDragMoved: ((CGPoint, CGSize) -> Void)?
        var onDragEnded: ((CGPoint?, CGSize) -> Void)?
        var onScroll: ((CGSize) -> Void)?
        var onPan: ((CGSize, CGSize) -> Void)?
        var onPinch: ((CGFloat) -> Void)?
        var onHover: ((CGPoint, CGSize) -> Void)?
        var onWatchTap: (() -> Void)?
        private let tap = UITapGestureRecognizer()
        private let hover = UIHoverGestureRecognizer()
        /// Two-finger trackpad and mouse-wheel scrolling from an iPad pointer device.
        private let pointerScroll = UIPanGestureRecognizer()
        private var lastMoveTime: CFTimeInterval?
        private var momentum: ComputerScrollMomentum?
        private var momentumLink: CADisplayLink?
        private var momentumLastTime: CFTimeInterval?
        private let rightTap = UITapGestureRecognizer()
        /// A mouse's right button or a trackpad's secondary click on an iPad.
        private let secondaryClick = UITapGestureRecognizer()
        private let rightHold = UILongPressGestureRecognizer()
        private let dragHold = UILongPressGestureRecognizer()
        private let singlePan = UIPanGestureRecognizer()
        private let twoPan = UIPanGestureRecognizer()
        private let pinch = UIPinchGestureRecognizer()
        private var repeatTouch = false
        private var dragActive = false
        private var suppressTap = false
        private var holdPoint: CGPoint?
        private var pinchStartScale: CGFloat = 1
        /// Two fingers scrolling never keep exactly the same distance apart. A
        /// pinch is a zoom only once the fingers have clearly spread or closed.
        private var pinchEngaged = false
        private var pinchEngagedAt: CGFloat = 1
        private static let pinchDeadZone: CGFloat = 0.12

        override init(frame: CGRect) {
            super.init(frame: frame)
            backgroundColor = .clear
            isOpaque = false
            isAccessibilityElement = false
            accessibilityElementsHidden = true
            tap.addTarget(self, action: #selector(tapped))
            rightTap.numberOfTouchesRequired = 2
            rightTap.addTarget(self, action: #selector(rightTapped))
            // UIKit checks buttonMaskRequired only for pointer input, and without
            // indirect input events a mouse arrives as touches; the delegate
            // therefore admits only events that carry the secondary button.
            secondaryClick.buttonMaskRequired = .secondary
            secondaryClick.addTarget(self, action: #selector(secondaryClicked))
            rightHold.minimumPressDuration = 0.5
            rightHold.addTarget(self, action: #selector(rightHeld))
            dragHold.numberOfTapsRequired = 1
            dragHold.minimumPressDuration = 0.18
            dragHold.addTarget(self, action: #selector(dragHeld))
            singlePan.maximumNumberOfTouches = 1
            singlePan.addTarget(self, action: #selector(singlePanned))
            twoPan.minimumNumberOfTouches = 2
            twoPan.maximumNumberOfTouches = 2
            twoPan.addTarget(self, action: #selector(twoPanned))
            pinch.addTarget(self, action: #selector(pinched))
            hover.addTarget(self, action: #selector(hovered))
            addGestureRecognizer(hover)
            pointerScroll.allowedScrollTypesMask = .all
            pointerScroll.allowedTouchTypes = []
            pointerScroll.addTarget(self, action: #selector(pointerScrolled))
            pointerScroll.delegate = self
            addGestureRecognizer(pointerScroll)
            addInteraction(UIPointerInteraction(delegate: self))
            tap.require(toFail: singlePan)
            tap.require(toFail: rightHold)
            rightHold.require(toFail: dragHold)
            // The delegate reserves the hold's priority for its second contact.
            // An unconditional wait delays normal pans until touch-up.
            rightTap.require(toFail: twoPan)
            rightTap.require(toFail: pinch)
            for gesture in [tap, rightTap, secondaryClick, rightHold, dragHold, singlePan, twoPan, pinch] {
                gesture.delegate = self
                addGestureRecognizer(gesture)
            }
        }
        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

        func cancelDrag() {
            if dragActive { onDragEnded?(nil, bounds.size) }
            dragActive = false
            holdPoint = nil
        }
        override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
            if !dragActive { suppressTap = false }
            // A new touch catches a coasting scroll, like a scroll view.
            stopMomentum()
            super.touchesBegan(touches, with: event)
        }
        override func willMove(toWindow newWindow: UIWindow?) {
            super.willMove(toWindow: newWindow)
            if newWindow == nil { stopMomentum() }
        }

        /// The Mac draws its own pointer in the stream; hide the iPad's over it
        /// while controlling so only one pointer shows.
        func pointerInteraction(_ interaction: UIPointerInteraction, styleFor region: UIPointerRegion) -> UIPointerStyle? {
            allowsInput ? .hidden() : nil
        }

        @objc private func hovered() {
            guard allowsInput, hover.state == .began || hover.state == .changed else { return }
            onHover?(hover.location(in: self), bounds.size)
        }

        /// A finger delta scaled by how fast it moved (trackpad mode only).
        private func pointerDelta(_ delta: CGPoint, ended: Bool = false) -> CGSize {
            let now = CACurrentMediaTime()
            defer { lastMoveTime = ended ? nil : now }
            let raw = CGSize(width: delta.x, height: delta.y)
            guard let last = lastMoveTime else { return raw }
            return ComputerPointerAcceleration.accelerated(raw, interval: now - last)
        }

        private func startMomentum(velocity: CGPoint) {
            stopMomentum()
            guard allowsInput, UIAccessibility.isReduceMotionEnabled == false,
                  let momentum = ComputerScrollMomentum(releaseVelocity: CGVector(dx: velocity.x, dy: velocity.y)) else { return }
            self.momentum = momentum
            let link = CADisplayLink(target: self, selector: #selector(momentumTick(_:)))
            link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 60, preferred: 60)
            link.add(to: .main, forMode: .common)
            momentumLink = link
            momentumLastTime = nil
        }

        func stopMomentum() {
            momentumLink?.invalidate()
            momentumLink = nil
            momentum = nil
            momentumLastTime = nil
        }

        @objc private func momentumTick(_ link: CADisplayLink) {
            guard allowsInput, var current = momentum else { stopMomentum(); return }
            let interval = momentumLastTime.map { link.timestamp - $0 } ?? link.duration
            momentumLastTime = link.timestamp
            guard let delta = current.step(interval: interval) else { stopMomentum(); return }
            momentum = current
            onScroll?(delta)
        }

        @objc private func pointerScrolled() {
            guard allowsInput else { return }
            let delta = pointerScroll.translation(in: self)
            pointerScroll.setTranslation(.zero, in: self)
            switch pointerScroll.state {
            case .began:
                stopMomentum()
                onScroll?(CGSize(width: delta.x, height: delta.y))
            case .changed:
                onScroll?(CGSize(width: delta.x, height: delta.y))
            case .ended:
                startMomentum(velocity: pointerScroll.velocity(in: self))
            default: break
            }
        }
        @objc private func tapped() {
            guard allowsInput else { onWatchTap?(); return }
            guard !suppressTap else { return }
            onTap?(tap.location(in: self), bounds.size, "left", Self.modifierKeys(tap.modifierFlags))
        }
        @objc private func rightTapped() {
            guard allowsInput else { return }
            cancelDrag()
            onTap?(rightTap.location(in: self), bounds.size, "right", [])
        }
        @objc private func secondaryClicked() {
            guard allowsInput else { return }
            cancelDrag()
            onTap?(secondaryClick.location(in: self), bounds.size, "right", Self.modifierKeys(secondaryClick.modifierFlags))
        }
        @objc private func rightHeld() {
            guard allowsInput, rightHold.state == .began else { return }
            cancelDrag()
            onTap?(rightHold.location(in: self), bounds.size, "right", [])
        }

        /// Modifier keys held on an iPad's hardware keyboard, by the names the
        /// computer's input accepts (each host maps ⌘ to its own key).
        static func modifierKeys(_ flags: UIKeyModifierFlags) -> [String] {
            let names: [(flag: UIKeyModifierFlags, key: String)] = [
                (.control, "control"), (.alternate, "option"), (.shift, "shift"), (.command, "command"),
            ]
            return names.filter { flags.contains($0.flag) }.map(\.key)
        }
        @objc private func dragHeld() {
            guard allowsInput else { cancelDrag(); return }
            let point = dragHold.location(in: self)
            switch dragHold.state {
            case .began:
                suppressTap = true
                dragActive = true
                holdPoint = point
                onDragBegan?(point, bounds.size, Self.modifierKeys(dragHold.modifierFlags))
            case .changed:
                guard dragActive else { return }
                if mode == .trackpad, let previous = holdPoint {
                    onMove?(pointerDelta(CGPoint(x: point.x - previous.x, y: point.y - previous.y)), bounds.size)
                } else { onDragMoved?(point, bounds.size) }
                holdPoint = point
            case .ended:
                if dragActive { onDragEnded?(point, bounds.size) }
                dragActive = false
                holdPoint = nil
                lastMoveTime = nil
            case .cancelled, .failed: cancelDrag(); lastMoveTime = nil
            default: break
            }
        }
        @objc private func singlePanned() {
            guard allowsInput else {
                cancelDrag()
                // Watching a zoomed picture: one finger moves around it.
                let delta = singlePan.translation(in: self)
                singlePan.setTranslation(.zero, in: self)
                if zoomScale > 1, singlePan.state == .began || singlePan.state == .changed {
                    onPan?(CGSize(width: delta.x, height: delta.y), bounds.size)
                }
                return
            }
            guard !pinchEngaged else { cancelDrag(); return }
            let point = singlePan.location(in: self)
            let delta = singlePan.translation(in: self)
            singlePan.setTranslation(.zero, in: self)
            switch singlePan.state {
            case .began:
                lastMoveTime = nil
                if mode == .directTouch {
                    dragActive = true
                    onDragBegan?(CGPoint(x: point.x - delta.x, y: point.y - delta.y), bounds.size,
                                 Self.modifierKeys(singlePan.modifierFlags))
                    onDragMoved?(point, bounds.size)
                } else { onMove?(pointerDelta(delta), bounds.size) }
            case .changed:
                if mode == .directTouch {
                    if dragActive { onDragMoved?(point, bounds.size) }
                } else { onMove?(pointerDelta(delta), bounds.size) }
            case .ended:
                if dragActive { onDragEnded?(point, bounds.size) }
                dragActive = false
                lastMoveTime = nil
            case .cancelled, .failed: cancelDrag(); lastMoveTime = nil
            default: break
            }
        }
        @objc private func twoPanned() {
            guard !pinchEngaged else { twoPan.setTranslation(.zero, in: self); return }
            let delta = twoPan.translation(in: self)
            twoPan.setTranslation(.zero, in: self)
            // The trackpad pointer pulls a zoomed picture along, so two fingers
            // always scroll the computer there. Direct touch pans a zoomed picture.
            let scrolls = allowsInput && (mode == .trackpad || zoomScale <= 1)
            if twoPan.state == .ended, scrolls {
                // A flick keeps scrolling and slows down, like a Mac trackpad.
                startMomentum(velocity: twoPan.velocity(in: self))
                return
            }
            guard twoPan.state == .began || twoPan.state == .changed else { return }
            cancelDrag()
            if scrolls { onScroll?(CGSize(width: delta.x, height: delta.y)) }
            else if zoomScale > 1 { onPan?(CGSize(width: delta.x, height: delta.y), bounds.size) }
        }
        @objc private func pinched() {
            switch pinch.state {
            case .began:
                pinchEngaged = false
                pinchStartScale = zoomScale
                pinchEngagedAt = 1
            case .changed:
                if !pinchEngaged, abs(pinch.scale - 1) >= Self.pinchDeadZone {
                    pinchEngaged = true
                    pinchEngagedAt = pinch.scale
                    cancelDrag()
                }
                // Zoom follows the fingers from where the dead zone ended, so
                // engaging does not jump the picture by the dead zone itself.
                if pinchEngaged { onPinch?(pinchStartScale * pinch.scale / pinchEngagedAt) }
            default: pinchEngaged = false
            }
        }
        override func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
            allowsInput || gestureRecognizer === pinch || gestureRecognizer === tap
                || ((gestureRecognizer === twoPan || gestureRecognizer === singlePan) && zoomScale > 1)
        }
        func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
            if gestureRecognizer === singlePan { repeatTouch = touch.tapCount > 1 }
            return true
        }
        func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldRequireFailureOf other: UIGestureRecognizer) -> Bool {
            gestureRecognizer === singlePan && other === dragHold && repeatTouch
        }
        func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive event: UIEvent) -> Bool {
            // Only pointer scrolling reaches pointerScroll; touches never do.
            if gestureRecognizer === pointerScroll { return event.type == .scroll }
            // A finger tap never right-clicks; a mouse or trackpad secondary click does.
            if gestureRecognizer === secondaryClick { return ComputerGestureSurface.isSecondaryClick(event.buttonMask) }
            return true
        }
        func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
            (gestureRecognizer === pinch && (other === twoPan || other === singlePan))
                || (other === pinch && (gestureRecognizer === twoPan || gestureRecognizer === singlePan))
                || (gestureRecognizer === tap && other === dragHold)
                || (gestureRecognizer === dragHold && other === tap)
        }
    }
}


/// The separate trackpad below the fold (iPhone Duo laptop pose). Raw touches
/// go through `ComputerTrackpadGestures`, which owns the Moonlight rules; this
/// view only tracks touch identity and the 650 ms hold timer.
struct ComputerTrackpadPad: UIViewRepresentable {
    let allowsInput: Bool
    let onOutput: (ComputerTrackpadGestures.Output) -> Void

    func makeUIView(context: Context) -> PadView { PadView() }
    func updateUIView(_ view: PadView, context: Context) {
        if view.allowsInput && !allowsInput { view.cancel() }
        view.allowsInput = allowsInput
        view.onOutput = onOutput
    }
    static func dismantleUIView(_ view: PadView, coordinator: ()) {
        view.cancel()
        view.onOutput = nil
    }

    final class PadView: UIView {
        var allowsInput = false
        var onOutput: ((ComputerTrackpadGestures.Output) -> Void)?
        private var gestures = ComputerTrackpadGestures()
        private var touchIDs: [ObjectIdentifier: Int] = [:]
        private var nextTouchID = 0
        private var holdTimer: Timer?
        private var momentum: ComputerScrollMomentum?
        private var momentumLink: CADisplayLink?
        private var momentumLastTime: CFTimeInterval?

        override init(frame: CGRect) {
            super.init(frame: frame)
            isMultipleTouchEnabled = true
            isAccessibilityElement = false
            backgroundColor = .clear
        }
        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

        override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
            // A new touch catches a coasting scroll, like the picture.
            stopMomentum()
            guard allowsInput else { return }
            for touch in touches {
                nextTouchID &+= 1
                touchIDs[ObjectIdentifier(touch)] = nextTouchID
                emit(gestures.began(id: nextTouchID, at: touch.location(in: self), time: touch.timestamp))
            }
            scheduleHold()
        }
        override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
            guard allowsInput else { return }
            for touch in touches {
                guard let id = touchIDs[ObjectIdentifier(touch)] else { continue }
                emit(gestures.moved(id: id, to: touch.location(in: self), time: touch.timestamp))
            }
            scheduleHold()
        }
        override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
            for touch in touches {
                guard let id = touchIDs.removeValue(forKey: ObjectIdentifier(touch)) else { continue }
                emit(gestures.ended(id: id, at: touch.location(in: self), time: touch.timestamp))
            }
            scheduleHold()
        }
        override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { cancel() }
        override func willMove(toWindow newWindow: UIWindow?) {
            super.willMove(toWindow: newWindow)
            if newWindow == nil { cancel() }
        }

        /// Also stops coasting: control ended, the source switched, the app left
        /// the foreground or the pad left the screen.
        func cancel() {
            holdTimer?.invalidate()
            holdTimer = nil
            stopMomentum()
            touchIDs.removeAll()
            emit(gestures.cancelled())
        }

        private func emit(_ outputs: [ComputerTrackpadGestures.Output]) {
            for output in outputs {
                if case .coast(let velocity) = output { startMomentum(velocity: velocity) } else { onOutput?(output) }
            }
        }

        /// The same coasting as two-finger scrolling on the picture.
        private func startMomentum(velocity: CGVector) {
            stopMomentum()
            guard allowsInput, UIAccessibility.isReduceMotionEnabled == false,
                  let momentum = ComputerScrollMomentum(releaseVelocity: velocity) else { return }
            self.momentum = momentum
            let link = CADisplayLink(target: self, selector: #selector(momentumTick(_:)))
            link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 60, preferred: 60)
            link.add(to: .main, forMode: .common)
            momentumLink = link
            momentumLastTime = nil
        }

        private func stopMomentum() {
            momentumLink?.invalidate()
            momentumLink = nil
            momentum = nil
            momentumLastTime = nil
        }

        @objc private func momentumTick(_ link: CADisplayLink) {
            guard allowsInput, var current = momentum else { stopMomentum(); return }
            let interval = momentumLastTime.map { link.timestamp - $0 } ?? link.duration
            momentumLastTime = link.timestamp
            guard let delta = current.step(interval: interval) else { stopMomentum(); return }
            momentum = current
            onOutput?(.scroll(delta))
        }

        private func scheduleHold() {
            holdTimer?.invalidate()
            holdTimer = nil
            guard let deadline = gestures.holdDeadline else { return }
            let delay = max(0, deadline - ProcessInfo.processInfo.systemUptime)
            let timer = Timer(timeInterval: delay, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.emit(self.gestures.tick(time: max(deadline, ProcessInfo.processInfo.systemUptime)))
                }
            }
            RunLoop.main.add(timer, forMode: .common)
            holdTimer = timer
        }
    }
}

// Clipboard safety rules informed by Sill (https://github.com/Saffsanity/sill),
// Saffsanity/sill is Apache-2.0. Independently implemented for Wonder's session relay.
struct ComputerClipboardUpdate: Decodable, Sendable {
    let subscriptionId: String?
    let sequence: UInt64
    let status: String
    let text: String?

    /// A reply to an older subscription is ignored; a missing update or `off` is current.
    static func isCurrent(_ update: ComputerClipboardUpdate?, subscriptionID: String) -> Bool {
        guard let update, update.status != "off" else { return true }
        return update.subscriptionId == subscriptionID
    }

    /// Only what the owner can act on or must know: nil while sharing simply works or is starting.
    /// A missing update means the Mac cannot share; `unavailable` is a transient helper failure that reconnects on its own.
    static func message(for update: ComputerClipboardUpdate?, subscriptionID: String, hostName: String) -> String? {
        guard let update else { return "Clipboard sharing is unavailable on \(hostName)." }
        guard update.subscriptionId == subscriptionID else { return nil }
        switch update.status {
        case "permissionRequired": return "Allow Cupola in Mac System Settings → Privacy & Security → Paste from Other Apps."
        case "unavailable": return "Reconnecting to the clipboard on \(hostName)…"
        default: return nil
        }
    }
}

struct ComputerClipboardSubscription: Sendable {
    let id: String?
    let cursor: UInt64
}

struct ComputerClipboardPollRequest: Encodable {
    let signaling: ComputerSignalingPollRequest
    let clipboard: ComputerClipboardSubscription
    private enum CodingKeys: String, CodingKey { case clipboardSubscriptionId, clipboardCursor }
    func encode(to encoder: Encoder) throws {
        try signaling.encode(to: encoder)
        var fields = encoder.container(keyedBy: CodingKeys.self)
        try fields.encodeIfPresent(clipboard.id, forKey: .clipboardSubscriptionId)
        try fields.encode(clipboard.cursor, forKey: .clipboardCursor)
    }
}

struct ComputerClipboardPollResponse: Decodable, Sendable {
    let signaling: ComputerSignalingResponse
    let clipboard: ComputerClipboardUpdate?
    private enum CodingKeys: String, CodingKey { case clipboard }
    init(from decoder: Decoder) throws {
        signaling = try ComputerSignalingResponse(from: decoder)
        clipboard = try decoder.container(keyedBy: CodingKeys.self).decodeIfPresent(ComputerClipboardUpdate.self, forKey: .clipboard)
    }
}

/// Text a computer copies to this device: bounded, kept off Universal
/// Clipboard and expiring. The computer view and Terminal both write here,
/// and only while the owner's clipboard sharing (`sharingKey`) is on.
@MainActor
enum ComputerPhoneClipboard {
    static let limit = 256 * 1024
    /// Off by default; the computer view's Share Mac clipboard and Terminal's
    /// Share Clipboard are the same choice.
    static let sharingKey = "computer.shareClipboard"

    /// False when nothing was written (empty, or over `limit`).
    @discardableResult
    static func write(_ text: String, to board: UIPasteboard = .general) -> Bool {
        guard !text.isEmpty, text.utf8.count <= limit else { return false }
        board.setItems([["public.utf8-plain-text": text]], options: [
            .localOnly: true, .expirationDate: Date().addingTimeInterval(600),
        ])
        return true
    }
}

/// Hears every touch that ends anywhere in the app's window without taking part
/// in it, so a ⌘ held for the app switcher can be let go on any tap, wherever
/// it lands (the picture, a button, the chat beside it).
@MainActor
final class ComputerTouchEndObserver: NSObject, UIGestureRecognizerDelegate {
    private final class Recognizer: UIGestureRecognizer {
        var onEnd: (() -> Void)?
        override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
            onEnd?()
            state = .failed
        }
        override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) { state = .failed }
        override func reset() {}
    }

    private var recognizer: Recognizer?

    func start(_ onEnd: @escaping () -> Void) {
        stop()
        guard let window = UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene })
            .flatMap(\.windows).first(where: \.isKeyWindow) else { return }
        let recognizer = Recognizer()
        recognizer.onEnd = onEnd
        recognizer.cancelsTouchesInView = false
        recognizer.delaysTouchesBegan = false
        recognizer.delaysTouchesEnded = false
        recognizer.delegate = self
        window.addGestureRecognizer(recognizer)
        self.recognizer = recognizer
    }

    func stop() {
        if let recognizer { recognizer.view?.removeGestureRecognizer(recognizer) }
        recognizer = nil
    }

    /// The recognizer sits on the app's window, so it must not outlive the
    /// model that owns this observer when the switcher is still open.
    isolated deinit { stop() }

    nonisolated func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                                       shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool { true }
}
