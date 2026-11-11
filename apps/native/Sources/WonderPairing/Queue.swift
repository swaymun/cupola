import Foundation
public struct QueuedMessage: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let clientMessageId: String
    public let body: String
    public let revision: Int
    public let attachmentIds: [String]
    public var codexSkills: [CodexSkillSelection]? = nil
    /// T3 Code on the computer holds it in its thread's queue: Send now,
    /// Edit and Cancel change it there.
    public var waitsInT3Code: Bool? = nil
}
