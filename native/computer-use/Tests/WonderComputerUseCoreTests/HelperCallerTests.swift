import XCTest
@testable import WonderComputerUseCore

final class HelperCallerTests: XCTestCase {
    private let helper = URL(fileURLWithPath: "/Applications/Cupola.app/Contents/Helpers/WonderComputerUse")

    func testOnlyTheBundledHostMayStartTheHelper() {
        let host = URL(fileURLWithPath: "/Applications/Cupola.app/Contents/MacOS/cupola")
        XCTAssertTrue(HelperCaller.permits(parentExecutable: host, helperAt: helper))
    }

    func testAnotherProgramOrAnotherCopyOfCupolaIsRefused() {
        for parent in ["/bin/zsh", "/usr/bin/python3", "/Users/other/Cupola.app/Contents/MacOS/cupola",
                       "/Applications/Cupola.app/Contents/Helpers/WonderComputerUse"] {
            XCTAssertFalse(HelperCaller.permits(parentExecutable: URL(fileURLWithPath: parent), helperAt: helper), parent)
        }
        XCTAssertFalse(HelperCaller.permits(parentExecutable: nil, helperAt: helper))
    }

    func testAHelperOutsideAnAppBundleHasNoHostSoNothingMayStartIt() {
        let loose = URL(fileURLWithPath: "/tmp/WonderComputerUse")
        XCTAssertNil(HelperCaller.hostExecutable(forHelperAt: loose))
        XCTAssertFalse(HelperCaller.permits(parentExecutable: URL(fileURLWithPath: "/tmp/cupola"), helperAt: loose))
    }

    func testAProcessReportsItsOwnExecutable() {
        XCTAssertNotNil(HelperCaller.executable(ofProcess: getpid()))
        XCTAssertNil(HelperCaller.executable(ofProcess: -1))
    }
}
