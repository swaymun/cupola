import Foundation
import CryptoKit
import AVFoundation
import CoreGraphics

public enum FileFailure: Error {
    case tooLarge, integrity, stale, unsupported, notUploaded
    /// A picture the Mac kept by reference is no longer there as it was.
    case gone
}

/// The audio and video formats Cupola plays. The computer streams only these
/// five, each checked against its file signature (`media_mime` in wonderd), so
/// other formats are named as unsupported before anything is downloaded.
public enum PlayableMedia {
    public static let fileExtensions: Set<String> = ["mp4", "mov", "m4a", "mp3", "wav"]
    public static let mimeTypes: Set<String> = ["video/mp4", "video/quicktime", "audio/mp4", "audio/mpeg", "audio/wav"]
    /// Common audio and video files Cupola can't play. The computer labels
    /// most of them as generic data, so the name is what identifies them.
    static let otherExtensions: Set<String> = [
        "m4v", "aac", "webm", "mkv", "avi", "flac", "ogg", "oga", "ogv", "opus", "aif", "aiff", "aifc",
        "caf", "wma", "wmv", "3gp", "3g2", "mpg", "mpeg", "m4b", "amr", "flv"
    ]
    public static let unsupportedMessage =
        "Cupola can play MP4, MOV, M4A, MP3 and WAV files. Open this one on your computer."

    private static func fileExtension(_ name: String) -> String? {
        let parts = name.split(separator: ".")
        guard parts.count > 1, let last = parts.last else { return nil }
        return last.lowercased()
    }

    /// A file Files streams to the built-in player.
    public static func isPlayable(fileName: String) -> Bool {
        fileExtension(fileName).map { fileExtensions.contains($0) } ?? false
    }

    /// Audio or video in a format Cupola doesn't play: say so instead of
    /// downloading the whole file to show "Preview unavailable".
    public static func isUnsupported(fileName: String, mimeType: String?) -> Bool {
        guard !isPlayable(fileName: fileName) else { return false }
        let mime = (mimeType ?? "").lowercased()
        if mime.hasPrefix("audio/") || mime.hasPrefix("video/") { return !mimeTypes.contains(mime) }
        return fileExtension(fileName).map { otherExtensions.contains($0) } ?? false
    }
}

/// What the composer says when staged attachments could not be uploaded. The
/// files stay staged, so each message says whether sending again can help.
public enum AttachmentUploadFailure {
    public static func message(for error: Error) -> String {
        switch error {
        case FileFailure.tooLarge, PairingFailure.response(413):
            return "Videos must be 1 GB (1,024 MB) or smaller; other files must be 8 MB or smaller."
        case FileFailure.unsupported, PairingFailure.response(415):
            return "Your computer can't accept one of these file types. Remove it, then send again."
        case FileFailure.integrity:
            return "Your computer received a different copy of a file. Your files are saved."
        case let error as URLError where error.code == .timedOut:
            return "The upload took too long. Your files are saved; send again on a stronger connection."
        case is URLError:
            return "The connection to your computer dropped during the upload. Your files are saved."
        default:
            return "Attachment upload was not confirmed. Your files are saved."
        }
    }
}

/// A note about exact preview bytes. The host validates the Project, path,
/// source hash and selected region again before accepting the attachment.
public struct ArtifactAnnotation: Codable, Sendable, Hashable {
    public static let mimeType = "application/vnd.wonder.artifact-annotation+json"
    public enum Anchor: Codable, Sendable, Hashable {
        case textLines(startLine: Int, endLine: Int)
        /// One-based rows in the exact displayed unified patch, never current-file lines.
        case diffLines(scope: WorkspaceChangeScope, originalPath: String?, startLine: Int, endLine: Int)
        /// UTF-8 byte offsets into the pinned source, with an exclusive end.
        case textRange(startByte: Int, endByte: Int)
        case imageRegion(x: Double, y: Double, width: Double, height: Double)
        case pdfRegion(page: Int, x: Double, y: Double, width: Double, height: Double)

