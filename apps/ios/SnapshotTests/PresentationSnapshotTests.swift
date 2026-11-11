import AppKit
import ImageIO
import SwiftUI
import UniformTypeIdentifiers
import XCTest
@testable import WonderPresentation
@testable import WonderNativeReplyUI

/// Independent pixel oracle: tolerate small rasterization noise, never moved geometry.
private struct Pixels {
    let width: Int
    let height: Int
    var rgba: [UInt8]

    init(_ image: CGImage) throws {
        width = image.width; height = image.height
        rgba = .init(repeating: 0, count: width * height * 4)
        let rendered = rgba.withUnsafeMutableBytes { bytes -> Bool in
            guard let context = CGContext(data: bytes.baseAddress, width: width, height: height,
                bitsPerComponent: 8, bytesPerRow: width * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return false }
            context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
            return true
        }
        guard rendered else { throw SnapshotError.bitmap }
    }

    // A pixel differs only if any channel moves by > 8/255. At most 0.1% may differ.
    func difference(from expected: Pixels) -> (fraction: Double, heatmap: [UInt8])? {
        guard width == expected.width, height == expected.height else { return nil }
        var changed = 0
        var heatmap = [UInt8](repeating: 235, count: rgba.count)
        for offset in stride(from: 0, to: rgba.count, by: 4) {
            let differs = abs(Int(rgba[offset]) - Int(expected.rgba[offset])) > 8
                || abs(Int(rgba[offset + 1]) - Int(expected.rgba[offset + 1])) > 8
                || abs(Int(rgba[offset + 2]) - Int(expected.rgba[offset + 2])) > 8
                || abs(Int(rgba[offset + 3]) - Int(expected.rgba[offset + 3])) > 8
            if differs {
                changed += 1
                heatmap[offset] = 255; heatmap[offset + 1] = 0; heatmap[offset + 2] = 80
            }
            heatmap[offset + 3] = 255
        }
        return (Double(changed) / Double(width * height), heatmap)
    }
}

private enum SnapshotError: Error { case bitmap, image }

private struct Variant {
    let name: String
    let scheme: ColorScheme
    let typeSize: DynamicTypeSize
    let fonts: PreparedChatFonts

    // macOS ignores Dynamic Type for semantic fonts. Supply explicit iOS-sized
    // metrics as prepared inputs, while exercising the actual accessibility layout branch.
    static let all: [Variant] = [ColorScheme.light, .dark].flatMap { scheme in
        [false, true].map { large in
            Variant(name: "\(scheme == .dark ? "dark" : "light")-\(large ? "accessibility3" : "large")",
                scheme: scheme, typeSize: large ? .accessibility3 : .large,
                fonts: PreparedChatFonts(body: .system(size: large ? 40 : 17),
                    caption: .system(size: large ? 29 : 12), subheadline: .system(size: large ? 38 : 15),
                    permissionHeading: .system(size: large ? 38 : 15, weight: .semibold),
                    emptyHeading: .system(size: large ? 48 : 22, weight: .bold)))
        }
    }
}

/// Synthetic, prepared content; no ConnectionModel, global preferences, tasks or network.
@MainActor private struct Fixtures {
    let fonts: PreparedChatFonts
    let dark: Bool
    var surface: Color { dark ? Color(white: 0.14) : Color(white: 0.94) }
    var userSurface: Color { dark ? Color(white: 0.28) : Color(white: 0.82) }

