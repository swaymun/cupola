import Foundation
import SwiftUI
import Charts

/// The v1 catalog is deliberately finite: no URLs, expressions, code, arbitrary
/// styles or tool names. Its wire contract lives in packages/protocol.
struct NativeUIDocument: Decodable, Equatable, Sendable {
    let version: Int
    let fallback: String
    let components: [NativeUIComponent]
    let displayItemCount: Int

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["version", "fallback", "components"])
        version = try c.decode(Int.self, forKey: .init("version"))
        guard version == 1 else { throw NativeUIFailure.unsupported }
        fallback = try c.string("fallback", limit: 4_000)
        components = try c.decode([NativeUIComponent].self, forKey: .init("components"))
        guard (1...24).contains(components.count), Set(components.map(\.id)).count == components.count else {
            throw NativeUIFailure.invalid
        }
        displayItemCount = components.reduce(0) { $0 + $1.displayItemCount }
        guard displayItemCount <= 80 else { throw NativeUIFailure.oversized }
    }

    static func decode(_ source: String) throws -> Self {
        guard source.utf8.count <= 32_768 else { throw NativeUIFailure.oversized }
        // Bound nesting before entering Foundation's recursive decoder. Ignore
        // brackets inside strings (including escaped quotes and backslashes).
        var depth = 0, quoted = false, escaped = false
        for byte in source.utf8 {
            if quoted {
                if escaped { escaped = false }
                else if byte == 92 { escaped = true }
                else if byte == 34 { quoted = false }
            } else if byte == 34 { quoted = true }
            else if byte == 123 || byte == 91 {
                depth += 1
                guard depth <= 16 else { throw NativeUIFailure.oversized }
            } else if byte == 125 || byte == 93 { depth -= 1 }
        }
        return try JSONDecoder().decode(Self.self, from: Data(source.utf8))
    }
}

private struct NativeUIKey: CodingKey {
    let stringValue: String
    var intValue: Int? { nil }
    init(_ value: String) { stringValue = value }
    init?(stringValue: String) { self.init(stringValue) }
    init?(intValue: Int) { return nil }
}

private extension KeyedDecodingContainer where Key == NativeUIKey {
    func keys(_ allowed: Set<String>, optional: Set<String> = []) throws {
        let actual = Set(allKeys.map(\.stringValue))
        guard actual.isSubset(of: allowed), allowed.subtracting(optional).isSubset(of: actual) else {
            throw NativeUIFailure.invalid
        }
    }
    func string(_ key: String, limit: Int = 256) throws -> String {
        let value = try decode(String.self, forKey: .init(key))
        guard !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, value.unicodeScalars.count <= limit else {
            throw NativeUIFailure.invalid
        }
        return value
    }
    func strings(_ key: String, count: ClosedRange<Int> = 1...20, limit: Int = 256) throws -> [String] {
        let values = try decode([String].self, forKey: .init(key))
        guard count.contains(values.count), values.allSatisfy({ !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && $0.unicodeScalars.count <= limit }) else {
            throw NativeUIFailure.invalid
        }
        return values
    }
    func optionalString(_ key: String, limit: Int) throws -> String? {
        guard contains(.init(key)) else { return nil }
        return try string(key, limit: limit)
    }
    func identifier(_ key: String) throws -> String {
        let value = try string(key, limit: 64)
        guard value.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 95 }) else {
            throw NativeUIFailure.invalid
        }
        return value
    }
}

enum NativeUIFailure: Error { case invalid, unsupported, oversized }

struct NativeUIEntry: Decodable, Equatable, Sendable {
    let label: String
    let value: String
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["label", "value"])
        label = try c.string("label", limit: 120)
        value = try c.string("value", limit: 512)
    }
}

struct NativeUIBar: Decodable, Equatable, Sendable {
    let label: String
    let value: Double
    let display: String
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["label", "value"])
        label = try c.string("label", limit: 120)
        value = try c.decode(Double.self, forKey: .init("value"))
        guard value.isFinite, (0...1_000_000_000).contains(value) else { throw NativeUIFailure.invalid }
        display = String(format: "%.6g", locale: Locale(identifier: "en_US_POSIX"), value)
    }
}

struct NativeUIField: Decodable, Equatable, Identifiable, Sendable {
    enum Kind: String, Decodable, Sendable { case text, toggle, select }
    let id: String
    let label: String
    let kind: Kind
    let required: Bool
    let options: [String]
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        id = try c.identifier("id")
        label = try c.string("label", limit: 120)
        kind = try c.decode(Kind.self, forKey: .init("kind"))
        switch kind {
        case .text:
            try c.keys(["id", "label", "kind", "required"])
            required = try c.decode(Bool.self, forKey: .init("required"))
            options = []
        case .toggle:
            try c.keys(["id", "label", "kind"])
            required = false; options = []
        case .select:
            try c.keys(["id", "label", "kind", "options"])
            required = false
            options = try c.strings("options", count: 1...12, limit: 120)
            guard Set(options).count == options.count else { throw NativeUIFailure.invalid }
        }
    }
    var initialValue: String { kind == .toggle ? "false" : options.first ?? "" }
}

/// A meaning, never a colour: the reader's theme decides how each looks
/// (`NativeUITheme`). `series1`…`series6` tell chart series and mockup areas apart.
enum NativeUITone: String, Decodable, Sendable, CaseIterable {
    case neutral, accent, success, warning, error, info, series1, series2, series3, series4, series5, series6
}

struct NativeUIStat: Decodable, Equatable, Sendable {
    let label: String
    let value: String
    let delta: String?
    let tone: NativeUITone
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["label", "value", "delta", "tone"], optional: ["delta", "tone"])
        label = try c.string("label", limit: 120)
        value = try c.string("value", limit: 40)
        delta = try c.optionalString("delta", limit: 40)
        tone = try c.decodeIfPresent(NativeUITone.self, forKey: .init("tone")) ?? .neutral
    }
}

struct NativeUISeries: Decodable, Equatable, Sendable {
    let name: String
    let values: [Double]
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["name", "values"])
        name = try c.string("name", limit: 60)
        values = try c.decode([Double].self, forKey: .init("values"))
        guard values.allSatisfy({ $0.isFinite && abs($0) <= 1_000_000_000 }) else { throw NativeUIFailure.invalid }
    }
}

struct NativeUIStep: Decodable, Equatable, Sendable {
    enum Status: String, Decodable, Sendable { case done, current, pending, failed }
    let title: String
    let detail: String?
    let status: Status
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["title", "detail", "status"], optional: ["detail"])
        title = try c.string("title", limit: 120)
        detail = try c.optionalString("detail", limit: 512)
        status = try c.decode(Status.self, forKey: .init("status"))
    }
}

struct NativeUICheck: Decodable, Equatable, Sendable {
    let text: String
    let done: Bool
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["text", "done"])
        text = try c.string("text", limit: 256)
        done = try c.decode(Bool.self, forKey: .init("done"))
    }
}

struct NativeUIBadge: Decodable, Equatable, Sendable {
    let text: String
    let tone: NativeUITone
    init(text: String, tone: NativeUITone) { self.text = text; self.tone = tone }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        try c.keys(["text", "tone"], optional: ["tone"])
        text = try c.string("text", limit: 60)
        tone = try c.decodeIfPresent(NativeUITone.self, forKey: .init("tone")) ?? .neutral
    }
}