        private enum CodingKeys: String, CodingKey { case kind, scope, originalPath, startLine, endLine, startByte, endByte, page, x, y, width, height }
        public init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            switch try values.decode(String.self, forKey: .kind) {
            case "textLines":
                self = .textLines(startLine: try values.decode(Int.self, forKey: .startLine),
                                  endLine: try values.decode(Int.self, forKey: .endLine))
            case "diffLines":
                self = .diffLines(scope: try values.decode(WorkspaceChangeScope.self, forKey: .scope),
                                  originalPath: try values.decodeIfPresent(String.self, forKey: .originalPath),
                                  startLine: try values.decode(Int.self, forKey: .startLine),
                                  endLine: try values.decode(Int.self, forKey: .endLine))
            case "textRange":
                self = .textRange(startByte: try values.decode(Int.self, forKey: .startByte),
                                  endByte: try values.decode(Int.self, forKey: .endByte))
            case "imageRegion":
                self = .imageRegion(x: try values.decode(Double.self, forKey: .x),
                                    y: try values.decode(Double.self, forKey: .y),
                                    width: try values.decode(Double.self, forKey: .width),
                                    height: try values.decode(Double.self, forKey: .height))
            case "pdfRegion":
                self = .pdfRegion(page: try values.decode(Int.self, forKey: .page),
                                  x: try values.decode(Double.self, forKey: .x),
                                  y: try values.decode(Double.self, forKey: .y),
                                  width: try values.decode(Double.self, forKey: .width),
                                  height: try values.decode(Double.self, forKey: .height))
            default:
                throw DecodingError.dataCorruptedError(forKey: .kind, in: values, debugDescription: "Unsupported preview anchor")
            }
        }
        public func encode(to encoder: Encoder) throws {
            var values = encoder.container(keyedBy: CodingKeys.self)
            switch self {
            case .textLines(let start, let end):
                try values.encode("textLines", forKey: .kind)
                try values.encode(start, forKey: .startLine)
                try values.encode(end, forKey: .endLine)
            case .diffLines(let scope, let originalPath, let start, let end):
                try values.encode("diffLines", forKey: .kind)
                try values.encode(scope, forKey: .scope)
                try values.encodeIfPresent(originalPath, forKey: .originalPath)
                try values.encode(start, forKey: .startLine)
                try values.encode(end, forKey: .endLine)
            case .textRange(let start, let end):
                try values.encode("textRange", forKey: .kind)
                try values.encode(start, forKey: .startByte)
                try values.encode(end, forKey: .endByte)
            case .imageRegion(let x, let y, let width, let height):
                try values.encode("imageRegion", forKey: .kind)
                try values.encode(x, forKey: .x); try values.encode(y, forKey: .y)
                try values.encode(width, forKey: .width); try values.encode(height, forKey: .height)
            case .pdfRegion(let page, let x, let y, let width, let height):
                try values.encode("pdfRegion", forKey: .kind)
                try values.encode(page, forKey: .page)
                try values.encode(x, forKey: .x); try values.encode(y, forKey: .y)
                try values.encode(width, forKey: .width); try values.encode(height, forKey: .height)
            }
        }
    }
    public let version: Int
    public let projectId: String
    public let conversationId: String
    public let rootId: String
    public let path: String
    public let sourceSha256: String
    public let anchor: Anchor
    public var note: String

    public init(projectId: String, conversationId: String, rootId: String, path: String,
                source: Data, startLine: Int, endLine: Int, note: String) throws {
        version = 1
        self.projectId = projectId; self.conversationId = conversationId
        self.rootId = rootId; self.path = path
        sourceSha256 = ConversationFile.digest(source)
        anchor = .textLines(startLine: startLine, endLine: endLine)
        self.note = note
        guard let text = String(data: source, encoding: .utf8) else { throw FileFailure.integrity }
        let lines = max(1, text.split(separator: "\n", omittingEmptySubsequences: false).count - (text.hasSuffix("\n") ? 1 : 0))
        try validate(sourceLineCount: lines)
    }

    public init(projectId: String, conversationId: String, rootId: String, path: String,
                source: Data, mimeType: String, startByte: Int, endByte: Int, note: String) throws {
        guard Self.isTextFormat(mimeType), startByte >= 0, endByte > startByte,
              endByte <= source.count, String(data: source, encoding: .utf8) != nil,
              String(data: source.subdata(in: startByte..<endByte), encoding: .utf8) != nil
        else { throw FileFailure.integrity }
        version = 1
        self.projectId = projectId; self.conversationId = conversationId
        self.rootId = rootId; self.path = path
        sourceSha256 = ConversationFile.digest(source)
        anchor = .textRange(startByte: startByte, endByte: endByte)
        self.note = note
        try validate()
    }

    public init(projectId: String, conversationId: String, rootId: String, path: String,
                patch: DiffAnnotationSource, scope: WorkspaceChangeScope, originalPath: String?,
                startLine: Int, endLine: Int, note: String) throws {
        guard patch.validSelection(startLine...max(startLine, endLine)), endLine >= startLine else { throw FileFailure.integrity }
        version = 1; self.projectId = projectId; self.conversationId = conversationId
        self.rootId = rootId; self.path = path
        sourceSha256 = patch.sha256
        anchor = .diffLines(scope: scope, originalPath: originalPath, startLine: startLine, endLine: endLine)
        self.note = note
        try validate()
    }

    public var isDiff: Bool {
        if case .diffLines = anchor { return true }
        return false
    }

    private static func isTextFormat(_ mimeType: String) -> Bool {
        (mimeType.hasPrefix("text/") && mimeType != "text/html") ||
        ["application/json", "application/yaml", "application/xml", "application/sql"].contains(mimeType)
    }

    public init(projectId: String, conversationId: String, rootId: String, path: String,
                source: Data, imageX x: Double, y: Double, width: Double, height: Double,
                note: String) throws {
        version = 1; self.projectId = projectId; self.conversationId = conversationId
        self.rootId = rootId; self.path = path
        sourceSha256 = ConversationFile.digest(source)
        anchor = .imageRegion(x: x, y: y, width: width, height: height)
        self.note = note
        guard !source.isEmpty else { throw FileFailure.integrity }
        try validate()
    }

    public init(projectId: String, conversationId: String, rootId: String, path: String,
                source: Data, pdfPage page: Int, x: Double, y: Double, width: Double,
                height: Double, note: String) throws {
        version = 1; self.projectId = projectId; self.conversationId = conversationId
        self.rootId = rootId; self.path = path
        sourceSha256 = ConversationFile.digest(source)
        anchor = .pdfRegion(page: page, x: x, y: y, width: width, height: height)
        self.note = note
        guard source.starts(with: Data("%PDF-".utf8)) else { throw FileFailure.integrity }
        try validate()
    }

    private static func validRegion(_ x: Double, _ y: Double, _ width: Double, _ height: Double) -> Bool {
        [x, y, width, height].allSatisfy(\.isFinite) && x >= 0 && y >= 0 &&
        width > 0 && height > 0 && x + width <= 1 && y + height <= 1
    }

    public func validate(sourceLineCount: Int? = nil) throws {
        let components = path.split(separator: "/", omittingEmptySubsequences: false)
        guard version == 1, !projectId.isEmpty, !conversationId.isEmpty,
              !rootId.isEmpty, rootId.utf8.count <= 128,
              !path.isEmpty, path.utf8.count <= 4096, !path.hasPrefix("/"),
              components.allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }),
              sourceSha256.utf8.count == 64,
              sourceSha256.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
              !note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              note.utf8.count <= 4096,
              !note.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) && $0 != "\n" && $0 != "\t" })
        else { throw FileFailure.integrity }
        switch anchor {
        case .textLines(let start, let end):
            guard start > 0, end >= start, sourceLineCount.map({ end <= $0 }) ?? true else { throw FileFailure.integrity }
        case .diffLines(_, let originalPath, let start, let end):
            guard start > 0, end >= start, end - start < 100, end <= 20_000 else { throw FileFailure.integrity }
            if let originalPath {
                guard !originalPath.isEmpty, originalPath.utf8.count <= 4096, !originalPath.hasPrefix("/"),
                      originalPath.split(separator: "/", omittingEmptySubsequences: false).allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }),
                      !originalPath.contains("\0") else { throw FileFailure.integrity }
            }
        case .textRange(let start, let end):
            guard start >= 0, end > start else { throw FileFailure.integrity }
        case .imageRegion(let x, let y, let width, let height):
            guard Self.validRegion(x, y, width, height) else { throw FileFailure.integrity }
        case .pdfRegion(let page, let x, let y, let width, let height):
            guard page > 0, page <= 10_000, Self.validRegion(x, y, width, height) else { throw FileFailure.integrity }
        }
    }

    public static func read(_ data: Data) throws -> Self {
        let value = try JSONDecoder().decode(Self.self, from: data)
        try value.validate()
        return value
    }

    public func replacingNote(_ note: String) throws -> Self {
        var copy = self
        copy.note = note
        try copy.validate()
        return copy
    }

    /// A New Chat preview can validate its anchor before a conversation exists.
    /// Bind those same source bytes and note only after prepare-only creation.
    public func bound(to conversationID: String) throws -> Self {
        let copy = Self(version: version, projectId: projectId, conversationId: conversationID,
                        rootId: rootId, path: path, sourceSha256: sourceSha256,
                        anchor: anchor, note: note)
        try copy.validate()
        return copy
    }

    private init(version: Int, projectId: String, conversationId: String, rootId: String,
                 path: String, sourceSha256: String, anchor: Anchor, note: String) {
        self.version = version; self.projectId = projectId; self.conversationId = conversationId
        self.rootId = rootId; self.path = path; self.sourceSha256 = sourceSha256
        self.anchor = anchor; self.note = note
    }

    public func stagedFile() throws -> StagedFile {
        try validate()
        let name = String((path.split(separator: "/").last ?? "File").prefix(48)) + ".annotation.json"
        let encoder = JSONEncoder()
        encoder.outputFormatting = .sortedKeys
        return try StagedFile(name: name, mimeType: Self.mimeType, data: encoder.encode(self))
    }
}

