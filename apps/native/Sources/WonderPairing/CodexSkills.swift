import Foundation

/// An explicit native selection, never inferred from ordinary dollar text.
public struct CodexSkillSelection: Codable, Hashable, Identifiable, Sendable {
    public let name: String
    public let path: String
    public let projectId: String
    public let folderId: String
    public let rootsRevision: Int
    public var id: String { path }
    public init(name: String, path: String, projectId: String, folderId: String, rootsRevision: Int) {
        self.name = name; self.path = path; self.projectId = projectId; self.folderId = folderId; self.rootsRevision = rootsRevision
    }
    private func tokenRanges(in text: String) -> [Range<String.Index>] {
        let token = "$" + name
        var search = text.startIndex..<text.endIndex, result: [Range<String.Index>] = []
        func word(_ character: Character) -> Bool { character.isLetter || character.isNumber || "_-$".contains(character) }
        while let range = text.range(of: token, options: .literal, range: search) {
            let before = range.lowerBound == text.startIndex ? nil : text[text.index(before: range.lowerBound)]
            let after = range.upperBound == text.endIndex ? nil : text[range.upperBound]
            if before.map({ !word($0) }) ?? true, after.map({ !word($0) }) ?? true { result.append(range) }
            search = range.upperBound..<text.endIndex
        }
        return result
    }
    public func isPresent(in text: String) -> Bool { !tokenRanges(in: text).isEmpty }
    public static func remove(path: String, text: inout String, skills: inout [Self]?) {
        guard let removed = skills?.first(where: { $0.path == path }) else { return }
        skills?.removeAll { $0.path == path }
        // A same-name selection still uses this token. Removing the last one
        // removes its exact token too, avoiding an accidental implicit mention.
        if skills?.contains(where: { $0.name == removed.name }) != true {
            for range in removed.tokenRanges(in: text).reversed() { text.removeSubrange(range) }
        }
        if skills?.isEmpty == true { skills = nil }
    }
    public static func retaining(_ skills: [Self]?, in text: String) -> [Self]? {
        let result = (skills ?? []).filter { $0.isPresent(in: text) }
        return result.isEmpty ? nil : result
    }
}