/// A sketch of a screen layout: a frame of a given shape split into areas
/// side by side or stacked, in proportion to their weights, optionally with a
/// card or a full cover over it and a status badge. Areas are a titled pane,
/// a chat (agent and user bubbles over a composer), placeholder lines (a
/// sidebar) or a divider with an optional control glyph.
struct NativeUIMockup: Equatable, Sendable {
    enum Direction: String, Decodable, Sendable { case horizontal, vertical }
    struct Region: Decodable, Equatable, Sendable {
        enum Kind: String, Decodable, Sendable { case pane, chat, lines, divider }
        let kind: Kind
        let title: String?
        let label: String?
        let tone: NativeUITone
        let weight: Double
        /// Chat bubbles in order; true is the user's.
        let bubbles: [Bool]
        let composer: Bool
        let count: Int
        let glyph: String?
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: NativeUIKey.self)
            kind = try c.decode(Kind.self, forKey: .init("kind"))
            let optional: Set<String> = ["weight"]
            switch kind {
            case .pane: try c.keys(["kind", "title", "label", "tone", "weight"], optional: optional.union(["title", "label", "tone"]))
            case .chat: try c.keys(["kind", "bubbles", "composer", "weight"], optional: optional.union(["composer"]))
            case .lines: try c.keys(["kind", "count", "weight"], optional: optional)
            case .divider: try c.keys(["kind", "glyph"], optional: ["glyph"])
            }
            title = try c.optionalString("title", limit: 60)
            label = try c.optionalString("label", limit: 60)
            tone = try c.decodeIfPresent(NativeUITone.self, forKey: .init("tone")) ?? .neutral
            weight = try c.decodeIfPresent(Double.self, forKey: .init("weight")) ?? 1
            guard weight.isFinite, (0.05...10).contains(weight) else { throw NativeUIFailure.invalid }
            let roles = try c.decodeIfPresent([String].self, forKey: .init("bubbles")) ?? []
            guard roles.count <= 8, roles.allSatisfy({ $0 == "agent" || $0 == "user" }) else { throw NativeUIFailure.invalid }
            bubbles = roles.map { $0 == "user" }
            composer = try c.decodeIfPresent(Bool.self, forKey: .init("composer")) ?? (kind == .chat)
            count = try c.decodeIfPresent(Int.self, forKey: .init("count")) ?? 0
            guard kind != .lines || (1...8).contains(count) else { throw NativeUIFailure.invalid }
            glyph = try c.optionalString("glyph", limit: 3)
            guard kind != .pane || title != nil || label != nil else { throw NativeUIFailure.invalid }
        }
    }
    struct Overlay: Decodable, Equatable, Sendable {
        enum Style: String, Decodable, Sendable { case sheet, full }
        let style: Style
        let dim: Bool
        let region: Region
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: NativeUIKey.self)
            try c.keys(["style", "dim", "region"], optional: ["dim"])
            style = try c.decode(Style.self, forKey: .init("style"))
            dim = try c.decodeIfPresent(Bool.self, forKey: .init("dim")) ?? false
            region = try c.decode(Region.self, forKey: .init("region"))
            guard region.kind == .pane || region.kind == .chat else { throw NativeUIFailure.invalid }
        }
    }
    let aspect: Double
    let direction: Direction
    let regions: [Region]
    let overlay: Overlay?
    let badge: NativeUIBadge?
    let caption: String?
}

struct NativeUIComponent: Decodable, Identifiable, Equatable, Sendable {
    enum Content: Equatable, Sendable {
        case card(String)
        case table(columns: [String], rows: [[String]])
        case list([String])
        case keyValue([NativeUIEntry])
        case chart(bars: [NativeUIBar], maximum: Double)
        /// Categories along one axis, one to six series of values for each.
        case plot(kind: PlotKind, labels: [String], series: [NativeUISeries])
        case progress(value: Double, label: String, display: String)
        case button(message: String)
        case form(fields: [NativeUIField], submit: String, message: String)
        case diagram(steps: [String])
        case callout(String, tone: NativeUITone)
        case stats([NativeUIStat])
        case steps([NativeUIStep])
        case checklist([NativeUICheck])
        case badges([NativeUIBadge])
        case mockup(NativeUIMockup)
    }
    enum PlotKind: String, Decodable, Sendable { case bar, line, area }
    let id: String
    let type: String
    let title: String
    let content: Content
    /// A table's cells with their column widths measured, prepared while
    /// decoding (on the cache actor) rather than when drawn.
    private(set) var table: NativeTable?

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: NativeUIKey.self)
        id = try c.identifier("id")
        type = try c.string("type", limit: 20)
        title = try c.string("title", limit: 120)
        let common: Set<String> = ["id", "type", "title"]
        switch type {
        case "card":
            try c.keys(common.union(["body"]))
            content = .card(try c.string("body", limit: 2_000))
        case "table":
            try c.keys(common.union(["columns", "rows"]))
            let columns = try c.strings("columns", count: 1...8, limit: 120)
            let rows = try c.decode([[String]].self, forKey: .init("rows"))
            guard (1...20).contains(rows.count), rows.allSatisfy({ row in
                row.count == columns.count && row.allSatisfy { $0.unicodeScalars.count <= 256 }
            }) else { throw NativeUIFailure.invalid }
            content = .table(columns: columns, rows: rows)
            table = NativeTable(columns: columns, rows: rows)
        case "list":
            try c.keys(common.union(["items"]))
            content = .list(try c.strings("items"))
        case "keyValue":
            try c.keys(common.union(["entries"]))
            let entries = try c.decode([NativeUIEntry].self, forKey: .init("entries"))
            guard (1...20).contains(entries.count) else { throw NativeUIFailure.invalid }
            content = .keyValue(entries)
        case "chart" where c.contains(.init("bars")):
            try c.keys(common.union(["bars"]))
            let bars = try c.decode([NativeUIBar].self, forKey: .init("bars"))
            guard (1...12).contains(bars.count) else { throw NativeUIFailure.invalid }
            content = .chart(bars: bars, maximum: max(1, bars.map(\.value).max() ?? 1))
        case "chart":
            try c.keys(common.union(["kind", "labels", "series"]), optional: ["kind"])
            let kind = try c.decodeIfPresent(PlotKind.self, forKey: .init("kind")) ?? .bar
            let labels = try c.strings("labels", count: 1...24, limit: 60)
            let series = try c.decode([NativeUISeries].self, forKey: .init("series"))
            guard (1...6).contains(series.count), Set(series.map(\.name)).count == series.count,
                  Set(labels).count == labels.count,
                  series.allSatisfy({ $0.values.count == labels.count }) else { throw NativeUIFailure.invalid }
            content = .plot(kind: kind, labels: labels, series: series)
        case "callout":
            try c.keys(common.union(["body", "tone"]), optional: ["tone"])
            content = .callout(try c.string("body", limit: 2_000),
                               tone: try c.decodeIfPresent(NativeUITone.self, forKey: .init("tone")) ?? .info)
        case "stats":
            try c.keys(common.union(["items"]))
            let items = try c.decode([NativeUIStat].self, forKey: .init("items"))
            guard (1...8).contains(items.count) else { throw NativeUIFailure.invalid }
            content = .stats(items)
        case "steps":
            try c.keys(common.union(["steps"]))
            let steps = try c.decode([NativeUIStep].self, forKey: .init("steps"))
            guard (1...12).contains(steps.count) else { throw NativeUIFailure.invalid }
            content = .steps(steps)
        case "checklist":
            try c.keys(common.union(["items"]))
            let items = try c.decode([NativeUICheck].self, forKey: .init("items"))
            guard (1...20).contains(items.count) else { throw NativeUIFailure.invalid }
            content = .checklist(items)
        case "badges":
            try c.keys(common.union(["items"]))
            let items = try c.decode([NativeUIBadge].self, forKey: .init("items"))
            guard (1...12).contains(items.count) else { throw NativeUIFailure.invalid }
            content = .badges(items)
        case "mockup":
            try c.keys(common.union(["aspect", "direction", "regions", "overlay", "badge", "caption"]),
                       optional: ["direction", "overlay", "badge", "caption"])
            let aspect = try c.decode(Double.self, forKey: .init("aspect"))
            let regions = try c.decode([NativeUIMockup.Region].self, forKey: .init("regions"))
            guard aspect.isFinite, (0.25...4).contains(aspect), (1...6).contains(regions.count),
                  regions.contains(where: { $0.kind != .divider }) else { throw NativeUIFailure.invalid }
            content = .mockup(NativeUIMockup(
                aspect: aspect,
                direction: try c.decodeIfPresent(NativeUIMockup.Direction.self, forKey: .init("direction")) ?? .horizontal,
                regions: regions,
                overlay: try c.decodeIfPresent(NativeUIMockup.Overlay.self, forKey: .init("overlay")),
                badge: try c.decodeIfPresent(NativeUIBadge.self, forKey: .init("badge")),
                caption: try c.optionalString("caption", limit: 500)))
        case "progress":
            try c.keys(common.union(["value", "label"]))
            let value = try c.decode(Double.self, forKey: .init("value"))
            guard value.isFinite, (0...1).contains(value) else { throw NativeUIFailure.invalid }
            content = .progress(value: value, label: try c.string("label"), display: "\(Int((value * 100).rounded()))%")
        case "button":
            try c.keys(common.union(["message"]))
            content = .button(message: try c.string("message", limit: 2_000))
        case "form":
            try c.keys(common.union(["fields", "submit", "message"]))
            let fields = try c.decode([NativeUIField].self, forKey: .init("fields"))
            guard (1...8).contains(fields.count), Set(fields.map(\.id)).count == fields.count else { throw NativeUIFailure.invalid }
            content = .form(fields: fields, submit: try c.string("submit", limit: 120), message: try c.string("message", limit: 2_000))
        case "diagram":
            try c.keys(common.union(["steps"]))
            content = .diagram(steps: try c.strings("steps", count: 2...12))
        default: throw NativeUIFailure.unsupported
        }
    }

    /// A normal user message, never a tool invocation or executable template.
    func draft(values: [String: String] = [:]) -> String? {
        switch content {
        case .button(let message): return message
        case .form(let fields, _, let message):
            var lines = [message, ""]
            for field in fields {
                let value = values[field.id] ?? field.initialValue
                guard value.count <= 512,
                      !field.required || !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                      field.kind != .select || field.options.contains(value),
                      field.kind != .toggle || ["true", "false"].contains(value) else { return nil }
                lines.append(field.label + ": " + (field.kind == .toggle ? (value == "true" ? "Yes" : "No") : value))
            }
            return lines.joined(separator: "\n")
        default: return nil
        }
    }

    /// Count visible data items before constructing views. A table counts each
    /// cell; other collections count their entries/fields/nodes.
    var displayItemCount: Int {
        switch content {
        case .card, .progress, .button: 1
        case .table(let columns, let rows): columns.count * rows.count
        case .list(let items): items.count
        case .keyValue(let entries): entries.count
        case .chart(let bars, _): bars.count
        case .form(let fields, _, _): fields.count
        case .diagram(let steps): steps.count
        case .plot(_, let labels, let series): labels.count * series.count
        case .callout: 1
        case .stats(let items): items.count
        case .steps(let steps): steps.count
        case .checklist(let items): items.count
        case .badges(let items): items.count
        case .mockup(let mockup): mockup.regions.count + (mockup.overlay == nil ? 0 : 1) + (mockup.badge == nil ? 0 : 1)
        }
    }
}

