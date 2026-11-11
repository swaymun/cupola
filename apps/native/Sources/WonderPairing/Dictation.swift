import Foundation

public struct DictationFailure: Error, Sendable {
    public let errorCategory: String
    public init(errorCategory: String) { self.errorCategory = errorCategory }
}

/// In-memory ownership for one native speech session; never a saved draft.
public struct DictationIntent: Sendable {
    public let requestID = UUID().uuidString.lowercased()
    public let hostID: String
    public let deviceID: String
    public let conversationID: String
    public var phase = "recording"
    public init(hostID: String, deviceID: String, conversationID: String, conversationTitle: String, modelID: String) {
        self.hostID = hostID; self.deviceID = deviceID; self.conversationID = conversationID
    }
}

/// A volatile replacement in UTF-16 coordinates, matching UITextView selection.
/// The saved draft remains `base` until the user finishes or starts editing.
public struct DictationProjection: Sendable {
    public let base: String
    public let range: NSRange
    public private(set) var transcript = ""
    public init(base: String, selection: NSRange) {
        self.base = base
        let length = (base as NSString).length
        let location = min(max(0, selection.location), length)
        range = NSRange(location: location, length: min(max(0, selection.length), length - location))
    }
    public var text: String {
        guard !transcript.isEmpty else { return base }
        return (base as NSString).replacingCharacters(in: range, with: insertion)
    }
    public var insertion: String {
        guard !transcript.isEmpty else { return "" }
        let before = (base as NSString).substring(to: range.location)
        let after = (base as NSString).substring(from: NSMaxRange(range))
        let left = before.last.map { !$0.isWhitespace } == true ? " " : ""
        let right = after.first.map { !$0.isWhitespace && !$0.isPunctuation } == true ? " " : ""
        return left + transcript + right
    }
    @discardableResult public mutating func update(_ words: String) -> Bool {
        let previous = transcript
        transcript = words.trimmingCharacters(in: .whitespacesAndNewlines)
        guard text.utf8.count <= 65536 else { transcript = previous; return false }
        return true
    }
    public func selection(afterReplacing previous: Self, selection: NSRange) -> NSRange {
        let oldEnd = previous.transcript.isEmpty ? NSMaxRange(range) : range.location + (previous.insertion as NSString).length
        let newEnd = transcript.isEmpty ? NSMaxRange(range) : range.location + (insertion as NSString).length
        func move(_ offset: Int) -> Int {
            if offset < range.location { return offset }
            if offset <= oldEnd { return newEnd }
            return offset + newEnd - oldEnd
        }
        let start = move(selection.location), end = move(NSMaxRange(selection))
        return NSRange(location: start, length: max(0, end - start))
    }
}

/// SpeechAnalyzer finalizes phrases, not the whole recording. Keep finalized
/// ranges once and replace only the current volatile range. None is a saved draft.
public struct DictationTranscript: Sendable {
    private var finalized = ""
    private var provisional = ""
    private var finalizedEnd = -Double.infinity
    public init() {}
    public var text: String { finalized + provisional }
    @discardableResult public mutating func update(_ words: String, start: Double, end: Double, isFinal: Bool) -> Bool {
        guard start.isFinite, end.isFinite, end > start, start >= finalizedEnd else { return false }
        // Preserve the recognizer's spacing and punctuation, including languages
        // that do not put spaces between words or phrase boundaries.
        if isFinal {
            finalized += words
            finalizedEnd = end
            provisional = ""
        } else { provisional = words }
        return true
    }
}
