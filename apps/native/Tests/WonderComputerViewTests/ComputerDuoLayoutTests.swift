import CoreGraphics
import XCTest
@testable import WonderComputerView

/// The iPhone Duo laptop pose puts the computer above the fold and the trackpad
/// below it; every other window keeps today's layout.
final class ComputerFoldLayoutTests: XCTestCase {
    // Inner display upright (laptop/tabletop) and its 40 pt fold, as measured in
    // the iOS 27.1 simulator (plans/research/remote-desktop-sil-duo.md §3).
    private let upright = CGSize(width: 669, height: 951)
    private let uprightFold = CGRect(x: 0, y: 455.5, width: 669, height: 40)
    private let sideways = CGSize(width: 951, height: 669)
    private let sidewaysFold = CGRect(x: 455.5, y: 0, width: 40, height: 669)

}

/// Moonlight's touchpad rules on the separate trackpad: tap = click, two-finger
/// tap = right click, two fingers scroll, a 650 ms hold presses for a drag.
final class ComputerTrackpadGesturesTests: XCTestCase {
    private enum Event {
        case down(Int, CGFloat, CGFloat, TimeInterval)
        case move(Int, CGFloat, CGFloat, TimeInterval)
        case up(Int, CGFloat, CGFloat, TimeInterval)
        case tick(TimeInterval)
        case cancel
    }

    private func run(_ events: [Event]) -> [ComputerTrackpadGestures.Output] {
        var pad = ComputerTrackpadGestures()
        var output: [ComputerTrackpadGestures.Output] = []
        for event in events {
            switch event {
            case let .down(id, x, y, t): output += pad.began(id: id, at: CGPoint(x: x, y: y), time: t)
            case let .move(id, x, y, t): output += pad.moved(id: id, to: CGPoint(x: x, y: y), time: t)
            case let .up(id, x, y, t): output += pad.ended(id: id, at: CGPoint(x: x, y: y), time: t)
            case let .tick(t): output += pad.tick(time: t)
            case .cancel: output += pad.cancelled()
            }
        }
        return output
    }

    func testEventOrderingsTable() {
        typealias Out = ComputerTrackpadGestures.Output
        let rows: [(String, [Event], [Out])] = [
            ("tap clicks", [.down(1, 100, 100, 0), .up(1, 100, 100, 0.1)], [.click(button: "left")]),
            ("a wobble within the slop is still a tap",
             [.down(1, 100, 100, 0), .move(1, 104, 103, 0.05), .up(1, 104, 103, 0.1)], [.click(button: "left")]),
            ("slow move goes 1:1 and never clicks",
             [.down(1, 100, 100, 0), .move(1, 112, 100, 0.1), .move(1, 114, 100, 0.2), .up(1, 114, 100, 0.3)],
             [.move(CGSize(width: 12, height: 0)), .move(CGSize(width: 2, height: 0))]),
            ("two-finger tap right-clicks once",
             [.down(1, 100, 100, 0), .down(2, 140, 100, 0.02), .up(1, 100, 100, 0.12), .up(2, 140, 100, 0.13)],
             [.click(button: "right")]),
            ("two fingers moving together scroll, no click; resting before lifting does not coast",
             [.down(1, 100, 100, 0), .down(2, 140, 100, 0.02),
              .move(1, 100, 120, 0.05), .move(2, 140, 120, 0.06), .move(1, 100, 130, 0.08),
              .up(1, 100, 130, 0.3), .up(2, 140, 120, 0.31)],
             [.scroll(CGSize(width: 0, height: 20)), .scroll(CGSize(width: 0, height: 5))]),
            ("two fingers held too long are not a right click",
             [.down(1, 100, 100, 0), .down(2, 140, 100, 0.02), .up(1, 100, 100, 0.9), .up(2, 140, 100, 0.9)], []),
            ("moving, then a second finger, scrolls and never right-clicks",
             [.down(1, 100, 100, 0), .move(1, 120, 100, 0.1), .down(2, 160, 100, 0.15),
              .up(1, 120, 100, 0.2), .up(2, 160, 100, 0.21)],
             [.move(CGSize(width: 20, height: 0))]),
            ("650 ms hold presses, movement drags, lifting releases",
             [.down(1, 100, 100, 0), .tick(0.65), .move(1, 110, 105, 0.75), .up(1, 110, 105, 0.8)],
             [.dragBegan, .move(CGSize(width: 10, height: 5)), .dragEnded]),
            ("an early timer tick does not press",
             [.down(1, 100, 100, 0), .tick(0.4), .up(1, 100, 100, 0.45)], [.click(button: "left")]),
            ("a hold that moved first is a move, not a drag",
             [.down(1, 100, 100, 0), .move(1, 130, 100, 0.2), .tick(0.65), .up(1, 130, 100, 0.7)],
             [.move(CGSize(width: 30, height: 0))]),
            ("a second finger during a drag keeps the drag",
             [.down(1, 100, 100, 0), .tick(0.7), .down(2, 150, 100, 0.8), .move(2, 160, 120, 0.85),
              .move(1, 104, 100, 0.9), .up(1, 104, 100, 1.0), .up(2, 160, 120, 1.1)],
             [.dragBegan, .move(CGSize(width: 4, height: 0)), .dragEnded]),
            ("a cancelled drag releases the button", [.down(1, 100, 100, 0), .tick(0.7), .cancel], [.dragBegan, .dragEnded]),
            ("a cancelled tap does not click", [.down(1, 100, 100, 0), .cancel, .up(1, 100, 100, 0.1)], []),
            ("the leftover finger after a right click does nothing",
             [.down(1, 100, 100, 0), .down(2, 140, 100, 0.01), .up(2, 140, 100, 0.1),
              .move(1, 160, 100, 0.2), .up(1, 160, 100, 0.3)],
             [.click(button: "right")]),
            ("three-finger tap is not a right click",
             [.down(1, 100, 100, 0), .down(2, 140, 100, 0.01), .down(3, 180, 100, 0.02),
              .up(1, 100, 100, 0.1), .up(2, 140, 100, 0.1), .up(3, 180, 100, 0.1)], []),
            ("after everything lifts, a new tap clicks again",
             [.down(1, 100, 100, 0), .down(2, 140, 100, 0.01), .up(1, 100, 100, 0.1), .up(2, 140, 100, 0.1),
              .down(3, 50, 50, 1), .up(3, 50, 50, 1.1)],
             [.click(button: "right"), .click(button: "left")]),
        ]
        for (name, events, expected) in rows {
            XCTAssertEqual(run(events), expected, name)
        }
    }

}