struct NativeReplyProjection: Equatable, Sendable {
    enum Piece: Equatable, Sendable {
        case markdown(String)
        case interface(NativeUIDocument)
        case unavailable(summary: String, source: String)
    }
    let pieces: [Piece]

    static func prepare(_ source: String) -> Self {
        guard source.contains("```wonder-ui") else {
            return Self(pieces: [.markdown(source)])
        }
        guard source.utf8.count <= 131_072 else {
            return Self(pieces: [.unavailable(summary: "Interactive reply is too large to display. Copy the message to read its full text.",
                                               source: String(source.prefix(4_000)))])
        }
        let lines = source.components(separatedBy: "\n")
        var pieces: [Piece] = [], ordinary: [String] = [], payload: [String]? = nil
        var opening = ""
        var otherFence: (Character, Int)?
        var count = 0
        var componentCount = 0, itemCount = 0
        for (lineIndex, line) in lines.enumerated() {
            let trim = line.trimmingCharacters(in: .whitespacesAndNewlines)
            let leading = line.prefix(while: { $0 == " " || $0 == "\t" })
            let fenceIndent = leading.count <= 3 && !leading.contains("\t")
            if payload != nil {
                if fenceIndent, trim == "```" {
                    count += 1
                    let json = payload!.joined(separator: "\n")
                    do {
                        let document = try NativeUIDocument.decode(json)
                        guard componentCount + document.components.count <= 24,
                              itemCount + document.displayItemCount <= 80 else { throw NativeUIFailure.oversized }
                        componentCount += document.components.count
                        itemCount += document.displayItemCount
                        pieces.append(.interface(document))
                    } catch {
                        let fallback: String?
                        if json.utf8.count <= 32_768,
                           let object = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any],
                           let value = object["fallback"] as? String,
                           !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, value.unicodeScalars.count <= 4_000 {
                            fallback = value
                        } else { fallback = nil }
                        pieces.append(.unavailable(summary: fallback ?? "Interactive reply unavailable. The original reply text is below.",
                                                   source: String(json.prefix(4_000))))
                    }
                    payload = nil
                } else { payload?.append(line) }
            } else if let fence = otherFence {
                ordinary.append(line)
                if fenceIndent, trim.prefix(while: { $0 == fence.0 }).count >= fence.1,
                   trim.allSatisfy({ $0 == fence.0 }) { otherFence = nil }
            } else if fenceIndent, trim == "```wonder-ui" {
                if !ordinary.isEmpty { pieces.append(.markdown(ordinary.joined(separator: "\n"))); ordinary = [] }
                guard count < 4 else {
                    // A fifth block is one bounded text fallback, including an
                    // unfinished block. Never build thousands of fallback views.
                    pieces.append(.unavailable(summary: "More interactive content is available in the full message. Copy the message to read it.",
                        source: String(lines[lineIndex...].joined(separator: "\n").prefix(4_000))))
                    return Self(pieces: pieces)
                }
                payload = []
                opening = line
            } else {
                ordinary.append(line)
                if fenceIndent, let char = trim.first, char == "`" || char == "~" {
                    let length = trim.prefix(while: { $0 == char }).count
                    if length >= 3 { otherFence = (char, length) }
                }
            }
        }
        // Streaming/incomplete fences remain text and never expose live controls.
        if let payload { ordinary.append(opening); ordinary.append(contentsOf: payload) }
        if !ordinary.isEmpty { pieces.append(.markdown(ordinary.joined(separator: "\n"))) }
        return Self(pieces: pieces)
    }
}

/// Swift String equality normalizes Unicode. Task identity must compare the
/// actual UTF-8 instead, without allocating a byte buffer in the view body.
struct NativeReplyRevision: Equatable {
    let source: String
    let bytes: Int
    init(_ source: String) { self.source = source; bytes = source.utf8.count }
    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.bytes == rhs.bytes && lhs.source.utf8.elementsEqual(rhs.source.utf8)
    }
}

