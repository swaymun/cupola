import CoreGraphics
import XCTest
@testable import WonderComputerUseCore

final class SystemDialogTests: XCTestCase {
    // First input must wait for activation without blocking the main actor that
    // delivers the focus change, and must never outlive the selected binding.
    @MainActor func testFirstInputWaitsForBackgroundWindowActivation() async {
        var focused = false
        var raises = 0
        let prepared = await SystemDialogWindowRaiser.prepareForInput(raise: {
            raises += 1
            Task { @MainActor in focused = true }
            return .raised
        }, isFocused: { focused }, isValid: { true }, pollNanoseconds: 1_000_000)
        XCTAssertTrue(prepared, "The first input must survive asynchronous foreground activation")
        XCTAssertEqual(raises, 1)
        XCTAssertTrue(focused, "Activation must get a main-actor turn before input is accepted")
    }

    // Switching windows while in control: focus on A must not satisfy B, and
    // B's first input must raise B and wait for its activation.
    @MainActor func testSwitchingWindowsInControlRaisesAndWaitsForTheNewWindow() async {
        var focusedWindow: UInt32?
        var raised: [UInt32] = []
        func prepare(_ window: UInt32) async -> Bool {
            await SystemDialogWindowRaiser.prepareForInput(raise: {
                raised.append(window)
                Task { @MainActor in focusedWindow = window }
                return .raised
            }, isFocused: { focusedWindow == window }, isValid: { true }, pollNanoseconds: 1_000_000)
        }
        let firstA = await prepare(1)
        let againA = await prepare(1)
        let firstB = await prepare(2)
        XCTAssertTrue(firstA)
        XCTAssertTrue(againA)
        XCTAssertTrue(firstB, "The first input after a switch must survive B's activation")
        XCTAssertEqual(raised, [1, 2], "An already-focused window is not raised again")
        XCTAssertEqual(focusedWindow, 2)
    }

    /// Measured 2026-10-21 (macOS 26): while a sheet is open, the app's focused
    /// window is the `AXSheet`, a child of its window. Input to that window or
    /// to the sheet must not stop control; another window is still refused.
    func testAnOpenSheetAndItsWindowAreOneInputTarget() {
        let sheetFocused = SystemDialogWindowRaiser.FocusedWindow(windowID: 10_542, sheetParentID: 10_538, sheetIDs: [])
        XCTAssertTrue(sheetFocused.accepts(10_542), "The sheet itself")
        XCTAssertTrue(sheetFocused.accepts(10_538), "The window showing the sheet")
        XCTAssertFalse(sheetFocused.accepts(10_541), "An unrelated window of the same app")
        let windowFocused = SystemDialogWindowRaiser.FocusedWindow(windowID: 10_538, sheetParentID: nil, sheetIDs: [10_542])
        XCTAssertTrue(windowFocused.accepts(10_542))
        XCTAssertTrue(windowFocused.accepts(10_538))
        XCTAssertFalse(windowFocused.accepts(10_540))
        XCTAssertFalse(SystemDialogWindowRaiser.FocusedWindow(windowID: 1, sheetParentID: nil, sheetIDs: []).accepts(2))
    }

    @MainActor func testFocusWaitRefusesAChangedSelection() async {
        var valid = true
        var focused = false
        let prepared = await SystemDialogWindowRaiser.prepareForInput(raise: {
            Task { @MainActor in valid = false; focused = true }
            return .raised
        }, isFocused: { focused }, isValid: { valid }, pollNanoseconds: 1_000_000)
        XCTAssertFalse(prepared)
        XCTAssertFalse(valid, "The selection change must be observed while activation is pending")
    }

    @MainActor func testFocusWaitIsBoundedAndDoesNotRaiseAnotherWindow() async {
        var checks = 0
        var raises = 0
        let prepared = await SystemDialogWindowRaiser.prepareForInput(raise: {
            raises += 1
            return .raised
        }, isFocused: { checks += 1; return false }, isValid: { true }, attempts: 2, pollNanoseconds: 1)
        XCTAssertFalse(prepared)
        XCTAssertEqual(raises, 1)
        XCTAssertGreaterThan(checks, 1)
        XCTAssertLessThanOrEqual(checks, 4, "A window that never gains focus must not keep polling")
    }

    @MainActor func testCancelledFocusWaitCannotAcceptLateActivation() async {
        var focused = false
        let waiting = Task { @MainActor in
            await SystemDialogWindowRaiser.prepareForInput(raise: { .raised },
                isFocused: { focused }, isValid: { true })
        }
        await Task.yield()
        waiting.cancel()
        focused = true
        let prepared = await waiting.value
        XCTAssertFalse(prepared)
    }

