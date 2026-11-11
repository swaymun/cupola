import XCTest
@testable import WonderPairing

/// Mirrors crates/wonderd/src/injected_context.rs tests beyond the shared fixture.
final class OwnerDisplayTextTests: XCTestCase {
    func testMarkupTheOwnerTypedStaysAndHarnessWrappersGo() {
        let envelope = "\n\n<t3_context version=\"1\">\n<context kind=\"thread\" id=\"thread_1\">\ntitle: A\n</context>\n</t3_context>"
        let kept = [
            "why does `<t3_context version=\"1\">` appear?", "see [Thread: Title; ref=thread_1] in the docs",
            "<system-reminder>x</system-reminder>\nFix it", "<INSTRUCTIONS>\nBe brief.\n</INSTRUCTIONS>",
            "```xml\n<environment_context>\n<cwd>/x</cwd>\n</environment_context>\n```", "a < b and c > d", "  padded  ", "",
            "<command-name>/review</command-name>\n<command-args>42</command-args>",
        ]
        for text in kept { XCTAssertEqual(OwnerDisplayText.render(text), text) }
        let cases: [(String, String)] = [
            ("explain `<t3_context>` and [Thread: A; ref=thread_1]" + envelope, "explain `<t3_context>` and Thread: A"),
            ("open [Mention: @a/b.swift; ref=thread_1] and [Skill: $x; ref=thread_1]" + envelope, "open @a/b.swift and $x"),
            ("cut short [Thread: A; ref=thread_1]\n\n<t3_context version=\"1\">\n<context kind=\"thread\" id=\"thread_1\">\ntitle", "cut short Thread: A"),
            ("fix this @terminal-1:3-4\n\n<terminal_context>\n- Terminal 1 lines 3-4:\n  3 | error\n</terminal_context>", "fix this @terminal-1:3-4"),
            ("<environment_context>\n  <cwd>/Users/me/app</cwd>\n</environment_context>", ""),
            ("# AGENTS.md instructions for /app\n\n<INSTRUCTIONS>\nBe brief.\n</INSTRUCTIONS>\n\nrename it", "rename it"),
            ("<image name=[Image #1]>", ""), ("</image>", ""),
        ]
        for (input, expected) in cases { XCTAssertEqual(OwnerDisplayText.render(input), expected, input) }
    }

    func testTheRowShowsOwnerWordsAndKeepsTheStoredItemText() {
        let raw = "hi [Thread: A; ref=thread_1]\n\n<t3_context version=\"1\">\n<context kind=\"thread\" id=\"thread_1\">\n</context>\n</t3_context>"
        let item = ReadItem(id: "u", type: "userMessage", state: "completed", text: raw, createdAt: "1000")
        let row = ReadRow(id: "u", author: "You", text: raw, isUser: true, timestamp: "1000", item: item)
        XCTAssertEqual(row.text, "hi Thread: A")
        XCTAssertEqual(row.item?.text, raw)
    }
}