/// Retains prepared content while a newer revision is prepared off MainActor.
/// Completion is fenced by request identity as well as task cancellation.
struct NativeReplyPreparation {
    private(set) var revision: NativeReplyRevision?
    private(set) var prepared: (revision: NativeReplyRevision, value: NativeReplyProjection)?
    var isCurrent: Bool { prepared != nil && prepared?.revision == revision }
    var projection: NativeReplyProjection? { prepared?.value }

    mutating func begin(_ revision: NativeReplyRevision) { self.revision = revision }
    mutating func finish(_ value: NativeReplyProjection, for revision: NativeReplyRevision) {
        guard self.revision == revision else { return }
        prepared = (revision, value)
    }
}

/// Shared by the real conversation row and the host regression checks.
struct NativeReplyBody<Markdown: View>: View {
    let preparation: NativeReplyPreparation
    var requestedRevision: NativeReplyRevision? = nil
    var onDraft: ((String) -> String?)? = nil
    var theme = NativeUITheme()
    var fonts = NativeUIFonts()
    @ViewBuilder var markdown: (String) -> Markdown

    var body: some View {
        let current = preparation.isCurrent && (requestedRevision == nil || preparation.revision == requestedRevision)
        if let projection = preparation.projection {
            VStack(alignment: .leading, spacing: 12) {
                ForEach(projection.pieces.indices, id: \.self) { i in
                    switch projection.pieces[i] {
                    case .markdown(let source): markdown(source)
                    case .interface(let document):
                        NativeUIDocumentView(document: document, onDraft: current ? onDraft : nil,
                                             theme: theme, fonts: fonts)
                            .disabled(!current)
                    case .unavailable(let summary, let source):
                        VStack(alignment: .leading, spacing: 8) {
                            Text(summary).textSelection(.enabled)
                            DisclosureGroup("Show reply text") {
                                Text(source).font(.caption.monospaced()).textSelection(.enabled)
                            }
                        }.accessibilityIdentifier("genui-fallback")
                    }
                }
            }
        } else {
            ProgressView("Preparing reply").accessibilityIdentifier("native-reply-preparing")
        }
    }
}

/// Exact UTF-8 is the cache key. Actor isolation keeps scanning and decoding off
/// MainActor. Both retained input bytes and entry count are bounded.
actor NativeReplyCache {
    static let shared = NativeReplyCache()
    private var entries: [Data: NativeReplyProjection] = [:]
    private var order: [Data] = []
    private(set) var preparations = 0
    private(set) var bytes = 0
    func projection(for source: String) -> NativeReplyProjection {
        guard source.utf8.count <= 131_072 else {
            preparations += 1
            return NativeReplyProjection.prepare(source)
        }
        let key = Data(source.utf8)
        if let hit = entries[key] { return hit }
        preparations += 1
        let value = NativeReplyProjection.prepare(source)
        while !order.isEmpty && (order.count >= 48 || bytes + key.count > 1_048_576) {
            let old = order.removeFirst(); entries.removeValue(forKey: old); bytes -= old.count
        }
        entries[key] = value; order.append(key); bytes += key.count
        return value
    }
    func removeAll() { entries.removeAll(); order.removeAll(); bytes = 0 }
    var count: Int { entries.count }
}

/// The reader's theme as the catalog needs it. The app fills it from its
/// palette (status colours derived with contrast checks on the Mac side of the
/// package, `ThemePalette.status`); the defaults are system colours, for the
/// host snapshot package.
struct NativeUITheme: Equatable {
    /// Cards, tiles and code behind content that sits on a message.
    var surface: Color = .secondary.opacity(0.08)
    /// The opaque page under a message, for cards drawn over other content.
    #if canImport(UIKit)
    var background: Color = Color(uiColor: .systemBackground)
    #else
    var background: Color = Color(nsColor: .windowBackgroundColor)
    #endif
    var separator: Color = .secondary.opacity(0.35)
    var secondaryText: Color = .secondary
    var accent: Color = .accentColor
    var success: Color = .green
    var warning: Color = .orange
    var error: Color = .red
    var info: Color = .blue
    /// Chart series and mockup areas, in order; `series1` is the accent.
    var series: [Color] = [.accentColor, .teal, .orange, .purple, .pink, .green]

    func color(_ tone: NativeUITone) -> Color {
        switch tone {
        case .neutral: secondaryText
        case .accent: accent
        case .success: success
        case .warning: warning
        case .error: error
        case .info: info
        case .series1: series[0]
        case .series2: series[1 % series.count]
        case .series3: series[2 % series.count]
        case .series4: series[3 % series.count]
        case .series5: series[4 % series.count]
        case .series6: series[5 % series.count]
        }
    }

    static func symbol(_ tone: NativeUITone) -> String {
        switch tone {
        case .success: "checkmark.circle.fill"
        case .warning: "exclamationmark.triangle.fill"
        case .error: "xmark.octagon.fill"
        default: "info.circle.fill"
        }
    }

    /// Spoken with a status so its meaning never rests on colour alone.
    static func spoken(_ tone: NativeUITone) -> String? {
        switch tone {
        case .success: "Success"
        case .warning: "Warning"
        case .error: "Problem"
        case .info: "Note"
        default: nil
        }
    }
}

/// Shared verbatim by iOS and the host snapshot package. All input has already
/// been validated and prepared; body only lays out a small, fixed catalog.
struct NativeUIFonts {
    var body: Font = .body
    var heading: Font = .headline
    var caption: Font = .footnote
    var title: Font = .title2
    var section: Font = .title3
    /// Table cells; Markdown, `wonder-ui` and page tables share it.
    var table: Font = .subheadline
}

/// Control injection lets the host pixel suite cover the same field layout
/// without ImageRenderer's unsupported AppKit views. iOS always uses native.
struct NativeUIControls {
    // Keep identity on the interactive control, not its inactive labeled row.
    var input: @MainActor (NativeUIField, Binding<String>, NativeUIFonts, String) -> AnyView = { field, binding, fonts, identifier in
        switch field.kind {
        case .text:
            AnyView(TextField("Your answer", text: binding).font(fonts.body).textFieldStyle(.roundedBorder)
                .frame(minHeight: 44)
                .accessibilityLabel(field.label + (field.required ? ", required" : ""))
                .accessibilityIdentifier(identifier))
        case .toggle:
            AnyView(Toggle("", isOn: Binding(get: { binding.wrappedValue == "true" },
                                           set: { binding.wrappedValue = $0 ? "true" : "false" }))
                .labelsHidden().font(fonts.body).frame(minHeight: 44)
                .accessibilityLabel(field.label).accessibilityIdentifier(identifier))
        case .select:
            AnyView(Picker("", selection: binding) {
                ForEach(field.options, id: \.self) { Text($0).tag($0) }
            }.pickerStyle(.menu).labelsHidden().font(fonts.body).frame(minHeight: 44)
                .accessibilityLabel(field.label).accessibilityValue(binding.wrappedValue)
                .accessibilityIdentifier(identifier))
        }
    }
}

private struct NativeUIBarTrack: View {
    let value: Double
    let total: Double
    var color: Color = .accentColor
    var body: some View {
        GeometryReader { geometry in
            Capsule().fill(.secondary.opacity(0.15))
            Capsule().fill(color).frame(width: geometry.size.width * (value / total))
        }.frame(height: 8).accessibilityHidden(true)
    }
}

struct NativeUIDocumentView: View {
    let document: NativeUIDocument
    var onDraft: ((String) -> String?)? = nil
    var theme = NativeUITheme()
    var fonts = NativeUIFonts()
    var controls = NativeUIControls()
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            ForEach(document.components) { component in
                NativeUIComponentView(component: component, onDraft: onDraft, theme: theme, fonts: fonts, controls: controls)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .font(fonts.body)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("native-reply-ui")
    }
}

