import XCTest
@testable import WonderPairing

final class ActivityPresentationTests: XCTestCase {

    private func row(_ id: String, type: String, state: String = "completed", turn: String = "turn", payload: [String: ThreadValue] = [:], itemText: String? = "Message") -> ReadRow {
        ReadRow(id: id, author: "Ada", text: "Message", isUser: type == "userMessage", timestamp: "1000",
                turnId: turn, item: ReadItem(id: id, type: type, state: state, text: itemText, createdAt: "1000", payload: payload))
    }
    func testVerifiedArtifactsStayOutOfTextAndRetainMetadata() throws {
        let file: ThreadValue = .object(["id":.string("file"), "name":.string("Chart.png"), "mimeType":.string("image/png"), "byteSize":.number(42), "sha256":.string("digest"), "state":.string("available"), "updatedAt":.string("now")])
        let result: ThreadValue = .object(["content":.array([.object(["type":.string("text"),"text":.string("Here is the chart")]), .object(["type":.string("wonderArtifact"),"file":file])])])
        let value = row("tool", type:"mcpToolCall", payload:["result":result])
        XCTAssertEqual(value.toolFiles.map(\.id), ["file"])
        XCTAssertEqual(value.activity?.details.first?.text, "Here is the chart")
        XCTAssertEqual(value.toolFiles.first?.sha256, "digest")
        let localImage = row("local", type: "imageView", payload: ["result":result])
        XCTAssertEqual(localImage.activity?.title, "Viewed image")
        XCTAssertEqual(localImage.toolFiles.map(\.id), ["file"])
        XCTAssertTrue(ChatFeedEntry.grouping([localImage])[0].isActivity)
        XCTAssertNil(localImage.unsavedMedia, "A verified file is shown, not reported as unsaved")
    }

    func testGeneratedImageLegacyPayloadAndFailureNeverPrintImageBytes() throws {
        let legacy = row("legacy", type:"imageGeneration", payload:["result":.string("PRIVATE_BASE64"), "savedPath":.string("/private/generated.png")])
        let text = try XCTUnwrap(legacy.activity).details.map(\.text).joined()
        XCTAssertTrue(text.contains("Preview unavailable"))
        XCTAssertFalse(text.contains("PRIVATE_BASE64"))
        XCTAssertFalse(text.contains("/private"))
        let failed = row("failed", type:"imageGeneration", payload:["result":.null, "failure":.string("Generation failed")])
        XCTAssertTrue(try XCTUnwrap(failed.activity).failed)
        XCTAssertEqual(failed.activity?.details.map(\.text), ["Generation failed"])
        XCTAssertTrue(row("pending", type:"imageGeneration", state:"started", payload:["result":.null]).activity?.isRunning == true)
    }

    func testObservedMixedToolOutputShapesNeverPrintEncodedMedia() throws {
        for types in [["input_text", "input_image", "input_audio"], ["inputText", "inputImage", "inputAudio"], ["text", "image", "audio"]] {
            let blocks: ThreadValue = .array([
                .object(["type": .string(types[0]), "text": .string("Found a chart.")]),
                .object(["type": .string(types[1]), "imageUrl": .string("data:image/png;base64,PRIVATE_IMAGE"), "data": .string("PRIVATE_IMAGE")]),
                .object(["type": .string(types[2]), "audioUrl": .string("data:audio/wav;base64,PRIVATE_AUDIO")])])
            for (kind, key, result) in [("dynamicToolCall", "contentItems", blocks), ("functionCallOutput", "output", blocks), ("mcpToolCall", "result", .object(["content": blocks]))] {
                let activity = try XCTUnwrap(row("output", type: kind, payload: [key: result]).activity)
                let text = activity.details.map(\.text).joined()
                XCTAssertTrue(text.contains("Found a chart."))
                XCTAssertTrue(text.contains("Image result"))
                XCTAssertTrue(text.contains("Audio result"))
                XCTAssertFalse(text.contains("PRIVATE"))
                XCTAssertFalse(text.contains("base64"))
            }
        }
    }

}
