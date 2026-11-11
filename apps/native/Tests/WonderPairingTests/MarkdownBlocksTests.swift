import XCTest
@testable import WonderPairing

final class MarkdownBlocksTests: XCTestCase {

    func testEscapedPipesBelongToTheirTableCellIncludingCodeAndTrailingPipes() {
        let input = #"""
        | Value | Meaning |
        | --- | --- |
        | `left\|right` | Both sides |
        | end\| | Trailing pipe |
        | two\\| next |
        """#
        XCTAssertEqual(MarkdownBlock.parse(input), [.table(header: ["Value", "Meaning"], rows: [
            ["`left|right`", "Both sides"], ["end|", "Trailing pipe"], [#"two\\"#, "next"]])])
        XCTAssertEqual(MarkdownBlock.parse("a|b\n---|---\nx|y"), [.table(header: ["a", "b"], rows: [["x", "y"]])])
        // Empty pipe-only lines are literal prose, not a zero-column table.
        XCTAssertEqual(MarkdownBlock.parse("|\n|"), [.paragraph("|\n|")])
        // GFM separators need one dash per cell; alignment colons and short rules are valid.
        for (separator, header) in [("|-|-|", ["A", "B"]), ("|:-:|:-:|", ["A", "B"]), ("|:--|--:|", ["A", "B"]), ("-|-", ["A", "B"])] {
            let source = "| A | B |\n" + separator + "\n| one | two |"
            XCTAssertEqual(MarkdownBlock.parse(source), [.table(header: header, rows: [["one", "two"]])], separator)
        }
        // A malformed separator is ordinary text, not a table that discards a column.
        for separator in ["| --- |", "| --- | : |", "| --- | --- | --- |", "| - | -- | - |", "| :: | -- |"] {
            let source = "| A | B |\n" + separator + "\n| one | two |"
            XCTAssertEqual(MarkdownBlock.parse(source), [.paragraph(source)])
        }
    }

    func testNestedFencesKeepCodeAndUnfinishedStreamsReadable() {
        let input = "1. Prepare\n   - Nested\n    ```swift\n    let count = 2\n      print(count)\n    ```\n2. Finish"
        XCTAssertEqual(MarkdownBlock.parse(input), [.listItem(marker: "1.", depth: 0, text: "Prepare"),
            .listItem(marker: "•", depth: 1, text: "Nested"), .code(language: "swift", text: "let count = 2\n  print(count)"),
            .listItem(marker: "2.", depth: 0, text: "Finish")])
        XCTAssertEqual(MarkdownBlock.parse("- Run\n\n    ~~~text\n    still streaming"), [
            .listItem(marker: "•", depth: 0, text: "Run"), .code(language: "text", text: "still streaming")])
        var progress: MarkdownBlock.Progress?
        for length in 1...input.count {
            let source = String(input.prefix(length))
            let next = MarkdownBlock.parse(source, continuing: progress)
            XCTAssertEqual(next.blocks, MarkdownBlock.parse(source), "prefix \(length)")
            progress = next
        }
    }

    func testLiteralMarkupIdsJsonRepetitionAndLongPayloadsAreNotDeleted() {
        let literal = #"""
        <system-reminder>sample</system-reminder>
        <task-notification>sample</task-notification>
        <pasted_content id="demo">sample</pasted_content>
        <command-name>/sample</command-name>
        {"tool_call_id":"call_synthetic123","arguments":{"n":2}}
        """#
        XCTAssertEqual(MarkdownBlock.parse("```text\n" + literal + "\n```"), [.code(language: "text", text: literal)])
        XCTAssertEqual(MarkdownBlock.parse("Repeat exactly.\n\nRepeat exactly."), [.paragraph("Repeat exactly."), .paragraph("Repeat exactly.")])
        let long = String(repeating: "0123456789abcdef", count: 70)
        XCTAssertEqual(MarkdownBlock.parse(long), [.paragraph(long)])
        let imageSource = "data:image/png;base64," + String(repeating: "QUJD", count: 140)
        XCTAssertEqual(MarkdownBlock.parse("```text\n" + imageSource + "\n```"), [.code(language: "text", text: imageSource)])
        XCTAssertEqual(MarkdownBlock.plainText("Fish &amp; chips; &lt;tag&gt; is text."), "Fish & chips; <tag> is text.")
    }

    // A streaming reply continues its parse from the last finished block; at
    // every length it must equal a parse of the whole text. Regression: lists
    // continued after a blank line, code with blank lines, tables and quotes.
    func testStreamingParseContinuesToTheSameBlocksAsAFullParse() {
        let reply = """
        ## Plan
        First **step** here.

        - One
          still one

          continued after a blank line
        - Two

        ```swift
        let a = 1

        let b = 2
        ```

        > A quote

        | A | B |
        |---|---|
        | 1 | 2 |

        Last ☃ paragraph
        with two lines.
        """
        for step in [1, 3, 7, 40] {
            var progress: MarkdownBlock.Progress?
            var length = 0
            while length < reply.count {
                length = min(reply.count, length + step)
                let text = String(reply.prefix(length))
                let next = MarkdownBlock.parse(text, continuing: progress)
                XCTAssertEqual(next.blocks, MarkdownBlock.parse(text), "step \(step), length \(length)")
                progress = next
            }
            // A text that does not continue the previous one is read whole.
            XCTAssertEqual(MarkdownBlock.parse("Different\n\nreply", continuing: progress).blocks,
                           MarkdownBlock.parse("Different\n\nreply"))
        }
    }

}