private struct NativeUIComponentView: View {
    let component: NativeUIComponent
    let onDraft: ((String) -> String?)?
    let theme: NativeUITheme
    let fonts: NativeUIFonts
    let controls: NativeUIControls
    @Environment(\.dynamicTypeSize) private var typeSize
    @State private var values: [String: String] = [:]
    @State private var feedback: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if component.type != "button", component.type != "mockup" {
                Text(component.title).font(fonts.heading).accessibilityAddTraits(.isHeader)
                    .accessibilityIdentifier("genui-" + component.id)
            }
            content
            if let feedback {
                Text(feedback).font(fonts.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("genui-feedback-" + component.id)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .onChange(of: component) { _, _ in values = [:]; feedback = nil }
    }

    @ViewBuilder private var content: some View {
        switch component.content {
        case .card(let body):
            Text(body).textSelection(.enabled)
        case .table:
            if let table = component.table {
                NativeTableView(table: table, theme: theme, font: fonts.table)
                    .accessibilityIdentifier("genui-table-" + component.id)
            }
        case .list(let items):
            VStack(alignment: .leading, spacing: 8) {
                ForEach(items.indices, id: \.self) { i in
                    Label { Text(items[i]).textSelection(.enabled) } icon: {
                        Image(systemName: "circle.fill").font(.system(size: 5)).accessibilityHidden(true)
                    }
                }
            }
        case .keyValue(let entries):
            VStack(alignment: .leading, spacing: 8) {
                ForEach(entries.indices, id: \.self) { i in pair(entries[i].label, entries[i].value) }
            }
        case .chart(let bars, let maximum):
            VStack(alignment: .leading, spacing: 10) {
                ForEach(bars.indices, id: \.self) { i in
                    VStack(alignment: .leading, spacing: 4) {
                        pair(bars[i].label, bars[i].display)
                        NativeUIBarTrack(value: bars[i].value, total: maximum, color: theme.accent)
                    }
                }
            }
        case .progress(let value, let label, let display):
            VStack(alignment: .leading, spacing: 6) {
                pair(label, display)
                NativeUIBarTrack(value: value, total: 1, color: theme.accent)
            }.accessibilityElement(children: .combine)
        case .button:
            draftButton(component.title, draft: component.draft())
        case .form(let fields, let submit, _):
            VStack(alignment: .leading, spacing: 12) {
                ForEach(fields) { field in fieldView(field) }
                draftButton(submit, draft: component.draft(values: values))
                Text("Review your draft before sending.").font(fonts.caption).foregroundStyle(.secondary)
            }
        case .diagram(let steps):
            VStack(alignment: .leading, spacing: 6) {
                ForEach(steps.indices, id: \.self) { i in
                    if i > 0 {
                        Image(systemName: "arrow.down").foregroundStyle(.secondary).padding(.leading, 16)
                            .accessibilityLabel("then")
                    }
                    Text(steps[i]).padding(10).frame(maxWidth: .infinity, alignment: .leading)
                        .background(theme.surface, in: RoundedRectangle(cornerRadius: 8))
                }
            }
        case .plot(let kind, let labels, let series):
            NativeUIPlot(kind: kind, labels: labels, series: series, theme: theme, fonts: fonts)
        case .callout(let body, let tone):
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: NativeUITheme.symbol(tone)).foregroundStyle(theme.color(tone)).accessibilityHidden(true)
                Text(body).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(12)
            .background(theme.color(tone).opacity(0.12), in: RoundedRectangle(cornerRadius: 8))
            .accessibilityElement(children: .combine)
            .accessibilityLabel([NativeUITheme.spoken(tone), body].compactMap { $0 }.joined(separator: ": "))
        case .stats(let items):
            NativeUIAdaptiveGrid(minimum: typeSize.isAccessibilitySize ? 260 : 140, spacing: 8) {
                ForEach(items.indices, id: \.self) { i in statTile(items[i]) }
            }
        case .steps(let steps):
            VStack(alignment: .leading, spacing: 0) {
                ForEach(steps.indices, id: \.self) { i in stepRow(steps[i], last: i == steps.count - 1) }
            }
        case .checklist(let items):
            VStack(alignment: .leading, spacing: 8) {
                ForEach(items.indices, id: \.self) { i in
                    Label {
                        Text(items[i].text).textSelection(.enabled)
                            .foregroundStyle(items[i].done ? theme.secondaryText : Color.primary)
                    } icon: {
                        Image(systemName: items[i].done ? "checkmark.square.fill" : "square")
                            .foregroundStyle(items[i].done ? theme.success : theme.secondaryText)
                    }
                    .accessibilityElement(children: .combine)
                    .accessibilityValue(items[i].done ? "Done" : "Not done")
                }
            }
        case .badges(let items):
            NativeUIFlow(spacing: 6) {
                ForEach(items.indices, id: \.self) { i in NativeUIBadgeView(badge: items[i], theme: theme, font: fonts.caption) }
            }
        case .mockup(let mockup):
            NativeUIMockupView(mockup: mockup, theme: theme, fonts: fonts)
        }
    }

    private func statTile(_ stat: NativeUIStat) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(stat.value).font(fonts.section).fontWeight(.semibold)
                .foregroundStyle(stat.tone == .neutral ? Color.primary : theme.color(stat.tone))
            Text(stat.label).font(fonts.caption).foregroundStyle(theme.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            if let delta = stat.delta {
                Text(delta).font(fonts.caption).fontWeight(.medium)
                    .foregroundStyle(stat.tone == .neutral ? theme.secondaryText : theme.color(stat.tone))
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(theme.background, in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .combine)
    }

    private func stepRow(_ step: NativeUIStep, last: Bool) -> some View {
        let (symbol, color, spoken): (String, Color, String) = switch step.status {
        case .done: ("checkmark.circle.fill", theme.success, "Done")
        case .current: ("circle.inset.filled", theme.accent, "In progress")
        case .pending: ("circle", theme.secondaryText, "Not started")
        case .failed: ("xmark.circle.fill", theme.error, "Failed")
        }
        // The connector runs the row's own height, set by its text.
        return HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol).foregroundStyle(color).font(fonts.body)
                .frame(maxHeight: .infinity, alignment: .top)
                .background(alignment: .top) {
                    if !last {
                        Rectangle().fill(theme.separator).frame(width: 2).padding(.top, 22).padding(.bottom, -2)
                    }
                }
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(step.title).fontWeight(step.status == .current ? .semibold : nil)
                if let detail = step.detail {
                    Text(detail).font(fonts.caption).foregroundStyle(theme.secondaryText)
                }
            }
            .padding(.bottom, last ? 0 : 10)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .fixedSize(horizontal: false, vertical: true)
        .accessibilityElement(children: .combine)
        .accessibilityValue(spoken)
    }

    @ViewBuilder private func pair(_ label: String, _ value: String) -> some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                Text(label).foregroundStyle(.secondary)
                Spacer(minLength: 0)
                Text(value).textSelection(.enabled)
            }.fixedSize(horizontal: true, vertical: false)
            VStack(alignment: .leading, spacing: 2) {
                Text(label).foregroundStyle(.secondary)
                Text(value).textSelection(.enabled)
            }
        }.accessibilityElement(children: .combine)
    }

    private func draftButton(_ title: String, draft: String?) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Button {
                guard let draft, let onDraft else { return }
                feedback = onDraft(draft) ?? "Added to your draft. Review and send."
            } label: {
                Text(title).font(fonts.body).lineLimit(nil).multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true).frame(minHeight: 44).frame(maxWidth: .infinity)
            }
            .buttonStyle(.bordered)
            .disabled(onDraft == nil || draft == nil)
            .accessibilityHint("Adds a message to the composer for you to review and send.")
            .accessibilityIdentifier("genui-action-" + component.id)
            if onDraft == nil {
                Text("Actions unavailable in this conversation.").font(fonts.caption).foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder private func fieldView(_ field: NativeUIField) -> some View {
        let binding = Binding(get: { values[field.id] ?? field.initialValue },
                              set: { values[field.id] = String($0.prefix(512)); feedback = nil })
        Group {
            if field.kind == .text {
                VStack(alignment: .leading, spacing: 4) {
                    Text(field.label + (field.required ? " (required)" : "")).font(fonts.body)
                    input(field, binding: binding)
                }
            } else if typeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: 6) {
                    fieldLabel(field)
                    input(field, binding: binding)
                }
            } else {
                HStack(spacing: 12) {
                    fieldLabel(field)
                    Spacer(minLength: 0)
                    input(field, binding: binding)
                }
            }
        }
    }

    private func fieldLabel(_ field: NativeUIField) -> some View {
        Text(field.label).font(fonts.body).fixedSize(horizontal: false, vertical: true)
            .accessibilityHidden(true)
    }

    private func input(_ field: NativeUIField, binding: Binding<String>) -> some View {
        controls.input(field, binding, fonts, "genui-field-" + component.id + "-" + field.id)
    }
}

