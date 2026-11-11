import XCTest
import ScreenCaptureKit
@testable import WonderComputerUseCore

final class CaptureCoreTests: XCTestCase {
    /// A still Simulator sends no frames of its own: the picture is resent quickly
    /// right after a start or a new link, then once a second, so a viewer
    /// connecting late is never left waiting for the screen to change.
    func testStillSimulatorResendsQuicklyAfterAStartThenOncePerSecond() {
        var cadence = SimulatorRefreshCadence()
        cadence.startQuickPhase(at: 100)
        XCTAssertFalse(cadence.resendIsDue(now: 100.1, lastDelivery: 100))
        XCTAssertTrue(cadence.resendIsDue(now: 100.2, lastDelivery: 100))
        XCTAssertTrue(cadence.resendIsDue(now: 100.16, lastDelivery: 100), "an early tick still counts")
        XCTAssertTrue(cadence.resendIsDue(now: 102.9, lastDelivery: 102.7))
        XCTAssertFalse(cadence.resendIsDue(now: 103.5, lastDelivery: 103.2), "idle pace after the quick window")
        XCTAssertTrue(cadence.resendIsDue(now: 104.2, lastDelivery: 103.2))
        // A new link restarts the quick phase.
        cadence.startQuickPhase(at: 200)
        XCTAssertTrue(cadence.resendIsDue(now: 200.2, lastDelivery: 200))
    }

    func testSimulatorUsesItsOwnSurfaceWithoutGrantingScreenRecording() {
        var machine = CaptureStateMachine()
        let prepared = machine.prepare(sessionID: "sim", generation: 1, viewerCount: 1,
                                       screenRecordingAuthorized: false, sourceKind: .simulator)
        XCTAssertEqual(prepared.state, .awaitingSource)
        XCTAssertFalse(prepared.screenRecordingAuthorized)
        let source = CaptureSourceDescriptor(id: "simulator:fixture", kind: .simulator, title: "Phone",
            width: 1206, height: 2622, scale: 1,
            contentRect: CaptureRect(x: 0, y: 0, width: 1206, height: 2622))
        XCTAssertEqual(machine.selectSource(source).state, .ready)
        XCTAssertEqual(machine.requestStart().state, .starting)
        XCTAssertEqual(machine.captureStarted().state, .capturing)
        XCTAssertEqual(machine.updateScreenRecordingAuthorization(false).state, .capturing)
        XCTAssertEqual(machine.pause().state, .paused)
        XCTAssertEqual(machine.resume().state, .starting)
        XCTAssertEqual(machine.sourceRemoved().state, .sourceRemoved)
        XCTAssertEqual(machine.resume().state, .sourceRemoved)
        // A new ordinary screen session still needs the actual Mac permission.
        XCTAssertEqual(machine.prepare(sessionID: "screen", generation: 2, viewerCount: 1,
                                       screenRecordingAuthorized: false).state, .permissionDenied)
        XCTAssertEqual(machine.selectSource(source).state, .permissionDenied)
    }

    func testSelectionPlanPrefersTheMainDisplayWhenMultipleDisplaysAreAvailable() {
        let plan = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: nil,
            displayIDs: [1, 2],
            sourceIDs: ["display:1", "display:2", "window:9"],
            mainDisplayID: 2,
            pickerAvailable: true
        )

