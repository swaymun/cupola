import Foundation

/// A share whose delivery the computer has not confirmed. It is saved before
/// the first request that changes anything on the computer and updated after
/// each answer, so a share sheet that restarts mid-send can finish it with the
/// same bytes and request identities instead of sending it again. A new share
/// always gets a new item, even with identical content.
public struct PendingShare: Codable, Identifiable, Sendable {
    public enum Destination: Codable, Equatable, Sendable {
        /// An existing thread. `attach` asks the computer for its conversation first.
        case existing(targetID: String, projectID: String, reference: String, conversationID: String?, attach: Bool)
        /// A new Project thread, created with exactly `request`.
        case newThread(projectID: String, request: Data)
    }
    public struct File: Codable, Equatable, Sendable {
        public let id: String
        public let name: String
        public let mimeType: String
        public let sha256: String
    }

    /// Also the new thread's creation identity and the message's client ID.
    public let id: String
    public let host: String
    public let deviceID: String
    /// What a retry on the same sheet must match: the thread, or the new
    /// thread's Project, provider and model.
    public let target: String
    public let destination: Destination
    public let body: String
    public let files: [File]
    /// Where it was going, in words, for the recovery prompt.
    public let title: String
    public let createdAt: Date
    public var conversationID: String?
    /// Verified uploads, by file ID.
    public var uploads: [String: ConversationFile] = [:]
    public var uploadIDs: [String: String]?
    public var request: SendRequest?

    public init(id: String = UUID().uuidString.lowercased(), host: String, deviceID: String, target: String, destination: Destination,
                body: String, files: [StagedFile], title: String, createdAt: Date = Date()) {
        self.id = id; self.host = host; self.deviceID = deviceID; self.target = target; self.destination = destination
        self.body = body; self.title = title; self.createdAt = createdAt
        self.files = files.map { File(id: $0.id, name: $0.name, mimeType: $0.mimeType, sha256: $0.sha256) }
        if case .existing(_, _, _, let conversationID, false) = destination { self.conversationID = conversationID }
    }

    /// The same share retried in one session: same computer, destination and content.
    public func isRetry(host: String, target: String, body: String, fileIDs: [String]) -> Bool {
        self.host == host && self.target == target && self.body == body && files.map(\.id) == fileIDs
    }
}

/// The share extension's own record of shares not yet confirmed, in its
/// container. Each item is a folder: the files' bytes, then `share.json`,
/// written last and atomically, so an item exists only once its bytes do.
public struct ShareJournal: Sendable {
    public let directory: URL
    /// Older unconfirmed shares are dropped rather than offered again.
    public static let maximumAge: TimeInterval = 7 * 24 * 60 * 60
    public static let maximumItems = 8

    public init(directory: URL) { self.directory = directory }

    public static var standard: ShareJournal {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return ShareJournal(directory: base.appendingPathComponent("PendingShares", isDirectory: true))
    }

    /// Unconfirmed shares, oldest first. An unreadable or expired item is removed.
    public func load(now: Date = Date()) -> [PendingShare] {
        let folders = (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? []
        var items: [PendingShare] = []
        for folder in folders {
            guard let data = try? Data(contentsOf: folder.appendingPathComponent("share.json")),
                  let item = try? JSONDecoder().decode(PendingShare.self, from: data),
                  item.id == folder.lastPathComponent, now.timeIntervalSince(item.createdAt) < Self.maximumAge else {
                try? FileManager.default.removeItem(at: folder)
                continue
            }
            items.append(item)
        }
        items.sort { $0.createdAt < $1.createdAt }
        for stale in items.dropLast(Self.maximumItems) { remove(stale.id) }
        return Array(items.suffix(Self.maximumItems))
    }

    /// Saves a new item with its files' bytes, before any request is sent for it.
    public func begin(_ item: PendingShare, files: [StagedFile]) throws {
        let folder = try self.folder(item.id)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        for (index, file) in files.enumerated() {
            guard item.files.indices.contains(index), item.files[index].id == file.id else { throw FileFailure.integrity }
            try file.data.write(to: folder.appendingPathComponent("file-\(index)"), options: .atomic)
        }
        try update(item)
    }

    /// Records an answer (a conversation, an upload, the message request).
    public func update(_ item: PendingShare) throws {
        let data = try JSONEncoder().encode(item)
        try data.write(to: try folder(item.id).appendingPathComponent("share.json"), options: .atomic)
    }

    /// The item's files as they were shared, each checked against its saved hash.
    public func files(for item: PendingShare) throws -> [StagedFile] {
        let folder = try self.folder(item.id)
        return try item.files.enumerated().map { index, file in
            let data = try Data(contentsOf: folder.appendingPathComponent("file-\(index)"), options: .alwaysMapped)
            guard ConversationFile.digest(data) == file.sha256 else { throw FileFailure.integrity }
            return try StagedFile(id: file.id, name: file.name, mimeType: file.mimeType, data: data, source: AttachmentFileSource(url: folder.appendingPathComponent("file-\(index)")))
        }
    }

    /// Retires an item once the computer confirmed it, refused it, or the person discarded it.
    public func remove(_ id: String) {
        guard let folder = try? folder(id) else { return }
        try? FileManager.default.removeItem(at: folder)
    }

    private func folder(_ id: String) throws -> URL {
        guard !id.isEmpty, id.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") }) else { throw FileFailure.integrity }
        return directory.appendingPathComponent(id, isDirectory: true)
    }
}