// MARK: - Tables

/// A table ready to draw: its cells, each column's width in ems and what
/// VoiceOver reads for each row, measured once when the text changes (for
/// replies and pages on their cache actor). Markdown tables, `wonder-ui`
/// tables and converted HTML tables all draw with `NativeTableView`.
struct NativeTable: Equatable, Sendable {
    struct Column: Equatable, Sendable {
        /// Wide enough for the longest word, and for the longest cell in
        /// three lines (with room for wrapping), within bounds.
        var minimum: Double
        /// The longest cell on one line, within bounds.
        var preferred: Double
    }
    let header: [AttributedString]
    let rows: [[AttributedString]]
    let columns: [Column]
    /// Cells that may run past three lines at the column's narrowest; a tap
    /// shows their whole text.
    let long: [[Bool]]
    /// One line per row with its column names: "Tool: Computer, Today: …".
    let spoken: [String]

    static let minimumWidth = 3.0, minimumCap = 9.0, preferredCap = 18.0
    static let lines = 3

    init(header: [AttributedString], rows: [[AttributedString]]) {
        let width = max(header.count, rows.map(\.count).max() ?? 0)
        func pad(_ row: [AttributedString]) -> [AttributedString] {
            (0..<width).map { $0 < row.count ? row[$0] : AttributedString() }
        }
        self.header = pad(header)
        self.rows = rows.map(pad)
        let names = self.header.map { String($0.characters) }
        let plain = self.rows.map { $0.map { String($0.characters) } }
        let columns = (0..<width).map { col -> Column in
            let cells = [names[col]] + plain.map { $0[col] }
            let word = cells.flatMap { $0.split(whereSeparator: \.isWhitespace) }.map { Self.ems($0) }.max() ?? 0
            let natural = cells.map { Self.ems($0[...]) }.max() ?? 0
            let minimum = min(Self.minimumCap, max(Self.minimumWidth, word + 0.6, natural / Double(Self.lines) * 1.15 + 0.5))
            return Column(minimum: minimum, preferred: min(Self.preferredCap, max(minimum, natural + 0.5)))
        }
        self.columns = columns
        long = plain.map { row in
            row.indices.map { Self.ems(row[$0][...]) > columns[$0].minimum * (Double(Self.lines) - 0.4) }
        }
        spoken = plain.map { row in
            row.indices.compactMap { col in
                row[col].isEmpty ? nil : names[col].isEmpty ? row[col] : names[col] + ": " + row[col]
            }.joined(separator: ", ")
        }
    }

    init(columns: [String], rows: [[String]]) {
        self.init(header: columns.map { AttributedString($0) }, rows: rows.map { $0.map { AttributedString($0) } })
    }

    /// An estimate of the text's width in ems for the system font: no font
    /// lookup, so it runs anywhere and costs one pass over the text.
    static func ems(_ text: Substring) -> Double {
        text.unicodeScalars.reduce(0) { total, scalar in
            switch scalar.value {
            case 0x20: total + 0.28
            case 0x41...0x5A, 0x6D, 0x77: total + 0.74
            case 0x21...0x7E: total + 0.58
            case 0x2E80...: total + 1
            default: total + 0.6
            }
        }
    }

    /// Column widths in points for the space offered: each column's preferred
    /// width when it fits, squeezed toward the minimums when not, and the
    /// minimums (the table then scrolls sideways) when even those don't fit.
    static func widths(_ columns: [Column], em: CGFloat, padding: CGFloat, available: CGFloat?) -> [CGFloat] {
        let minimum = columns.map { CGFloat($0.minimum) * em + padding * 2 }
        let preferred = columns.map { CGFloat($0.preferred) * em + padding * 2 }
        guard let available else { return minimum }
        let low = minimum.reduce(0, +), high = preferred.reduce(0, +)
        if available >= high { return preferred }
        if available <= low || high <= low { return minimum }
        let share = (available - low) / (high - low)
        return zip(minimum, preferred).map { $0 + ($1 - $0) * share }
    }
}

/// A grid with a header row: cells wrap to three lines (a tap on a longer one
/// shows all of it) and, when the columns can't fit, the table scrolls
/// sideways with its first column pinned. Plain stacks only, so it can sit
/// in the conversation's lazy stack.
struct NativeTableView: View {
    let table: NativeTable
    var theme = NativeUITheme()
    var font: Font = .subheadline
    @ScaledMetric(relativeTo: .subheadline) private var em: CGFloat = 15
    @Environment(\.pixelLength) private var pixel
    static let padding: CGFloat = 8

    var body: some View {
        // The grid when its narrowest columns fit, otherwise the same grid
        // scrolling sideways: decided in one layout pass, so a row never
        // changes height after it appears.
        ViewThatFits(in: .horizontal) {
            grid(pinned: false)
            ScrollView(.horizontal) { grid(pinned: true) }
                .scrollBounceBehavior(.basedOnSize, axes: .horizontal)
                .accessibilityIdentifier("native-table-scroll")
        }
        .background(theme.surface)
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Table, \(table.rows.count) \(table.rows.count == 1 ? "row" : "rows")")
    }

    private func grid(pinned: Bool) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            row(table.header, long: nil, pinned: pinned)
                .accessibilityHidden(true)
            ForEach(table.rows.indices, id: \.self) { index in
                theme.separator.frame(height: pixel)
                row(table.rows[index], long: table.long[index], pinned: pinned)
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(table.spoken[index])
            }
        }
    }

    /// The first cell comes last so it draws over the cells it is pinned above.
    private func row(_ cells: [AttributedString], long: [Bool]?, pinned: Bool) -> some View {
        NativeTableRowLayout(columns: table.columns, em: em, padding: Self.padding) {
            ForEach(Array(cells.indices.dropFirst()), id: \.self) { col in
                NativeTableCell(text: cells[col], long: long?[col] == true, header: long == nil, theme: theme, font: font)
            }
            if !cells.isEmpty {
                NativeTableCell(text: cells[0], long: long?[0] == true, header: long == nil, theme: theme, font: font)
                    .background(pinned ? theme.surface : .clear)
                    .overlay(alignment: .trailing) { if pinned { theme.separator.frame(width: pixel) } }
                    .visualEffect { content, proxy in
                        content.offset(x: pinned ? max(0, -proxy.frame(in: .scrollView(axis: .horizontal)).minX) : 0)
                    }
            }
        }
    }
}

