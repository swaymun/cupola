import Foundation

/// What the composer is completing at the caret. Offsets are UTF-16, as in
/// `UITextView.selectedRange`. Following T3 Code's composer: a provider command
/// must start the message (`/` at offset 0, caret still in that first word),
/// and `@` starts a file mention at the beginning of any word.
public enum ComposerTrigger: Equatable, Sendable {
    case command(query: String, range: NSRange)
    case file(query: String, range: NSRange)

    public var query: String {
        switch self { case .command(let query, _), .file(let query, _): query }
    }
    public var range: NSRange {
        switch self { case .command(_, let range), .file(_, let range): range }
    }
    public var isCommand: Bool { if case .command = self { true } else { false } }

    static func isSpace(_ unit: unichar) -> Bool {
        guard let scalar = Unicode.Scalar(unit) else { return false }
        return CharacterSet.whitespacesAndNewlines.contains(scalar)
    }

    /// The trigger at `selection`, or nil. Only a caret (no selected text)
    /// completes, and each kind only when the provider offers it.
    public static func detect(in text: String, selection: NSRange, commands: Bool, files: Bool) -> Self? {
        guard selection.length == 0, selection.location != NSNotFound else { return nil }
        let string = text as NSString
        let caret = min(max(0, selection.location), string.length)
        if commands, caret > 0, string.character(at: 0) == 0x2F {
            let word = NSRange(location: 1, length: caret - 1)
            if string.rangeOfCharacter(from: .whitespacesAndNewlines, options: [], range: word).location == NSNotFound {
                return .command(query: string.substring(with: word), range: NSRange(location: 0, length: caret))
            }
        }
        guard files else { return nil }
        var start = caret
        while start > 0, !isSpace(string.character(at: start - 1)) { start -= 1 }
        guard start < caret, string.character(at: start) == 0x40 else { return nil }
        let query = string.substring(with: NSRange(location: start + 1, length: caret - start - 1))
        // A finished quoted mention (`@"a b"`) or a second `@` is not a search.
        guard !query.hasPrefix("\""), !query.contains("@") else { return nil }
        return .file(query: query, range: NSRange(location: start, length: caret - start))
    }

    /// `text` with this trigger's word replaced by `insertion` and one space
    /// after it, and the caret after that space. The rest of the word after the
    /// caret is replaced too, so choosing again never leaves a fragment.
    public func replacing(in text: String, with insertion: String) -> (text: String, caret: Int)? {
        let string = text as NSString
        guard range.location != NSNotFound, NSMaxRange(range) <= string.length else { return nil }
        var end = NSMaxRange(range)
        while end < string.length, !Self.isSpace(string.character(at: end)) { end += 1 }
        let followedBySpace = end < string.length
        let replacement = followedBySpace ? insertion : insertion + " "
        let replaced = string.replacingCharacters(in: NSRange(location: range.location, length: end - range.location),
                                                  with: replacement)
        let caret = range.location + (replacement as NSString).length + (followedBySpace ? 1 : 0)
        return (replaced, caret)
    }
}

/// The provider's own command list for a thread
/// (`GET /api/v1/conversations/{id}/commands`).
public struct ComposerCommandCatalog: Decodable, Equatable, Sendable {
    public enum Availability: String, Decodable, Sendable {
        /// The complete native list; it may be empty.
        case ready
        /// Last catalog reported for this account and Project folder.
        case cached
        /// The live session has not reported a usable list yet.
        case unknown
        /// No live session on the Mac holds this thread yet.
        case unavailable
        public init(from decoder: Decoder) throws {
            self = Self(rawValue: try decoder.singleValueContainer().decode(String.self)) ?? .unknown
        }
    }
    public struct Command: Decodable, Hashable, Sendable {
        public let name: String
        public let description: String
        public let argumentHint: String?
        public let aliases: [String]?
        public let insertionText: String?
        public init(name: String, description: String, argumentHint: String? = nil, aliases: [String]? = nil, insertionText: String? = nil) {
            self.name = name; self.description = description; self.argumentHint = argumentHint; self.aliases = aliases
            self.insertionText = insertionText
        }
        /// What choosing it writes into the draft; the provider runs it on Send.
        public var insertion: String { insertionText ?? "/" + name }
    }
    public let availability: Availability
    public let commands: [Command]
    public let revision: String?
    public let fetchedAt: String?
    public init(availability: Availability, commands: [Command], revision: String? = nil, fetchedAt: String? = nil) {
        self.availability = availability; self.commands = commands; self.revision = revision
        self.fetchedAt = fetchedAt
    }

    /// Commands for `query`: name prefix, then alias prefix, then name
    /// substring, each in the provider's own order.
    public func matching(_ query: String) -> [Command] {
        let query = query.lowercased()
        guard !query.isEmpty else { return commands }
        func tier(_ command: Command) -> Int? {
            let name = command.name.lowercased()
            if name.hasPrefix(query) { return 0 }
            if command.aliases?.contains(where: { $0.lowercased().hasPrefix(query) }) == true { return 1 }
            if name.contains(query) { return 2 }
            return nil
        }
        return commands.enumerated()
            .compactMap { offset, command in tier(command).map { ($0, offset, command) } }
            .sorted { ($0.0, $0.1) < ($1.0, $1.1) }
            .map(\.2)
    }
}

/// Other threads in this Project. The host supplies a durable, readable
/// reference; the title alone is never used as the thread's identity.
public struct ComposerThreadResults: Decodable, Equatable, Sendable {
    public struct Entry: Decodable, Hashable, Identifiable, Sendable {
        public let conversationId: String
        public let title: String
        public let family: AgentFamily
        public let mention: String
        public var id: String { conversationId }
    }
    public let entries: [Entry]
    public let truncated: Bool
}

/// Project files matching an `@` search
/// (`GET /api/v1/conversations/{id}/workspace/search`), ranked by the Mac.
public struct WorkspaceSearchResults: Decodable, Equatable, Sendable {
    public struct Entry: Decodable, Hashable, Identifiable, Sendable {
        public let rootId: String
        public let rootLabel: String
        public let path: String
        public let name: String
        public let isDirectory: Bool
        /// The provider's own mention for this path; inserted verbatim.
        public let mention: String
        public var id: String { rootId + "\n" + path }
        public init(rootId: String, rootLabel: String, path: String, name: String, isDirectory: Bool, mention: String) {
            self.rootId = rootId; self.rootLabel = rootLabel; self.path = path; self.name = name
            self.isDirectory = isDirectory; self.mention = mention
        }
        /// The folder part of `path`, shown under the name.
        public var folder: String {
            guard let slash = path.lastIndex(of: "/") else { return "" }
            return String(path[..<slash])
        }
    }
    public let entries: [Entry]
    public let truncated: Bool
    public init(entries: [Entry], truncated: Bool = false) { self.entries = entries; self.truncated = truncated }
}
