import Foundation

/// Names and grants only. There is intentionally no API for reading a value.
public struct SecretList: Decodable, Sendable {
    public let secrets: [SavedSecret]
}
public struct SavedSecret: Decodable, Identifiable, Sendable {
    public var id: String { secretRef }
    public let secretRef: String
    public let name: String
    public let projectId: String
    public let projectName: String
    public let uses: [SecretUse]
    public struct SecretUse: Decodable, Sendable {
        public let conversationId: String
        public let provider: String
        public let title: String
    }
    public func deletionBody() throws -> Data {
        try JSONSerialization.data(withJSONObject: ["projectId": projectId, "secretRef": secretRef], options: [.sortedKeys])
    }
}