public struct ConversationFile: Codable, Identifiable, Sendable {
    public let id: String
    public let name: String
    public let mimeType: String?
    public let byteSize: Int?
    public let sha256: String?
    public let state: String
    public let updatedAt: String
    /// Pixel size the Mac read from an image's header, when it is known.
    public var width: Int? = nil
    public var height: Int? = nil
    public func verify(_ data: Data, mime: String?) throws {
        try AttachmentLimits.validate(byteSize: data.count, mimeType: mimeType ?? "")
        guard state == "available", byteSize == data.count,
              sha256 == Self.digest(data), mimeType?.lowercased() == mime?.lowercased() else { throw FileFailure.integrity }
        try Self.validateContent(data, mime: mimeType ?? "")
    }
    /// What a published page's file may be: an HTML page, or the wonder-ui
    /// v1 document a visual was given (`wonder_show_visual` components).
    public static let publishedPageTypes: Set<String> = ["text/html", wonderUIType]
    public static let wonderUIType = "application/vnd.wonder.ui+json"
    public static func digest(_ data: Data) -> String { SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined() }
    /// A phone-sized JPEG of `original`: the Mac makes it only from bytes that
    /// matched the original's hash and names its own; it must be that JPEG.
    public static func verifyPreview(_ data: Data, mime: String?, sha256: String, of original: ConversationFile) throws {
        guard data.count <= 8 * 1024 * 1024 else { throw FileFailure.tooLarge }
        guard original.state == "available", (original.mimeType ?? "").lowercased().hasPrefix("image/"),
              mime?.lowercased() == "image/jpeg", sha256.lowercased() == digest(data) else { throw FileFailure.integrity }
        try validateContent(data, mime: "image/jpeg")
    }
    /// An image, video or audio file: what a tool made to be seen or heard.
    public var isVisualMedia: Bool {
        let mime = (mimeType ?? "").lowercased()
        return mime.hasPrefix("image/") || mime.hasPrefix("video/") || mime.hasPrefix("audio/")
    }
    public static func validateContent(_ data: Data, mime: String) throws {
        if mime == "application/pdf", !data.starts(with: Data("%PDF-".utf8)) { throw FileFailure.integrity }
        if mime == "image/png", !data.starts(with: [137,80,78,71,13,10,26,10]) { throw FileFailure.integrity }
        if mime == "image/jpeg", !data.starts(with: [255,216,255]) { throw FileFailure.integrity }
        if mime.hasPrefix("text/"), String(data: data, encoding: .utf8) == nil { throw FileFailure.integrity }
        if mime == "image/svg+xml", !isSafeSVG(data) { throw FileFailure.integrity }
        if ["video/mp4", "video/quicktime", "audio/mp4"].contains(mime), data.count < 12 || Array(data[data.startIndex + 4 ..< data.startIndex + 8]) != Array("ftyp".utf8) { throw FileFailure.integrity }
        if mime == "audio/mpeg", !(data.starts(with: Array("ID3".utf8)) || (data.count >= 2 && data[data.startIndex] == 0xff && data[data.startIndex + 1] & 0xe0 == 0xe0)) { throw FileFailure.integrity }
        if mime == "audio/wav", !(data.starts(with: Array("RIFF".utf8)) && data.count >= 12 && Array(data[data.startIndex + 8 ..< data.startIndex + 12]) == Array("WAVE".utf8)) { throw FileFailure.integrity }
    }
    /// An SVG drawn in the sandboxed preview: UTF-8 with an `<svg` root and no
    /// document type or entity declarations (the Mac checks the same).
    public static func isSafeSVG(_ data: Data) -> Bool {
        guard let text = String(data: data, encoding: .utf8)?.lowercased() else { return false }
        return text.contains("<svg") && !text.contains("<!doctype") && !text.contains("<!entity")
    }
    /// Audio or video a phone can play inline.
    public var isPlayableMedia: Bool {
        PlayableMedia.mimeTypes.contains((mimeType ?? "").lowercased())
    }
    /// A picture the photo viewer shows (SVG draws in its own sandboxed preview).
    public var isStillImage: Bool {
        let mime = (mimeType ?? "").lowercased()
        return mime.hasPrefix("image/") && mime != "image/svg+xml"
    }
}

