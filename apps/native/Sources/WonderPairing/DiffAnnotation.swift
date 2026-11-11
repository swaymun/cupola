import Foundation

/// A bounded, complete unified patch. Row identity includes Git's header rows;
/// old/new file line numbers are labels only and never become file anchors.
public struct DiffAnnotationSource: Sendable {
    public struct Line: Identifiable, Sendable {
        public let id: Int
        public let oldLine: Int?
        public let newLine: Int?
        public let text: String
        public let preview: String
        public let sign: Character
        public var label: String {
            switch sign {
            case "-": "Removed line \(oldLine ?? 0)"
            case "+": "Added line \(newLine ?? 0)"
            default: "Unchanged line \(newLine ?? 0)"
            }
        }
    }
    public let sha256: String
    public let lines: [Line]
    private let selectionSizes: [Int: Int]

    public init(patch: String) throws {
        guard !patch.isEmpty, patch.utf8.count <= 512 * 1024 else { throw FileFailure.tooLarge }
        let raw = patch.split(separator: "\n", omittingEmptySubsequences: false)
        guard raw.count <= 20_000 else { throw FileFailure.tooLarge }
        var parsed: [Line] = []
        var old = 0, new = 0, oldLeft = 0, newLeft = 0
        var hunks = 0
        for (index, value) in raw.enumerated() {
            if value.hasPrefix("@@ ") {
                guard oldLeft == 0, newLeft == 0 else { throw FileFailure.integrity }
                let fields = value.split(separator: " ")
                guard fields.count >= 4, fields[3] == "@@",
                      let before = Self.range(fields[1], prefix: "-"),
                      let after = Self.range(fields[2], prefix: "+") else { throw FileFailure.integrity }
                (old, oldLeft) = before; (new, newLeft) = after; hunks += 1
            } else if oldLeft > 0 || newLeft > 0 {
                if value == "\\ No newline at end of file" { continue }
                guard let sign = value.first, [" ", "+", "-"].contains(sign) else { throw FileFailure.integrity }
                let usesOld = sign != "+", usesNew = sign != "-"
                guard (!usesOld || oldLeft > 0), (!usesNew || newLeft > 0) else { throw FileFailure.integrity }
                parsed.append(Line(id: index + 1, oldLine: usesOld ? old : nil, newLine: usesNew ? new : nil,
                                   text: String(value.dropFirst()), preview: String(value.dropFirst().prefix(2048)) + (value.count > 2049 ? "…" : ""), sign: sign))
                if usesOld { old += 1; oldLeft -= 1 }
                if usesNew { new += 1; newLeft -= 1 }
            } else if value.hasPrefix("@@") || value.hasPrefix("Binary files ") || value == "GIT binary patch" {
                throw FileFailure.unsupported
            }
        }
        guard hunks > 0, oldLeft == 0, newLeft == 0, !parsed.isEmpty else { throw FileFailure.integrity }
        sha256 = ConversationFile.digest(Data(patch.utf8)); lines = parsed
        let encoder = JSONEncoder(); encoder.outputFormatting = .withoutEscapingSlashes
        selectionSizes = Dictionary(uniqueKeysWithValues: try parsed.map { line in
            let old = line.oldLine.map(String.init) ?? "—", new = line.newLine.map(String.init) ?? "—"
            let prefix = "Patch row \(line.id) (old \(old), new \(new)): "
            return (line.id, prefix.utf8.count + (try encoder.encode(String(line.sign) + line.text)).count + 1)
        })
    }
    public func validSelection(_ range: ClosedRange<Int>) -> Bool {
        guard range.lowerBound > 0, range.upperBound - range.lowerBound < 100 else { return false }
        var size = 0
        for row in range {
            guard let count = selectionSizes[row] else { return false }
            size += count
        }
        return size <= 16 * 1024
    }
    private static func range(_ value: Substring, prefix: Character) -> (Int, Int)? {
        guard value.first == prefix else { return nil }
        let fields = value.dropFirst().split(separator: ",", omittingEmptySubsequences: false)
        guard (1...2).contains(fields.count), let start = Int(fields[0]), start >= 0,
              let count = fields.count == 2 ? Int(fields[1]) : 1, count >= 0,
              start <= 10_000_000, count <= 20_000, (count == 0 || start > 0) else { return nil }
        return (start, count)
    }
}