    var chats: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Chats").font(fonts.emptyHeading)
            Text("Studio · My Mac").font(fonts.caption).foregroundStyle(.secondary)
            PreparedChatRow(title: "Review the onboarding flow", project: "Wonder", location: "main",
                            locationTint: .blue, isComputerSession: false, leading: 4) {
            } provider: {
                Image(systemName: "sparkle").font(.caption).accessibilityLabel("Claude")
            } status: { Circle().fill(Color.primary).frame(width: 9, height: 9).padding(.leading, 8) }
            PreparedChatRow(title: "A longer chat title that must leave room for its status",
                            project: "Website", location: "agent/onboarding-copy",
                            locationTint: .blue, activity: "Reading files", isComputerSession: true, leading: 4) {
            } provider: {
                Image(systemName: "curlybraces").font(.caption).accessibilityLabel("Codex")
            } status: { Circle().fill(Color.blue).frame(width: 9, height: 9).padding(.leading, 8) }
            // Too long to follow the branch: left out at standard sizes, own line at accessibility sizes.
            PreparedChatRow(title: "Fix the release build", project: "Website", location: "agent/release-signing",
                            locationTint: .blue, activity: "Waiting for a permission on your Mac",
                            isComputerSession: false, leading: 4) {
            } provider: {
                Image(systemName: "curlybraces").font(.caption).accessibilityLabel("Codex")
            } status: { Circle().fill(Color.blue).frame(width: 9, height: 9).padding(.leading, 8) }
            PreparedChatRow(title: "Outside Git", isComputerSession: false, leading: 4) {
            } provider: {
                Image(systemName: "sparkle").font(.caption).accessibilityLabel("Claude")
            } status: { EmptyView() }
            Spacer(minLength: 0)
        }.padding(16)
    }

    var messages: some View {
        VStack(spacing: 12) {
            bubble("Please check the spacing.", isUser: true)
            bubble("The preview is ready for your review.", isUser: false)
            bubble("Keep the composer visible.", isUser: true)
            // Long enough to wrap: the bubbles' widths are part of the contract.
            bubble("A longer request wraps across most of the row.", isUser: true)
            bubble("A longer reply also wraps, using nearly the whole row.", isUser: false)
        }
    }

    private func bubble(_ text: String, isUser: Bool) -> some View {
        PreparedMessageLayout(isUser: isUser) {
            Text(text).font(fonts.body)
                .modifier(PreparedBubbleSurface(foreground: .primary, background: isUser ? userSurface : surface))
                .accessibilityLabel((isUser ? "You: " : "Claude: ") + text)
        } receipt: { EmptyView() }
    }

    var timeline: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Wonder · Claude").font(fonts.subheadline).fontWeight(.semibold)
            messages
            Spacer(minLength: 0)
        }.padding(16)
    }

    var composer: some View {
        PreparedConversationLayout {
            Text("Conversation").font(fonts.subheadline).foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading).padding(16)
        } bottom: {
            PreparedComposerSurface(background: surface) {
                VStack(alignment: .leading, spacing: 0) {
                    // UIKit editor and control actions remain integration-owned;
                    // this fixture protects the shipped container and bottom placement.
                    Text("A draft ready to send").font(fonts.body)
                        .frame(maxWidth: .infinity, alignment: .leading).padding(8)
                    PreparedComposerControls {
                        Image(systemName: "plus").font(.system(size: 22)).frame(width: 44, height: 44)
                        Image(systemName: "mic").font(.system(size: 22)).frame(width: 44, height: 44)
                        Spacer(minLength: 0)
                        PreparedSendSymbol(preparing: false, progressTint: dark ? .black : .white)
                            .foregroundStyle(dark ? Color.black : .white)
                            .background(Color.primary, in: Circle())
                    }
                }
            }.modifier(PreparedComposerDock())
        }
    }

    var approval: some View {
        VStack(alignment: .leading, spacing: 12) {
            PreparedPermissionDetails(reason: "Allow the agent to run the local checks?",
                command: "swift test", path: "/Projects/Website",
                permissionDetails: ["This runs a command on your Mac."])
            // As AttentionRow lays them out: one-tap choices without radios, then
            // Stop apart as a red button with its explanation under it.
            VStack(alignment: .leading, spacing: 8) {
                PreparedChoiceLabel(title: "Allow once", detail: "Only for this request",
                                    accent: .blue, background: surface)
                PreparedChoiceLabel(title: "Decline", accent: .blue, background: surface)
            }
            VStack(alignment: .leading, spacing: 4) {
                PreparedStopLabel(title: "Stop this turn")
                Text("Decline the request and stop the current response.").font(fonts.caption).foregroundStyle(.secondary)
            }.padding(.top, 8)
            Spacer(minLength: 0)
        }.padding(.vertical, 8).padding(.horizontal, 16)
    }

    var question: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Which day works best?").font(fonts.permissionHeading)
            PreparedChoiceLabel(title: "Saturday", detail: "Keep Sunday free.", selected: false,
                                accent: .blue, background: surface)
            PreparedChoiceLabel(title: "Sunday", selected: true, accent: .blue, background: surface)
            Spacer(minLength: 0)
        }.padding(.vertical, 8).padding(.horizontal, 16)
    }
}

final class PresentationSnapshotTests: XCTestCase {
    // Each case protects a shipped catalog component in light/dark and large text.
    @MainActor func testNativeCardKeepsTitleAndBodyReadable() throws { try nativeSnapshot("card", height: 700) }
    @MainActor func testNativeTableKeepsColumnMeaningAtLargeText() throws { try nativeSnapshot("table", height: 1100) }
    @MainActor func testNativeListWrapsWithoutLosingMarkers() throws { try nativeSnapshot("list", height: 900) }
    @MainActor func testNativeKeyValueKeepsLabelsBesideOrAboveValues() throws { try nativeSnapshot("keyValue", height: 1000) }
    @MainActor func testNativeChartKeepsEveryLabelAndValue() throws { try nativeSnapshot("chart", height: 1100) }
    @MainActor func testNativeProgressKeepsItsExplanation() throws { try nativeSnapshot("progress", height: 700) }
    @MainActor func testNativeButtonHasAReadableReviewAction() throws { try nativeSnapshot("button", height: 500) }
    @MainActor func testNativeFormKeepsLabelsFieldsAndSubmitReachable() throws { try nativeSnapshot("form", height: 1800) }
    @MainActor func testNativeDiagramKeepsItsOrderedFlow() throws { try nativeSnapshot("diagram", height: 1700) }
    @MainActor func testNativeCalloutShowsItsToneBesideTheText() throws { try nativeSnapshot("callout", height: 700) }
    @MainActor func testNativeStatsKeepFigureLabelAndChange() throws { try nativeSnapshot("stats", height: 1100) }
    @MainActor func testNativeStepsKeepOrderAndStatus() throws { try nativeSnapshot("steps", height: 1100) }
    @MainActor func testNativeChecklistShowsDoneAndOpenItems() throws { try nativeSnapshot("checklist", height: 800) }
    @MainActor func testNativeBadgesWrapWithTheirTones() throws { try nativeSnapshot("badges", height: 600) }
    @MainActor func testNativeMockupKeepsItsShapeAndProportions() throws { try nativeSnapshot("mockup", height: 900) }
    @MainActor func testNativeSeriesChartKeepsLegendAndAxes() throws { try nativeSnapshot("trend", height: 800) }