/// The box a tool's picture takes before its bytes arrive, so the row never
/// changes height under a scroll. A known pixel size keeps its proportions:
/// tall pictures stop at a height cap and very wide ones at a minimum height,
/// and the picture fills the box, cropped evenly on its long side. Without a
/// size (`single` is nil) the fixed box shows the whole picture.
public struct ToolImageLayout: Equatable, Sendable {
    /// Box width over height.
    public let aspectRatio: Double
    public let maxWidth: Double

    public static let maximumWidth = 360.0
    public static let maximumHeight = 320.0
    public static let widestRatio = 3.0
    public static let galleryHeight = 200.0
    public static let galleryWidths = 120.0...300.0

    /// One picture on its own row.
    public static func single(_ file: ConversationFile) -> Self? {
        guard let ratio = ratio(file) else { return nil }
        // A small picture is not blown up past its own width, but stays tappable.
        let width = min(maximumWidth, max(88, Double(file.width ?? 0)))
        return Self(aspectRatio: min(widestRatio, max(width / maximumHeight, ratio)), maxWidth: width)
    }

    /// One tile of a row of pictures: every tile has the same height, and a
    /// picture of unknown size gets a square.
    public static func tileWidth(_ file: ConversationFile) -> Double {
        guard let ratio = ratio(file) else { return galleryHeight }
        return min(galleryWidths.upperBound, max(galleryWidths.lowerBound, (galleryHeight * ratio).rounded()))
    }

