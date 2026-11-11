import XCTest
@testable import WonderPairing

final class JSONFormattingTests: XCTestCase {
    private func formatted(_ text: String, file: StaticString = #filePath, line: UInt = #line) -> String {
        guard case .formatted(let value) = JSONFormatter.format(text) else {
            XCTFail("expected valid JSON", file: file, line: line); return ""
        }
        return value
    }
    private func error(_ text: String, file: StaticString = #filePath, line: UInt = #line) -> JSONFormatError {
        guard case .invalid(let value) = JSONFormatter.format(text) else {
            XCTFail("expected invalid JSON", file: file, line: line)
            return JSONFormatError(line: 0, column: 0, message: "")
        }
        return value
    }

    func testBigNumbersAndEscapesKeepTheirExactText() {
        let out = formatted(#"[12345678901234567890123, 0.10000000000000000555, 1E+400, -0, 1.50, "\u00e9\n\"\/"]"#)
        for expected in ["12345678901234567890123,", "0.10000000000000000555,", "1E+400,", "-0,", "1.50,", #""\u00e9\n\"\/""#] {
            XCTAssertTrue(out.contains(expected), expected)
        }
    }

    func testSizeAndDepthCapsFailInsteadOfHanging() {
        XCTAssertEqual(JSONFormatter.format("[1,2,3]", maxBytes: 4), .tooLarge)
        XCTAssertTrue(error(String(repeating: "[", count: JSONFormatter.maxDepth + 1)).message.contains("levels deep"))
        let atLimit = String(repeating: "[", count: JSONFormatter.maxDepth) + String(repeating: "]", count: JSONFormatter.maxDepth)
        if case .formatted = JSONFormatter.format(atLimit) {} else { XCTFail("depth at the limit is allowed") }
    }

    func testJSONLCapsRecordCountAndRecordSize() {
        let lines = (1...10).map { "{\"n\":\($0)}" }.joined(separator: "\n")
        let capped = JSONLDocument(parsing: lines, maxRecords: 3)
        XCTAssertEqual(capped.records.count, 3)
        XCTAssertEqual(capped.omittedLines, 7)

        let big = "{\"v\":\"" + String(repeating: "x", count: 100) + "\"}"
        let doc = JSONLDocument(parsing: big + "\n{\"ok\":1}", maxRecordBytes: 50)
        XCTAssertTrue(doc.records[0].isOversize)
        XCTAssertNil(doc.records[0].formatted)
        XCTAssertNotNil(doc.records[1].formatted)

        let long = JSONLDocument(parsing: String(repeating: "y", count: 9_000))
        XCTAssertLessThanOrEqual(long.records[0].raw.count, JSONLDocument.rawDisplayLimit + 1)
    }

}
