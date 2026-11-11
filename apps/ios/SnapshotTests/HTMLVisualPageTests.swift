import XCTest
@testable import WonderNativeReplyUI

/// Observable contract: an agent's HTML page reads as native blocks and v1
/// catalog components, deterministically and within fixed bounds; nothing in it
/// runs or loads, and what has no native form is counted, so the page says
/// once that parts of it can't be shown rather than dropping them silently.
/// The fixtures are two real agent-written pages (Fixtures/).
final class HTMLVisualPageTests: XCTestCase {
    private func fixture(_ name: String) throws -> String {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "html", subdirectory: "Fixtures"))
        return try String(contentsOf: url, encoding: .utf8)
    }

    private func components(_ page: HTMLVisualPage) -> [NativeUIComponent] {
        page.blocks.flatMap { block -> [NativeUIComponent] in
            if case .interface(let document) = block { document.components } else { [] }
        }
    }

    private func component(_ page: HTMLVisualPage, _ title: String) throws -> NativeUIComponent {
        try XCTUnwrap(components(page).first { $0.title == title }, "no component titled \(title)")
    }

    private func texts(_ page: HTMLVisualPage) -> [String] {
        page.blocks.compactMap { block in
            switch block {
            case .text(let text), .callout(let text, _), .code(let text), .heading(let text, _), .labeled(_, let text): text
            case .interface, .images, .drawing, .details, .rule: nil
            }
        }
    }

    func testConversionIsDeterministic() throws {
        for name in ["t3-split-view", "t3-portrait-split"] {
            let html = try fixture(name)
            XCTAssertEqual(HTMLVisualPage.convert(html), HTMLVisualPage.convert(html))
        }
    }

    func testChartsComeFromNumericTablesSVGBarsAndStyledBars() throws {
        let html = """
        <h2>Lines by area</h2>
        <table><tr><th>Area</th><th>Lines</th></tr><tr><td>Rust</td><td>94,946</td></tr><tr><td>Swift</td><td>21.6k</td></tr></table>
        <svg viewBox="0 0 300 100" aria-label="Builds per day"><rect x="0" y="0" width="300" height="100"/>
          <text x="0" y="15">Mon</text><rect x="40" y="5" width="120" height="14"/><text x="170" y="15">12</text>
          <text x="0" y="40">Tue</text><rect x="40" y="30" width="60" height="14"/><text x="110" y="40">6</text></svg>
        <h3>Turns</h3>
        <div class="chart">
          <div class="row"><span>Codex</span><div class="track"><div class="bar" style="width: 80%"></div></div><span>40</span></div>
          <div class="row"><span>Claude</span><div class="track"><div class="bar" style="width:20%"></div></div></div>
        </div>
        """
        let charts = components(HTMLVisualPage.convert(html)).compactMap { component -> (String, [String], [Double])? in
            guard case .chart(let bars, _) = component.content else { return nil }
            return (component.title, bars.map(\.label), bars.map(\.value))
        }
        XCTAssertEqual(charts.map(\.0), ["Lines by area", "Builds per day", "Turns"])
        XCTAssertEqual(charts[0].1, ["Rust", "Swift"]); XCTAssertEqual(charts[0].2, [94_946, 21_600])
        XCTAssertEqual(charts[1].1, ["Mon", "Tue"]); XCTAssertEqual(charts[1].2, [12, 6])
        XCTAssertEqual(charts[2].1, ["Codex", "Claude"]); XCTAssertEqual(charts[2].2, [40, 20])
    }

    func testCalloutsCodeListsAndEntities() {
        let page = HTMLVisualPage.convert("""
        <div class="callout warning"><b>Heads up:</b> restart &amp; retry</div>
        <pre><code>let x = 1
          y()</code></pre>
        <ul><li>One</li><li>Two <code>x</code></li></ul>
        <p>5 &lt; 6 &#x2192; *ok*</p>
        <dl><dt>Branch</dt><dd>main</dd></dl>
        """)
        XCTAssertEqual(Array(page.blocks.prefix(4)), [
            .callout("**Heads up:** restart & retry", tone: .warning),
            .code("let x = 1\n  y()"),
            .text("- One\n- Two `x`"),
            .text("5 \\< 6 → \\*ok\\*"),
        ])
        guard case .keyValue(let entries) = components(page).first?.content else { return XCTFail() }
        XCTAssertEqual(entries.map(\.label), ["Branch"]); XCTAssertEqual(entries.map(\.value), ["main"])
    }

    func testScriptsFormsFramesAndLinksNeverBecomeContent() {
        XCTAssertEqual(HTMLVisualPage.convert("<p>Done</p>").blocks, [.text("Done")])
        let page = HTMLVisualPage.convert("""
        <style>p{color:red}</style><script>document.write("pwned")</script>
        <form><input value="secret"><button>Go</button></form><nav><a href="#x">Jump</a></nav>
        <p>Body <a href="https://example.com/x">link</a></p>
        <iframe src="https://example.com"></iframe><img src="https://example.com/p.png" alt="Logo">
        """)
        XCTAssertEqual(page.blocks, [.text("Body link")])
        XCTAssertEqual(page.omitted, 3, "the frame, the remote picture and the script")
    }

    func testBoundsHoldForDeepLongAndOversizedPages() {
        let deep = HTMLVisualPage.convert(String(repeating: "<div>", count: 5_000) + "deep" + String(repeating: "</div>", count: 5_000))
        XCTAssertTrue(deep.truncated)
        XCTAssertTrue(deep.blocks.contains(.text("deep")))

        let long = HTMLVisualPage.convert((0..<3_000).map { "<p>Line \($0) of the report.</p>" }.joined())
        XCTAssertTrue(long.truncated)
        XCTAssertLessThanOrEqual(long.blocks.count, HTMLVisualPage.Limits.blocks + 1)
        XCTAssertTrue(long.isPartial)
        // The note about missing parts follows the whole page, not its opening.
        XCTAssertFalse(long.opening(16).page.isPartial)
        XCTAssertTrue(long.opening(long.blocks.count).page.isPartial)

        let oversized = HTMLVisualPage.convert(String(repeating: "<b>x</b>", count: 140_000))
        XCTAssertTrue(oversized.truncated)
    }

    func testHTMLFenceNeedsACompleteBlock() {
        XCTAssertEqual(HTMLVisualPage.htmlFence(in: "Here it is:\n```html\n<h1>A</h1>\n```\nDone."), "<h1>A</h1>")
        XCTAssertNil(HTMLVisualPage.htmlFence(in: "Streaming:\n```html\n<h1>A</h1>"))
        XCTAssertNil(HTMLVisualPage.htmlFence(in: "```htmlish\n<h1>A</h1>\n```"))
        XCTAssertNil(HTMLVisualPage.htmlFence(in: "```html\n\n```"))
    }

    func testCachePreparesEachExactPageOnce() async {
        let cache = HTMLVisualCache()
        let reply = "Page:\n```html\n<h1>Report</h1><p>Ready</p>\n```"
        let first = await cache.prepare(reply, fence: true)
        let second = await cache.prepare("<h1>Report</h1><p>Ready</p>", fence: false)
        XCTAssertEqual(first, second)
        XCTAssertEqual(first?.page.title, "Report")
        let conversions = await cache.conversions
        XCTAssertEqual(conversions, 1)
        let unfinished = await cache.prepare("```html\n<h1>Report", fence: true)
        XCTAssertNil(unfinished)
    }

    private func sketches(_ page: HTMLVisualPage) -> [NativeUIMockup] {
        components(page).compactMap { if case .mockup(let sketch) = $0.content { sketch } else { nil } }
    }

    /// Owner (Oct 22): the two pages a T3 Code thread published today are CSS
    /// device mockups. Each frame becomes a native sketch with its shape, its
    /// areas in proportion, its divider control, any card or cover over it,
    /// its status label and the caption under it; nothing is left out.
    func testTodaysT3PagesBecomeNativeLayoutSketches() throws {
        let split = HTMLVisualPage.convert(try fixture("t3-split-view"))
        XCTAssertEqual(split.omitted, 0)
        XCTAssertFalse(split.truncated)
        let frames = sketches(split)
        XCTAssertEqual(frames.count, 9)
        XCTAssertTrue(frames.allSatisfy { $0.caption != nil && $0.direction == .horizontal })
        XCTAssertEqual(frames.prefix(6).map { ($0.aspect * 100).rounded() }, Array(repeating: 133, count: 6))
        XCTAssertEqual(frames.suffix(3).map { ($0.aspect * 100).rounded() }, Array(repeating: 130, count: 3))

        // Today: a sidebar (22% wide) beside Files, marked as a problem.
        XCTAssertEqual(frames[0].regions.map(\.kind), [.lines, .pane])
        XCTAssertEqual(frames[0].regions[0].weight, 22.0 / 78.0, accuracy: 0.001)
        XCTAssertEqual(frames[0].badge?.tone, .error)
        // A sheet over a dimmed chat, tinted like the page's terminal colour.
        XCTAssertEqual(frames[1].regions.map(\.kind), [.lines, .chat])
        XCTAssertEqual(frames[1].overlay?.style, .sheet)
        XCTAssertEqual(frames[1].overlay?.dim, true)
        XCTAssertEqual(frames[1].overlay?.region.tone, .series2)
        // A full-screen cover with nothing beneath is the screen itself.
        XCTAssertEqual(frames[2].regions.map(\.kind), [.pane])
        XCTAssertNil(frames[2].overlay)
        // Proposed: the chat and a tool split by a swap control, in proportion.
        XCTAssertEqual(frames[3].regions.map(\.kind), [.chat, .divider, .pane])
        XCTAssertEqual(frames[3].regions[0].bubbles, [false, true, false, true])
        XCTAssertEqual(frames[3].regions[1].glyph, "⇄")
        XCTAssertEqual(frames[3].regions[2].weight, 1.25)
        XCTAssertEqual(frames[3].regions[2].tone, .series1)
        XCTAssertEqual(frames[4].regions.map(\.kind), [.pane, .divider, .chat], "swapped sides stay swapped")
        guard case .table(let columns, let rows) = components(split).last?.content else { return XCTFail() }
        XCTAssertEqual(columns.count, 3); XCTAssertEqual(rows.count, 11)

        // Owner (Oct 23): this table read as stacked label/value pairs on a
        // phone. It is one grid with a header row everywhere: too wide for a
        // phone's reply, it scrolls sideways at its columns' narrowest; an
        // iPad fits it. VoiceOver reads each row with its column names.
        let table = try XCTUnwrap(components(split).last?.table)
        XCTAssertEqual(table.header.map { String($0.characters) }, ["Tool", "Today (iPad / unfolded Duo)", "Proposed"])
        XCTAssertEqual(table.spoken[0], "Tool: Computer, Today (iPad / unfolded Duo): Full screen / full screen, "
                       + "Proposed: Split, with ⤢ for full screen; laptop pose unchanged")
        let narrowest = NativeTable.widths(table.columns, em: 15, padding: 8, available: nil).reduce(0, +)
        XCTAssertGreaterThan(narrowest, 330, "scrolls in an iPhone reply")
        XCTAssertLessThan(narrowest, 600, "fits beside an iPad's chat")
        let squeezed = NativeTable.widths(table.columns, em: 15, padding: 8, available: 560)
        XCTAssertEqual(squeezed.reduce(0, +), 560, accuracy: 0.5, "fills the space between narrowest and widest")
        XCTAssertEqual(NativeTable.widths(table.columns, em: 15, padding: 8, available: 5_000),
                       NativeTable.widths(table.columns, em: 15, padding: 8, available: .infinity), "never wider than its text")
        for (col, column) in table.columns.enumerated() {
            let longestWord = ([table.header[col]] + table.rows.map { $0[col] })
                .flatMap { String($0.characters).split(separator: " ") }.map { NativeTable.ems($0) }.max() ?? 0
            XCTAssertGreaterThanOrEqual(column.minimum, min(NativeTable.minimumCap, longestWord), "column \(col) splits a word")
            XCTAssertLessThanOrEqual(column.minimum, column.preferred)
        }
        // Only a cell that can run past three lines at its column's narrowest opens in full.
        XCTAssertTrue(table.long.allSatisfy { !$0[0] && !$0[1] })
        XCTAssertFalse(table.long[2][2], "Split")
        XCTAssertEqual(table.long[7], [false, false, true], "Approvals: keep in the chat… is the longest proposal")
        XCTAssertEqual(split.blocks.filter { if case .heading = $0 { true } else { false } }.count, 3, "the last heading titles the table")

        let portrait = HTMLVisualPage.convert(try fixture("t3-portrait-split"))
        XCTAssertEqual(portrait.omitted, 0)
        let stacked = sketches(portrait)
        XCTAssertEqual(stacked.count, 3)
        XCTAssertTrue(stacked.allSatisfy { $0.direction == .vertical && $0.caption != nil && $0.regions.count == 3 })
        XCTAssertEqual(stacked.map { $0.regions[1].glyph }, ["⇅", "⇅", "⇅"])
        XCTAssertEqual(stacked[0].regions.map(\.kind), [.pane, .divider, .chat])
        XCTAssertEqual(stacked[1].regions.map(\.kind), [.chat, .divider, .pane])
        XCTAssertEqual(stacked[0].regions[0].weight, 1.1)
    }

    /// Colours stay the reader's: page variables become tones the theme
    /// draws, and status-shaped content becomes the matching component.
    func testPageColourVariablesBecomeTonesAndStatusShapesBecomeComponents() {
        let page = HTMLVisualPage.convert("""
        <style>
        .note{border-left:3px solid var(--warning)} .up{color:var(--success)} .n{font-size:24px}
        .chip{border-radius:9px} .chip.ok{background:color-mix(in srgb,var(--success) 20%,transparent)} .chip.bad{color:var(--destructive)}
        @media (prefers-color-scheme: light){.chip.ok{background:var(--destructive)}}
        </style>
        <div class="note">Check the <b>fold</b> first.</div>
        <aside class="tip">Saved.</aside>
        <div class="tiles"><div><div class="n">42</div><div>builds</div><div class="up">+12%</div></div><div><div class="n">3</div><div>failures</div></div></div>
        <div><span class="chip ok">Passing</span><span class="chip bad">Blocked</span></div>
        <h3>Release</h3><ul><li>✓ Tests</li><li>☐ Notes</li></ul>
        <ol class="steps"><li class="done"><b>Build</b> in 4 min</li><li class="current"><b>Upload</b></li><li><b>Review</b></li></ol>
        <table><tr><th>Day</th><th>Codex</th><th>Claude</th></tr><tr><td>Mon</td><td>4</td><td>6</td></tr><tr><td>Tue</td><td>5</td><td>3</td></tr></table>
        <table><tr><th>a</th><th>b</th><th>c</th><th>d</th><th>e</th><th>f</th></tr><tr><td>1</td><td>x</td><td>y</td><td>z</td><td>w</td><td>v</td></tr></table>
        """)
        XCTAssertEqual(Array(page.blocks.prefix(2)), [.callout("Check the **fold** first.", tone: .warning), .callout("Saved.", tone: .success)])
        let all = components(page)
        XCTAssertEqual(all.map(\.type), ["stats", "badges", "checklist", "steps", "chart", "table"])
        guard case .stats(let tiles) = all[0].content else { return XCTFail() }
        XCTAssertEqual(tiles.map(\.value), ["42", "3"]); XCTAssertEqual(tiles[0].delta, "+12%"); XCTAssertEqual(tiles[0].tone, .success)
        guard case .badges(let chips) = all[1].content else { return XCTFail() }
        XCTAssertEqual(chips.map(\.tone), [.success, .error], "rules inside @media are not applied")
        guard case .checklist(let checks) = all[2].content else { return XCTFail() }
        XCTAssertEqual(all[2].title, "Release"); XCTAssertEqual(checks.map(\.done), [true, false])
        guard case .steps(let steps) = all[3].content else { return XCTFail() }
        XCTAssertEqual(steps.map(\.status), [.done, .current, .pending]); XCTAssertEqual(steps[0].detail, "in 4 min")
        guard case .plot(_, let labels, let series) = all[4].content else { return XCTFail() }
        XCTAssertEqual(labels, ["Mon", "Tue"]); XCTAssertEqual(series.map(\.name), ["Codex", "Claude"])
        guard case .table(let columns, _) = all[5].content else { return XCTFail() }
        XCTAssertEqual(columns.count, 6)
    }

    /// Owner (Oct 23): literal colours follow the theme too. Each maps to its
    /// nearest tone by hue, deterministically; greys and tints too faint to
    /// mean anything stay neutral, and a page variable still wins.
    func testLiteralColoursBecomeTheNearestThemeTone() {
        let cases: [(String, NativeUITone?)] = [
            ("#ef4444", .error), ("1px solid #DC2626", .error), ("rgb(220 38 38 / 0.2)", .error), ("crimson", .error),
            ("#f59e0b", .warning), ("#fef3c7", .warning), ("hsl(45, 93%, 47%)", .warning), ("orange", .warning),
            ("#16a34a", .success), ("rgba(34,197,94,.15)", .success), ("#84cc16", .success),
            ("#2563eb", .info), ("#0891b2", .info), ("hsl(210deg 80% 50%)", .info),
            ("#7c3aed", .accent), ("#db2777", .accent),
            ("#64748b", nil), ("#f8fafc", nil), ("#111", nil), ("rgb(0 0 0 / 0)", .none), ("transparent", nil),
            ("white", nil), ("solid 1px", nil), ("#e448", .error),
            ("var(--success, #ef4444)", .success),
        ]
        for (value, tone) in cases {
            XCTAssertEqual(HTMLStyleSheet.tone(in: value), tone, value)
        }
        let page = HTMLVisualPage.convert("""
        <style>.warn{border-left:4px solid #d97706;background:#fffbeb} .chip{border-radius:8px;background:#dcfce7;color:#15803d}</style>
        <div class="warn">Check the hinge.</div><p><span class="chip">Ready</span> Shipping today.</p>
        """)
        XCTAssertEqual(page.blocks, [.callout("Check the hinge.", tone: .warning),
                                     .labeled([NativeUIBadge(text: "Ready", tone: .success)], "Shipping today.")])
    }

    /// A visual given as a wonder-ui document (Cupola's `wonder_show_visual`)
    /// is one native interface block; one the decoder refuses keeps only its
    /// fallback, and HTML is unaffected.
    func testAVisualDocumentBecomesOneInterfaceBlock() async throws {
        let json = #"{"version":1,"fallback":"Riverside: 45 minutes.","components":[{"id":"t","type":"table","title":"Routes","columns":["Route","Time"],"rows":[["Riverside","45 minutes"]]}]}"#
        let prepared = await HTMLVisualCache().prepare(page: Data(json.utf8))
        let page = try XCTUnwrap(prepared?.page)
        XCTAssertEqual(page.blocks, [.interface(try NativeUIDocument.decode(json))])
        XCTAssertFalse(page.isPartial)
        let refused = try XCTUnwrap(HTMLVisualPage.document(#"  {"version":2,"fallback":"Plain *facts*.","components":[]}"#))
        XCTAssertEqual(refused.blocks, [.text(#"Plain \*facts\*."#)])
        XCTAssertTrue(refused.isPartial)
        XCTAssertNil(HTMLVisualPage.document("<p>{not json}</p>"))
    }

    /// The visual-tool scenario's fixtures (the daemon's visual tests check
    /// the same files): the document decodes whole, and the page's literal
    /// colours become toned badges and a warning callout.
    func testVisualToolFixturesReadNatively() throws {
        let json = try String(contentsOf: try XCTUnwrap(Bundle.module.url(forResource: "visual-tool-components", withExtension: "json", subdirectory: "Fixtures")), encoding: .utf8)
        let document = try XCTUnwrap(HTMLVisualPage.document(json))
        guard case .interface(let decoded) = document.blocks.first else { return XCTFail("not one interface block") }
        XCTAssertEqual(decoded.components.map(\.type), ["stats", "chart", "callout", "checklist"])
        let html = try String(contentsOf: try XCTUnwrap(Bundle.module.url(forResource: "visual-tool-page", withExtension: "html", subdirectory: "Fixtures")), encoding: .utf8)
        let page = HTMLVisualPage.convert(html)
        XCTAssertEqual(page.blocks.filter { if case .labeled = $0 { true } else { false } }, [
            .labeled([NativeUIBadge(text: "Recommended", tone: .success)], "Riverside, 45 minutes, mostly shaded."),
            .labeled([NativeUIBadge(text: "Steep", tone: .error)], "Hilltop loop, 70 minutes, 180 m of climb."),
        ])
        XCTAssertTrue(page.blocks.contains(.callout("The hilltop path closes at dusk from November.", tone: .warning)))
        XCTAssertFalse(page.isPartial)
    }

    /// Pictures inside the page show natively only when their bytes are the
    /// format they claim; one after another they share a grid.
    func testEmbeddedPicturesAreCheckedAndGrouped() throws {
        let png = Data([137, 80, 78, 71, 13, 10, 26, 10]) + Data(repeating: 0, count: 24)
        let fake = Data("<svg onload=alert(1)>".utf8)
        let page = HTMLVisualPage.convert("""
        <style>.pair{display:grid;grid-template-columns:1fr 1fr}</style>
        <div class="pair"><figure><img alt="Before" src="data:image/png;base64,\(png.base64EncodedString())"><figcaption>Before the fix</figcaption></figure>
        <img alt="After" src="data:image/png;base64,\(png.base64EncodedString())"></div>
        <p>Between</p>
        <figure><img alt="Still" src="data:image/png;base64,\(png.base64EncodedString())"><p>A plain paragraph captions it.</p></figure>
        <img alt="Next" src="data:image/png;base64,\(png.base64EncodedString())">
        <img alt="Claimed" src="data:image/png;base64,\(fake.base64EncodedString())">
        <img alt="Remote" src="https://example.com/x.png">
        """)
        // Side by side in the page (a two-column grid): one row of pictures.
        guard case .images(let pair) = page.blocks.first else { return XCTFail("\(page.blocks)") }
        XCTAssertEqual(pair.map(\.alt), ["Before", "After"])
        XCTAssertEqual(pair[0].caption, "Before the fix")
        XCTAssertEqual(pair[1].data, png)
        // One under another: each keeps its own full-width block, in order.
        XCTAssertEqual(Array(page.blocks.dropFirst()), [
            .text("Between"),
            .images([.init(data: png, alt: "Still", caption: "A plain paragraph captions it.")]),
            .images([.init(data: png, alt: "Next", caption: nil)]),
        ])
        XCTAssertEqual(page.omitted, 2, "a picture whose bytes aren't an image, and a remote one")
    }

    /// What agents commonly write, element by element (HTML Living Standard
    /// content models), and the native block each becomes.
    func testHTMLElementsMapToNativeBlocks() throws {
        let png = Data([137, 80, 78, 71, 13, 10, 26, 10]) + Data(repeating: 0, count: 24)
        let pill = "<style>.k{display:inline-block;border-radius:5px;padding:2px 7px;background:var(--success-surface);color:var(--success);text-transform:uppercase}</style>"
        let cases: [(html: String, blocks: [HTMLVisualPage.Block])] = [
            ("<h2>Section</h2><h4>Part</h4><p>Body text here.</p>",
             [.heading("Section", level: 2), .heading("Part", level: 3), .text("Body text here.")]),
            ("<ul><li>One<ul><li>Inner</li></ul></li><li>Two</li></ul>", [.text("- One\n  - Inner\n- Two")]),
            ("<ol start=\"3\"><li>Third</li><li>Fourth</li></ol>", [.text("3. Third\n4. Fourth")]),
            ("<ol reversed><li>Second</li><li>First</li></ol>", [.text("2. Second\n1. First")]),
            ("<blockquote>Quoted words.</blockquote>", [.callout("Quoted words.", tone: .info)]),
            ("<pre><code>let x = 1</code></pre>", [.code("let x = 1")]),
            ("<p>Press <kbd>K</kbd> to <strong>open</strong>, <em>then</em> <s>wait</s> <a href=\"https://example.com\">read</a>.</p>",
             [.text("Press `K` to **open**, *then* ~~wait~~ read.")]),
            ("<p>Before the line.</p><hr><p>After the line.</p>", [.text("Before the line."), .rule, .text("After the line.")]),
            ("<details><summary>More detail</summary><p>Hidden</p><ul><li>a</li></ul></details>",
             [.details("More detail", [.text("Hidden"), .text("- a")])]),
            ("<section><article><header><h2>Head</h2></header><main><p>Main text.</p></main><footer><p>Foot text.</p></footer></article></section><nav><a href=\"#x\">Jump</a></nav>",
             [.heading("Head", level: 2), .text("Main text."), .text("Foot text.")]),
            ("<figure><img alt=\"Shot\" src=\"data:image/png;base64,\(png.base64EncodedString())\"><figcaption>The shot.</figcaption></figure>",
             [.images([.init(data: png, alt: "Shot", caption: "The shot.")])]),
            (pill + "<p><span class=\"k\">fix</span>Remove the strobe.</p>",
             [.labeled([NativeUIBadge(text: "FIX", tone: .success)], "Remove the strobe.")]),
            (pill + "<p>Status <span class=\"k\">fix</span> now in place.</p>", [.text("Status **FIX** now in place.")]),
        ]
        for (html, blocks) in cases {
            XCTAssertEqual(HTMLVisualPage.convert(html).blocks, blocks, html)
        }

        // A drawing keeps its own markup (case and all) and its shape.
        let drawn = HTMLVisualPage.convert("<svg viewBox=\"0 0 200 100\" aria-label=\"Lamp\"><linearGradient id=\"g\"/><circle cx=\"50\" cy=\"50\" r=\"40\"/></svg>")
        guard case .drawing(let drawing) = drawn.blocks.first else { return XCTFail("\(drawn.blocks)") }
        XCTAssertEqual(drawing.aspect, 2); XCTAssertEqual(drawing.label, "Lamp")
        XCTAssertTrue(drawing.svg.hasPrefix("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 200 100\""))
        XCTAssertTrue(drawing.svg.contains("<linearGradient") && drawing.svg.hasSuffix("</svg>"))

        // Lists, terms and tables: caption, head, foot, and spans.
        guard case .interface(let terms) = HTMLVisualPage.convert("<dl><dt>Branch</dt><dd>main</dd></dl>").blocks.first,
              case .keyValue = terms.components[0].content else { return XCTFail() }
        let table = HTMLVisualPage.convert("""
        <table><caption>Plan</caption><thead><tr><th>Day</th><th>Where</th><th>Note</th></tr></thead>
        <tbody><tr><td rowspan="2">Sat</td><td colspan="2">Museum, all day</td></tr><tr><td>Market</td><td>Cash</td></tr></tbody>
        <tfoot><tr><td>Sun</td><td>Home</td><td>Rest</td></tr></tfoot></table>
        """)
        guard case .interface(let grid) = table.blocks.first, case .table(let columns, let rows) = grid.components[0].content
        else { return XCTFail("\(table.blocks)") }
        XCTAssertEqual(grid.components[0].title, "Plan")
        XCTAssertEqual(columns, ["Day", "Where", "Note"])
        XCTAssertEqual(rows, [["Sat", "Museum, all day", ""], ["Sat", "Market", "Cash"], ["Sun", "Home", "Rest"]])
    }

    /// Owner (Oct 23): real pages from a T3 thread lost pictures, showed their
    /// coloured "cause/fix" pills as run-on words and dropped their drawing.
    func testCourtyardAuditKeepsEveryPicturePillAndDrawing() throws {
        let page = HTMLVisualPage.convert(try fixture("t3-courtyard-audit"))
        XCTAssertEqual(page.title, "Courtyard audit")
        let pictures = page.blocks.reduce(0) { if case .images(let images) = $1 { $0 + images.count } else { $0 } }
        XCTAssertEqual(pictures, 14)
        XCTAssertNil(page.partialNote, "nothing left out")
        let labels = page.blocks.compactMap { if case .labeled(let badges, _) = $0 { badges } else { nil } }.flatMap { $0 }
        XCTAssertEqual(Set(labels.map(\.text)), ["CAUSE", "FIX", "PROPOSAL", "MEASURED"])
        XCTAssertEqual(Set(labels.filter { $0.text == "FIX" }.map(\.tone)), [.success])
        XCTAssertEqual(Set(labels.filter { $0.text == "CAUSE" }.map(\.tone)), [.error])
        XCTAssertTrue(page.blocks.contains(.labeled([NativeUIBadge(text: "CAUSE", tone: .error)], "Mostly deliberate, and it reads as a bug:")))
        XCTAssertEqual(page.blocks.filter { if case .drawing = $0 { true } else { false } }.count, 1)
        // A pair of figures side by side stays a pair, with their captions.
        XCTAssertTrue(page.blocks.contains { if case .images(let pair) = $0 { pair.map(\.caption) == ["Ours", "Rear Window"] } else { false } })

        let stills = HTMLVisualPage.convert(try fixture("t3-film-stills"))
        XCTAssertEqual(stills.blocks.map { block -> [String?] in if case .images(let images) = block { images.map(\.caption) } else { [] } }, [
            ["Top: before (tall egg heads). Bottom: after (smaller, rounder heads with jaw and chin)."],
            ["In the scene: the sign beat, the shop view, and the watcher at the end."],
        ], "stacked full-width stills stay stacked, each with its caption")
    }

    /// Owner (Oct 23): the Thailand trip page builds its timeline and weekly
    /// rhythm with a script. The app runs such a page's own scripts offscreen
    /// first; whether to is decided here, and T3 Code's theme bootstrap (in
    /// every T3 page) doesn't count as the page's own script.
    func testOnlyAPagesOwnScriptsAreRun() throws {
        let trip = try fixture("t3-thailand-trip")
        XCTAssertTrue(HTMLVisualPage.runsScripts(trip))
        for name in ["t3-courtyard-audit", "t3-film-stills", "t3-split-view"] {
            XCTAssertFalse(HTMLVisualPage.runsScripts(try fixture(name)), "\(name) has only T3's bootstrap")
        }
        XCTAssertFalse(HTMLVisualPage.runsScripts("<script type=\"application/json\">{\"a\":1}</script><p>x</p>"))
        XCTAssertFalse(HTMLVisualPage.runsScripts("<script>  </script>"))
        XCTAssertTrue(HTMLVisualPage.runsScripts("<SCRIPT src=\"chart.js\"></SCRIPT>"))

        // As written, the page says its script-built parts are missing; as its
        // scripts left it (here, a stand-in for what they build) it doesn't.
        XCTAssertEqual(HTMLVisualPage.convert(trip).partialNote, "Some parts of this page can't be shown here.")
        let built = trip.replacingOccurrences(of: "<div class=\"tl\" id=\"tl\"></div>",
                                              with: "<div class=\"tl\" id=\"tl\"><div class=\"visa\">30 days from Dec 14</div></div>")
        let ran = HTMLVisualPage.convert(built, scriptsRan: true)
        XCTAssertNil(ran.partialNote)
        XCTAssertTrue(texts(ran).contains { $0.contains("30 days from Dec 14") })

        // The note under the budget bar starts with "+": it stays text, not a bullet.
        XCTAssertTrue(ran.blocks.contains { block in
            if case .callout(let text, _) = block { text.hasPrefix("\\+ training (\\~$279 BFL month") } else { false }
        }, "\(ran.blocks)")
        XCTAssertEqual(HTMLVisualPage.convert("<p>- not a list</p><p>3. not a number either</p>").blocks,
                       [.text("\\- not a list"), .text("3\\. not a number either")])
    }

    func testCachePreservesHTMLForTheBrowserAndKeepsJSONNative() async {
        let cache = HTMLVisualCache()
        let html = "<script>document.body.textContent='Built'</script><div class=grid>Original</div>"
        let first = await cache.prepare(html, fence: false)
        let again = await cache.prepare(html, fence: false)
        XCTAssertEqual(first, again)
        XCTAssertEqual(first?.html, html)
        XCTAssertEqual(first?.rendersHTML, true)
        let json = await cache.prepare(#"{"version":1,"components":[{"type":"text","text":"Hello"}]}"#, fence: false)
        XCTAssertEqual(json?.rendersHTML, false)
    }

    /// Pictures past the page's budget are counted, not dropped silently.
    func testPicturesBeyondTheBudgetAreCounted() {
        let png = Data([137, 80, 78, 71, 13, 10, 26, 10]) + Data(repeating: 0, count: 24)
        let tag = "<img src=\"data:image/png;base64,\(png.base64EncodedString())\">"
        let page = HTMLVisualPage.convert(String(repeating: tag + "<p>x</p>", count: HTMLVisualPage.Limits.images + 3))
        XCTAssertEqual(page.blocks.filter { if case .images = $0 { true } else { false } }.count, HTMLVisualPage.Limits.images)
        XCTAssertEqual(page.skippedPictures, 3)
        XCTAssertEqual(page.omitted, 0)
        XCTAssertEqual(page.partialNote, "3 more pictures aren't shown here.")
    }
}