    private static func ratio(_ file: ConversationFile) -> Double? {
        guard let width = file.width, let height = file.height, width > 0, height > 0 else { return nil }
        return Double(width) / Double(height)
    }
}

public struct WorkspaceRoot: Codable, Identifiable, Hashable, Sendable {
    public let id: String
    public let label: String
    public let path: String
    public let isDirectory: Bool
    public let kind: String
    public let readOnly: Bool
    public let byteSize: UInt64?
    public let mimeType: String?
    public init(id: String, label: String, path: String, isDirectory: Bool, kind: String, readOnly: Bool, byteSize: UInt64? = nil, mimeType: String? = nil) {
        self.id = id; self.label = label; self.path = path; self.isDirectory = isDirectory; self.kind = kind; self.readOnly = readOnly; self.byteSize = byteSize; self.mimeType = mimeType
    }
}

public struct WorkspaceRootsResponse: Codable, Sendable {
    public let available: Bool
    public let detail: String?
    public let roots: [WorkspaceRoot]
    public let attachments: [ConversationFile]
    public init(available: Bool, detail: String?, roots: [WorkspaceRoot], attachments: [ConversationFile]) {
        self.available = available; self.detail = detail; self.roots = roots; self.attachments = attachments
    }
}

public struct WorkspaceEntry: Codable, Identifiable, Hashable, Sendable {
    public let name: String
    public let path: String
    public let isDirectory: Bool
    public let byteSize: UInt64?
    public let mimeType: String?
    public var id: String { path }
    public init(name: String, path: String, isDirectory: Bool, byteSize: UInt64?, mimeType: String?) {
        self.name = name; self.path = path; self.isDirectory = isDirectory; self.byteSize = byteSize; self.mimeType = mimeType
    }
}

public struct WorkspaceDirectoryPage: Codable, Sendable {
    public let rootId: String
    public let path: String
    public let parentPath: String?
    public let entries: [WorkspaceEntry]
    public let nextOffset: Int?
    public init(rootId: String, path: String, parentPath: String?, entries: [WorkspaceEntry], nextOffset: Int?) {
        self.rootId = rootId; self.path = path; self.parentPath = parentPath; self.entries = entries; self.nextOffset = nextOffset
    }
}

public struct WorkspaceGitChange: Codable, Identifiable, Hashable, Sendable {
    public let path: String
    public let originalPath: String?
    public let state: String
    public let indexStatus: String
    public let worktreeStatus: String
    public var id: String { path + ":" + state }
    public init(path: String, originalPath: String?, state: String, indexStatus: String, worktreeStatus: String) {
        self.path = path; self.originalPath = originalPath; self.state = state; self.indexStatus = indexStatus; self.worktreeStatus = worktreeStatus
    }
}

public struct WorkspaceGitStatusResponse: Codable, Sendable {
    public let available: Bool
    public let detail: String?
    public let repositoryPath: String?
    public let changes: [WorkspaceGitChange]
    public init(available: Bool, detail: String?, repositoryPath: String?, changes: [WorkspaceGitChange]) {
        self.available = available; self.detail = detail; self.repositoryPath = repositoryPath; self.changes = changes
    }
}

public struct WorkspaceDiffResponse: Codable, Sendable {
    public let path: String
    public let staged: Bool
    public let diff: String
    public let annotationVersion: Int?
    public init(path: String, staged: Bool, diff: String, annotationVersion: Int? = nil) {
        self.path = path; self.staged = staged; self.diff = diff; self.annotationVersion = annotationVersion
    }
}

/// Keeps an imported movie alive for previews and upload. Copies of a staged
/// attachment share this owner; the temporary source goes away with the last one.
public final class AttachmentFileSource: Sendable {
    public let url: URL
    private let temporary: Bool
    public convenience init(url: URL) { self.init(url: url, temporary: false) }
    fileprivate init(url: URL, temporary: Bool) { self.url = url; self.temporary = temporary }
    deinit { if temporary { try? FileManager.default.removeItem(at: url) } }
    static func storing(_ data: Data) throws -> AttachmentFileSource {
        let source = AttachmentFileSource(url: FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString), temporary: true)
        try data.write(to: source.url)
        return source
    }
}