    /// A busy shared app must not hold the helper's main run loop, which also
    /// answers heartbeats: the caller gets no answer within the budget, the
    /// main actor keeps running, and new reads do not queue behind the hung one.
    @MainActor func testAHungAccessibilityReadReturnsWithinItsBudgetWithoutBlockingTheMainActor() async {
        let worker = AccessibilityWorker(label: "test.accessibility")
        let release = DispatchSemaphore(value: 0)
        var mainActorRan = false
        Task { @MainActor in mainActorRan = true }
        let started = Date()
        let hung = await worker.run(budget: 0.05) { _ -> Bool in
            release.wait()
            return true
        }
        XCTAssertNil(hung, "A read past its budget must not answer")
        XCTAssertLessThan(Date().timeIntervalSince(started), 1)
        XCTAssertTrue(mainActorRan, "The main actor must keep running while the app is busy")
        let queued = await worker.run(budget: 1) { _ in true }
        XCTAssertNil(queued, "A new read must be refused, not queued behind the hung one")
        release.signal()
        var recovered: Bool?
        for _ in 0..<200 where recovered == nil {
            recovered = await worker.run(budget: 1) { _ in true }
            if recovered == nil { try? await Task.sleep(nanoseconds: 5_000_000) }
        }
        XCTAssertEqual(recovered, true, "Once the app answers, reads work again")
    }

    /// The whole preparation has one budget, however many focus checks it
    /// makes, so a slow app cannot outlast the daemon's 2 s wait for a batch.
    @MainActor func testFocusWaitStopsAtItsTotalBudgetWhenEachCheckIsSlow() async {
        var checks = 0
        let started = Date()
        let prepared = await SystemDialogWindowRaiser.prepareForInput(raise: { .raised }, isFocused: {
            checks += 1
            try? await Task.sleep(nanoseconds: 40_000_000)
            return false
        }, isValid: { true }, attempts: 40, pollNanoseconds: 1_000_000, budget: 0.15)
        XCTAssertFalse(prepared)
        XCTAssertLessThan(Date().timeIntervalSince(started), 0.6)
        XCTAssertLessThan(checks, 10, "Forty slow checks must not all run")
    }

    /// The lease or view can end while a focus read is in flight; its answer
    /// then belongs to no binding.
    @MainActor func testFocusAnsweredAfterTheSelectionChangedIsRefused() async {
        var valid = true
        let prepared = await SystemDialogWindowRaiser.prepareForInput(raise: { .raised }, isFocused: {
            valid = false
            return true
        }, isValid: { valid })
        XCTAssertFalse(prepared)
    }

    private func shared(_ number: Int, pid: Int, x: Int = 0, y: Int = 0, width: Int = 400, height: Int = 300) -> [String: Any] {
        ["kCGWindowNumber": NSNumber(value: number), "kCGWindowOwnerPID": NSNumber(value: pid), "kCGWindowLayer": 0,
         "kCGWindowAlpha": 1, "kCGWindowIsOnscreen": true,
         "kCGWindowBounds": ["Height": height, "Width": width, "X": x, "Y": y] as NSDictionary]
    }

    /// Losing focus or the window mid-control says what happened instead of
    /// the generic "did not accept this input".
    func testControlledWindowReportsClosedAndUnfocusedSpecifically() throws {
        let windows = [shared(7, pid: 42, x: 10, y: 20)]
        XCTAssertEqual(try ControlWindowFailure.frame(windowID: 7, expectedPID: 42, frontmostPID: 42, windows: windows),
                       CGRect(x: 10, y: 20, width: 400, height: 300))
        XCTAssertThrowsError(try ControlWindowFailure.frame(windowID: 7, expectedPID: 42, frontmostPID: 99, windows: windows)) {
            XCTAssertEqual($0 as? ControlWindowFailure, .notFocused)
        }
        XCTAssertThrowsError(try ControlWindowFailure.frame(windowID: 8, expectedPID: 42, frontmostPID: 42, windows: windows)) {
            XCTAssertEqual($0 as? ControlWindowFailure, .notFound)
        }
        // A recycled window number in another process is a different window.
        XCTAssertThrowsError(try ControlWindowFailure.frame(windowID: 7, expectedPID: 43, frontmostPID: 43, windows: windows)) {
            XCTAssertEqual($0 as? ControlWindowFailure, .notFound)
        }
    }

    func testAWindowInFrontOfTheSharedOneReportsItIsCovered() throws {
        let banner = shared(9, pid: 77, x: 300, y: 0, width: 360, height: 80)
        let windows = [banner, shared(7, pid: 42)]
        XCTAssertNoThrow(try ControlWindowFailure.requireInput(windowID: 7, at: CGPoint(x: 100, y: 150), windows: windows))
        XCTAssertThrowsError(try ControlWindowFailure.requireInput(windowID: 7, at: CGPoint(x: 350, y: 40), windows: windows)) {
            XCTAssertEqual($0 as? ControlWindowFailure, .covered)
        }
    }

