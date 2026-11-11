import CoreGraphics
import XCTest
import WonderPairing
@testable import WonderComputerView

/// The simulator view's input mapping: a point on the phone becomes the
/// normalized point of the simulator's native display the host injects.
final class SimulatorScreenTests: XCTestCase {
    private func assertPoint(_ point: CGPoint?, _ x: CGFloat, _ y: CGFloat, file: StaticString = #filePath, line: UInt = #line) {
        guard let point else { return XCTFail("no point", file: file, line: line) }
        XCTAssertEqual(point.x, x, accuracy: 0.001, file: file, line: line)
        XCTAssertEqual(point.y, y, accuracy: 0.001, file: file, line: line)
    }

    /// Turned a quarter clockwise, the native top-left corner shows at the
    /// top right and the screen is landscape; every turn round-trips.
    func testQuarterTurnsMapBackToTheNativeDisplay() {
        let viewport = CGSize(width: 400, height: 800)
        let native = CGSize(width: 100, height: 200)
        let turned = SimulatorScreenGeometry(viewport: viewport, nativeSize: native, quarterTurns: 1)
        XCTAssertEqual(turned.screen.width, 400, accuracy: 0.001)
        XCTAssertEqual(turned.screen.height, 200, accuracy: 0.001)
        XCTAssertEqual(turned.unrotatedSize, CGSize(width: 200, height: 400))
        assertPoint(turned.devicePoint(CGPoint(x: turned.screen.maxX, y: turned.screen.minY)), 0, 0)
        assertPoint(turned.devicePoint(CGPoint(x: turned.screen.minX, y: turned.screen.minY)), 0, 1)
        for turns in 0..<4 {
            let geometry = SimulatorScreenGeometry(viewport: viewport, nativeSize: native, quarterTurns: turns)
            for point in [CGPoint(x: 0.1, y: 0.2), CGPoint(x: 0.9, y: 0.6), CGPoint(x: 0.5, y: 0.5)] {
                assertPoint(geometry.devicePoint(geometry.location(for: point)), point.x, point.y)
            }
        }
        XCTAssertEqual(SimulatorScreenGeometry(viewport: viewport, nativeSize: native, quarterTurns: 5).quarterTurns, 1)
        XCTAssertEqual(SimulatorScreenGeometry(viewport: viewport, nativeSize: native, quarterTurns: -1).quarterTurns, 3)
    }

    /// Accessibility gestures are single batches the host accepts: pauses
    /// stay within its 1 s limit and every touch lifts where it ends.
    func testAccessibilityGesturesAreSingleBatchesWithinTheHostLimits() {
        XCTAssertEqual(SimulatorGesture.tap(at: CGPoint(x: 0.5, y: 0.5)).last, .touch(x: 0.5, y: 0.5, phase: "up", delayMs: 50))
        XCTAssertEqual(SimulatorGesture.longPress(at: CGPoint(x: 0.5, y: 0.5)).last, .touch(x: 0.5, y: 0.5, phase: "up", delayMs: 800))
        for direction in SimulatorGesture.Direction.allCases {
            let actions = SimulatorGesture.swipe(direction)
            guard case let .touch(_, _, "down", _) = actions.first, case let .touch(x, y, "up", _) = actions.last,
                  case let .touch(moveX, moveY, "move", _) = actions[1] else { return XCTFail("\(direction)") }
            XCTAssertEqual(CGPoint(x: x, y: y), CGPoint(x: moveX, y: moveY))
        }
        guard case let .touch(_, startY, _, _) = SimulatorGesture.swipe(.up).first,
              case let .touch(_, endY, _, _) = SimulatorGesture.swipe(.up).last else { return XCTFail() }
        XCTAssertGreaterThan(startY, endY, "Swipe up drags the finger upwards")
    }
}