public struct StagedFile: Codable, Identifiable, Sendable {
    public let id: String
    public let name: String
    public let mimeType: String
    public let data: Data
    public let source: AttachmentFileSource?
    /// Hash of `data`, computed once when the file is staged. A saved draft
    /// names its body by this hash (see `StagedFileBodies`) instead of
    /// carrying the bytes, so a draft save never re-encodes attachments.
    public let sha256: String
    public var uploadID: String?
    public var uploaded: ConversationFile?
    public init(name: String, mimeType: String, data: Data) throws {
        try self.init(id: UUID().uuidString, name: name, mimeType: mimeType, data: data)
    }
    public init(id: String, name: String, mimeType: String, data: Data, uploaded: ConversationFile? = nil, source: AttachmentFileSource? = nil) throws {
        try AttachmentLimits.validate(byteSize: data.count, mimeType: mimeType)
        try ConversationFile.validateContent(data, mime: mimeType)
        guard !id.isEmpty, !name.isEmpty, !mimeType.isEmpty else { throw FileFailure.integrity }
        self.id = id; self.name = name; self.mimeType = mimeType; self.data = data; self.uploaded = uploaded
        sha256 = ConversationFile.digest(data)
        self.source = try source ?? (mimeType.hasPrefix("video/") ? AttachmentFileSource.storing(data) : nil)
    }
    /// Check the provider's size before copying its ephemeral file. Call on a worker
    /// while the provider callback still owns the source; AVFoundation reuses this URL.
    public static func importingVideo(_ url: URL, name: String, mimeType: String) throws -> StagedFile {
        let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? Int.max
        try AttachmentLimits.validate(byteSize: size, mimeType: mimeType)
        let source = AttachmentFileSource(url: FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString).appendingPathExtension(url.pathExtension), temporary: true)
        try FileManager.default.copyItem(at: url, to: source.url)
        return try StagedFile(id: UUID().uuidString, name: name, mimeType: mimeType,
                              data: Data(contentsOf: source.url, options: .alwaysMapped), source: source)
    }
    private enum CodingKeys: String, CodingKey { case id, name, mimeType, data, sha256, byteSize, uploaded, uploadID }
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        name = try container.decode(String.self, forKey: .name)
        mimeType = try container.decode(String.self, forKey: .mimeType)
        uploadID = try container.decodeIfPresent(String.self, forKey: .uploadID)
        uploaded = try container.decodeIfPresent(ConversationFile.self, forKey: .uploaded)
        if let inline = try container.decodeIfPresent(Data.self, forKey: .data) {
            // Drafts saved before bodies moved out of the intent carry them inline.
            data = inline
            try AttachmentLimits.validate(byteSize: inline.count, mimeType: mimeType)
            source = try mimeType.hasPrefix("video/") ? AttachmentFileSource.storing(inline) : nil
            sha256 = try container.decodeIfPresent(String.self, forKey: .sha256) ?? ConversationFile.digest(inline)
            (decoder.userInfo[StagedFileBodies.key] as? StagedFileBodies)?.noteInlineBody()
        } else {
            guard let bodies = decoder.userInfo[StagedFileBodies.key] as? StagedFileBodies else { throw FileFailure.integrity }
            sha256 = try container.decode(String.self, forKey: .sha256)
            data = try bodies.read(sha256: sha256, byteSize: container.decode(Int.self, forKey: .byteSize))
            source = try AttachmentFileSource(url: bodies.bodyURL(sha256: sha256))
        }
        try AttachmentLimits.validate(byteSize: data.count, mimeType: mimeType)
        guard !id.isEmpty, !name.isEmpty, !mimeType.isEmpty else { throw FileFailure.integrity }
    }
    /// With `StagedFileBodies` in the encoder's user info, only a verified
    /// reference is encoded and the body is written beside the intent first;
    /// otherwise the body stays inline.
    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(id, forKey: .id)
        try container.encode(name, forKey: .name)
        try container.encode(mimeType, forKey: .mimeType)
        try container.encodeIfPresent(uploadID, forKey: .uploadID)
        try container.encodeIfPresent(uploaded, forKey: .uploaded)
        try container.encode(sha256, forKey: .sha256)
        if let bodies = encoder.userInfo[StagedFileBodies.key] as? StagedFileBodies {
            try bodies.write(data, sha256: sha256)
            try container.encode(data.count, forKey: .byteSize)
        } else {
            try container.encode(data, forKey: .data)
        }
    }
    /// The body sent to the Mac. Runs off the UI actor; it is where a body
    /// read back from its sidecar is checked against the hash it was saved under.
    public func uploadBody() throws -> Data {
        guard ConversationFile.digest(data) == sha256 else { throw FileFailure.integrity }
        return try JSONSerialization.data(withJSONObject: ["clientUploadId": id, "name": name, "mimeType": mimeType, "contentBase64": data.base64EncodedString()])
    }
}