private struct NativeTableCell: View {
    let text: AttributedString
    let long: Bool
    let header: Bool
    let theme: NativeUITheme
    let font: Font
    @State private var showsAll = false

    var body: some View {
        if long {
            Button { showsAll = true } label: { label.contentShape(Rectangle()) }
                .buttonStyle(.plain)
                .popover(isPresented: $showsAll) {
                    ScrollView {
                        Text(text).font(font).textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading).padding()
                    }
                    .frame(idealWidth: 320, maxWidth: 420, maxHeight: 420)
                    .presentationCompactAdaptation(.popover)
                    .accessibilityIdentifier("native-table-cell-full")
                }
        } else {
            label.textSelection(.enabled)
        }
    }

    private var label: some View {
        Text(text)
            .font(header ? font.weight(.semibold) : font)
            .foregroundStyle(header ? theme.secondaryText : Color.primary)
            .lineLimit(header ? nil : NativeTable.lines)
            .padding(.horizontal, NativeTableView.padding)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}

/// One row: every cell gets its column's width (the same in every row, as
/// they share the offered width) and the row's tallest height. The last
/// subview is the first column.
struct NativeTableRowLayout: Layout {
    let columns: [NativeTable.Column]
    let em: CGFloat
    let padding: CGFloat

    private func column(_ index: Int, of count: Int) -> Int { index == count - 1 ? 0 : index + 1 }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let widths = NativeTable.widths(columns, em: em, padding: padding, available: proposal.width)
        guard subviews.count == widths.count else { return .zero }
        let height = subviews.indices.reduce(CGFloat(0)) { tallest, index in
            max(tallest, subviews[index].sizeThatFits(.init(width: widths[column(index, of: subviews.count)], height: nil)).height)
        }
        return CGSize(width: widths.reduce(0, +), height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let widths = NativeTable.widths(columns, em: em, padding: padding, available: bounds.width)
        guard subviews.count == widths.count else { return }
        var starts: [CGFloat] = [], x = bounds.minX
        for width in widths { starts.append(x); x += width }
        for index in subviews.indices {
            let col = column(index, of: subviews.count)
            subviews[index].place(at: CGPoint(x: starts[col], y: bounds.minY), anchor: .topLeading,
                                  proposal: .init(width: widths[col], height: bounds.height))
        }
    }
}

// MARK: - Charts, badges and layout sketches

/// One to six series over shared categories, drawn with Swift Charts in the
/// theme's series colours. Values were bounded by the decoder.
private struct NativeUIPlot: View {
    let kind: NativeUIComponent.PlotKind
    let labels: [String]
    let series: [NativeUISeries]
    let theme: NativeUITheme
    let fonts: NativeUIFonts

    var body: some View {
        Chart {
            ForEach(series.indices, id: \.self) { s in
                ForEach(labels.indices, id: \.self) { i in
                    switch kind {
                    case .bar:
                        BarMark(x: .value("Category", labels[i]), y: .value("Value", series[s].values[i]))
                            .foregroundStyle(by: .value("Series", series[s].name))
                            .position(by: .value("Series", series[s].name))
                    case .line:
                        LineMark(x: .value("Category", labels[i]), y: .value("Value", series[s].values[i]))
                            .foregroundStyle(by: .value("Series", series[s].name))
                            .symbol(by: .value("Series", series[s].name))
                    case .area:
                        AreaMark(x: .value("Category", labels[i]), y: .value("Value", series[s].values[i]))
                            .foregroundStyle(by: .value("Series", series[s].name))
                    }
                }
            }
        }
        .chartForegroundStyleScale(domain: series.map(\.name), range: Array(theme.series.prefix(series.count)))
        .chartLegend(series.count > 1 ? .visible : .hidden)
        .font(fonts.caption)
        .frame(height: 200)
    }
}

/// Short items that wrap onto as many lines as they need.
struct NativeUIFlow: Layout {
    var spacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let rows = arrange(subviews, width: proposal.width ?? .infinity)
        return CGSize(width: rows.map(\.width).max() ?? 0,
                      height: rows.reduce(0) { $0 + $1.height } + spacing * CGFloat(max(0, rows.count - 1)))
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var y = bounds.minY
        for row in arrange(subviews, width: bounds.width) {
            var x = bounds.minX
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.init(width: bounds.width, height: nil))
                subviews[index].place(at: CGPoint(x: x, y: y),
                                      proposal: .init(width: min(size.width, bounds.width), height: size.height))
                x += size.width + spacing
            }
            y += row.height + spacing
        }
    }

    private func arrange(_ subviews: Subviews, width: CGFloat) -> [(indices: [Int], width: CGFloat, height: CGFloat)] {
        var rows: [(indices: [Int], width: CGFloat, height: CGFloat)] = []
        var current: (indices: [Int], width: CGFloat, height: CGFloat) = ([], 0, 0)
        for index in subviews.indices {
            let size = subviews[index].sizeThatFits(.init(width: width, height: nil))
            let needed = current.indices.isEmpty ? size.width : current.width + spacing + size.width
            if !current.indices.isEmpty, needed > width {
                rows.append(current)
                current = ([index], size.width, size.height)
            } else {
                current = (current.indices + [index], needed, max(current.height, size.height))
            }
        }
        if !current.indices.isEmpty { rows.append(current) }
        return rows
    }
}

/// Columns at least `minimum` wide that fill the width, as `GridItem.adaptive`
/// does, laid out eagerly. Replies sit in the conversation's lazy stack, which
/// must not hold another lazy container (a nested one caused a watchdog).
struct NativeUIAdaptiveGrid: Layout {
    var minimum: CGFloat
    var spacing: CGFloat = 8
    /// Where an item sits in its cell when it is narrower than the column.
    var anchor: UnitPoint = .topLeading

    /// How many columns fit and how wide each is; at least one column.
    static func columns(count: Int, width: CGFloat, minimum: CGFloat, spacing: CGFloat) -> (count: Int, width: CGFloat) {
        guard width.isFinite else { return (max(1, count), minimum) }
        let fitting = max(1, Int(((width + spacing) / (minimum + spacing)).rounded(.down)))
        return (fitting, max(0, (width - spacing * CGFloat(fitting - 1)) / CGFloat(fitting)))
    }

    /// Cells for items of the given heights, filled row by row; a row is as
    /// tall as its tallest item.
    static func cells(heights: [CGFloat], columns: (count: Int, width: CGFloat), spacing: CGFloat) -> [CGRect] {
        var cells: [CGRect] = []
        var y: CGFloat = 0
        for start in stride(from: 0, to: heights.count, by: columns.count) {
            let row = start..<min(start + columns.count, heights.count)
            let height = row.map { heights[$0] }.max() ?? 0
            for index in row {
                let x = CGFloat(index - start) * (columns.width + spacing)
                cells.append(CGRect(x: x, y: y, width: columns.width, height: height))
            }
            y += height + spacing
        }
        return cells
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? .infinity
        let (columns, cells) = arrange(subviews, width: width)
        let used = CGFloat(columns.count) * columns.width + spacing * CGFloat(columns.count - 1)
        return CGSize(width: width.isFinite ? width : used, height: cells.last?.maxY ?? 0)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let (columns, cells) = arrange(subviews, width: bounds.width)
        for (index, cell) in zip(subviews.indices, cells) {
            subviews[index].place(at: CGPoint(x: bounds.minX + cell.minX + cell.width * anchor.x,
                                              y: bounds.minY + cell.minY),
                                  anchor: UnitPoint(x: anchor.x, y: 0),
                                  proposal: .init(width: columns.width, height: nil))
        }
    }