    @MainActor private func nativeSnapshot(_ kind: String, height: CGFloat) throws {
        let document = try NativeUITestData.document(kind)
        try snapshots("genui-" + kind, height: height) { fixture in
            VStack(alignment: .leading) {
                NativeUIDocumentView(document: document, onDraft: { _ in nil }, theme: NativeUITheme(surface: fixture.surface),
                    fonts: NativeUIFonts(body: fixture.fonts.body, heading: fixture.fonts.permissionHeading,
                                         caption: fixture.fonts.caption), controls: NativeUIControls(input: { field, value, fonts, _ in
                        // AppKit controls cannot enter ImageRenderer.
                        // Only the unsupported AppKit control is replaced. Production
                        // owns the shared field labels and responsive row/stack layout;
                        // wonderctl checks native hit targets and submission.
                        AnyView(HStack {
                            Text(field.kind == .text ? "Your answer" : field.kind == .toggle ? "Off" : value.wrappedValue)
                                .font(fonts.body).foregroundStyle(.secondary)
                            Spacer(minLength: 0)
                            if field.kind == .select { Image(systemName: "chevron.down") }
                            if field.kind == .toggle { Image(systemName: "switch.2") }
                        }.padding(8).background(fixture.surface, in: RoundedRectangle(cornerRadius: 6))
                            .fixedSize(horizontal: field.kind != .text, vertical: false))
                    }))
                Spacer(minLength: 0)
            }.padding(16)
        }
    }

    // Contract: provider slot, project preview and status remain legible; long titles wrap at AX sizes.
    @MainActor func testChatsPreserveIdentityPreviewAndStatusGap() throws {
        try snapshots("chats", height: 960) { $0.chats }
    }

    // Contract: sent messages align right; agent replies align left with distinct bubble surfaces;
    // a long message of either kind uses most of the row's width.
    @MainActor func testTimelineKeepsUserRightAgentLeftAndSpeakerSurfaces() throws {
        try snapshots("timeline", height: 920) { $0.timeline }
    }

    // Contract: the composer remains at the bottom, within margins, without clipping its draft or controls.
    @MainActor func testComposerStaysAtBottomWithReadableDraftAndControls() throws {
        try snapshots("composer", height: 600) { $0.composer }
    }

    // Contract: permission wording, command context and distinct allow/decline choices remain readable.
    @MainActor func testApprovalUsesUserLanguageAndReadableChoices() throws {
        try snapshots("approval", height: 1000) { $0.approval }
    }

    // Contract: a question's options and their radios read at full contrast, never as disabled.
    @MainActor func testQuestionOptionsReadAsAvailable() throws {
        try snapshots("question", height: 700) { $0.question }
    }

    @MainActor private func snapshots<Content: View>(_ name: String, height: CGFloat,
        @ViewBuilder content: (Fixtures) -> Content, file: StaticString = #filePath, line: UInt = #line) throws {
        let env = ProcessInfo.processInfo.environment
        let references = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("References")
        let repository = references.deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        let evidence = env["WONDER_SNAPSHOT_OUTPUT"].map { URL(fileURLWithPath: $0) }
            ?? repository.appendingPathComponent(".local/evidence/swiftui-snapshots/direct-\(ProcessInfo.processInfo.processIdentifier)")
        try FileManager.default.createDirectory(at: evidence, withIntermediateDirectories: true)
        for variant in Variant.all {
            let key = "\(name)-\(variant.name)"
            let page = variant.scheme == .dark ? Color(white: 0.035) : Color.white
            let view = content(Fixtures(fonts: variant.fonts, dark: variant.scheme == .dark))
                .font(variant.fonts.body)
                .frame(width: 390, height: height).background(page)
                .environment(\.colorScheme, variant.scheme)
                .environment(\.dynamicTypeSize, variant.typeSize)
                .environment(\.preparedChatFonts, variant.fonts)
                .environment(\.locale, Locale(identifier: "en_US_POSIX"))
                .environment(\.calendar, Calendar(identifier: .gregorian))
                .environment(\.timeZone, TimeZone(secondsFromGMT: 0)!)
                .transaction { $0.animation = nil }
            let renderer = ImageRenderer(content: view)
            renderer.scale = 1
            let actual = try XCTUnwrap(renderer.cgImage, "ImageRenderer must produce pixels", file: file, line: line)
            let actualURL = evidence.appendingPathComponent(key + "-actual.png")
            try write(actual, to: actualURL)
            let referenceURL = references.appendingPathComponent(key + ".png")
            if env["WONDER_SNAPSHOT_RECORD"] == "1" {
                try write(actual, to: referenceURL)
                print("SNAPSHOT recorded \(key) \(actual.width)x\(actual.height)")
                continue
            }
            guard let source = CGImageSourceCreateWithURL(referenceURL as CFURL, nil),
                  let expected = CGImageSourceCreateImageAtIndex(source, 0, nil) else {
                XCTFail("Missing reference \(key); review and run apps/ios/snapshot-tests --record", file: file, line: line)
                continue
            }
            try write(expected, to: evidence.appendingPathComponent(key + "-reference.png"))
            let pixels = try Pixels(actual)
            guard let difference = pixels.difference(from: try Pixels(expected)) else {
                XCTFail("\(key): dimensions changed (\(actual.width)x\(actual.height) vs \(expected.width)x\(expected.height))", file: file, line: line)
                continue
            }
            if difference.fraction > 0.001 {
                let data = Data(difference.heatmap)
                let provider = try XCTUnwrap(CGDataProvider(data: data as CFData))
                let heatmap = try XCTUnwrap(CGImage(width: pixels.width, height: pixels.height,
                    bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: pixels.width * 4,
                    space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: .init(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                    provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent))
                try write(heatmap, to: evidence.appendingPathComponent(key + "-diff.png"))
            }
            print("SNAPSHOT compared \(key) changed=\(difference.fraction)")
            XCTAssertLessThanOrEqual(difference.fraction, 0.001,
                "\(key): >0.1% pixels differ by >8/255; inspect actual/reference/diff in \(evidence.path)", file: file, line: line)
        }
    }

    private func write(_ image: CGImage, to url: URL) throws {
        guard let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)
        else { throw SnapshotError.image }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination) else { throw SnapshotError.image }
    }

    func testPixelOracleToleratesRasterNoiseButRejectsMovedContent() throws {
        let provider = CGDataProvider(data: Data(repeating: 100, count: 100 * 100 * 4) as CFData)!
        let image = CGImage(width: 100, height: 100, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: 400,
            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: .init(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
            provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)!
        let reference = try Pixels(image)
        var noise = reference
        noise.rgba = noise.rgba.map { $0 + 8 }
        XCTAssertEqual(noise.difference(from: reference)?.fraction, 0)
        var moved = reference
        for offset in stride(from: 0, to: 400, by: 4) { moved.rgba[offset] = 255 }
        XCTAssertGreaterThan(try XCTUnwrap(moved.difference(from: reference)?.fraction), 0.001)
    }
}