/// Staged attachment bodies of one conversation's saved draft, one file per
/// body named by its SHA-256. The intent JSON holds only the hash and size,
/// so saving a draft (typing, an upload finishing, removing a file) writes a
/// few hundred bytes instead of re-encoding megabytes of attachments. A body
/// is written once, before any intent that references it; bodies no committed
/// intent references are removed after the next commit.
public final class StagedFileBodies: @unchecked Sendable {
    static let key = CodingUserInfoKey(rawValue: "wonder.stagedFileBodies")!
    public let directory: URL
    private let lock = NSLock()
    private var referenced: Set<String> = []
    private var inline = false
    public init(directory: URL) { self.directory = directory }

    /// Hashes the encoded intent references; pass them to `retain` after commit.
    public var referencedDigests: Set<String> { lock.withLock { referenced } }
    /// The decoded intent still carried a body inline (saved before sidecars).
    public var decodedInlineBodies: Bool { lock.withLock { inline } }
    func noteInlineBody() { lock.withLock { inline = true } }

    func bodyURL(sha256: String) throws -> URL {
        // The name comes from saved JSON: only a lowercase hex digest is a path.
        guard sha256.utf8.count == 64, sha256.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else { throw FileFailure.integrity }
        return directory.appendingPathComponent(sha256 + ".body")
    }
    func write(_ data: Data, sha256: String) throws {
        let url = try bodyURL(sha256: sha256)
        lock.withLock { _ = referenced.insert(sha256) }
        let manager = FileManager.default
        if let size = (try? manager.attributesOfItem(atPath: url.path))?[.size] as? NSNumber, size.intValue == data.count { return }
        try manager.createDirectory(at: directory, withIntermediateDirectories: true)
        #if os(iOS)
        try data.write(to: url, options: [.atomic, .completeFileProtection])
        #else
        try data.write(to: url, options: .atomic)
        #endif
    }
    func read(sha256: String, byteSize: Int) throws -> Data {
        let data = try Data(contentsOf: bodyURL(sha256: sha256), options: .alwaysMapped)
        // The full hash is checked off the UI actor before upload (`uploadBody`).
        guard data.count == byteSize else { throw FileFailure.integrity }
        return data
    }
    /// Removes bodies the committed intent no longer references. Only called
    /// after a commit, so a body is never removed before the intent that
    /// replaces its reference is durable. A preparation racing this commit
    /// fails its revision check and writes its bodies again.
    public func retain(_ digests: Set<String>) {
        let manager = FileManager.default
        guard let names = try? manager.contentsOfDirectory(atPath: directory.path) else { return }
        for name in names where name.hasSuffix(".body") && !digests.contains(String(name.dropLast(5))) {
            try? manager.removeItem(at: directory.appendingPathComponent(name))
        }
    }
}

/// Matches the host's declared limits before reading or uploading a file.
public enum AttachmentLimits {
    public static let maximumVideoBytes = 1_073_741_824
    public static let videoTypes: Set<String> = ["video/mp4", "video/quicktime", "video/x-m4v", "video/3gpp", "video/3gpp2", "video/webm", "video/x-matroska", "video/x-msvideo"]
    public static func validate(byteSize: Int, mimeType: String) throws {
        if mimeType.hasPrefix("video/"), !videoTypes.contains(mimeType) { throw FileFailure.unsupported }
        let limit = mimeType.hasPrefix("video/") ? maximumVideoBytes : 8 * 1024 * 1024
        guard byteSize > 0, byteSize <= limit else { throw FileFailure.tooLarge }
    }
}

public struct AttachmentUploadReceipt: Decodable, Sendable {
    public let uploadId: String
    public let receivedBytes: Int
    public let totalBytes: Int
    public let maxChunkBytes: Int
    public let state: String
    public let file: ConversationFile?
}

extension PairingAPI {
    /// Chunk encoding, hashing and byte verification run on the generic executor.
    /// Repeating begin recovers the durable offset, including a lost completion receipt.
    public func uploadAttachment(_ file: StagedFile, conversationPath: String, origin: String,
                                 credential: Credential,
                                 progress: @escaping @Sendable (Int, Int) async -> Void = { _, _ in }) async throws -> ConversationFile {
        let worker = Task.detached(priority: .userInitiated) {
            try await self.performAttachmentUpload(file, conversationPath: conversationPath, origin: origin,
                                                   credential: credential, progress: progress)
        }
        return try await withTaskCancellationHandler(operation: { try await worker.value }, onCancel: { worker.cancel() })
    }