        XCTAssertEqual(plan, .select(sourceID: "display:2"))
    }

    func testSavedDisplayWinsAndDisconnectedChoiceFallsBackToMain() {
        let available = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: nil,
            displayIDs: [1, 2],
            sourceIDs: ["display:1", "display:2"],
            mainDisplayID: 2,
            pickerAvailable: true,
            preferredDisplayID: 1
        )
        XCTAssertEqual(available, .select(sourceID: "display:1"))

        let disconnected = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: nil,
            displayIDs: [1, 2],
            sourceIDs: ["display:1", "display:2"],
            mainDisplayID: 2,
            pickerAvailable: true,
            preferredDisplayID: 3
        )
        XCTAssertEqual(disconnected, .select(sourceID: "display:2"))
    }

    func testSelectionPlanDoesNotChooseAnUnrelatedWindowWhenTheMainDisplayIsMissing() {
        let plan = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: nil,
            displayIDs: [1],
            sourceIDs: ["display:1", "window:9"],
            mainDisplayID: 2,
            pickerAvailable: true
        )

        XCTAssertEqual(plan, .presentSharingPicker)
    }

    func testSelectionPlanUsesTheMainDisplayWhenThePickerIsUnavailable() {
        let plan = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: nil,
            displayIDs: [1, 2],
            sourceIDs: ["display:1", "display:2", "window:9"],
            mainDisplayID: 2,
            pickerAvailable: false
        )

        XCTAssertEqual(plan, .select(sourceID: "display:2"))
    }

    func testSelectionPlanHonorsAnExplicitWindow() {
        let plan = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: "window:9",
            displayIDs: [1],
            sourceIDs: ["display:1", "window:9"],
            mainDisplayID: 1,
            pickerAvailable: true
        )

        XCTAssertEqual(plan, .select(sourceID: "window:9"))
    }

    func testSelectionPlanRejectsAnExplicitSourceThatWasNotEnumerated() {
        let plan = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: "window:9",
            displayIDs: [1],
            sourceIDs: ["display:1"],
            mainDisplayID: 1,
            pickerAvailable: true
        )

        XCTAssertEqual(plan, .unavailable(reason: "source_not_found"))
    }

    func testLifecycleRequiresPermissionAndThenUsesExplicitPickerPendingState() {
        var machine = CaptureStateMachine(helperInstanceID: "helper", pickerAvailable: true)

        let denied = machine.prepare(sessionID: "session", generation: 1, viewerCount: 1, screenRecordingAuthorized: false)
        XCTAssertEqual(denied.state, .permissionDenied)
        XCTAssertEqual(denied.message, "Allow Screen Recording for Cupola on your Mac")
        XCTAssertEqual(denied.reason, "screen_recording_permission_required")

        XCTAssertEqual(machine.beginPickerSelection().state, .permissionDenied)
        let authorized = machine.updateScreenRecordingAuthorization(true)
        XCTAssertTrue(authorized.screenRecordingAuthorized)
        let pending = machine.beginPickerSelection()
        XCTAssertEqual(pending.state, .awaitingSource)
        XCTAssertEqual(pending.message, "Choose what to share on your Mac")

        let source = CaptureSourceDescriptor(
            id: "display:1",
            kind: .display,
            title: "Display 1",
            width: 3_840,
            height: 2_160,
            scale: 2,
            contentRect: CaptureRect(x: 0, y: 0, width: 1_920, height: 1_080)
        )
        XCTAssertEqual(machine.selectSource(source).state, .ready)
        XCTAssertEqual(machine.requestStart().state, .starting)
        XCTAssertEqual(machine.captureStarted().state, .capturing)
        XCTAssertEqual(machine.pause().state, .paused)
        XCTAssertEqual(machine.resume().state, .starting)
        XCTAssertEqual(machine.captureStarted().state, .capturing)
        XCTAssertEqual(machine.suspended().state, .suspended)
        XCTAssertEqual(machine.resumedFromSuspension().state, .capturing)
        XCTAssertEqual(machine.setViewerCount(0).state, .stopped)
        XCTAssertEqual(machine.status.reason, "no_authorized_viewers")
    }

    func testNoAuthorizedViewerStopsAReadySessionBeforeCaptureStarts() {
        var machine = CaptureStateMachine()
        _ = machine.prepare(sessionID: "session", generation: 1, viewerCount: 1, screenRecordingAuthorized: true)
        XCTAssertEqual(machine.setViewerCount(0).state, .stopped)
        XCTAssertEqual(machine.status.reason, "no_authorized_viewers")
    }

    func testNativeWindowSuspensionResumesTheSameSourceOnAUsableFrame() {
        for unavailable in [SCFrameStatus.suspended, .blank] {
            var machine = capturingWindow()
            let sourceID = machine.status.sourceID
            XCTAssertEqual(machine.recordFrameStatus(unavailable).state, .suspended)
            XCTAssertEqual(machine.status.queuedFrameCount, 0)
            XCTAssertEqual(machine.recordFrameStatus(.idle).state, .suspended)
            XCTAssertEqual(machine.recordFrameStatus(.complete).state, .capturing)
            XCTAssertEqual(machine.status.sourceID, sourceID)
            XCTAssertNil(machine.status.reason)
            XCTAssertEqual(machine.recordFrameStatus(.stopped).state, .stopped)
            XCTAssertEqual(machine.recordFrameStatus(.started).state, .stopped)
        }
    }

    /// Closing or minimizing the shown window suspends it; choosing another
    /// window then must move the same stream instead of ending the view.
    func testSuspendedWindowCanSwitchToAnotherWindow() async {
        var machine = capturingWindow()
        XCTAssertEqual(machine.suspended().state, .suspended)
        let session = ScreenCaptureSession(machine: machine, screenRecordingAccess: { true },
                                           sourceExists: { $0 != "window:404" })

        let other = await session.rebind(sessionID: "window-session", generation: 2, sourceID: "window:10")
        XCTAssertEqual(other.errorCode, "invalid_capture_state", "Another session's switch is refused")
        let missing = await session.rebind(sessionID: "window-session", generation: 1, sourceID: "window:404")
        XCTAssertEqual(missing.errorCode, "source_not_found", "A suspended view looks up the chosen window")
        XCTAssertEqual(missing.status.state, .suspended, "A window that is gone keeps the current view")

        // Past both checks the switch selects the new source. An unknown
        // simulator fails selection without ScreenCaptureKit or CoreSimulator.
        let selected = await session.rebind(sessionID: "window-session", generation: 1, sourceID: "simulator:fixture")
        XCTAssertEqual(selected.errorCode, "simulator_unavailable")
    }

    func testNativeFrameNotificationsCannotUndoPauseRevocationOrSourceRemoval() {
        var paused = capturingWindow()
        paused.pause()
        var revoked = capturingWindow()
        revoked.permissionDenied()
        var removed = capturingWindow()
        removed.sourceRemoved()
        for frameStatus in [SCFrameStatus.suspended, .blank, .stopped, .complete, .started] {
            XCTAssertEqual(paused.recordFrameStatus(frameStatus).state, .paused)
            XCTAssertEqual(revoked.recordFrameStatus(frameStatus).state, .permissionDenied)
            XCTAssertEqual(removed.recordFrameStatus(frameStatus).state, .sourceRemoved)
        }
    }

    private func capturingWindow() -> CaptureStateMachine {
        var machine = CaptureStateMachine()
        machine.prepare(sessionID: "window-session", generation: 1, viewerCount: 1, screenRecordingAuthorized: true)
        machine.selectSource(CaptureSourceDescriptor(
            id: "window:9", kind: .window, title: "Fixture", width: 460, height: 292,
            scale: 1, contentRect: CaptureRect(x: 180, y: 430, width: 460, height: 292)
        ))
        machine.requestStart()
        machine.captureStarted()
        return machine
    }

    func testPermissionRevocationStopsTruthfullyAndCannotSelectASource() {
        var machine = CaptureStateMachine(pickerAvailable: true)
        _ = machine.prepare(sessionID: "session", generation: 1, viewerCount: 1, screenRecordingAuthorized: true)
        let revoked = machine.updateScreenRecordingAuthorization(false)
        XCTAssertEqual(revoked.state, .permissionDenied)
        XCTAssertFalse(revoked.screenRecordingAuthorized)
        XCTAssertEqual(revoked.message, "Allow Screen Recording for Cupola on your Mac")

        let source = CaptureSourceDescriptor(
            id: "display:1",
            kind: .display,
            title: "Display 1",
            width: 1_280,
            height: 720,
            scale: 2,
            contentRect: .zero
        )
        XCTAssertEqual(machine.selectSource(source).state, .permissionDenied)
        XCTAssertEqual(machine.beginPickerSelection().state, .permissionDenied)
    }

    func testStaleSelectionCannotMutateANewerPreparedSession() {
        var machine = CaptureStateMachine()
        _ = machine.prepare(sessionID: "old", generation: 1, viewerCount: 1, screenRecordingAuthorized: true)
        _ = machine.prepare(sessionID: "new", generation: 2, viewerCount: 1, screenRecordingAuthorized: true)
        let source = CaptureSourceDescriptor(
            id: "display:1",
            kind: .display,
            title: "Display 1",
            width: 1_280,
            height: 720,
            scale: 1,
            contentRect: .zero
        )

        let unchanged = machine.selectSource(source, sessionID: "old", generation: 1)

        XCTAssertEqual(unchanged.state, .awaitingSource)
        XCTAssertEqual(unchanged.sessionID, "new")
        XCTAssertNil(unchanged.sourceID)
    }

    func testRestartInvalidatesIdentityAndStopIsIdempotent() {
        var machine = CaptureStateMachine(helperInstanceID: "helper")
        _ = machine.prepare(sessionID: "session", generation: 5, viewerCount: 1, screenRecordingAuthorized: true)
        let restarted = machine.helperRestarted()
        XCTAssertEqual(restarted.state, .helperRestarted)
        XCTAssertNil(restarted.sessionID)
        XCTAssertEqual(restarted.generation, 6)

        let stopped = machine.stop()
        XCTAssertEqual(stopped.state, .stopped)
        XCTAssertEqual(stopped.queuedFrameCount, 0)
        XCTAssertEqual(machine.stop(), stopped)
    }

    func testLatestFrameQueueDropsSupersededFramesAndNeverExceedsBound() {
        let queue = LatestFrameQueue<Int>(capacity: 2)
        XCTAssertFalse(queue.offer(1))
        XCTAssertFalse(queue.offer(2))
        XCTAssertTrue(queue.offer(3))
        XCTAssertEqual(queue.statistics(), .init(count: 2, droppedCount: 1))
        XCTAssertEqual(queue.takeLatest(), 3)
        XCTAssertNil(queue.takeLatest())
        XCTAssertEqual(queue.statistics().droppedCount, 1)
        queue.reset()
        XCTAssertEqual(queue.statistics(), .init(count: 0, droppedCount: 0))
    }

    func testFrameMetadataIsBoundedAndCarriesGeometryAndIdentity() {
        let source = CaptureSourceDescriptor(
            id: "window:7",
            kind: .window,
            title: "Fixture",
            width: 1_920,
            height: 1_080,
            scale: 2,
            contentRect: CaptureRect(x: 10, y: 20, width: 960, height: 540)
        )
        let metadata = CaptureFrameMetadata.make(
            timestamp: .infinity,
            sourceWidth: 1_280,
            sourceHeight: 720,
            source: source,
            geometryRevision: 4,
            frameSequence: 9,
            sessionID: "session",
            generation: 3
        )
        XCTAssertTrue(metadata.captureTimestamp.isFinite)
        XCTAssertEqual(metadata.geometryRevision, 4)
        XCTAssertEqual(metadata.sourceWidth, 1_280)
        XCTAssertEqual(metadata.cropRect.width, 1_280)
        XCTAssertEqual(metadata.cropRect.height, 720)
        XCTAssertEqual(metadata.frameSequence, 9)
        XCTAssertEqual(metadata.sessionID, "session")
        XCTAssertEqual(metadata.generation, 3)
    }

    func testCaptureConfigurationBoundsAndQualityChoices() {
        let configuration = CaptureConfiguration(width: 4_000, height: 3_000, framesPerSecond: 120, audioEnabled: true, queueDepth: 100)
        XCTAssertEqual(configuration.width, 2_560)
        XCTAssertEqual(configuration.height, 1_440)
        XCTAssertEqual(configuration.framesPerSecond, 120)
        XCTAssertEqual(CaptureConfiguration(framesPerSecond: 500).framesPerSecond, 120)
        XCTAssertEqual(CaptureConfiguration().framesPerSecond, 15)
        XCTAssertEqual(CaptureVideoQuality.medium.configuration(framesPerSecond: 60).framesPerSecond, 60)
        XCTAssertFalse(configuration.audioEnabled)
        XCTAssertEqual(configuration.queueDepth, 2)
        XCTAssertEqual(CaptureVideoQuality.standard.configuration, CaptureConfiguration(width: 1_280, height: 720))
        XCTAssertEqual(CaptureVideoQuality.medium.configuration, CaptureConfiguration(width: 1_920, height: 1_080))
        XCTAssertEqual(CaptureVideoQuality.auto.configuration, CaptureConfiguration(width: 2_560, height: 1_440))
        XCTAssertEqual(CaptureVideoQuality.high.configuration, CaptureConfiguration(width: 2_560, height: 1_440))
        var machine = CaptureStateMachine(configuration: .init(), pickerAvailable: true)
        let prepared = machine.prepare(sessionID: "session", generation: 1, viewerCount: 1,
                                       screenRecordingAuthorized: true, configuration: CaptureVideoQuality.high.configuration)
        XCTAssertEqual(prepared.configuration, CaptureVideoQuality.high.configuration)
        let source = CaptureSourceDescriptor(id: "display:1", kind: .display, title: "Display 1",
                                             width: 1920, height: 1080, scale: 1,
                                             contentRect: CaptureRect(x: 0, y: 0, width: 1920, height: 1080))
        let ready = machine.selectSource(source)
        XCTAssertNil(machine.updateConfiguration(CaptureVideoQuality.standard.configuration,
                                                 sessionID: "old-session", generation: 1))
        let resized = machine.updateConfiguration(CaptureVideoQuality.medium.configuration,
                                                  sessionID: "session", generation: 1)
        XCTAssertEqual(resized?.sessionID, ready.sessionID)
        XCTAssertEqual(resized?.geometryRevision, ready.geometryRevision)
        XCTAssertEqual(resized?.sourceID, ready.sourceID)
        XCTAssertEqual(resized?.state, .ready)
        XCTAssertEqual(resized?.configuration, CaptureVideoQuality.medium.configuration)
    }

    func testCaptureConfigurationCentersSourceWithoutChangingEncodedDimensions() {
        let cases: [(Double, Double, CaptureRect)] = [
            (1_920, 1_080, CaptureRect(x: 0, y: 0, width: 1_280, height: 720)),
            (1_440, 900, CaptureRect(x: 64, y: 0, width: 1_152, height: 720)),
            (1_024, 768, CaptureRect(x: 160, y: 0, width: 960, height: 720)),
            (900, 1_600, CaptureRect(x: 437.5, y: 0, width: 405, height: 720)),
            (3_840, 1_080, CaptureRect(x: 0, y: 180, width: 1_280, height: 360)),
        ]
        for (width, height, expected) in cases {
            let source = CaptureSourceDescriptor(
                id: "display:1", kind: .display, title: "Fixture",
                width: Int(width * 2), height: Int(height * 2), scale: 2,
                contentRect: CaptureRect(x: -1_920, y: 100, width: width, height: height)
            )
            // Inspect the actual ScreenCaptureKit configuration, not only the
            // geometry helper. No screen recording or permission is required.
            let configuration = ScreenCaptureSession.makeStreamConfiguration(CaptureConfiguration(), source: source)
            XCTAssertEqual(configuration.width, 1_280)
            XCTAssertEqual(configuration.height, 720)
            XCTAssertEqual(configuration.destinationRect.origin.x, expected.x, accuracy: 0.001)
            XCTAssertEqual(configuration.destinationRect.origin.y, expected.y, accuracy: 0.001)
            XCTAssertEqual(configuration.destinationRect.width, expected.width, accuracy: 0.001)
            XCTAssertEqual(configuration.destinationRect.height, expected.height, accuracy: 0.001)
            XCTAssertTrue(configuration.scalesToFit)
            XCTAssertEqual(configuration.pixelFormat, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
            if #available(macOS 14.0, *) {
                XCTAssertTrue(configuration.preservesAspectRatio)
            }
        }
    }

    func testStreamIntervalFollowsTheFrameRateCeiling() {
        let source = CaptureSourceDescriptor(
            id: "display:1", kind: .display, title: "Fixture", width: 2_560, height: 1_440, scale: 2,
            contentRect: CaptureRect(x: 0, y: 0, width: 1_280, height: 720))
        for fps in [15, 30, 60, 120] {
            let configuration = ScreenCaptureSession.makeStreamConfiguration(
                CaptureConfiguration(framesPerSecond: fps), source: source)
            XCTAssertEqual(CMTimeGetSeconds(configuration.minimumFrameInterval), 1 / Double(fps), accuracy: 1e-9)
        }
    }

    func testCaptureDestinationUsesLogicalBoundsAndFallsBackForMissingGeometry() {
        let logicalSource = CaptureSourceDescriptor(
            id: "display:1", kind: .display, title: "Fixture",
            width: 1_280, height: 720, scale: 2,
            contentRect: CaptureRect(x: 40, y: -200, width: 1_440, height: 900)
        )
        let configuration = CaptureConfiguration(width: 640, height: 360)
        XCTAssertEqual(configuration.centeredDestinationRect(for: logicalSource),
                       CaptureRect(x: 32, y: 0, width: 576, height: 360))

        let fallbackSource = CaptureSourceDescriptor(
            id: "window:7", kind: .window, title: "Fixture",
            width: 1_024, height: 768, scale: 1, contentRect: .zero
        )
        XCTAssertEqual(configuration.centeredDestinationRect(for: fallbackSource),
                       CaptureRect(x: 80, y: 0, width: 480, height: 360))

        let invalidSource = CaptureSourceDescriptor(
            id: "picker-selection", kind: .picker, title: "Fixture",
            width: 0, height: 0, scale: 1,
            contentRect: CaptureRect(x: 0, y: 0, width: .infinity, height: .nan)
        )
        XCTAssertEqual(configuration.centeredDestinationRect(for: invalidSource),
                       CaptureRect(x: 0, y: 0, width: 640, height: 360))
    }
}