private enum NativeUITestData {
    static let components: [String: String] = [
        "card": #"{"id":"card","type":"card","title":"Weekend plan","body":"Take the riverside walk, then stop for lunch. Leave Sunday open."}"#,
        "table": #"{"id":"table","type":"table","title":"Options","columns":["Route","Time","Notes"],"rows":[["Riverside","45 minutes","Flat and shaded"],["Hilltop loop","70 minutes","A steep start, then wide views over the bay, the old harbour and the islands all the way down to the station"]]}"#,
        "list": #"{"id":"list","type":"list","title":"Bring along","items":["Water and a small snack","A light jacket for the walk home"]}"#,
        "keyValue": #"{"id":"keyValue","type":"keyValue","title":"Details","entries":[{"label":"Meet at","value":"Riverside station"},{"label":"When","value":"Saturday, 10:00"}]}"#,
        "chart": #"{"id":"chart","type":"chart","title":"Weekly distance (km)","bars":[{"label":"Monday","value":3},{"label":"Wednesday","value":5},{"label":"Saturday","value":8}]}"#,
        "progress": #"{"id":"progress","type":"progress","title":"Preparation","value":0.6,"label":"3 of 5 items ready"}"#,
        "button": #"{"id":"button","type":"button","title":"Draft a shorter route","message":"Suggest a shorter riverside route."}"#,
        "form": #"{"id":"form","type":"form","title":"Adjust the plan","fields":[{"id":"notes","kind":"text","label":"Notes","required":true},{"id":"pace","kind":"select","label":"Pace","options":["Relaxed","Brisk"]},{"id":"lunch","kind":"toggle","label":"Include lunch"}],"submit":"Draft my preferences","message":"Update the walking plan with these preferences."}"#,
        "diagram": #"{"id":"diagram","type":"diagram","title":"How the day flows","steps":["Meet at the station","Walk beside the river","Lunch near the park"]}"#,
        "callout": #"{"id":"callout","type":"callout","title":"Rain later","body":"Showers are likely after 15:00, so start early.","tone":"warning"}"#,
        "stats": #"{"id":"stats","type":"stats","title":"This week","items":[{"label":"Walks","value":"4","delta":"+1","tone":"success"},{"label":"Distance","value":"16 km"},{"label":"Rest days","value":"2"}]}"#,
        "steps": #"{"id":"steps","type":"steps","title":"Saturday","steps":[{"title":"Meet at the station","status":"done"},{"title":"Riverside walk","detail":"About 45 minutes","status":"current"},{"title":"Lunch near the park","status":"pending"}]}"#,
        "checklist": #"{"id":"checklist","type":"checklist","title":"Packed","items":[{"text":"Water","done":true},{"text":"Jacket","done":true},{"text":"Snacks","done":false}]}"#,
        "badges": #"{"id":"badges","type":"badges","title":"Trail status","items":[{"text":"Open","tone":"success"},{"text":"Muddy","tone":"warning"},{"text":"Dogs on lead"}]}"#,
        "mockup": #"{"id":"mockup","type":"mockup","title":"Map beside the plan","aspect":1.333,"direction":"horizontal","regions":[{"kind":"chat","bubbles":["agent","user","agent"]},{"kind":"divider","glyph":"⇄"},{"kind":"pane","title":"Map","label":"Riverside route","tone":"series2","weight":1.25}],"caption":"The plan on the left, the map on the right."}"#,
    ]
    /// The series form of `chart` (Swift Charts), beside the bar form above.
    static let trend = #"{"id":"trend","type":"chart","title":"Steps per day (thousands)","kind":"line","labels":["Mon","Wed","Sat"],"series":[{"name":"This week","values":[6,8,11]},{"name":"Last week","values":[5,7,9]}]}"#
    static func source(_ kind: String) -> String {
        #"{"version":1,"fallback":"A weekend walking plan with options and preferences.","components":["# + (kind == "trend" ? trend : components[kind]!) + "]}"
    }
    static func document(_ kind: String) throws -> NativeUIDocument { try .decode(source(kind)) }
    static func fence(_ source: String) -> String { "```wonder-ui\n" + source + "\n```" }
}