    private func performAttachmentUpload(_ file: StagedFile, conversationPath: String, origin: String,
                                         credential: Credential,
                                         progress: @escaping @Sendable (Int, Int) async -> Void) async throws -> ConversationFile {
        try AttachmentLimits.validate(byteSize: file.data.count, mimeType: file.mimeType)
        guard ConversationFile.digest(file.data) == file.sha256 else { throw FileFailure.integrity }
        if !file.mimeType.hasPrefix("video/") {
            let uploaded: ConversationFile = try await request(conversationPath + "/files", origin: origin,
                body: file.uploadBody(), credential: credential)
            try uploaded.verify(file.data, mime: file.mimeType)
            await progress(file.data.count, file.data.count)
            return uploaded
        }
        let id = file.uploadID ?? file.id
        let base = conversationPath + "/files/uploads"
        let path = base + "/" + id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed)!
        do {
            let body = try JSONSerialization.data(withJSONObject: ["clientUploadId": id, "name": file.name,
                "mimeType": file.mimeType, "byteSize": file.data.count, "sha256": file.sha256])
            var receipt: AttachmentUploadReceipt = try await request(base, origin: origin, body: body, credential: credential)
            while receipt.state != "complete", receipt.receivedBytes < file.data.count {
                try Task.checkCancellation()
                guard receipt.uploadId == id, receipt.totalBytes == file.data.count,
                      receipt.receivedBytes >= 0, receipt.maxChunkBytes > 0 else { throw FileFailure.integrity }
                await progress(receipt.receivedBytes, file.data.count)
                let offset = receipt.receivedBytes
                let chunkLimit = isRelay(origin) ? 128 * 1024 : 4 * 1024 * 1024
                let end = min(file.data.count, offset + min(receipt.maxChunkBytes, chunkLimit))
                let chunk = try JSONSerialization.data(withJSONObject: ["offset": offset,
                    "contentBase64": file.data.subdata(in: offset..<end).base64EncodedString()])
                receipt = try await request(path, origin: origin, body: chunk, credential: credential, method: "PATCH")
                guard receipt.receivedBytes == end else { throw FileFailure.integrity }
            }
            try Task.checkCancellation()
            await progress(receipt.receivedBytes, file.data.count)
            if receipt.state != "complete" {
                receipt = try await request(path + "/complete", origin: origin, body: Data("{}".utf8), credential: credential)
            }
            guard receipt.state == "complete", receipt.uploadId == id, receipt.receivedBytes == file.data.count,
                  receipt.totalBytes == file.data.count, let uploaded = receipt.file else { throw FileFailure.integrity }
            try uploaded.verify(file.data, mime: file.mimeType)
            return uploaded
        } catch {
            if Task.isCancelled {
                // A new unstructured task can clean up after the request task is cancelled.
                await Task {
                    struct Empty: Decodable, Sendable {}
                    let _: Empty? = try? await self.request(path, origin: origin, credential: credential, method: "DELETE")
                }.value
                throw CancellationError()
            }
            throw error
        }
    }
}
/// Bounded poster cache shared by chat and New chat. AVFoundation work is isolated
/// from the UI actor; maximumSize bounds decode size before the image is produced.
public actor AttachmentVideoPosters {
    public struct Poster: @unchecked Sendable { public let image: CGImage; public let duration: String }
    public static let shared = AttachmentVideoPosters()
    private final class Entry: NSObject { let poster: Poster; init(_ poster: Poster) { self.poster = poster } }
    private let cache = NSCache<NSString, Entry>()
    private var pending: [String: Task<Poster, Error>] = [:]
    public init() {
        cache.countLimit = 24
        cache.totalCostLimit = 4 * 1024 * 1024
    }
    public func poster(key: String, source: AttachmentFileSource, mimeType: String) async throws -> Poster {
        if let value = cache.object(forKey: key as NSString) { return value.poster }
        if let task = pending[key] { return try await task.value }
        let task = Task.detached(priority: .userInitiated) {
            // Capture the owner, not only its URL, until decoding has finished.
            defer { withExtendedLifetime(source) {} }
            // Durable bodies intentionally have no media extension. Use their verified MIME type.
            let asset = AVURLAsset(url: source.url, options: [AVURLAssetOverrideMIMETypeKey: mimeType])
            let seconds = try await asset.load(.duration).seconds
            let generator = AVAssetImageGenerator(asset: asset)
            generator.appliesPreferredTrackTransform = true
            generator.maximumSize = CGSize(width: 192, height: 192)
            let result = try await generator.image(at: .zero)
            let duration = seconds.isFinite ? max(0, Int(seconds.rounded())) : 0
            return Poster(image: result.image, duration: String(format: "%d:%02d", duration / 60, duration % 60))
        }
        pending[key] = task
        defer { pending[key] = nil }
        let value = try await task.value
        cache.setObject(Entry(value), forKey: key as NSString, cost: value.image.bytesPerRow * value.image.height)
        return value
    }
}