    private func arrange(_ subviews: Subviews, width: CGFloat) -> ((count: Int, width: CGFloat), [CGRect]) {
        let columns = Self.columns(count: subviews.count, width: width, minimum: minimum, spacing: spacing)
        let heights = subviews.map { $0.sizeThatFits(.init(width: columns.width, height: nil)).height }
        return (columns, Self.cells(heights: heights, columns: columns, spacing: spacing))
    }
}

struct NativeUIBadgeView: View {
    let badge: NativeUIBadge
    let theme: NativeUITheme
    let font: Font
    var body: some View {
        let color = badge.tone == .neutral ? theme.secondaryText : theme.color(badge.tone)
        Text(badge.text).font(font).fontWeight(.medium)
            .foregroundStyle(color)
            .padding(.horizontal, 8).padding(.vertical, 2)
            .background(color.opacity(0.16), in: Capsule())
            .accessibilityLabel([NativeUITheme.spoken(badge.tone), badge.text].compactMap { $0 }.joined(separator: ": "))
    }
}

/// A layout sketch: the frame keeps its shape and its areas their
/// proportions at any width; text inside shrinks to fit rather than moving
/// the areas. VoiceOver reads it as one description.
private struct NativeUIMockupView: View {
    let mockup: NativeUIMockup
    let theme: NativeUITheme
    let fonts: NativeUIFonts
    private static let divider: CGFloat = 14

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            sketch
                .aspectRatio(mockup.aspect, contentMode: .fit)
                .frame(maxWidth: 340, alignment: .leading)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(Self.describe(mockup))
                .accessibilityAddTraits(.isImage)
            if let caption = mockup.caption {
                Text(caption).font(fonts.caption).textSelection(.enabled)
            }
        }
    }

    private var sketch: some View {
        let shape = RoundedRectangle(cornerRadius: 12, style: .continuous)
        return GeometryReader { geometry in
            let horizontal = mockup.direction == .horizontal
            let length = horizontal ? geometry.size.width : geometry.size.height
            let dividers = mockup.regions.filter { $0.kind == .divider }.count
            let weights = mockup.regions.filter { $0.kind != .divider }.map(\.weight).reduce(0, +)
            let share = max(0, length - CGFloat(dividers) * Self.divider) / max(weights, 0.05)
            Group {
                if horizontal {
                    HStack(spacing: 0) { regions(share: share, horizontal: true) }
                } else {
                    VStack(spacing: 0) { regions(share: share, horizontal: false) }
                }
            }
            .frame(width: geometry.size.width, height: geometry.size.height)
            .overlay { overlay(in: geometry.size) }
            .overlay(alignment: .bottom) {
                if let badge = mockup.badge {
                    NativeUIBadgeView(badge: badge, theme: theme, font: .system(size: 10))
                        .background(theme.background, in: Capsule())
                        .padding(.bottom, geometry.size.height * 0.08)
                }
            }
        }
        .background(theme.surface)
        .background(theme.background)
        .clipShape(shape)
        .overlay(shape.strokeBorder(theme.separator, lineWidth: 2))
    }

    private func regions(share: CGFloat, horizontal: Bool) -> some View {
        ForEach(mockup.regions.indices, id: \.self) { index in
            let region = mockup.regions[index]
            let size = region.kind == .divider ? Self.divider : share * region.weight
            regionView(region, horizontal: horizontal)
                .frame(width: horizontal ? size : nil, height: horizontal ? nil : size)
                .frame(maxWidth: horizontal ? nil : .infinity, maxHeight: horizontal ? .infinity : nil)
        }
    }

    @ViewBuilder private func overlay(in size: CGSize) -> some View {
        if let overlay = mockup.overlay {
            ZStack {
                if overlay.dim { Color.black.opacity(0.45) }
                switch overlay.style {
                case .sheet:
                    regionView(overlay.region, horizontal: false)
                        .background(theme.background, in: RoundedRectangle(cornerRadius: 8))
                        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(theme.separator))
                        .padding(EdgeInsets(top: size.height * 0.14, leading: size.width * 0.18,
                                            bottom: size.height * 0.08, trailing: size.width * 0.18))
                case .full:
                    regionView(overlay.region, horizontal: false).padding(2).background(theme.background)
                }
            }
        }
    }

    @ViewBuilder private func regionView(_ region: NativeUIMockup.Region, horizontal: Bool) -> some View {
        switch region.kind {
        case .pane:
            VStack(alignment: .leading, spacing: 3) {
                if let title = region.title {
                    Text(title).font(.system(size: 10, weight: .semibold)).foregroundStyle(theme.secondaryText)
                        .lineLimit(1).minimumScaleFactor(0.6)
                }
                RoundedRectangle(cornerRadius: 5)
                    .fill(region.tone == .neutral ? theme.separator.opacity(0.4) : theme.color(region.tone).opacity(0.3))
                    .overlay {
                        if let label = region.label {
                            Text(label).font(.system(size: 11, weight: .semibold)).multilineTextAlignment(.center)
                                .lineLimit(2).minimumScaleFactor(0.5).padding(2)
                        }
                    }
            }
            .padding(6)
        case .chat:
            GeometryReader { geometry in
                VStack(spacing: 4) {
                    ForEach(region.bubbles.indices, id: \.self) { i in
                        let user = region.bubbles[i]
                        Capsule().fill(user ? theme.accent.opacity(0.6) : theme.secondaryText.opacity(0.4))
                            .frame(width: geometry.size.width * (user ? 0.5 : 0.7), height: 7)
                            .frame(maxWidth: .infinity, alignment: user ? .trailing : .leading)
                    }
                    Spacer(minLength: 0)
                    if region.composer {
                        Capsule().strokeBorder(theme.separator).background(theme.background, in: Capsule()).frame(height: 12)
                    }
                }
            }
            .padding(6)
        case .lines:
            VStack(alignment: .leading, spacing: 5) {
                ForEach(0..<region.count, id: \.self) { _ in
                    Capsule().fill(theme.secondaryText.opacity(0.3)).frame(height: 5)
                }
                Spacer(minLength: 0)
            }
            .padding(6)
            .background(theme.separator.opacity(0.25))
        case .divider:
            ZStack {
                Rectangle().fill(theme.separator).frame(width: horizontal ? 1 : nil, height: horizontal ? nil : 1)
                if let glyph = region.glyph {
                    Text(glyph).font(.system(size: 11, weight: .semibold)).foregroundStyle(theme.background)
                        .frame(width: 20, height: 20).background(theme.accent, in: Circle())
                }
            }
        }
    }

    /// "Layout sketch, side by side: Files, Files list; chat. Marked: chat hidden"
    static func describe(_ mockup: NativeUIMockup) -> String {
        func name(_ region: NativeUIMockup.Region) -> String? {
            switch region.kind {
            case .pane: [region.title, region.label].compactMap { $0 }.joined(separator: ", ")
            case .chat: "chat"
            case .lines: "sidebar"
            case .divider: region.glyph.map { "divider with \($0)" }
            }
        }
        var parts = ["Layout sketch, " + (mockup.direction == .horizontal ? "side by side" : "stacked") + ": "
                     + mockup.regions.compactMap(name).joined(separator: "; ")]
        if let overlay = mockup.overlay, let covering = name(overlay.region) {
            parts.append((overlay.style == .sheet ? "A card over it: " : "Covered by: ") + covering)
        }
        if let badge = mockup.badge { parts.append("Marked: " + badge.text) }
        return parts.joined(separator: ". ")
    }
}