/// Observable contracts: untrusted replies render only the documented catalog,
/// preserve non-UI text, and can propose only an explicit normal-message draft.
/// This host package owns the exact parser and renderer compiled by the app.
final class NativeReplyUITests: XCTestCase {
    func testPublishedSchemaExampleDecodesTheWholeCatalog() throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        let schema = try JSONSerialization.jsonObject(with: Data(contentsOf: root.appendingPathComponent("packages/protocol/schemas/wonder-ui-v1.json"))) as! [String: Any]
        let examples = try XCTUnwrap(schema["examples"] as? [[String: Any]])
        for example in examples {
            let document = try JSONDecoder().decode(NativeUIDocument.self, from: JSONSerialization.data(withJSONObject: example))
            XCTAssertEqual(Set(document.components.map(\.type)), Set(NativeUITestData.components.keys))
        }
    }

    func testEveryCatalogComponentDecodesAndKeepsItsContent() throws {
        for kind in NativeUITestData.components.keys {
            let document = try NativeUITestData.document(kind)
            XCTAssertEqual(document.components.count, 1)
            XCTAssertEqual(document.components[0].type, kind)
            XCTAssertFalse(document.fallback.isEmpty)
        }
        guard case .card(let body) = try NativeUITestData.document("card").components[0].content else { return XCTFail() }
        XCTAssertTrue(body.contains("Sunday"))
        guard case .table(let columns, let rows) = try NativeUITestData.document("table").components[0].content else { return XCTFail() }
        XCTAssertEqual(columns, ["Route", "Time", "Notes"]); XCTAssertEqual(Array(rows[1].prefix(2)), ["Hilltop loop", "70 minutes"])
        guard case .chart(let bars, let maximum) = try NativeUITestData.document("chart").components[0].content else { return XCTFail() }
        XCTAssertEqual(maximum, 8); XCTAssertEqual(bars[1].display, "5")
        guard case .list(let items) = try NativeUITestData.document("list").components[0].content else { return XCTFail() }
        XCTAssertEqual(items, ["Water and a small snack", "A light jacket for the walk home"])
        guard case .keyValue(let entries) = try NativeUITestData.document("keyValue").components[0].content else { return XCTFail() }
        XCTAssertEqual(entries[0].label, "Meet at"); XCTAssertEqual(entries[0].value, "Riverside station")
        guard case .progress(let value, let label, let display) = try NativeUITestData.document("progress").components[0].content else { return XCTFail() }
        XCTAssertEqual(value, 0.6); XCTAssertEqual(label, "3 of 5 items ready"); XCTAssertEqual(display, "60%")
        guard case .diagram(let steps) = try NativeUITestData.document("diagram").components[0].content else { return XCTFail() }
        XCTAssertEqual(steps, ["Meet at the station", "Walk beside the river", "Lunch near the park"])
    }

    /// The additions agents can emit directly (and the HTML converter emits):
    /// they decode with their meaning intact and fail closed when malformed.
    func testToneStatsStepsChecklistBadgesMockupAndSeriesCharts() throws {
        guard case .callout(_, let tone) = try NativeUITestData.document("callout").components[0].content else { return XCTFail() }
        XCTAssertEqual(tone, .warning)
        guard case .stats(let stats) = try NativeUITestData.document("stats").components[0].content else { return XCTFail() }
        XCTAssertEqual(stats.map(\.delta), ["+1", nil, nil]); XCTAssertEqual(stats.map(\.tone), [.success, .neutral, .neutral])
        guard case .steps(let steps) = try NativeUITestData.document("steps").components[0].content else { return XCTFail() }
        XCTAssertEqual(steps.map(\.status), [.done, .current, .pending])
        guard case .checklist(let checks) = try NativeUITestData.document("checklist").components[0].content else { return XCTFail() }
        XCTAssertEqual(checks.map(\.done), [true, true, false])
        guard case .mockup(let sketch) = try NativeUITestData.document("mockup").components[0].content else { return XCTFail() }
        XCTAssertEqual(sketch.regions.map(\.kind), [.chat, .divider, .pane])
        XCTAssertEqual(sketch.regions[0].bubbles, [false, true, false]); XCTAssertTrue(sketch.regions[0].composer)
        XCTAssertEqual(sketch.regions[2].weight, 1.25); XCTAssertEqual(sketch.regions[1].glyph, "⇄")
        guard case .plot(let kind, let labels, let series) = try NativeUITestData.document("trend").components[0].content else { return XCTFail() }
        XCTAssertEqual(kind, .line); XCTAssertEqual(labels.count, 3); XCTAssertEqual(series.map(\.values.count), [3, 3])
        XCTAssertEqual(try NativeUITestData.document("trend").displayItemCount, 6)

        let invalid = [
            NativeUITestData.trend.replacingOccurrences(of: "[5,7,9]", with: "[5,7]"),
            NativeUITestData.trend.replacingOccurrences(of: #""Last week""#, with: #""This week""#),
            NativeUITestData.components["callout"]!.replacingOccurrences(of: #""warning""#, with: #""red""#),
            NativeUITestData.components["mockup"]!.replacingOccurrences(of: #""title":"Map","label":"Riverside route","#, with: ""),
            NativeUITestData.components["mockup"]!.replacingOccurrences(of: #""aspect":1.333"#, with: #""aspect":9"#),
            NativeUITestData.components["mockup"]!.replacingOccurrences(of: #""user""#, with: #""system""#),
            NativeUITestData.components["steps"]!.replacingOccurrences(of: #""pending""#, with: #""later""#),
            NativeUITestData.components["stats"]!.replacingOccurrences(of: #""+1""#, with: #""+1","color":"red""#),
        ]
        for component in invalid {
            XCTAssertThrowsError(try NativeUIDocument.decode(#"{"version":1,"fallback":"x","components":["# + component + "]}"), component)
        }
        let wide = #"{"id":"w","type":"table","title":"Wide","columns":["a","b","c","d","e","f","g","h"],"rows":[["1","2","3","4","5","6","7","8"]]}"#
        XCTAssertNoThrow(try NativeUIDocument.decode(#"{"version":1,"fallback":"x","components":["# + wide + "]}"))
        XCTAssertThrowsError(try NativeUIDocument.decode(#"{"version":1,"fallback":"x","components":["#
            + wide.replacingOccurrences(of: #""h"]"#, with: #""h","i"]"#).replacingOccurrences(of: #""8"]"#, with: #""8","9"]"#) + "]}"))
    }

    func testMixedMarkdownAndMultipleDocumentsKeepTheirOrder() throws {
        let source = "Before\n" + NativeUITestData.fence(NativeUITestData.source("card")) + "\nBetween\n" + NativeUITestData.fence(NativeUITestData.source("progress")) + "\nAfter"
        let pieces = NativeReplyProjection.prepare(source).pieces
        XCTAssertEqual(pieces, [.markdown("Before"), .interface(try NativeUITestData.document("card")),
                                .markdown("Between"), .interface(try NativeUITestData.document("progress")), .markdown("After")])
    }

    func testOrdinaryFencesQuotesAndIndentedExamplesStayText() {
        let native = NativeUITestData.fence(NativeUITestData.source("card"))
        for source in ["````markdown\n" + native + "\n````", "~~~markdown\n" + native + "\n~~~",
                       native.components(separatedBy: "\n").map { "\t" + $0 }.joined(separator: "\n"),
                       native.components(separatedBy: "\n").map { "    " + $0 }.joined(separator: "\n"),
                       native.components(separatedBy: "\n").map { "> " + $0 }.joined(separator: "\n")] {
            XCTAssertEqual(NativeReplyProjection.prepare(source).pieces, [.markdown(source)])
        }
    }

    func testWindowsLineEndingsActivateCompletedFences() throws {
        let source = NativeUITestData.fence(NativeUITestData.source("button")).replacingOccurrences(of: "\n", with: "\r\n")
        XCTAssertEqual(NativeReplyProjection.prepare(source).pieces, [.interface(try NativeUITestData.document("button"))])
    }

    func testStreamingFenceDoesNotActivateUntilItCloses() throws {
        let source = "```wonder-ui\n" + NativeUITestData.source("button")
        XCTAssertEqual(NativeReplyProjection.prepare(source).pieces, [.markdown(source)])
        XCTAssertEqual(NativeReplyProjection.prepare(source + "\n```").pieces, [.interface(try NativeUITestData.document("button"))])
    }

    func testMalformedUnknownAndUnsupportedPayloadsDegradeToReadableText() {
        for source in ["{ broken JSON", NativeUITestData.source("card").replacingOccurrences(of: "\"version\":1", with: "\"version\":2"),
                       NativeUITestData.source("card").replacingOccurrences(of: "\"type\":\"card\"", with: "\"type\":\"webView\"")] {
            guard case .unavailable(let summary, let raw) = NativeReplyProjection.prepare(NativeUITestData.fence(source)).pieces.first else { return XCTFail() }
            XCTAssertFalse(summary.isEmpty); XCTAssertEqual(raw, source)
        }
        let unknown = NativeUITestData.source("card").replacingOccurrences(of: "\"type\":\"card\"", with: "\"type\":\"webView\"")
        guard case .unavailable(let fallback, _) = NativeReplyProjection.prepare(NativeUITestData.fence(unknown)).pieces[0] else { return XCTFail() }
        XCTAssertEqual(fallback, "A weekend walking plan with options and preferences.")
    }

    func testExecutableAndUnknownPropertiesAreRejected() {
        for property in [#""onClick":"delete_files""#, #""html":"<script>alert(1)</script>""#, #""url":"file:///etc/passwd""#] {
            let source = NativeUITestData.source("button").replacingOccurrences(of: #""message":"Suggest"#, with: property + #", "message":"Suggest"#)
            XCTAssertThrowsError(try NativeUIDocument.decode(source))
        }
        let source = NativeUITestData.source("card").replacingOccurrences(of: #""version":1"#, with: #""version":1,"actions":{"run":"shell"}"#)
        XCTAssertThrowsError(try NativeUIDocument.decode(source))
    }

    func testOversizedAndDeepPayloadsAreBoundedBeforeDecoding() {
        XCTAssertThrowsError(try NativeUIDocument.decode(String(repeating: " ", count: 32_769)))
        XCTAssertThrowsError(try NativeUIDocument.decode(String(repeating: "[", count: 17) + "0" + String(repeating: "]", count: 17)))
        let source = NativeUITestData.fence(String(repeating: "x", count: 40_000))
        guard case .unavailable(_, let raw) = NativeReplyProjection.prepare(source).pieces[0] else { return XCTFail() }
        XCTAssertEqual(raw.count, 4_000)
        let hugeMessage = source + String(repeating: "x", count: 131_073)
        guard case .unavailable(let explanation, let hugeRaw) = NativeReplyProjection.prepare(hugeMessage).pieces[0] else { return XCTFail() }
        XCTAssertTrue(explanation.contains("too large")); XCTAssertEqual(hugeRaw.count, 4_000)
    }

    func testFifthInterfaceFallsBackWithoutActivatingControls() {
        let source = Array(repeating: NativeUITestData.fence(NativeUITestData.source("button")), count: 5).joined(separator: "\n")
        let interfaces = NativeReplyProjection.prepare(source).pieces.filter { if case .interface = $0 { true } else { false } }
        XCTAssertEqual(interfaces.count, 4)
        guard case .unavailable = NativeReplyProjection.prepare(source).pieces.last else { return XCTFail() }
    }

    func testManySmallFencesCannotCreateAnUnboundedFallbackStack() {
        let source = Array(repeating: NativeUITestData.fence(NativeUITestData.source("button")), count: 100).joined(separator: "\n")
        XCTAssertLessThan(source.utf8.count, 131_072)
        let pieces = NativeReplyProjection.prepare(source).pieces
        XCTAssertLessThanOrEqual(pieces.count, 5)
        guard case .unavailable = pieces.last else { return XCTFail() }
    }

    func testDisplayBudgetBoundsIndividuallyValidTablesAndMultipleDocuments() throws {
        func table(_ id: String) -> String {
            let row = #"["a","b","c","d"]"#
            return #"{"id":"\#(id)","type":"table","title":"Options","columns":["A","B","C","D"],"rows":["# + Array(repeating: row, count: 20).joined(separator: ",") + "]}"
        }
        func document(_ components: [String]) -> String {
            #"{"version":1,"fallback":"Options are available as text.","components":["# + components.joined(separator: ",") + "]}"
        }
        let full = document([table("first")])
        XCTAssertNoThrow(try NativeUIDocument.decode(full))
        XCTAssertThrowsError(try NativeUIDocument.decode(document([table("first"), table("second")])))
        let pieces = NativeReplyProjection.prepare(NativeUITestData.fence(full) + "\n" + NativeUITestData.fence(full)).pieces
        XCTAssertEqual(pieces.filter { if case .interface = $0 { true } else { false } }.count, 1)
        guard case .unavailable = pieces.last else { return XCTFail() }
    }

    func testDuplicateIDsInvalidShapesAndNonfiniteRangesAreRejected() {
        let card = NativeUITestData.components["card"]!
        XCTAssertThrowsError(try NativeUIDocument.decode(#"{"version":1,"fallback":"Text","components":["# + card + "," + card + "]}"))
        for source in [NativeUITestData.source("table").replacingOccurrences(of: #"["Riverside","45 minutes","Flat and shaded"]"#, with: #"["Riverside","45 minutes"]"#),
                       NativeUITestData.source("progress").replacingOccurrences(of: "0.6", with: "1.1"),
                       NativeUITestData.source("progress").replacingOccurrences(of: "0.6", with: "-1"),
                       NativeUITestData.source("chart").replacingOccurrences(of: #""value":3"#, with: #""value":1e999"#),
                       NativeUITestData.source("card").replacingOccurrences(of: #""id":"card""#, with: #""id":"bad/id""#)] {
            XCTAssertThrowsError(try NativeUIDocument.decode(source))
        }
    }

    func testCollectionsAndStringsRespectCatalogLimits() {
        let base = NativeUITestData.source("list")
        for replacement in ["[]", "[" + Array(repeating: "\"item\"", count: 21).joined(separator: ",") + "]",
                            "[\"" + String(repeating: "a", count: 257) + "\"]"] {
            let source = base.replacingOccurrences(of: #"["Water and a small snack","A light jacket for the walk home"]"#, with: replacement)
            XCTAssertThrowsError(try NativeUIDocument.decode(source))
        }
    }

    func testStringLimitsMatchSchemaUnicodeCodePoints() {
        // A single displayed grapheme can contain several JSON Schema characters.
        let title = String(repeating: "a\u{0301}", count: 100)
        let source = NativeUITestData.source("card").replacingOccurrences(of: "Weekend plan", with: title)
        XCTAssertThrowsError(try NativeUIDocument.decode(source))
    }

    func testButtonOnlyProducesTheDocumentedNormalMessage() throws {
        let component = try NativeUITestData.document("button").components[0]
        XCTAssertEqual(component.draft(), "Suggest a shorter riverside route.")
        XCTAssertNil(try NativeUITestData.document("card").components[0].draft())
    }

    func testFormRequiresValidValuesAndProducesAnOrderedReviewableMessage() throws {
        let component = try NativeUITestData.document("form").components[0]
        XCTAssertNil(component.draft())
        XCTAssertNil(component.draft(values: ["notes": "  "]))
        XCTAssertNil(component.draft(values: ["notes": String(repeating: "x", count: 513)]))
        XCTAssertNil(component.draft(values: ["notes": "Easy walk", "pace": "Run shell"]))
        XCTAssertNil(component.draft(values: ["notes": "Easy walk", "lunch": "yes"]))
        XCTAssertEqual(component.draft(values: ["notes": "Easy walk", "lunch": "true"]),
                       "Update the walking plan with these preferences.\n\nNotes: Easy walk\nPace: Relaxed\nInclude lunch: Yes")
    }

    func testFieldCatalogRejectsDuplicatesAndUnknownKinds() {
        for source in [NativeUITestData.source("form").replacingOccurrences(of: #""kind":"text""#, with: #""kind":"password""#),
                       NativeUITestData.source("form").replacingOccurrences(of: #""id":"pace""#, with: #""id":"notes""#),
                       NativeUITestData.source("form").replacingOccurrences(of: #"["Relaxed","Brisk"]"#, with: #"["Relaxed","Relaxed"]"#)] {
            XCTAssertThrowsError(try NativeUIDocument.decode(source))
        }
    }

    /// A provider delta must leave the last rendered bubble in place while
    /// preparation is suspended. The shared body is the production renderer.
    @MainActor func testStreamingNativeReplyKeepsContentAndHeightBetweenPreparedDeltas() throws {
        let source = "Here is the plan.\n\n" + NativeUITestData.fence(NativeUITestData.source("card"))
        var preparation = NativeReplyPreparation()
        let first = NativeReplyRevision(source)
        preparation.begin(first)
        XCTAssertNil(preparation.projection, "Only the first preparation may show a placeholder")
        preparation.finish(NativeReplyProjection.prepare(source), for: first)
        func render() throws -> CGImage {
            let renderer = ImageRenderer(content: NativeReplyBody(preparation: preparation) { Text($0) }
                .frame(width: 350).padding(12).background(Color.white))
            renderer.scale = 1
            return try XCTUnwrap(renderer.cgImage)
        }
        for (index, delta) in ["\n\nA", "\n\nA short", "\n\nA short walk."].enumerated() {
            let prior = try render()
            let last = preparation.projection
            let revision = NativeReplyRevision(source + delta)
            preparation.begin(revision)
            XCTAssertNotNil(preparation.projection, "Streaming must not replace content with Preparing reply")
            XCTAssertEqual(preparation.projection, last)
            XCTAssertFalse(preparation.isCurrent, "Old controls must not act on a stale reply")
            let pending = try render()
            if let output = ProcessInfo.processInfo.environment["WONDER_SNAPSHOT_OUTPUT"] {
                for (name, image) in [("prepared", prior), ("pending", pending)] {
                    let url = URL(fileURLWithPath: output).appendingPathComponent("stream-\(index)-\(name).png")
                    let destination = try XCTUnwrap(CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil))
                    CGImageDestinationAddImage(destination, image, nil)
                    XCTAssertTrue(CGImageDestinationFinalize(destination))
                }
            }
            print("STREAM delta \(index): prepared=\(prior.height) pending=\(pending.height)")
            XCTAssertEqual(pending.height, prior.height, "No bubble height jump while preparing a delta")
            preparation.finish(NativeReplyProjection.prepare(revision.source), for: revision)
            XCTAssertTrue(preparation.isCurrent)
        }
    }

    func testLatePreparationCannotReplaceANewerReply() {
        var preparation = NativeReplyPreparation()
        let old = NativeReplyRevision("old"), current = NativeReplyRevision("current")
        preparation.begin(old)
        preparation.begin(current)
        preparation.finish(.prepare("current"), for: current)
        preparation.finish(.prepare("old"), for: old)
        XCTAssertEqual(preparation.projection, .prepare("current"))
        XCTAssertTrue(preparation.isCurrent)
    }

    func testCacheReusesExactRevisionAndInvalidatesChangedInput() async {
        let cache = NativeReplyCache()
        let source = NativeUITestData.fence(NativeUITestData.source("card"))
        let first = await cache.projection(for: source)
        let second = await cache.projection(for: source)
        XCTAssertEqual(first, second)
        let once = await cache.preparations
        XCTAssertEqual(once, 1)
        let changed = await cache.projection(for: source.replacingOccurrences(of: "Weekend", with: "Holiday"))
        XCTAssertNotEqual(first, changed)
        let twice = await cache.preparations
        XCTAssertEqual(twice, 2)
    }

    func testCacheBoundsAndMemoryPressureEviction() async {
        let cache = NativeReplyCache()
        for i in 0..<60 { _ = await cache.projection(for: "\(i)" + String(repeating: "x", count: 30_000)) }
        let count = await cache.count, bytes = await cache.bytes
        XCTAssertLessThanOrEqual(count, 48); XCTAssertLessThanOrEqual(bytes, 1_048_576)
        await cache.removeAll()
        let remaining = await cache.count, remainingBytes = await cache.bytes
        XCTAssertEqual(remaining, 0); XCTAssertEqual(remainingBytes, 0)
    }

    func testCacheDoesNotReuseCanonicallyEquivalentButInvalidUnicodeRevision() async {
        let cache = NativeReplyCache()
        let composed = NativeUITestData.fence(NativeUITestData.source("card").replacingOccurrences(of: "Weekend plan", with: String(repeating: "é", count: 100)))
        let decomposed = composed.decomposedStringWithCanonicalMapping
        XCTAssertEqual(composed, decomposed, "Swift String equality normalizes Unicode")
        XCTAssertNotEqual(NativeReplyRevision(composed), NativeReplyRevision(decomposed))
        XCTAssertNotEqual(NativeReplyRevision("a\u{0301}\u{0327}"), NativeReplyRevision("a\u{0327}\u{0301}"))
        guard case .interface = await cache.projection(for: composed).pieces.first else { return XCTFail() }
        guard case .unavailable = await cache.projection(for: decomposed).pieces.first else {
            return XCTFail("A byte-changed revision must be validated against the schema's code-point limit")
        }
    }
}
