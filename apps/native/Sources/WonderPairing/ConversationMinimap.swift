import Foundation

/// One mark per user turn, prepared once per timeline revision. The same
/// content bands drive drawing, cursor placement and hit testing.
public struct ConversationMinimapProjection: Sendable {
    public struct Mark: Identifiable, Equatable, Sendable {
        public let id: String
        /// Bounded Markdown previews; full text is used only while projecting.
        public let prompt: String
        public let reply: String?
        public var scrollID: String { id }
    }

    public enum Spacing: Sendable { case content, compact }

    /// A small navigation index, independent of live list geometry. A line of
    /// projected content occupies one point before fitting the rail to its cap.
    /// The compact prototype instead reserves nine points for every turn.
    public struct Layout: Equatable, Sendable {
        public struct Band: Equatable, Sendable {
            public let start: CGFloat
            public let end: CGFloat
            public var center: CGFloat { (start + end) / 2 }
        }
        public let boundaries: [CGFloat]
        public let spacing: Spacing
        public var count: Int { max(0, boundaries.count - 1) }
        public var extent: CGFloat { boundaries.last ?? 0 }

        fileprivate init(weights: [CGFloat], spacing: Spacing) {
            self.spacing = spacing
            var boundaries: [CGFloat] = [0]
            boundaries.reserveCapacity(weights.count + 1)
            for weight in weights { boundaries.append(boundaries.last! + max(4, weight)) }
            self.boundaries = boundaries
        }

        public func railHeight(viewportHeight: CGFloat) -> CGFloat {
            let tallest = min(360, max(44, viewportHeight - 96))
            return min(tallest, max(44, 16 + extent))
        }

        /// A minimum touch height must not stretch a two-turn chat's marks.
        public func drawnExtent(height: CGFloat) -> CGFloat { min(extent, max(1, height - 16)) }

        public func band(at index: Int, height: CGFloat) -> Band? {
            guard (0..<count).contains(index), extent > 0 else { return nil }
            let scale = drawnExtent(height: height) / extent
            return Band(start: 8 + boundaries[index] * scale, end: 8 + boundaries[index + 1] * scale)
        }

        /// Choose the band under the touch, including its small inter-row gap.
        /// Binary search keeps scrubbing bounded even in very long histories.
        public func index(atY y: CGFloat, height: CGFloat) -> Int? {
            guard count > 0 else { return nil }
            let fraction = y.isNaN ? 0 : min(1, max(0, (y - 8) / drawnExtent(height: height)))
            let target = fraction * extent
            var low = 0, high = count
            while low < high {
                let middle = (low + high) / 2
                if boundaries[middle + 1] <= target { low = middle + 1 }
                else { high = middle }
            }
            return min(count - 1, low)
        }

        /// Dense histories still draw at most one stroke per four points, but
        /// selection retains every turn. Otherwise a proportional block shows
        /// each turn's extent; compact rows keep a single two-point stroke.
        public func drawingBands(height: CGFloat) -> [Band] {
            let maximum = 1 + Int(max(0, drawnExtent(height: height) - 2) / 4)
            guard count > 0 else { return [] }
            if count > maximum {
                return (0..<maximum).map { index in
                    let y = 8 + CGFloat(index) * max(0, drawnExtent(height: height) - 2) / CGFloat(max(1, maximum - 1))
                    return Band(start: y, end: y + 2)
                }
            }
            let proposed: [Band] = (0..<count).compactMap { index in
                guard let band = band(at: index, height: height) else { return nil }
                if spacing == .compact { return Band(start: band.center - 1, end: band.center + 1) }
                if band.end - band.start < 3 {
                    return Band(start: max(8, band.center - 0.5), end: min(8 + drawnExtent(height: height), band.center + 0.5))
                }
                return Band(start: band.start + 1, end: band.end - 1)
            }
            // A very tall reply can compress nearby short turns below a pixel.
            // Merge their drawing only; the hit index keeps every content band.
            var drawn: [Band] = []
            for band in proposed {
                if let last = drawn.last, band.start < last.end {
                    drawn[drawn.count - 1] = Band(start: last.start, end: max(last.end, band.end))
                } else { drawn.append(band) }
            }
            return drawn
        }
    }

