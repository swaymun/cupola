import Foundation
import CoreGraphics

/// This is the snapshot that was laid out on screen, never the latest global cursor.
public struct VisibleReadReceipt: Equatable, Sendable {
    public let conversationId: String
    public let hostEpoch: String
    public let readThroughSequence: UInt64
    public init(snapshot: ConversationSnapshot) {
        conversationId = snapshot.conversationId
        hostEpoch = snapshot.hostEpoch
        readThroughSequence = snapshot.lastSequence
    }
    public func requestBody() throws -> Data {
        struct Request: Encodable {
            let markRead = true
            let hostEpoch: String
            let readThroughSequence: UInt64
        }
        return try JSONEncoder().encode(Request(hostEpoch: hostEpoch, readThroughSequence: readThroughSequence))
    }
}

public enum ReadVisibility {
    /// A long final message counts only when its end reaches the actual viewport.
    /// Merely constructing the message's VStack, or seeing its beginning, is insufficient.
    public static func latestEndIsVisible(frame: CGRect, viewport: CGRect) -> Bool {
        !frame.isEmpty && !viewport.isEmpty && !frame.isNull && !viewport.isNull &&
        frame.maxY >= viewport.minY && frame.maxY <= viewport.maxY &&
        frame.maxX > viewport.minX && frame.minX < viewport.maxX
    }
}