    /// The daemon already words these reasons; any other failure stays generic.
    func testDeliveryFailureReasonsKeepTheWindowCause() {
        XCTAssertEqual(ControlWindowFailure.deliveryReason(for: ControlWindowFailure.covered), "window_covered")
        XCTAssertEqual(ControlWindowFailure.deliveryReason(for: ControlWindowFailure.notFocused), "window_not_focused")
        XCTAssertEqual(ControlWindowFailure.deliveryReason(for: ControlWindowFailure.notFound), "window_not_found")
        XCTAssertEqual(ControlWindowFailure.deliveryReason(for: CocoaError(.featureUnsupported)), "input_delivery_failed")
    }

    /// Recorded 2026-10-14 on macOS 26.6.2 from a `CFUserNotificationDisplayAlert`,
    /// which `UserNotificationCenter` draws exactly like a TCC Automation prompt.
    private let promptWindow: [String: Any] = [
        "kCGWindowStoreType": 1, "kCGWindowNumber": NSNumber(value: 31_493), "kCGWindowSharingState": 1,
        "kCGWindowIsOnscreen": true, "kCGWindowOwnerName": "UserNotificationCenter", "kCGWindowAlpha": 1,
        "kCGWindowOwnerPID": 99_253, "kCGWindowLayer": 8,
        "kCGWindowBounds": ["Height": 202, "Width": 260, "X": 626, "Y": 189] as NSDictionary,
        "kCGWindowMemoryUsage": 2_368, "kCGWindowName": "",
    ]

    private func window(_ owner: String, number: Int, width: Int = 800, height: Int = 600, onScreen: Bool = true) -> [String: Any] {
        ["kCGWindowNumber": NSNumber(value: number), "kCGWindowOwnerName": owner, "kCGWindowIsOnscreen": onScreen,
         "kCGWindowLayer": 0, "kCGWindowBounds": ["Height": height, "Width": width, "X": 0, "Y": 0] as NSDictionary]
    }

    func testFindsTheRecordedPermissionPromptAmongOrdinaryWindows() {
        let dialogs = SystemDialogDetector.dialogs(in: [window("Google Chrome", number: 21_220), promptWindow,
                                                        window("Finder", number: 12)])
        XCTAssertEqual(dialogs, [SystemDialog(kind: .permission, owner: "UserNotificationCenter", windowID: 31_493,
                                              x: 626, y: 189, width: 260, height: 202)])
    }

    func testClassifiesEachSystemPromptOwner() {
        let dialogs = SystemDialogDetector.dialogs(in: [
            window("universalAccessAuthWarn", number: 4), window("SecurityAgent", number: 3),
            window("CoreServicesUIAgent", number: 2),
        ])
        XCTAssertEqual(dialogs.map(\.kind), [.openConfirmation, .password, .accessibility])
    }

    func testIgnoresSliversOffscreenWindowsAndOtherApps() {
        XCTAssertEqual(SystemDialogDetector.dialogs(in: [
            window("UserNotificationCenter", number: 5, width: 24, height: 24),
            window("UserNotificationCenter", number: 6, onScreen: false),
            window("Notification Center", number: 7),
            window("UserNotificationCenter", number: 0),
        ]), [])
    }

    func testRaiseResolvesOnlyTheObservedWindowAndOwnerEvenWithSeveralWindows() {
        var other = promptWindow
        other["kCGWindowNumber"] = NSNumber(value: 31_494)
        other["kCGWindowOwnerPID"] = NSNumber(value: 123)
        XCTAssertEqual(SystemDialogWindowRaiser.processID(windowID: 31_493, owner: "UserNotificationCenter", windows: [other, promptWindow]), 99_253)
        XCTAssertNil(SystemDialogWindowRaiser.processID(windowID: 31_493, owner: "SecurityAgent", windows: [other, promptWindow]))
        XCTAssertNil(SystemDialogWindowRaiser.processID(windowID: 31_493, owner: "UserNotificationCenter", windows: [other]))
        var hidden = promptWindow
        hidden["kCGWindowIsOnscreen"] = false
        XCTAssertNil(SystemDialogWindowRaiser.processID(windowID: 31_493, owner: "UserNotificationCenter", windows: [hidden]))
    }

    func testExactDialogCaptureIncludesItsElevatedLayerWithoutChoosingAnotherWindow() {
        let dialog = CaptureWindowCandidate(windowID: 31_493, applicationName: "UserNotificationCenter",
            width: 260, height: 202, windowLayer: 8)
        let exact = CaptureWindowTarget(appIdentifier: "UserNotificationCenter", windowNumber: 31_493)!
        XCTAssertEqual(CaptureWindowResolver.resolve(exact, candidates: [dialog]), 31_493)
        XCTAssertNil(CaptureWindowResolver.resolve(exact, candidates: []))
        XCTAssertNil(CaptureWindowResolver.resolve(CaptureWindowTarget(appIdentifier: "UserNotificationCenter")!, candidates: [dialog]))
        let wrong = CaptureWindowTarget(appIdentifier: "SecurityAgent", windowNumber: 31_493)!
        XCTAssertNil(CaptureWindowResolver.resolve(wrong, candidates: [dialog]))
    }
}