    public let marks: [Mark]
    private let positions: [String: Int]
    private let contentLayout: Layout
    private let compactLayout: Layout

    public init(entries: [ChatFeedEntry]) {
        var marks: [Mark] = []
        var replies: [String?] = []
        var weights: [CGFloat] = []
        var positions: [String: Int] = [:]
        for entry in entries {
            for row in entry.rows {
                if row.isUser, !row.text.isEmpty || !row.attachmentIds.isEmpty {
                    marks.append(Mark(id: row.id, prompt: Self.bounded(row.text), reply: nil))
                    replies.append(nil)
                    weights.append(0)
                } else if !marks.isEmpty, row.isAgentSpeech, !row.isCommentary {
                    replies[marks.count - 1] = row.text
                }
                let index = max(0, marks.count - 1)
                positions[row.id] = index
                positions["activity:" + row.id] = index
                positions["commands:" + row.id] = index
                if !weights.isEmpty, !entry.isActivity {
                    weights[index] += Self.contentLines(row.text) + 8 * CGFloat(row.attachmentIds.count)
                }
            }
            let index = max(0, marks.count - 1)
            // Folded activity is one small header, never its hidden tool output.
            // Disclosure changes therefore cannot shift a reader's navigation index.
            if entry.isActivity, !weights.isEmpty { weights[index] += 2 }
            for row in entry.rows {
                for file in row.toolFiles { positions["file:" + entry.id + ":" + file.id] = index }
                if !weights.isEmpty { weights[index] += 8 * CGFloat(row.toolFiles.count) }
            }
        }
        self.marks = zip(marks, replies).map { Mark(id: $0.id, prompt: $0.prompt, reply: $1.map(Self.bounded)) }
        self.positions = positions
        contentLayout = Layout(weights: weights, spacing: .content)
        compactLayout = Layout(weights: Array(repeating: 9, count: marks.count), spacing: .compact)
    }

    public func layout(spacing: Spacing = .content) -> Layout { spacing == .content ? contentLayout : compactLayout }

    /// The content estimate includes explicit line breaks and long unbroken
    /// lines at a representative 64 UTF-16 columns. It is deliberately stable
    /// across scrolling, disclosure and viewport changes, not measured pixels.
    private static func contentLines(_ text: String) -> CGFloat {
        guard !text.isEmpty else { return 0 }
        return CGFloat(text.split(separator: "\n", omittingEmptySubsequences: false).reduce(0) {
            $0 + max(1, ($1.utf16.count + 63) / 64)
        })
    }

    public func index(for nodeID: String?, selectedMarkID: String? = nil, atBottom: Bool = false) -> Int? {
        guard !marks.isEmpty else { return nil }
        if let selectedMarkID, let index = positions[selectedMarkID], marks.indices.contains(index),
           marks[index].id == selectedMarkID { return index }
        return atBottom ? marks.count - 1 : min(marks.count - 1, nodeID.flatMap { positions[$0] } ?? 0)
    }

    public func mark(id: String) -> Mark? {
        guard let index = positions[id], marks.indices.contains(index), marks[index].id == id else { return nil }
        return marks[index]
    }

    public func scrollTarget(for id: String, visibleNodeIDs: Set<String>) -> String? {
        guard let mark = mark(id: id), visibleNodeIDs.contains(mark.scrollID) else { return nil }
        return mark.scrollID
    }

    /// Cursor movement never changes geometry. A streamed reply redraws only
    /// when it crosses an estimated line boundary, not for each new character.
    public func drawsSameRail(as other: Self, spacing: Spacing = .content) -> Bool {
        layout(spacing: spacing) == other.layout(spacing: spacing)
    }

    static func bounded(_ text: String) -> String {
        guard let end = text.index(text.startIndex, offsetBy: 400, limitedBy: text.endIndex), end < text.endIndex else { return text }
        return String(text[..<end])
    }
}
