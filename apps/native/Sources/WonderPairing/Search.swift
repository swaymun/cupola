import Foundation

public struct SearchMessageFocus: Identifiable, Sendable {
    public let id = UUID()
    public let conversationID: String
    public let rowID: String
    public let scrollID: String
    public init(conversationID: String, rowID: String, scrollID: String) { self.conversationID = conversationID; self.rowID = rowID; self.scrollID = scrollID }
}

/// One `GET /api/v1/search` result (packages/protocol/search.md): a message
/// Wonder stored, or a match in a thread started on the Mac outside Wonder.
public struct MessageSearchResult: Codable, Hashable, Identifiable, Sendable {
    public let kind: String
    public let id: String
    public let title: String
    public let snippet: String?
    public let conversationId: String?
    public let updatedAt: String
    /// Native results: the Project the thread is attached in to open it,
    /// with the reference that Project's attach route takes.
    public let projectId: String?
    public let reference: String?
    /// The matching timeline item, when the Mac knows it.
    public let itemId: String?
    public init(kind: String, id: String, title: String, snippet: String?, conversationId: String?, updatedAt: String,
                projectId: String? = nil, reference: String? = nil, itemId: String? = nil) {
        self.kind = kind; self.id = id; self.title = title; self.snippet = snippet; self.conversationId = conversationId
        self.updatedAt = updatedAt; self.projectId = projectId; self.reference = reference; self.itemId = itemId
    }
    public var identity: String { kind + ":" + id }
    /// The snippet as runs of plain and matched text. `<mark>` delimits a
    /// match; the text is never interpreted as markup otherwise.
    public var snippetRuns: [SnippetRun] { Self.runs(snippet ?? "") }
    public var plainSnippet: String { snippetRuns.map(\.text).joined() }
    public var updatedDate: Date? { Double(updatedAt).map { Date(timeIntervalSince1970: $0 / 1000) } }

    public struct SnippetRun: Hashable, Sendable {
        public let text: String
        public let isMatch: Bool
    }
    static func runs(_ snippet: String) -> [SnippetRun] {
        var runs: [SnippetRun] = []
        var rest = Substring(snippet)
        while let open = rest.range(of: "<mark>") {
            if open.lowerBound > rest.startIndex { runs.append(SnippetRun(text: String(rest[..<open.lowerBound]), isMatch: false)) }
            let after = rest[open.upperBound...]
            guard let close = after.range(of: "</mark>") else { rest = after; break }
            if close.lowerBound > after.startIndex { runs.append(SnippetRun(text: String(after[..<close.lowerBound]), isMatch: true)) }
            rest = after[close.upperBound...]
        }
        if !rest.isEmpty { runs.append(SnippetRun(text: String(rest), isMatch: false)) }
        return runs
    }

    /// Where the opened conversation scrolls to. Wonder's own messages are
    /// found by message ID; native matches by timeline item, or by asking the
    /// Mac for the thread's occurrences when only the thread matched.
    public var focus: SearchFocusTarget? {
        switch kind {
        case "message": .message(id)
        case "assistant_message": .assistantMessage(id)
        default: itemId.map(SearchFocusTarget.item)
        }
    }
}

public enum SearchFocusTarget: Hashable, Sendable {
    case message(String)
    case assistantMessage(String)
    case item(String)

    /// The timeline row for this target among `rows` of `snapshot`, if loaded.
    public func rowID(snapshot: ConversationSnapshot?, rows: [ReadRow]) -> String? {
        let row: String?
        switch self {
        case .message(let id):
            row = snapshot?.messages.first { $0.messageId == id }.map { "user-" + ($0.clientMessageId ?? $0.messageId) }
        case .assistantMessage(let id):
            row = snapshot?.assistantMessages.first { $0.messageId == id }?.rowId
        case .item(let id):
            return rows.first { $0.item?.id == id }?.id ?? rows.first { $0.id == "user-" + id }?.id
        }
        return row.flatMap { target in rows.contains { $0.id == target } ? target : nil }
    }
}

public struct MessageSearchPage: Codable, Sendable {
    public var results: [MessageSearchResult]
    public var nextCursor: String?
    /// Some history on the Mac could not be searched completely.
    public var partial: Bool?
    public init(results: [MessageSearchResult] = [], nextCursor: String? = nil, partial: Bool? = nil) {
        self.results = results; self.nextCursor = nextCursor; self.partial = partial
    }
    /// Adds a later page, keeping one row per result identity.
    public mutating func append(_ page: Self) {
        var seen = Set(results.map(\.identity))
        results += page.results.filter { seen.insert($0.identity).inserted }
        nextCursor = page.nextCursor
        partial = (partial ?? false) || (page.partial ?? false)
    }
}

/// `GET /api/v1/conversations/{id}/search/occurrences`: matches inside one
/// thread's native history, oldest first.
public struct SearchOccurrences: Codable, Sendable {
    public struct Occurrence: Codable, Hashable, Sendable {
        public let itemId: String
        public let turnId: String?
        public let snippet: String
    }
    public let supported: Bool
    public let occurrences: [Occurrence]
}
