import XCTest
@testable import WonderPairing

/// The phone shows tool SVG, audio and video inline only after their bytes
/// match their declared type (the Mac checks the same before storing them).
final class ConversationFileContentTests: XCTestCase {
    private func accepts(_ bytes: [UInt8], _ mime: String) -> Bool {
        (try? ConversationFile.validateContent(Data(bytes), mime: mime)) != nil
    }
    private func accepts(_ text: String, _ mime: String) -> Bool { accepts(Array(text.utf8), mime) }

    func testSVGMustBeAnSVGWithoutDeclarations() {
        XCTAssertTrue(accepts(#"<svg xmlns="http://www.w3.org/2000/svg"/>"#, "image/svg+xml"))
        XCTAssertFalse(accepts("<html></html>", "image/svg+xml"))
        XCTAssertFalse(accepts(#"<!DOCTYPE svg [<!ENTITY x "y">]><svg>&x;</svg>"#, "image/svg+xml"))
        XCTAssertFalse(accepts([0xff, 0xfe, 0x3c], "image/svg+xml"))
    }

    func testAudioAndVideoMustMatchTheirLeadingBytes() {
        let ftyp: [UInt8] = [0, 0, 0, 0x18] + Array("ftypmp42".utf8)
        XCTAssertTrue(accepts(ftyp, "video/mp4"))
        XCTAssertTrue(accepts(ftyp, "video/quicktime"))
        XCTAssertTrue(accepts(ftyp, "audio/mp4"))
        XCTAssertFalse(accepts(Array("not a movie".utf8), "video/mp4"))
        XCTAssertFalse(accepts([0, 0, 0], "video/mp4"), "too short to carry a box header")
        XCTAssertTrue(accepts(Array("ID3\u{4}".utf8), "audio/mpeg"))
        XCTAssertTrue(accepts([0xff, 0xfb, 0x90], "audio/mpeg"))
        XCTAssertFalse(accepts(Array("OggS".utf8), "audio/mpeg"))
        XCTAssertTrue(accepts(Array("RIFF\0\0\0\0WAVE".utf8), "audio/wav"))
        XCTAssertFalse(accepts(Array("RIFF\0\0\0\0AVI ".utf8), "audio/wav"))
    }

    func testOnlyPlayableTypesPlayAndSVGIsNotARasterImage() {
        func file(_ mime: String) -> ConversationFile {
            try! JSONDecoder().decode(ConversationFile.self, from: Data(#"{"id":"f","name":"f","mimeType":"\#(mime)","state":"available","updatedAt":"now"}"#.utf8))
        }
        XCTAssertTrue(file("audio/mpeg").isPlayableMedia)
        XCTAssertTrue(file("video/quicktime").isPlayableMedia)
        XCTAssertFalse(file("audio/ogg").isPlayableMedia)
        XCTAssertTrue(file("image/svg+xml").isVisualMedia)
    }

    /// Files names an unplayable format up front instead of downloading it in
    /// full to show "Preview unavailable"; the computer labels these as data.
    func testOtherAudioAndVideoFormatsAreUnsupportedBeforeDownload() {
        for name in ["clip.MP4", "take.mov", "memo.m4a", "song.mp3", "tone.wav"] {
            XCTAssertTrue(PlayableMedia.isPlayable(fileName: name), name)
            XCTAssertFalse(PlayableMedia.isUnsupported(fileName: name, mimeType: nil), name)
        }
        for name in ["clip.m4v", "voice.aac", "screen.webm", "film.mkv", "song.flac"] {
            XCTAssertFalse(PlayableMedia.isPlayable(fileName: name), name)
            XCTAssertTrue(PlayableMedia.isUnsupported(fileName: name, mimeType: "application/octet-stream"), name)
        }
        XCTAssertTrue(PlayableMedia.isUnsupported(fileName: "stream", mimeType: "video/x-matroska"))
        XCTAssertFalse(PlayableMedia.isUnsupported(fileName: "notes.txt", mimeType: "text/plain"))
        XCTAssertFalse(PlayableMedia.isUnsupported(fileName: "index.ts", mimeType: "text/plain"),
                       "TypeScript is text, not an MPEG stream")
        XCTAssertFalse(PlayableMedia.isUnsupported(fileName: "mp4", mimeType: nil), "a bare name has no format")
    }
}
