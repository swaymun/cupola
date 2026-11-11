import Foundation

/// The address of one saved tool result. An item ID alone is not unique across turns.
public struct ToolOutputIdentity: Equatable, Sendable {
    public let conversationId: String
    public let turnId: String
    public let itemId: String
    public init(conversationId: String, turnId: String, itemId: String) {
        self.conversationId = conversationId; self.turnId = turnId; self.itemId = itemId
    }
    public var path: String {
        func escape(_ value: String) -> String {
            value.addingPercentEncoding(withAllowedCharacters: CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~")) ?? ""
        }
        return "/api/v1/conversations/\(escape(conversationId))/items/\(escape(itemId))/output?turnId=\(escape(turnId))"
    }
}

public struct ToolOutput: Codable, Equatable, Sendable {
    public let conversationId: String
    public let turnId: String
    public let itemId: String
    public let output: String
    public let outputBytes: Int

    public init(conversationId: String, turnId: String, itemId: String, output: String, outputBytes: Int) {
        self.conversationId = conversationId; self.turnId = turnId; self.itemId = itemId
        self.output = output; self.outputBytes = outputBytes
    }
}

extension PairingAPI {
    public func toolOutput(_ identity: ToolOutputIdentity, connection: SavedConnection) async throws -> ToolOutput {
        try await request(identity.path, origin: connection.origin, credential: connection.credential)
    }
}

extension ReadItem {
    public var outputTruncated: Bool { payload?["outputTruncated"]?.bool == true }
}

/// Expanded content belongs to the visible row, never the saved history or a
/// cache. The caller's task is cancelled when the row closes or changes host.
public enum ToolOutputDetail {
    public enum Failure: Error { case unavailable, mismatchedResponse }

    public static func prepare(
        row: ReadRow, expanded: Bool, conversationId: String,
        load: (@Sendable (ToolOutputIdentity) async throws -> ToolOutput)? = nil
    ) async throws -> ActivityPresentation? {
        guard expanded else { return nil }
        try Task.checkCancellation()
        var value = row
        if var item = row.item, item.outputTruncated {
            guard let turnId = row.turnId, let load else { throw Failure.unavailable }
            let identity = ToolOutputIdentity(conversationId: conversationId, turnId: turnId, itemId: item.id)
            let response = try await load(identity)
            try Task.checkCancellation()
            guard response.conversationId == identity.conversationId, response.turnId == identity.turnId,
                  response.itemId == identity.itemId, response.outputBytes == response.output.utf8.count else {
                throw Failure.mismatchedResponse
            }
            item.payload?["output"] = .string(response.output)
            item.payload?["outputBytes"] = .number(Double(response.outputBytes))
            item.payload?["outputTruncated"] = .bool(false)
            value = ReadRow(id: row.id, author: row.author, text: row.text, isUser: row.isUser,
                            timestamp: row.timestamp, turnId: row.turnId, item: item)
        }
        let prepared = value
        let result = await Task.detached(priority: .userInitiated) { prepared.activity }.value
        try Task.checkCancellation()
        return result
    }
}
