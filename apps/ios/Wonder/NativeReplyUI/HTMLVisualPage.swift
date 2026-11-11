import Foundation
import SwiftUI
import CryptoKit
import ImageIO

/// A deterministic, bounded reading of an agent's HTML page (a report, audit or
/// dashboard) as native blocks and v1 catalog documents. Nothing runs: scripts,
/// styles, forms, frames and links are dropped. Anything without a native
/// mapping (a diagram, an image, script-built content) is counted in
/// `omitted`, so the page can say once that parts of it can't be shown.
struct HTMLVisualPage: Equatable, Sendable {
    indirect enum Block: Equatable, Sendable {
        /// Level 2 for sections, 3 for cards and subsections.
        case heading(String, level: Int)
        /// Inline Markdown only: bold, italics and code spans.
        case text(String)
        /// A note, tip or warning; its tone comes from its class or the
        /// page's colour variables (`--warning`, `--destructive`…).
        case callout(String, tone: NativeUITone)
        case code(String)
        /// Every document went through the same decoder as a `wonder-ui` block.
        case interface(NativeUIDocument)
        /// Pictures embedded in the page itself (`data:` PNG, JPEG, GIF or
        /// WebP whose bytes match their type); nothing is fetched.
        case images([Image])
        /// A paragraph led by coloured labels (a "cause", "fix" or status
        /// pill styled in the page); the text may be empty.
        case labeled([NativeUIBadge], String)
        /// A drawing the page made in SVG, shown as a picture: its markup
        /// is drawn in the sandboxed image context, so nothing in it runs or loads.
        case drawing(Drawing)
        /// `<details>`: its summary, and what opens under it.
        case details(String, [Block])
        case rule
    }
    struct Drawing: Equatable, Sendable {
        let svg: String
        /// Width over height, from its `viewBox` or size.
        let aspect: Double
        let label: String?
    }
    struct Image: Equatable, Sendable {
        let data: Data
        let alt: String?
        let caption: String?
    }
    var title: String?
    var blocks: [Block]
    /// Parts with no native form (a diagram, an embedded page, a picture
    /// that isn't in the page, script-built content).
    var omitted: Int
    /// The page exceeded a bound and its end isn't shown.
    var truncated: Bool
    /// Pictures past the page's picture budget, said as "N more pictures".
    var skippedPictures = 0

    var isEmpty: Bool { title == nil && blocks.isEmpty }
    /// Some of the page can't be shown; said once, after the last part.
    var isPartial: Bool { omitted > 0 || truncated || skippedPictures > 0 }
    /// The one note under the page about what it can't show: pictures past
    /// its budget by number, anything else in general.
    var partialNote: String? {
        guard isPartial else { return nil }
        let pictures = skippedPictures == 0 ? nil
            : skippedPictures == 1 ? "1 more picture isn't shown here." : "\(skippedPictures) more pictures aren't shown here."
        let other = omitted > 0 || truncated ? "Some parts of this page can't be shown here." : nil
        return [other, pictures].compactMap { $0 }.joined(separator: " ")
    }

    /// The first `count` blocks without the page's own title, for a message
    /// row (its header names the page), and how many blocks were left out.
    /// The note about missing parts belongs to the end, so it stays with the
    /// rest of the page when there is more.
    func opening(_ count: Int) -> (page: Self, remaining: Int) {
        let remaining = max(0, blocks.count - count)
        return (Self(title: nil, blocks: Array(blocks.prefix(count)), omitted: remaining > 0 ? 0 : omitted,
                     truncated: remaining > 0 ? false : truncated, skippedPictures: remaining > 0 ? 0 : skippedPictures), remaining)
    }

    enum Limits {
        /// A conversation file's own limit; embedded pictures make pages large.
        static let inputBytes = 8 * 1_048_576
        static let nodes = 30_000
        static let depth = 96
        static let blocks = 400
        static let displayItems = 2_000
        static let text = 4_000
        static let code = 8_000
        static let styleBytes = 131_072
        static let styleRules = 2_000
        /// Pictures embedded in one page, each and together: pages are
        /// mostly pictures, so they have their own budget beside the text.
        static let images = 24
        static let imageBytes = 4 * 1_048_576
        static let imageTotalBytes = 16 * 1_048_576
        static let drawings = 8
        static let drawingBytes = 262_144
    }

    /// `scriptsRan`: a caller supplied an already-materialized DOM, so scripts
    /// remaining in that source do not count as omitted native content.
    static func convert(_ html: String, scriptsRan: Bool = false) -> Self {
        var builder = HTMLVisualBuilder()
        builder.scriptsRan = scriptsRan
        var source = html
        if source.utf8.count > Limits.inputBytes {
            source = String(decoding: source.utf8.prefix(Limits.inputBytes), as: UTF8.self)
            builder.truncated = true
        }
        let tree = HTMLTree(Array(source.utf8))
        if tree.truncated { builder.truncated = true }
        builder.documentTitle = tree.title
        builder.styles = HTMLStyleSheet(tree.styles)
        builder.visit(tree.root)
        return builder.finish()
    }

    /// A visual given as a wonder-ui v1 document (`wonder_show_visual`
    /// components) rather than a page: one interface block, through the same
    /// decoder as a reply's `wonder-ui` block. A document the decoder refuses
    /// shows its fallback text, or nothing. Nil when `source` is not JSON.
    static func document(_ source: String) -> Self? {
        guard source.utf8.first(where: { !(" \n\r\t".utf8.contains($0)) }) == UInt8(ascii: "{") else { return nil }
        if let document = try? NativeUIDocument.decode(source) {
            return Self(title: nil, blocks: [.interface(document)], omitted: 0, truncated: false)
        }
        let fallback = (try? JSONSerialization.jsonObject(with: Data(source.utf8.prefix(32_768))) as? [String: Any])?["fallback"] as? String
        let text = fallback.map { String($0.prefix(Limits.text)) }.flatMap { $0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : $0 }
        return Self(title: nil, blocks: text.map { [.text(HTMLVisualBuilder.markdownEscaped($0))] } ?? [], omitted: 1, truncated: false)
    }

    /// Whether the page has scripts of its own that could build content. T3
    /// Code's theme bootstrap (the script that reads `#t3-theme`) doesn't count.
    static func runsScripts(_ html: String) -> Bool {
        var rest = html[...]
        while let open = rest.range(of: "<script", options: .caseInsensitive) {
            guard let tagEnd = rest[open.upperBound...].firstIndex(of: ">") else { return false }
            let tag = rest[open.upperBound..<tagEnd]
            let close = rest[tagEnd...].range(of: "</script", options: .caseInsensitive)
            let body = rest[rest.index(after: tagEnd)..<(close?.lowerBound ?? rest.endIndex)]
            let type = tag.range(of: "type=", options: .caseInsensitive).map { tag[$0.upperBound...].prefix(30).lowercased() } ?? ""
            let data = type.contains("json") || type.contains("template") || type.contains("text/plain")
            if !data, !body.contains("t3-theme"), tag.range(of: "src=", options: .caseInsensitive) != nil
                || !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return true }
            guard let close else { return false }
            rest = rest[close.upperBound...]
        }
        return false
    }

    /// The first complete ```html fence of a reply. An unfinished (streaming)
    /// fence returns nil rather than a partial page.
    static func htmlFence(in reply: String) -> String? {
        guard reply.contains("```html") else { return nil }
        var payload: [Substring]?
        for line in reply.split(separator: "\n", omittingEmptySubsequences: false) {
            let leading = line.prefix(while: { $0 == " " })
            let trim = line.trimmingCharacters(in: .whitespaces)
            if payload != nil {
                if leading.count <= 3, trim == "```" {
                    let html = payload!.joined(separator: "\n")
                    return html.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : html
                }
                payload!.append(line)
            } else if leading.count <= 3, trim.lowercased() == "```html" {
                payload = []
            }
        }
        return nil
    }
}

/// The SHA-256 of the exact UTF-8 is the cache key. A prepared page holds no
/// colour (only tones the theme resolves when drawn), so one entry serves
/// every theme. Actor isolation keeps hashing and parsing off MainActor;
/// entries and retained bytes (page and pictures) are bounded, and the app
/// empties the cache on memory pressure.
actor HTMLVisualCache {
    struct Prepared: Equatable, Sendable {
        let html: String
        let page: HTMLVisualPage
        let rendersHTML: Bool
    }
    static let shared = HTMLVisualCache()
    private var entries: [Data: (value: Prepared, bytes: Int)] = [:]
    private var order: [Data] = []
    private var bytes = 0
    private(set) var conversions = 0
    static let maximumBytes = 24 * 1_048_576

    /// `fence` reads the first ```html block of a reply; otherwise `source` is a page.
    /// HTML retains its exact source for the sandboxed browser; JSON stays native.
    func prepare(_ source: String, fence: Bool) async -> Prepared? {
        guard let html = fence ? HTMLVisualPage.htmlFence(in: source) : source else { return nil }
        let key = Data(SHA256.hash(data: Data(html.utf8)))
        if let hit = entries[key] {
            order.removeAll { $0 == key }; order.append(key)
            return hit.value
        }
        let document = fence ? nil : HTMLVisualPage.document(html)
        conversions += 1
        let value = Prepared(html: html, page: document ?? HTMLVisualPage.convert(html), rendersHTML: document == nil)
        let size = html.utf8.count + value.page.blocks.reduce(0) { total, block in
            if case .images(let images) = block { total + images.reduce(0) { $0 + $1.data.count } } else { total }
        }
        guard size <= Self.maximumBytes / 2 else { return value }
        while !order.isEmpty && (order.count >= 8 || bytes + size > Self.maximumBytes) {
            let old = order.removeFirst()
            bytes -= entries.removeValue(forKey: old)?.bytes ?? 0
        }
        entries[key] = (value, size); order.append(key); bytes += size
        return value
    }
    /// A verified page file's bytes; nil unless they are UTF-8.
    func prepare(page data: Data) async -> Prepared? {
        guard let html = String(data: data, encoding: .utf8) else { return nil }
        return await prepare(html, fence: false)
    }

    /// A page already prepared from a file with this SHA-256 (the key is the
    /// same digest), so a row scrolled back into view needs no download.
    func cached(sha256 hex: String) -> Prepared? {
        guard hex.utf8.count == 64 else { return nil }
        var key = Data(capacity: 32)
        var index = hex.startIndex
        while index < hex.endIndex {
            let next = hex.index(index, offsetBy: 2)
            guard let byte = UInt8(hex[index..<next], radix: 16) else { return nil }
            key.append(byte)
            index = next
        }
        guard let hit = entries[key] else { return nil }
        order.removeAll { $0 == key }; order.append(key)
        return hit.value
    }

    func removeAll() { entries.removeAll(); order.removeAll(); bytes = 0 }
}

// MARK: - Tolerant HTML tree

/// Enough of HTML's tree rules for agent-written pages: void and raw-text
/// elements, implicit closing of p/li/td/tr/dt/dd, stray end tags ignored.
/// Node count and depth are bounded; deeper content joins its ancestor.
private struct HTMLTree {
    final class Node {
        let name: String
        let attributes: [String: String]
        var children: [Node] = []
        let text: String?
        /// An `<svg>` element's markup as written (names keep their case).
        var source: String?
        init(name: String, attributes: [String: String] = [:], text: String? = nil) {
            self.name = name; self.attributes = attributes; self.text = text
        }
        var isText: Bool { text != nil }
        func attribute(_ key: String) -> String? { attributes[key] }
        var classes: [String] { (attributes["class"] ?? "").lowercased().split(whereSeparator: \.isWhitespace).map(String.init) }
    }

    static let void: Set<String> = ["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta",
                                    "source", "track", "wbr", "param"]
    static let rawText: Set<String> = ["script", "style", "textarea", "title", "template", "noscript"]
    static let closesParagraph: Set<String> = ["address", "article", "aside", "blockquote", "details", "div", "dl",
        "fieldset", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "main", "nav",
        "ol", "p", "pre", "section", "table", "ul"]

    let root = Node(name: "#root")
    private(set) var title: String?
    private(set) var truncated = false
    /// Style sheets' text, read for layout and colour variables, never run.
    private(set) var styles: [String] = []

    init(_ bytes: [UInt8]) {
        var stack = [root]
        var count = 0
        var styleBytes = 0
        var i = 0, textStart = 0
        let n = bytes.count

        func flushText(_ end: Int) {
            guard end > textStart else { return }
            let raw = String(decoding: bytes[textStart..<end], as: UTF8.self)
            stack[stack.count - 1].children.append(Node(name: "#text", text: HTMLTree.decodeEntities(raw)))
        }
        func find(_ needle: [UInt8], from start: Int, caseInsensitive: Bool = false) -> Int? {
            guard needle.count <= n else { return nil }
            var j = start
            while j + needle.count <= n {
                var match = true
                for k in 0..<needle.count {
                    var b = bytes[j + k]
                    if caseInsensitive, (65...90).contains(b) { b += 32 }
                    if b != needle[k] { match = false; break }
                }
                if match { return j }
                j += 1
            }
            return nil
        }
        func isNameByte(_ b: UInt8) -> Bool {
            (48...57).contains(b) || (65...90).contains(b) || (97...122).contains(b) || b == 45 || b == 58 || b == 95
        }
        func readName(_ start: Int) -> (String, Int) {
            var j = start
            while j < n, isNameByte(bytes[j]) { j += 1 }
            return (String(decoding: bytes[start..<j], as: UTF8.self).lowercased(), j)
        }
        func close(_ name: String) {
            guard let index = stack.lastIndex(where: { $0.name == name }), index > 0 else { return }
            stack.removeLast(stack.count - index)
        }
        /// Close an open element of `names` unless a boundary element is nearer.
        func closeOpen(_ names: Set<String>, before boundary: Set<String>) {
            for index in stack.indices.reversed() where index > 0 {
                if names.contains(stack[index].name) { stack.removeLast(stack.count - index); return }
                if boundary.contains(stack[index].name) { return }
            }
        }

        parse: while i < n {
            guard bytes[i] == 60, i + 1 < n else { i += 1; continue }  // "<"
            let next = bytes[i + 1]
            if next == 33 {  // "<!"
                flushText(i)
                if i + 3 < n, bytes[i + 2] == 45, bytes[i + 3] == 45 {
                    i = (find(Array("-->".utf8), from: i + 4).map { $0 + 3 }) ?? n
                } else {
                    i = (find([62], from: i + 2).map { $0 + 1 }) ?? n
                }
                textStart = i; continue
            }
            if next == 63 {  // "<?"
                flushText(i)
                i = (find([62], from: i + 2).map { $0 + 1 }) ?? n
                textStart = i; continue
            }
            if next == 47 {  // "</"
                let (name, after) = readName(i + 2)
                guard !name.isEmpty else { i += 1; continue }
                flushText(i)
                i = (find([62], from: after).map { $0 + 1 }) ?? n
                textStart = i
                close(name)
                continue
            }
            guard (65...90).contains(next) || (97...122).contains(next) else { i += 1; continue }
            flushText(i)
            let tagStart = i
            let (name, afterName) = readName(i + 1)
            var j = afterName
            var attributes: [String: String] = [:]
            var selfClosing = false
            while j < n {
                while j < n, [9, 10, 12, 13, 32].contains(bytes[j]) { j += 1 }
                guard j < n else { break }
                if bytes[j] == 62 { j += 1; break }
                if bytes[j] == 47 { selfClosing = true; j += 1; continue }
                let start = j
                while j < n, ![9, 10, 12, 13, 32, 61, 62, 47].contains(bytes[j]) { j += 1 }
                let key = String(decoding: bytes[start..<j], as: UTF8.self).lowercased()
                if j == start { j += 1; continue }
                selfClosing = false
                while j < n, [9, 10, 12, 13, 32].contains(bytes[j]) { j += 1 }
                var value = ""
                if j < n, bytes[j] == 61 {
                    j += 1
                    while j < n, [9, 10, 12, 13, 32].contains(bytes[j]) { j += 1 }
                    if j < n, bytes[j] == 34 || bytes[j] == 39 {
                        let quote = bytes[j]
                        let end = bytes[(j + 1)...].firstIndex(of: quote) ?? n
                        value = String(decoding: bytes[(j + 1)..<end], as: UTF8.self)
                        j = min(n, end + 1)
                    } else {
                        let start = j
                        while j < n, ![9, 10, 12, 13, 32, 62].contains(bytes[j]) { j += 1 }
                        value = String(decoding: bytes[start..<j], as: UTF8.self)
                    }
                }
                if attributes[key] == nil { attributes[key] = HTMLTree.decodeEntities(value) }
            }
            i = j; textStart = i
            count += 1
            guard count <= HTMLVisualPage.Limits.nodes else { truncated = true; break parse }

            switch name {
            case "li": closeOpen(["li"], before: ["ul", "ol", "table"])
            case "td", "th": closeOpen(["td", "th"], before: ["tr", "table"])
            case "tr": closeOpen(["tr"], before: ["table", "thead", "tbody", "tfoot"])
            case "thead", "tbody", "tfoot": closeOpen(["thead", "tbody", "tfoot"], before: ["table"])
            case "dt", "dd": closeOpen(["dt", "dd"], before: ["dl"])
            case "option": closeOpen(["option"], before: ["select"])
            default: break
            }
            if HTMLTree.closesParagraph.contains(name) { closeOpen(["p"], before: ["div", "section", "table", "li", "td", "th", "blockquote", "article", "main"]) }

            if HTMLTree.rawText.contains(name) {
                let end = find(Array("</\(name)".utf8), from: i, caseInsensitive: true) ?? n
                let node = Node(name: name, attributes: attributes)
                if name == "title" {
                    let value = HTMLTree.collapse(HTMLTree.decodeEntities(String(decoding: bytes[i..<end], as: UTF8.self)))
                    if title == nil, !value.isEmpty { title = value }
                    node.children = [Node(name: "#text", text: value)]
                } else if name == "style", styleBytes + (end - i) <= HTMLVisualPage.Limits.styleBytes {
                    styleBytes += end - i
                    styles.append(String(decoding: bytes[i..<end], as: UTF8.self))
                }
                stack[stack.count - 1].children.append(node)
                i = end < n ? ((find([62], from: end).map { $0 + 1 }) ?? n) : n
                textStart = i
                continue
            }
            let node = Node(name: name, attributes: attributes)
            if name == "svg", !selfClosing, let end = find(Array("</svg".utf8), from: i, caseInsensitive: true),
               let close = find([62], from: end), close + 1 - tagStart <= HTMLVisualPage.Limits.drawingBytes {
                node.source = String(decoding: bytes[tagStart...close], as: UTF8.self)
            }
            stack[stack.count - 1].children.append(node)
            if HTMLTree.void.contains(name) || (selfClosing && !["div", "p", "span", "li", "td"].contains(name)) { continue }
            if stack.count <= HTMLVisualPage.Limits.depth { stack.append(node) } else { truncated = true }
        }
        flushText(min(i, n))
    }

    static let named: [String: String] = [
        "amp": "&", "lt": "<", "gt": ">", "quot": "\"", "apos": "'", "nbsp": " ", "middot": "·", "mdash": "—",
        "ndash": "–", "hellip": "…", "rarr": "→", "larr": "←", "harr": "↔", "uarr": "↑", "darr": "↓", "times": "×",
        "copy": "©", "reg": "®", "trade": "™", "rsquo": "’", "lsquo": "‘", "ldquo": "“", "rdquo": "”", "bull": "•",
        "le": "≤", "ge": "≥", "ne": "≠", "minus": "−", "plusmn": "±", "deg": "°", "check": "✓", "thinsp": " ",
        "ensp": " ", "emsp": " ", "laquo": "«", "raquo": "»", "sect": "§", "para": "¶", "euro": "€", "pound": "£",
    ]

    static func decodeEntities(_ text: String) -> String {
        guard text.contains("&") else { return text }
        var result = ""
        result.reserveCapacity(text.utf8.count)
        var index = text.startIndex
        while let amp = text[index...].firstIndex(of: "&") {
            result += text[index..<amp]
            let rest = text[text.index(after: amp)...]
            if let semi = rest.prefix(12).firstIndex(of: ";") {
                let name = String(rest[rest.startIndex..<semi])
                var replacement: String?
                if name.hasPrefix("#x") || name.hasPrefix("#X") {
                    replacement = UInt32(name.dropFirst(2), radix: 16).flatMap(Unicode.Scalar.init).map { String(Character($0)) }
                } else if name.hasPrefix("#") {
                    replacement = UInt32(name.dropFirst()).flatMap(Unicode.Scalar.init).map { String(Character($0)) }
                } else {
                    replacement = named[name]
                }
                if let replacement {
                    result += replacement
                    index = text.index(after: semi)
                    continue
                }
            }
            result += "&"
            index = text.index(after: amp)
        }
        result += text[index...]
        return result
    }

    /// HTML whitespace collapsing, trimmed.
    static func collapse(_ text: String) -> String {
        var result = ""
        var space = false
        for scalar in text.unicodeScalars {
            if scalar == " " || scalar == "\n" || scalar == "\t" || scalar == "\r" || scalar == "\u{0C}" {
                space = true
            } else {
                if space, !result.isEmpty { result += " " }
                space = false
                result.unicodeScalars.append(scalar)
            }
        }
        return result
    }
}

// MARK: - Style sheets

/// Enough of CSS to read an agent page's layout and colour intent: plain rules
/// whose last compound selector is a tag and/or classes, applied in source
/// order, then the element's own `style`. At-rules (`@media`…), ids,
/// attributes and pseudo-classes are ignored; nothing is evaluated.
struct HTMLStyleSheet: Sendable {
    struct Rule: Sendable {
        let tag: String?
        let classes: [String]
        let declarations: [(String, String)]
    }
    private(set) var rules: [Rule] = []
    private var byClass: [String: [Int]] = [:]
    private var byTag: [String: [Int]] = [:]

    init(_ sources: [String] = []) {
        for source in sources where rules.count < HTMLVisualPage.Limits.styleRules { parse(source) }
    }

    private mutating func parse(_ css: String) {
        var text = Substring(css)
        // Comments first, so a brace inside one cannot unbalance the scan.
        var plain = ""
        while let open = text.range(of: "/*") {
            plain += text[..<open.lowerBound]
            text = text[open.upperBound...]
            guard let close = text.range(of: "*/") else { text = ""; break }
            text = text[close.upperBound...]
        }
        plain += text
        var index = plain.startIndex
        while index < plain.endIndex, rules.count < HTMLVisualPage.Limits.styleRules {
            guard let open = plain[index...].firstIndex(of: "{") else { return }
            let selector = plain[index..<open].trimmingCharacters(in: .whitespacesAndNewlines)
            // The matching close brace, counting nested blocks (an at-rule's).
            var depth = 0, cursor = open, close: String.Index?
            while cursor < plain.endIndex {
                if plain[cursor] == "{" { depth += 1 } else if plain[cursor] == "}" { depth -= 1; if depth == 0 { close = cursor; break } }
                cursor = plain.index(after: cursor)
            }
            guard let close else { return }
            index = plain.index(after: close)
            guard !selector.hasPrefix("@") else { continue }
            let declarations = Self.declarations(plain[plain.index(after: open)..<close])
            guard !declarations.isEmpty else { continue }
            for part in selector.split(separator: ",") {
                let compound = part.split(whereSeparator: { $0 == " " || $0 == ">" || $0 == "+" || $0 == "~" || $0.isNewline }).last.map(String.init) ?? ""
                guard !compound.isEmpty, !compound.contains(where: { ":[#*()".contains($0) }) else { continue }
                let pieces = compound.split(separator: ".", omittingEmptySubsequences: false).map { $0.lowercased() }
                let tag = pieces[0].isEmpty ? nil : pieces[0]
                let classes = pieces.dropFirst().filter { !$0.isEmpty }
                guard tag != nil || !classes.isEmpty else { continue }
                let id = rules.count
                rules.append(Rule(tag: tag, classes: Array(classes), declarations: declarations))
                if let first = classes.first { byClass[first, default: []].append(id) } else if let tag { byTag[tag, default: []].append(id) }
            }
        }
    }

    static func declarations(_ body: Substring) -> [(String, String)] {
        body.split(separator: ";").prefix(64).compactMap { line in
            guard let colon = line.firstIndex(of: ":") else { return nil }
            let property = line[..<colon].trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
            var value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
            if value.hasSuffix("!important") { value = String(value.dropLast(10)).trimmingCharacters(in: .whitespaces) }
            return property.isEmpty || value.isEmpty ? nil : (property, value)
        }
    }

    /// Declarations that apply to an element with this tag, classes and
    /// inline style, the later winning.
    func style(tag: String, classes: [String], inline: String?) -> [String: String] {
        var ids = byTag[tag] ?? []
        for name in Set(classes) { ids += byClass[name] ?? [] }
        var result: [String: String] = [:]
        let present = Set(classes)
        for id in Set(ids).sorted() {
            let rule = rules[id]
            guard rule.tag == nil || rule.tag == tag, rule.classes.allSatisfy(present.contains) else { continue }
            for (property, value) in rule.declarations { result[property] = value }
        }
        if let inline { for (property, value) in Self.declarations(Substring(inline)) { result[property] = value } }
        return result
    }

    /// A page colour's meaning: T3 Code's variables `--success`, `--warning`,
    /// `--destructive`, `--info`, `--primary`/`--accent` and `--chart-1`…`6`,
    /// else a literal colour's nearest tone (`literalTone`).
    static func tone(in value: String?) -> NativeUITone? {
        guard let value else { return nil }
        return variableTone(in: value) ?? literalTone(in: value)
    }

    private static func variableTone(in value: String) -> NativeUITone? {
        var rest = Substring(value)
        while let range = rest.range(of: "var(--") {
            rest = rest[range.upperBound...]
            var name = String(rest.prefix { $0.isLetter || $0.isNumber || $0 == "-" })
            for suffix in ["-foreground", "-surface"] where name.hasSuffix(suffix) { name.removeLast(suffix.count) }
            switch name {
            case "success", "ok", "positive", "green", "good": return .success
            case "warning", "warn", "caution", "amber", "yellow", "serious": return .warning
            case "destructive", "danger", "error", "negative", "red", "critical": return .error
            case "info", "blue": return .info
            case "primary", "accent", "acc", "ring", "link": return .accent
            case "chart-1": return .series1
            case "chart-2": return .series2
            case "chart-3": return .series3
            case "chart-4": return .series4
            case "chart-5": return .series5
            case "chart-6": return .series6
            default: continue
            }
        }
        return nil
    }

    /// The nearest theme tone of the first literal colour in a CSS value
    /// (`#hex`, `rgb()`, `hsl()` or a colour name), so a page's hard-coded
    /// colours follow the reader's theme too (owner, Oct 23); the literal is
    /// never drawn. Greys, near-whites and near-blacks (chroma under 0.2)
    /// and transparent colours stay neutral (nil). A clear colour maps by
    /// hue: red to error, orange and yellow to warning, green to success,
    /// cyan and blue to info, violet and magenta to accent.
    static func literalTone(in value: String) -> NativeUITone? {
        guard let (red, green, blue, alpha) = literalColour(in: value), alpha > 0 else { return nil }
        let high = max(red, green, blue), chroma = high - min(red, green, blue)
        guard chroma >= 0.2 else { return nil }
        var hue = high == red ? (green - blue) / chroma : high == green ? (blue - red) / chroma + 2 : (red - green) / chroma + 4
        hue *= 60
        if hue < 0 { hue += 360 }
        switch hue {
        case ..<15, 345...: return .error
        case ..<70: return .warning
        case ..<165: return .success
        case ..<250: return .info
        default: return .accent
        }
    }

    private static let namedColours: [String: (Double, Double, Double)] = [
        "red": (1, 0, 0), "crimson": (0.86, 0.08, 0.24), "tomato": (1, 0.39, 0.28), "orangered": (1, 0.27, 0),
        "orange": (1, 0.65, 0), "gold": (1, 0.84, 0), "yellow": (1, 1, 0), "green": (0, 0.5, 0), "lime": (0, 1, 0),
        "seagreen": (0.18, 0.55, 0.34), "teal": (0, 0.5, 0.5), "cyan": (0, 1, 1), "aqua": (0, 1, 1), "blue": (0, 0, 1),
        "navy": (0, 0, 0.5), "royalblue": (0.25, 0.41, 0.88), "dodgerblue": (0.12, 0.56, 1), "purple": (0.5, 0, 0.5),
        "violet": (0.93, 0.51, 0.93), "indigo": (0.29, 0, 0.51), "magenta": (1, 0, 1), "fuchsia": (1, 0, 1),
    ]

    /// Red, green, blue and alpha (0…1) of the first literal colour in a value.
    static func literalColour(in value: String) -> (Double, Double, Double, Double)? {
        let text = value.lowercased()
        var best: (index: String.Index, colour: (Double, Double, Double, Double))?
        func consider(_ index: String.Index, _ colour: (Double, Double, Double, Double)?) {
            guard let colour, best.map({ index < $0.index }) ?? true else { return }
            best = (index, colour)
        }
        if let hash = text.firstIndex(of: "#") {
            let digits = text[text.index(after: hash)...].prefix { $0.isHexDigit }
            let expanded: String? = switch digits.count {
            case 3, 4: digits.map { "\($0)\($0)" }.joined()
            case 6, 8: String(digits)
            default: nil
            }
            if let expanded, let number = UInt64(expanded, radix: 16) {
                let parts = expanded.count == 8 ? (number >> 24, number >> 16, number >> 8, number) : (number >> 16, number >> 8, number, 255)
                consider(hash, (Double(parts.0 & 255) / 255, Double(parts.1 & 255) / 255, Double(parts.2 & 255) / 255, Double(parts.3 & 255) / 255))
            }
        }
        for function in ["rgb", "hsl"] {
            guard let open = text.range(of: function), let paren = text[open.upperBound...].firstIndex(of: "("),
                  text[open.upperBound..<paren].allSatisfy({ $0 == "a" }),
                  let close = text[paren...].firstIndex(of: ")") else { continue }
            let arguments = text[text.index(after: paren)..<close]
                .split(whereSeparator: { $0 == "," || $0 == " " || $0 == "/" }).map(String.init)
            guard arguments.count >= 3 else { continue }
            func number(_ raw: String, scale: Double) -> Double? {
                if raw.hasSuffix("%") { return Double(raw.dropLast()).map { $0 / 100 } }
                return Double(raw.hasSuffix("deg") ? String(raw.dropLast(3)) : raw).map { $0 / scale }
            }
            let alpha = arguments.count > 3 ? number(arguments[3], scale: 1) ?? 1 : 1
            if function == "rgb" {
                guard let r = number(arguments[0], scale: 255), let g = number(arguments[1], scale: 255),
                      let b = number(arguments[2], scale: 255) else { continue }
                consider(open.lowerBound, (r, g, b, alpha))
            } else {
                guard let h = number(arguments[0], scale: 1), let s = number(arguments[1], scale: 100),
                      let l = number(arguments[2], scale: 100) else { continue }
                let (saturation, lightness) = (min(max(s, 0), 1), min(max(l, 0), 1))
                let c = (1 - abs(2 * lightness - 1)) * saturation
                let x = c * (1 - abs((h / 60).truncatingRemainder(dividingBy: 2) - 1)), m = lightness - c / 2
                let sector = Int(((h.truncatingRemainder(dividingBy: 360)) + 360).truncatingRemainder(dividingBy: 360) / 60)
                let (r, g, b): (Double, Double, Double) = [(c, x, 0), (x, c, 0), (0, c, x), (0, x, c), (x, 0, c), (c, 0, x)][min(sector, 5)]
                consider(open.lowerBound, (r + m, g + m, b + m, alpha))
            }
        }
        for word in text.split(whereSeparator: { !$0.isLetter }) {
            if word == "transparent" { consider(word.startIndex, (0, 0, 0, 0)) }
            if let (r, g, b) = namedColours[String(word)] { consider(word.startIndex, (r, g, b, 1)) }
        }
        return best?.colour
    }
}

// MARK: - Mapping

private struct HTMLVisualBuilder {
    typealias Node = HTMLTree.Node
    var documentTitle: String?
    var title: String?
    var blocks: [HTMLVisualPage.Block] = []
    var omitted = 0
    var truncated = false
    var skippedPictures = 0
    var scriptsRan = false
    private var items = 0
    private var imageBytes = 0
    private var drawings = 0
    /// The side-by-side row (a multi-column grid or a flex row) being
    /// walked, and the one the last picture came from: pictures share a
    /// block only when the page puts them side by side.
    private var pictureRow = 0
    private var rows = 0
    private var lastPictureRow = -1
    /// Coloured labels waiting to lead the next paragraph.
    private var badgesLead: [NativeUIBadge] = []
    private var componentIndex = 0
    private var heading: (text: String, level: Int)?
    private var paragraph = ""
    /// The last thing added to `paragraph` was an inline element; a sibling
    /// element directly after it (flex chips, legends) gets a separator.
    private var afterInlineElement = false
    /// Short labels (a card number, a status chip) waiting to prefix the next
    /// card heading rather than standing alone.
    private var lead: [String] = []
    private var scripted = false
    var styles = HTMLStyleSheet()
    private let resolved = StyleMemo()
    private var imageCount = 0

    /// Each element's resolved declarations, computed once per conversion.
    final class StyleMemo { var values: [ObjectIdentifier: [String: String]] = [:] }

    static let skipped: Set<String> = ["head", "style", "script", "template", "noscript", "nav", "button", "form",
        "input", "select", "textarea", "option", "label", "meta", "link", "base", "dialog", "menu"]
    /// Elements with no native form; each counts as an omitted part.
    static let media: Set<String> = ["canvas", "video", "audio", "iframe", "object", "embed", "picture"]
    static let inline: Set<String> = ["a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "dfn", "em", "i",
        "kbd", "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "time", "u", "var", "del", "ins",
        "font", "tt", "big", "strike", "br", "wbr", "#text"]
    static let calloutClasses: Set<String> = ["callout", "note", "notice", "warning", "warn", "alert", "tip",
        "important", "caution", "danger", "admonition", "hint", "info", "banner"]
    static let structural: Set<String> = ["h1", "h2", "h3", "h4", "h5", "h6", "p", "ul", "ol", "table", "dl",
        "pre", "blockquote", "div", "section", "article", "header", "footer", "aside", "main", "figure", "svg",
        "details", "li", "hr", "img"]

    private var full: Bool { blocks.count >= HTMLVisualPage.Limits.blocks || items >= HTMLVisualPage.Limits.displayItems }

    mutating func finish() -> HTMLVisualPage {
        flushParagraph()
        flushHeading()
        flushLead()
        if scripted { omitted += 1 }
        if !badgesLead.isEmpty { append(.labeled(badgesLead, "")); badgesLead = [] }
        return HTMLVisualPage(title: title ?? documentTitle, blocks: blocks, omitted: omitted, truncated: truncated,
                              skippedPictures: skippedPictures)
    }

    // MARK: Walk

    mutating func visit(_ node: Node) {
        if full { truncated = true; return }
        if node.isText {
            let text = node.text ?? ""
            paragraph += Self.markdownEscaped(text)
            if !HTMLTree.collapse(text).isEmpty { afterInlineElement = false }
            return
        }
        let name = node.name
        if Self.skipped.contains(name) {
            if name == "script", !scriptsRan { scripted = true }
            return
        }
        if Self.media.contains(name) {
            omit()
            return
        }
        if let badge = badge(node), HTMLTree.collapse(paragraph).isEmpty {
            flushParagraph()
            badgesLead.append(badge)
            afterInlineElement = false
            return
        }
        if Self.inline.contains(name) && !containsStructure(node) {
            let markdown = inlineMarkdown(node)
            guard !HTMLTree.collapse(markdown).isEmpty else { return }
            if afterInlineElement, let last = paragraph.last, !last.isWhitespace { paragraph += " · " }
            paragraph += markdown
            afterInlineElement = true
            return
        }
        switch name {
        case "#root", "html", "body": children(node)
        case "h1":
            let text = plain(node)
            if title == nil, !text.isEmpty { flushParagraph(); title = clip(text, 200) }
            else { setHeading(text, level: 2) }
        case "h2": setHeading(plain(node), level: 2)
        case "h3", "h4", "h5", "h6": setHeading(plain(node), level: 3)
        case "p":
            flushParagraph()
            if isCallout(node) { callout(node); return }
            // Labels first ("cause", "fix"), then the sentence they label.
            var rest = node.children[...]
            while let first = rest.first, first.isText ? HTMLTree.collapse(first.text ?? "").isEmpty : badge(first) != nil {
                if let label = badge(first) { badgesLead.append(label) }
                rest = rest.dropFirst()
            }
            paragraph = rest.map(inlineMarkdown).joined()
            flushParagraph()
        case "ul", "ol": list(node, ordered: name == "ol")
        case "table": table(node)
        case "dl": definitions(node)
        case "pre": flushParagraph(); code(node)
        case "blockquote", "aside": flushParagraph(); callout(node)
        case "svg": flushParagraph(); svg(node)
        case "img": picture(node, caption: nil)
        case "figure":
            // One picture and the text beside it (a figcaption, or a plain
            // paragraph agents often use) read as a captioned picture.
            let pictures = node.children.filter { $0.name == "img" }
            let others = node.children.filter { $0.name != "img" && !($0.isText && HTMLTree.collapse($0.text ?? "").isEmpty) }
            if pictures.count == 1, others.allSatisfy({ ["figcaption", "p", "div", "span", "small"].contains($0.name) && !containsStructure($0, includingSelf: false) }) {
                let caption = others.map { plain($0) }.filter { !$0.isEmpty }.joined(separator: " ")
                picture(pictures[0], caption: caption.isEmpty ? nil : clip(caption, 500))
            } else {
                container(node)
            }
        case "details": details(node)
        case "hr":
            flushParagraph()
            flushHeading()
            if let last = blocks.last, last != .rule { append(.rule) }
        default: container(node)
        }
    }

    private mutating func children(_ node: Node) {
        for child in node.children { visit(child) }
    }

    private mutating func container(_ node: Node) {
        flushParagraph()
        let outerRow = pictureRow
        if isRow(node) { rows += 1; pictureRow = rows }
        defer { pictureRow = outerRow }
        if let sketch = mockup(node) { emitMockup(sketch, caption: nil); return }
        if isCallout(node) { callout(node); return }
        if let tiles = statTiles(node) {
            let title = takeHeading() ?? "Key figures"
            for (index, start) in stride(from: 0, to: tiles.count, by: 8).enumerated() {
                emit(type: "stats", title: index == 0 ? title : clip(title + " (continued)", 120),
                     ["items": tiles[start..<min(start + 8, tiles.count)].map { tile -> [String: Any] in
                         var item: [String: Any] = ["label": tile.label, "value": tile.value]
                         if let delta = tile.delta { item["delta"] = delta }
                         if let tone = tile.tone { item["tone"] = tone.rawValue }
                         return item
                     }])
            }
            return
        }
        if let bars = cssBars(node) {
            chart(bars, title: takeHeading() ?? label(of: node) ?? "Chart")
            return
        }
        if let badges = badges(node) {
            emit(type: "badges", title: takeHeading() ?? "Status", ["items": badges])
            return
        }
        // A layout sketch and the caption right after it read as one.
        let kids = node.children
        var index = 0
        while index < kids.count {
            let child = kids[index]
            if !child.isText, let sketch = mockup(child) {
                var next = index + 1
                while next < kids.count, kids[next].isText, HTMLTree.collapse(kids[next].text ?? "").isEmpty { next += 1 }
                var caption: String?
                if next < kids.count, !kids[next].isText, !Self.inline.contains(kids[next].name),
                   mockup(kids[next]) == nil, !containsStructure(kids[next], includingSelf: false) {
                    let text = plain(kids[next])
                    if !text.isEmpty, text.unicodeScalars.count <= 500 { caption = text; index = next }
                }
                flushParagraph()
                emitMockup(sketch, caption: caption)
            } else {
                visit(child)
            }
            index += 1
        }
        flushParagraph()
    }

    // MARK: Text

    private mutating func setHeading(_ text: String, level: Int) {
        flushParagraph()
        flushHeading()
        guard !text.isEmpty else { return }
        let prefix = level == 3 ? lead : []
        if level == 3 { lead = [] }
        heading = (clip((prefix + [text]).joined(separator: " · "), 120), level)
    }

    private mutating func flushHeading() {
        guard let pending = heading else { return }
        heading = nil
        append(.heading(pending.text, level: pending.level))
    }

    /// A heading directly above a component becomes its title.
    private mutating func takeHeading() -> String? {
        flushParagraph()
        defer { heading = nil }
        return heading?.text
    }

    private mutating func flushParagraph() {
        let lines = paragraph.components(separatedBy: "\u{2028}").map(HTMLTree.collapse).filter { !$0.isEmpty }
        paragraph = ""; afterInlineElement = false
        if !badgesLead.isEmpty {
            let labels = badgesLead
            badgesLead = []
            flushHeading()
            append(.labeled(labels, clip(lines.joined(separator: "\n"), HTMLVisualPage.Limits.text)))
            return
        }
        guard !lines.isEmpty else { return }
        let text = lines.joined(separator: "\n")
        if heading?.level ?? 2 == 2, lines.count == 1, text.count <= 16, lead.count < 3,
           !text.contains("**"), !".:;!?".contains(text.last!) {
            flushHeading()
            lead.append(text)
            return
        }
        appendText(text)
    }

    private mutating func appendText(_ text: String) {
        flushHeading()
        // Long text splits at a line or sentence rather than disappearing.
        var rest = Substring(text)
        while !rest.isEmpty, !full {
            var piece = rest.prefix(HTMLVisualPage.Limits.text)
            if piece.count < rest.count, let cut = piece.lastIndex(where: { $0 == "\n" || $0 == "." || $0 == " " }),
               cut > piece.startIndex {
                piece = rest[rest.startIndex...cut]
            }
            append(.text(String(piece).trimmingCharacters(in: .whitespaces)))
            rest = rest[piece.endIndex...]
        }
    }

    private mutating func append(_ block: HTMLVisualPage.Block) {
        guard blocks.count < HTMLVisualPage.Limits.blocks else { truncated = true; return }
        flushLead()
        blocks.append(block)
    }

    private mutating func flushLead() {
        guard !lead.isEmpty else { return }
        let text = lead.joined(separator: " · ")
        lead = []
        append(.text(text))
    }

    private mutating func omit() {
        flushParagraph()
        flushHeading()
        omitted += 1
    }

    /// `<details>`: the summary opens what follows it, built like the page.
    private mutating func details(_ node: Node) {
        flushParagraph()
        flushHeading()
        let summary = node.children.first { $0.name == "summary" }.map { plain($0) } ?? ""
        let start = blocks.count
        for child in node.children where child.name != "summary" { visit(child) }
        flushParagraph()
        flushHeading()
        flushLead()
        let inner = Array(blocks[start...])
        blocks.removeSubrange(start...)
        append(.details(summary.isEmpty ? "Details" : clip(summary, 200), inner))
    }

    private mutating func callout(_ node: Node) {
        flushHeading()
        let text = blockMarkdown(node)
        guard !text.isEmpty else { return }
        append(.callout(clip(text, HTMLVisualPage.Limits.text), tone: calloutTone(node)))
    }

    /// A callout's meaning from its class (`warning`, `tip`…), else from the
    /// colour variables its style uses; a plain note is `info`.
    private func calloutTone(_ node: Node) -> NativeUITone {
        for name in node.classes {
            switch name {
            case "warning", "warn", "caution", "alert", "important": return .warning
            case "danger", "error", "critical", "bad", "fail", "failed": return .error
            case "tip", "success", "ok", "good", "done", "pass": return .success
            default: continue
            }
        }
        switch tone(node) {
        case .success: return .success
        case .warning: return .warning
        case .error: return .error
        default: return .info
        }
    }

    private mutating func code(_ node: Node) {
        flushHeading()
        var text = raw(node)
        while text.hasPrefix("\n") { text.removeFirst() }
        while text.hasSuffix("\n") || text.hasSuffix(" ") { text.removeLast() }
        guard !text.isEmpty else { return }
        append(.code(clip(text, HTMLVisualPage.Limits.code)))
    }

    // MARK: Components

    private mutating func list(_ node: Node, ordered: Bool) {
        flushParagraph()
        let entries = node.children.filter { $0.name == "li" }
        guard !entries.isEmpty else { container(node); return }
        if let checks = checklist(entries) {
            let title = takeHeading() ?? "Checklist"
            for (index, start) in stride(from: 0, to: checks.count, by: 20).enumerated() {
                emit(type: "checklist", title: index == 0 ? title : clip(title + " (continued)", 120),
                     ["items": checks[start..<min(start + 20, checks.count)].map { ["text": $0.text, "done": $0.done] as [String: Any] }])
            }
            return
        }
        if let steps = steps(node, entries) {
            let title = takeHeading() ?? "Steps"
            for (index, start) in stride(from: 0, to: steps.count, by: 12).enumerated() {
                emit(type: "steps", title: index == 0 ? title : clip(title + " (continued)", 120),
                     ["steps": Array(steps[start..<min(start + 12, steps.count)])])
            }
            return
        }
        if let title = takeHeading() {
            let texts = entries.map { clip(plain($0), 256) }.filter { !$0.isEmpty }
            for (chunk, start) in stride(from: 0, to: texts.count, by: 20).map({ (texts[$0..<min($0 + 20, texts.count)], $0) }) {
                emit(type: "list", title: start == 0 ? title : clip(title + " (continued)", 120), ["items": Array(chunk)])
            }
        } else {
            flushHeading()
            let lines = listLines(node, ordered: ordered, depth: 0)
            if !lines.isEmpty { appendText(lines.joined(separator: "\n")) }
        }
    }

    /// Markdown list lines: nested lists indent under their item, and an
    /// ordered list keeps its `start` and `reversed` numbering.
    private func listLines(_ node: Node, ordered: Bool, depth: Int) -> [String] {
        let entries = node.children.filter { $0.name == "li" }
        let reversed = ordered && node.attribute("reversed") != nil
        let start = Int(node.attribute("start") ?? "") ?? (reversed ? entries.count : 1)
        let indent = String(repeating: "  ", count: min(depth, 6))
        var lines: [String] = []
        for (index, entry) in entries.enumerated() {
            let own = entry.children.filter { $0.name != "ul" && $0.name != "ol" }.map(inlineMarkdown).joined()
            let text = HTMLTree.collapse(own.replacingOccurrences(of: "\u{2028}", with: " "))
            if !text.isEmpty { lines.append(indent + (ordered ? "\(reversed ? start - index : start + index). " : "- ") + text) }
            for nested in entry.children where nested.name == "ul" || nested.name == "ol" {
                lines += listLines(nested, ordered: nested.name == "ol", depth: depth + 1)
            }
        }
        return lines
    }

    private mutating func definitions(_ node: Node) {
        flushParagraph()
        var entries: [(String, String)] = []
        var term: String?
        for child in node.children {
            if child.name == "dt" { term = plain(child) }
            else if child.name == "dd", let label = term, !label.isEmpty {
                let value = plain(child)
                entries.append((clip(label, 120), clip(value.isEmpty ? "—" : value, 512)))
            }
        }
        guard !entries.isEmpty else { container(node); return }
        let title = takeHeading() ?? "Details"
        for (index, start) in stride(from: 0, to: entries.count, by: 20).enumerated() {
            let chunk = entries[start..<min(start + 20, entries.count)]
            emit(type: "keyValue", title: index == 0 ? title : clip(title + " (continued)", 120),
                 ["entries": chunk.map { ["label": $0.0, "value": $0.1] }])
        }
    }

    private mutating func table(_ node: Node) {
        flushParagraph()
        var header: [String]?
        var rows: [[String]] = []
        var caption: String?
        var spans: [Int: (text: String, rows: Int)] = [:]
        func collect(_ parent: Node, inHead: Bool) {
            for child in parent.children {
                switch child.name {
                case "caption": caption = plain(child)
                case "thead": collect(child, inHead: true)
                case "tbody", "tfoot": collect(child, inHead: false)
                case "tr":
                    let cells = child.children.filter { $0.name == "td" || $0.name == "th" }
                    guard !cells.isEmpty else { continue }
                    // A cell spanning columns fills its first column and leaves
                    // the rest empty; one spanning rows repeats in the rows below.
                    var texts: [String] = []
                    for cell in cells {
                        while let carried = spans[texts.count], carried.rows > 0 {
                            texts.append(carried.text)
                            spans[texts.count - 1] = carried.rows > 1 ? (carried.text, carried.rows - 1) : nil
                        }
                        let text = plain(cell)
                        let rowSpan = min(50, max(1, Int(cell.attribute("rowspan") ?? "") ?? 1))
                        let colSpan = min(8, max(1, Int(cell.attribute("colspan") ?? "") ?? 1))
                        if rowSpan > 1 { spans[texts.count] = (text, rowSpan - 1) }
                        texts.append(text)
                        texts += Array(repeating: "", count: colSpan - 1)
                    }
                    while let carried = spans[texts.count], carried.rows > 0 {
                        texts.append(carried.text)
                        spans[texts.count - 1] = carried.rows > 1 ? (carried.text, carried.rows - 1) : nil
                    }
                    if header == nil, rows.isEmpty, inHead || cells.allSatisfy({ $0.name == "th" }) { header = texts }
                    else { rows.append(texts) }
                default: break
                }
            }
        }
        collect(node, inHead: false)
        rows = rows.filter { $0.contains { !$0.isEmpty } }
        guard !rows.isEmpty else {
            if let header, !header.isEmpty { appendText(header.map(Self.markdownEscaped).joined(separator: " · ")) }
            return
        }
        let width = max(header?.count ?? 0, rows.map(\.count).max() ?? 0)
        let columns = (0..<width).map { index -> String in
            let text = index < (header?.count ?? 0) ? header![index] : ""
            return clip(text.isEmpty ? "Column \(index + 1)" : text, 120)
        }
        rows = rows.map { row in (0..<width).map { $0 < row.count ? clip(row[$0], 256) : "" } }
        let named = takeHeading() ?? caption.flatMap { $0.isEmpty ? nil : clip($0, 120) }
        let title = named ?? header.map { clip($0.filter { !$0.isEmpty }.joined(separator: ", "), 120) }
            .flatMap { $0.isEmpty ? nil : $0 } ?? "Table"

        if width == 2, rows.count >= 2, rows.allSatisfy({ Self.number($0[1]) != nil && !$0[0].isEmpty }) {
            chart(rows.map { (clip($0[0], 120), Self.number($0[1])!) }, title: title)
        } else if (3...7).contains(width), header != nil, (2...24).contains(rows.count), rows.count * (width - 1) <= 80,
                  rows.allSatisfy({ !$0[0].isEmpty && $0.dropFirst().allSatisfy { Self.number($0) != nil } }),
                  Set(rows.map { clip($0[0], 60) }).count == rows.count, Set(columns.dropFirst()).count == width - 1 {
            // A label column and two to six numeric ones: one series per column.
            emit(type: "chart", title: title, ["kind": "bar", "labels": rows.map { clip($0[0], 60) },
                 "series": (1..<width).map { col in
                     ["name": clip(columns[col], 60), "values": rows.map { Self.number($0[col])! }] as [String: Any]
                 }])
        } else if width == 2, header == nil {
            emitEntries(rows.map { ($0[0].isEmpty ? "—" : clip($0[0], 120), $0[1].isEmpty ? "—" : $0[1]) }, title: title)
        } else if width <= 8 {
            // Each document keeps within the catalog's 80 cells.
            let chunk = min(20, 80 / width)
            for (index, start) in stride(from: 0, to: rows.count, by: chunk).enumerated() {
                emit(type: "table", title: index == 0 ? title : clip(title + " (continued)", 120),
                     ["columns": columns, "rows": Array(rows[start..<min(start + chunk, rows.count)])])
            }
        } else {
            // Wide tables read as one card per row: first cell as its title.
            flushHeading()
            if let named { append(.heading(named, level: 3)) }
            for (index, row) in rows.enumerated() where !full {
                let entries = (1..<width).compactMap { col -> (String, String)? in
                    row[col].isEmpty ? nil : (columns[col], row[col])
                }
                let rowTitle = row[0].isEmpty ? "Row \(index + 1)" : clip(row[0], 120)
                if entries.isEmpty { appendText(Self.markdownEscaped(rowTitle)) }
                else { emitEntries(entries, title: rowTitle) }
            }
        }
    }

    private mutating func emitEntries(_ entries: [(String, String)], title: String) {
        for (index, start) in stride(from: 0, to: entries.count, by: 20).enumerated() {
            let chunk = entries[start..<min(start + 20, entries.count)]
            emit(type: "keyValue", title: index == 0 ? title : clip(title + " (continued)", 120),
                 ["entries": chunk.map { ["label": clip($0.0, 120), "value": clip($0.1, 512)] }])
        }
    }

    private mutating func chart(_ bars: [(String, Double)], title: String) {
        for (index, start) in stride(from: 0, to: bars.count, by: 12).enumerated() {
            let chunk = bars[start..<min(start + 12, bars.count)]
            emit(type: "chart", title: index == 0 ? title : clip(title + " (continued)", 120),
                 ["bars": chunk.map { ["label": $0.0, "value": $0.1] as [String: Any] }])
        }
    }

    /// Builds one v1 document and decodes it with the `wonder-ui` decoder, so a
    /// converted page can never hold anything a reply could not.
    private mutating func emit(type: String, title: String, _ fields: [String: Any]) {
        flushParagraph()
        flushHeading()
        guard !full else { truncated = true; return }
        componentIndex += 1
        var component = fields
        component["id"] = "html-\(componentIndex)"
        component["type"] = type
        component["title"] = clip(title.isEmpty ? "Details" : title, 120)
        let object: [String: Any] = ["version": 1, "fallback": component["title"]!, "components": [component]]
        guard let data = try? JSONSerialization.data(withJSONObject: object),
              let document = try? JSONDecoder().decode(NativeUIDocument.self, from: data) else {
            omit()
            return
        }
        items += document.displayItemCount
        append(.interface(document))
    }

    // MARK: Patterns

    /// Two or more tiles, each a short figure (contains a digit) and a label,
    /// such as a dashboard's headline numbers.
    /// Each tile is a figure, its label and optionally a change ("+12%",
    /// "↓ 3"); the figure's or change's colour variable gives its tone.
    private func statTiles(_ node: Node) -> [(label: String, value: String, delta: String?, tone: NativeUITone?)]? {
        guard node.children.allSatisfy({ !$0.isText || HTMLTree.collapse($0.text ?? "").isEmpty }) else { return nil }
        let tiles = node.children.filter { !$0.isText }
        guard (2...20).contains(tiles.count) else { return nil }
        var result: [(String, String, String?, NativeUITone?)] = []
        for tile in tiles {
            let parts = tile.children.filter { !$0.isText || !HTMLTree.collapse($0.text ?? "").isEmpty }
            guard (2...3).contains(parts.count), parts.allSatisfy({ !$0.isText && !containsStructure($0, includingSelf: false) }) else { return nil }
            let texts = parts.map { plain($0) }
            guard texts.allSatisfy({ !$0.isEmpty }) else { return nil }
            let deltaIndex = parts.count == 3 ? texts.indices.last(where: { Self.isDelta(texts[$0]) }) : nil
            guard parts.count == 2 || deltaIndex != nil else { return nil }
            let rest = texts.indices.filter { $0 != deltaIndex }
            guard let valueIndex = rest.first(where: { Self.isFigure(texts[$0]) }),
                  let labelIndex = rest.first(where: { $0 != valueIndex }) else { return nil }
            let tone = self.tone(parts[valueIndex]) ?? deltaIndex.flatMap { self.tone(parts[$0]) }
            result.append((clip(texts[labelIndex], 120), clip(texts[valueIndex], 40), deltaIndex.map { clip(texts[$0], 40) },
                           tone.flatMap { [.success, .warning, .error, .info, .accent].contains($0) ? $0 : nil }))
        }
        return result
    }

    static func isDelta(_ text: String) -> Bool {
        guard let first = text.first, text.unicodeScalars.count <= 16 else { return false }
        return "+-−↑↓▲▼".contains(first) && text.contains(where: \.isNumber)
    }

    // MARK: Style-driven patterns

    private func css(_ node: Node) -> [String: String] {
        let key = ObjectIdentifier(node)
        if let hit = resolved.values[key] { return hit }
        let value = styles.style(tag: node.name, classes: node.classes, inline: node.attribute("style"))
        resolved.values[key] = value
        return value
    }

    /// The meaning of an element's colours: its background, then its text.
    private func tone(_ node: Node) -> NativeUITone? {
        let style = css(node)
        for key in ["background", "background-color", "color", "border-color", "border-left", "border-left-color", "border"] {
            if let tone = HTMLStyleSheet.tone(in: style[key]) { return tone }
        }
        return nil
    }

    static func pixels(_ value: String?) -> Double? {
        guard let value, value.hasSuffix("px") else { return nil }
        return Double(value.dropLast(2)).flatMap { $0.isFinite ? $0 : nil }
    }

    static func percent(_ value: String?) -> Double? {
        guard let value, value.hasSuffix("%"), let number = Double(value.dropLast()), (1...99).contains(number) else { return nil }
        return number
    }

    /// `4/3`, `3 / 2.3` or `1.5`.
    static func aspect(_ value: String?) -> Double? {
        guard let value else { return nil }
        let parts = value.split(separator: "/").map { Double($0.trimmingCharacters(in: .whitespaces)) }
        if parts.count == 2, let width = parts[0], let height = parts[1], height > 0 { return width / height }
        if parts.count == 1, let ratio = parts[0], ratio > 0 { return ratio }
        return nil
    }

    static func grow(_ style: [String: String]) -> Double? {
        let raw = style["flex-grow"] ?? style["flex"].flatMap { $0.split(separator: " ").first.map(String.init) }
        guard let raw, let value = Double(raw), value.isFinite, value > 0 else { return nil }
        return value
    }

    /// A flex box with a fixed shape whose children are areas (panes, a chat,
    /// placeholder lines, thin dividers), with at most one card or cover over
    /// it and one pinned label: a sketch of a screen. Anything else is not.
    private func mockup(_ node: Node) -> [String: Any]? {
        guard !node.isText, !Self.inline.contains(node.name), !Self.skipped.contains(node.name) else { return nil }
        let style = css(node)
        guard let aspect = Self.aspect(style["aspect-ratio"]), (0.25...4).contains(aspect),
              (style["display"] ?? "").contains("flex"),
              node.children.allSatisfy({ !$0.isText || HTMLTree.collapse($0.text ?? "").isEmpty }) else { return nil }
        let horizontal = !(style["flex-direction"] ?? "").hasPrefix("column")
        let elements = node.children.filter { !$0.isText }
        let layered = elements.filter { ["absolute", "fixed"].contains(css($0)["position"] ?? "") }
        let flow = elements.filter { child in !layered.contains { $0 === child } }
        guard flow.count <= 6, layered.count <= 4 else { return nil }
        var regions: [[String: Any]] = []
        var percents: [Int: Double] = [:]
        for (index, child) in flow.enumerated() {
            guard let (region, percent) = mockupRegion(child, horizontal: horizontal) else { return nil }
            regions.append(region)
            if let percent, region["kind"] as? String != "divider" { percents[index] = percent }
        }
        var result: [String: Any] = [:]
        var dim = false
        var overlay: [String: Any]?
        for child in layered {
            let text = plain(child)
            let kids = child.children.filter { !$0.isText }
            if text.isEmpty, kids.isEmpty { dim = true; continue }
            if kids.allSatisfy({ Self.inline.contains($0.name) }) {
                // A short label pinned over the sketch.
                guard result["badge"] == nil, text.unicodeScalars.count <= 60 else { return nil }
                var badge: [String: Any] = ["text": text]
                if let tone = tone(child) { badge["tone"] = tone.rawValue }
                result["badge"] = badge
                continue
            }
            guard overlay == nil else { return nil }
            let place = css(child)
            let covers = place["inset"] == "0" || ["left", "right", "top", "bottom"].allSatisfy { ["0", "0px"].contains(place[$0] ?? "") }
            let inner = kids.count == 1 && kids[0].children.contains(where: { !$0.isText }) ? kids[0] : child
            guard let (region, _) = mockupRegion(inner, horizontal: false), ["pane", "chat"].contains(region["kind"] as? String ?? "")
            else { return nil }
            overlay = ["style": covers ? "full" : "sheet", "region": region]
        }
        // A cover with nothing beneath it is the screen itself.
        if regions.isEmpty, let cover = overlay, cover["style"] as? String == "full", var region = cover["region"] as? [String: Any] {
            region["weight"] = 1
            regions = [region]; overlay = nil
        }
        guard (1...6).contains(regions.count), regions.contains(where: { $0["kind"] as? String != "divider" }) else { return nil }
        // An area sized by percent takes that share of the length; the
        // flexible ones share the rest by their weights.
        let percentTotal = percents.values.reduce(0, +)
        if !percents.isEmpty, percentTotal < 100 {
            let flexible = regions.indices.filter { percents[$0] == nil && regions[$0]["kind"] as? String != "divider" }
            let flexTotal = flexible.map { regions[$0]["weight"] as? Double ?? 1 }.reduce(0, +)
            for (index, share) in percents {
                regions[index]["weight"] = flexTotal > 0 ? share / (100 - percentTotal) * flexTotal : share / 100
            }
        }
        for index in regions.indices where regions[index]["weight"] != nil {
            let weight = regions[index]["weight"] as? Double ?? 1
            regions[index]["weight"] = (min(10, max(0.05, weight)) * 1000).rounded() / 1000
        }
        if var cover = overlay {
            cover["dim"] = dim
            result["overlay"] = cover
        }
        result["aspect"] = (aspect * 1000).rounded() / 1000
        result["direction"] = horizontal ? "horizontal" : "vertical"
        result["regions"] = regions
        return result
    }

    /// One area of a sketch and, when its size is a percentage of the
    /// sketch, that percentage.
    private func mockupRegion(_ node: Node, horizontal: Bool) -> ([String: Any], Double?)? {
        let style = css(node)
        let text = plain(node)
        if let thickness = Self.pixels(style[horizontal ? "width" : "height"]), thickness <= 24 {
            var region: [String: Any] = ["kind": "divider"]
            if (1...3).contains(text.unicodeScalars.count) { region["glyph"] = text } else if !text.isEmpty { return nil }
            return (region, nil)
        }
        let percent = Self.percent(style[horizontal ? "width" : "height"])
        let elements = node.children.filter { !$0.isText }
        var region: [String: Any]
        if text.isEmpty {
            // Bars without words: a chat when one sits at the end or a
            // composer is pushed to the bottom, otherwise placeholder lines.
            guard !elements.isEmpty else { return nil }
            let composer = elements.filter { css($0)["margin-top"] == "auto" }
            let bubbles = elements.filter { css($0)["margin-top"] != "auto" }
            func isUser(_ bubble: Node) -> Bool { ["flex-end", "end", "self-end"].contains(css(bubble)["align-self"] ?? "") }
            if !composer.isEmpty || bubbles.contains(where: isUser) {
                region = ["kind": "chat", "bubbles": bubbles.prefix(8).map { isUser($0) ? "user" : "agent" }, "composer": !composer.isEmpty]
            } else {
                region = ["kind": "lines", "count": min(8, elements.count)]
            }
        } else {
            // A pane: a short title and its content, tinted by the content's colour.
            let runs = textRuns(node).map { clip($0, 60) }
            let toned = firstToned(node, depth: 0)
            let label = toned.map { clip(plain($0), 60) } ?? (runs.count >= 2 ? clip(runs.dropFirst().joined(separator: " "), 60) : runs.first)
            let title = runs.first { $0 != label }
            guard title != nil || label != nil else { return nil }
            region = ["kind": "pane"]
            if let title { region["title"] = title }
            if let label, !label.isEmpty { region["label"] = label }
            if let toned, let tone = tone(toned) { region["tone"] = tone.rawValue }
        }
        region["weight"] = Self.grow(style) ?? 1
        return (region, percent)
    }

    private func firstToned(_ node: Node, depth: Int) -> Node? {
        guard depth < 4 else { return nil }
        for child in node.children where !child.isText {
            if tone(child) != nil, !plain(child).isEmpty { return child }
            if let found = firstToned(child, depth: depth + 1) { return found }
        }
        return nil
    }

    private mutating func emitMockup(_ sketch: [String: Any], caption: String?) {
        var fields = sketch
        if let caption { fields["caption"] = clip(caption, 500) }
        emit(type: "mockup", title: "Layout sketch", fields)
    }

    /// Two or more short coloured labels side by side (status chips).
    private func badges(_ node: Node) -> [[String: String]]? {
        guard node.children.allSatisfy({ !$0.isText || HTMLTree.collapse($0.text ?? "").isEmpty }) else { return nil }
        let elements = node.children.filter { !$0.isText }
        guard (2...12).contains(elements.count) else { return nil }
        var result: [[String: String]] = []
        for element in elements {
            guard ["span", "mark", "small", "b", "strong"].contains(element.name), element.children.allSatisfy(\.isText),
                  let tone = tone(element) else { return nil }
            let text = plain(element)
            guard !text.isEmpty, text.unicodeScalars.count <= 40 else { return nil }
            result.append(["text": text, "tone": tone.rawValue])
        }
        return result
    }

    /// Every item ticked or not: a box or mark first, or a checkbox input.
    private func checklist(_ entries: [Node]) -> [(text: String, done: Bool)]? {
        guard entries.count >= 2 else { return nil }
        var result: [(String, Bool)] = []
        for entry in entries {
            if let box = entry.children.first(where: { $0.name == "input" && $0.attribute("type")?.lowercased() == "checkbox" }) {
                let text = plain(entry)
                guard !text.isEmpty else { return nil }
                result.append((clip(text, 256), box.attribute("checked") != nil))
                continue
            }
            let text = plain(entry)
            guard let mark = text.first else { return nil }
            let rest = clip(String(text.dropFirst()), 256)
            guard !rest.isEmpty else { return nil }
            if "✓✔☑✅".contains(mark) { result.append((rest, true)) }
            else if "☐◻⬜□".contains(mark) { result.append((rest, false)) }
            else { return nil }
        }
        return result
    }

    /// A list marked as steps or a timeline, or whose every item carries a
    /// status class (done, current, pending, failed and their synonyms).
    private func steps(_ node: Node, _ entries: [Node]) -> [[String: String]]? {
        guard entries.count >= 2 else { return nil }
        func status(_ entry: Node) -> String? {
            for name in entry.classes {
                switch name {
                case "done", "complete", "completed", "finished", "ok", "passed": return "done"
                case "current", "active", "doing", "now", "in-progress", "running": return "current"
                case "pending", "todo", "upcoming", "next", "later", "queued": return "pending"
                case "failed", "fail", "blocked", "error": return "failed"
                default: continue
                }
            }
            return nil
        }
        let named = node.classes.contains { $0.contains("step") || $0.contains("timeline") }
        guard named || entries.allSatisfy({ status($0) != nil }) else { return nil }
        return entries.compactMap { entry in
            let strong = entry.children.first { ["b", "strong", "h3", "h4", "h5"].contains($0.name) }.map { plain($0) }
            let text = plain(entry)
            guard !text.isEmpty else { return nil }
            var step = ["status": status(entry) ?? "pending"]
            if let strong, !strong.isEmpty, strong != text {
                step["title"] = clip(strong, 120)
                let detail = String(text.dropFirst(text.hasPrefix(strong) ? strong.count : 0))
                    .trimmingCharacters(in: CharacterSet.whitespaces.union(.punctuationCharacters))
                if !detail.isEmpty { step["detail"] = clip(detail, 512) }
            } else {
                step["title"] = clip(text, 120)
            }
            return step
        }
    }

    /// A picture carried in the page: a `data:` URL of a PNG, JPEG, GIF or
    /// WebP whose bytes really are that format, within the size limits.
    private mutating func embeddedImage(_ node: Node, caption: String?) -> HTMLVisualPage.Image? {
        guard let source = node.attribute("src"), source.hasPrefix("data:image/"),
              let comma = source.firstIndex(of: ","), source[..<comma].hasSuffix(";base64"),
              source.utf8.count <= HTMLVisualPage.Limits.imageBytes / 3 * 4 + 64,
              let data = Data(base64Encoded: String(source[source.index(after: comma)...]), options: .ignoreUnknownCharacters),
              Self.isPicture(data) else { return nil }
        guard imageCount < HTMLVisualPage.Limits.images, imageBytes + data.count <= HTMLVisualPage.Limits.imageTotalBytes else {
            skippedPictures += 1
            return nil
        }
        imageCount += 1
        imageBytes += data.count
        let alt = HTMLTree.collapse(node.attribute("alt") ?? "")
        return HTMLVisualPage.Image(data: data, alt: alt.isEmpty ? nil : clip(alt, 240), caption: caption)
    }

    /// A picture in document order; one the page can't carry is counted.
    private mutating func picture(_ node: Node, caption: String?) {
        let skipped = skippedPictures
        if let image = embeddedImage(node, caption: caption) { appendImage(image) }
        else if skippedPictures == skipped { omit() }
    }

    /// An SVG drawing kept whole, within the drawing budget. The markup
    /// gains the SVG namespace it needs to stand alone as a picture.
    private mutating func drawing(_ node: Node, label: String?) -> HTMLVisualPage.Drawing? {
        guard var svg = node.source, drawings < HTMLVisualPage.Limits.drawings else { return nil }
        if !svg.contains("http://www.w3.org/2000/svg") {
            svg = "<svg xmlns=\"http://www.w3.org/2000/svg\"" + svg.dropFirst(4)
        }
        drawings += 1
        let box = (node.attribute("viewbox") ?? "").split(whereSeparator: { $0 == " " || $0 == "," }).compactMap { Double($0) }
        var aspect = box.count == 4 && box[2] > 0 && box[3] > 0 ? box[2] / box[3] : 0
        if aspect == 0, let width = Self.pixels(node.attribute("width").map { $0.hasSuffix("px") ? $0 : $0 + "px" }),
           let height = Self.pixels(node.attribute("height").map { $0.hasSuffix("px") ? $0 : $0 + "px" }), height > 0 {
            aspect = width / height
        }
        return HTMLVisualPage.Drawing(svg: svg, aspect: aspect > 0 ? min(4, max(0.25, aspect)) : 16.0 / 9.0, label: label)
    }

    /// A short label styled as a pill or tag (rounded, with a fill or a
    /// border) by the page's classes or its own style; its colour's meaning
    /// becomes the badge tone.
    private func badge(_ node: Node) -> NativeUIBadge? {
        guard !node.isText, ["span", "mark", "small", "b", "strong", "em", "code"].contains(node.name),
              node.children.allSatisfy(\.isText) else { return nil }
        let style = css(node)
        guard let radius = style["border-radius"], radius != "0", radius != "0px",
              style["background"] != nil || style["background-color"] != nil || style["border"] != nil
                || style["border-color"] != nil else { return nil }
        var text = plain(node)
        guard !text.isEmpty, text.unicodeScalars.count <= 40 else { return nil }
        if style["text-transform"] == "uppercase" { text = text.uppercased() }
        return NativeUIBadge(text: clip(text, 60), tone: tone(node) ?? .neutral)
    }

    static func isPicture(_ data: Data) -> Bool {
        let head = [UInt8](data.prefix(12))
        return head.starts(with: [137, 80, 78, 71, 13, 10, 26, 10]) || head.starts(with: [255, 216, 255])
            || head.starts(with: Array("GIF8".utf8))
            || (head.starts(with: Array("RIFF".utf8)) && head.count == 12 && Array(head[8..<12]) == Array("WEBP".utf8))
    }

    /// Pictures side by side in the page share one block, shown as a grid;
    /// pictures one under another stay apart, full width.
    private mutating func appendImage(_ image: HTMLVisualPage.Image) {
        flushParagraph()
        flushHeading()
        items += 1
        defer { lastPictureRow = pictureRow }
        if case .images(let images)? = blocks.last, lead.isEmpty, pictureRow != 0, lastPictureRow == pictureRow {
            blocks[blocks.count - 1] = .images(images + [image])
        } else {
            append(.images([image]))
        }
    }

    /// Rows that each hold a label and a styled bar (`width: 40%`).
    private func cssBars(_ node: Node) -> [(String, Double)]? {
        let rows = node.children.filter { !$0.isText }
        guard (2...50).contains(rows.count) else { return nil }
        var bars: [(String, Double)] = []
        for row in rows {
            guard let percent = barWidth(in: row) else { return nil }
            let runs = textRuns(row)
            guard let label = runs.first(where: { Self.number($0) == nil }) else { return nil }
            let value = runs.last(where: { Self.number($0) != nil }).flatMap(Self.number) ?? percent
            bars.append((clip(label, 120), value))
        }
        return bars
    }

    private func barWidth(in node: Node) -> Double? {
        if let style = node.attribute("style")?.lowercased().replacingOccurrences(of: " ", with: ""),
           plain(node).isEmpty,
           let range = style.range(of: "width:"), !style[..<range.lowerBound].hasSuffix("-") {
            let rest = style[range.upperBound...]
            let digits = rest.prefix(while: { $0.isNumber || $0 == "." })
            if rest.dropFirst(digits.count).hasPrefix("%"), let value = Double(digits), value.isFinite, value <= 100 {
                return value
            }
        }
        for child in node.children where !child.isText {
            if let value = barWidth(in: child) { return value }
        }
        return nil
    }

    /// Bars drawn with `rect`: two or more sharing a baseline with differing
    /// lengths. Labels are the nearest non-numeric text across the bar's axis.
    private mutating func svg(_ node: Node) {
        let label = label(of: node)
        if let bars = svgBars(node), bars.count >= 2 {
            chart(bars, title: takeHeading() ?? label ?? "Chart")
        } else if let drawing = drawing(node, label: label) {
            flushHeading()
            items += 1
            append(.drawing(drawing))
        } else {
            omit()
        }
    }

    private func svgBars(_ node: Node) -> [(String, Double)]? {
        struct Rect { let x, y, w, h: Double }
        struct Label { let x, y: Double; let text: String }
        var rects: [Rect] = [], labels: [Label] = []
        func number(_ key: String, _ element: Node) -> Double? {
            guard let raw = element.attribute(key)?.trimmingCharacters(in: .whitespaces), !raw.hasSuffix("%") else { return nil }
            return Double(raw.hasSuffix("px") ? String(raw.dropLast(2)) : raw)
        }
        func walk(_ element: Node, depth: Int) {
            guard depth < 32 else { return }
            if element.name == "rect", let w = number("width", element), let h = number("height", element), w > 0, h > 0 {
                rects.append(Rect(x: number("x", element) ?? 0, y: number("y", element) ?? 0, w: w, h: h))
            } else if element.name == "text" {
                let text = plain(element)
                if !text.isEmpty { labels.append(Label(x: number("x", element) ?? 0, y: number("y", element) ?? 0, text: text)) }
                return
            }
            for child in element.children where !child.isText { walk(child, depth: depth + 1) }
        }
        walk(node, depth: 0)
        let box = (node.attribute("viewbox") ?? "").split(whereSeparator: { $0 == " " || $0 == "," }).compactMap { Double($0) }
        let width = box.count == 4 ? box[2] : number("width", node) ?? .infinity
        let height = box.count == 4 ? box[3] : number("height", node) ?? .infinity
        let candidates = rects.filter { $0.w < width * 0.9 && $0.h < height * 0.9 }
        guard candidates.count >= 2, candidates.count <= 200 else { return nil }

        func group(_ key: (Rect) -> Double) -> [Rect] {
            Dictionary(grouping: candidates, by: { (key($0)).rounded() }).values.max(by: { $0.count < $1.count }) ?? []
        }
        let horizontal = group { $0.x }.sorted { $0.y < $1.y }
        let vertical = group { $0.y + $0.h }.sorted { $0.x < $1.x }
        let isHorizontal = horizontal.count >= 2 && Set(horizontal.map { $0.w.rounded() }).count > 1
            && Set(horizontal.map { $0.h.rounded() }).count == 1
        let isVertical = vertical.count >= 2 && Set(vertical.map { $0.h.rounded() }).count > 1
            && Set(vertical.map { $0.w.rounded() }).count == 1
        let useHorizontal = isHorizontal && (!isVertical || horizontal.count >= vertical.count)
        guard useHorizontal || isVertical else { return nil }
        let bars = useHorizontal ? horizontal : vertical
        let names = labels.filter { Self.number($0.text) == nil }
        let values = labels.filter { Self.number($0.text) != nil }
        let across: (Label) -> Double = { useHorizontal ? $0.y : $0.x }
        let result = bars.compactMap { bar -> (String, Double)? in
            let center = useHorizontal ? bar.y + bar.h / 2 : bar.x + bar.w / 2
            let tolerance = useHorizontal ? max(bar.h, 12) : max(bar.w, 12)
            guard let name = names.min(by: { abs(across($0) - center) < abs(across($1) - center) }),
                  abs(across(name) - center) <= tolerance else { return nil }
            let value = values.filter { abs(across($0) - center) <= tolerance }
                .min(by: { abs(across($0) - center) < abs(across($1) - center) }).flatMap { Self.number($0.text) }
            return (clip(name.text, 120), value ?? (useHorizontal ? bar.w : bar.h))
        }
        return result.count == bars.count ? result : nil
    }

    /// Lays its children side by side: a grid of two or more columns, or a
    /// flex row.
    private func isRow(_ node: Node) -> Bool {
        let style = css(node)
        if let display = style["display"], display.contains("flex"), !(style["flex-direction"] ?? "").hasPrefix("column") {
            return true
        }
        guard let columns = style["grid-template-columns"] else { return false }
        if let range = columns.range(of: "repeat("),
           let count = Int(columns[range.upperBound...].prefix { $0.isNumber }) { return count >= 2 }
        var depth = 0, tracks = 0, inTrack = false
        for character in columns {
            if character == "(" { depth += 1 } else if character == ")" { depth -= 1 }
            if character == " " && depth == 0 { inTrack = false } else if !inTrack { inTrack = true; tracks += 1 }
        }
        return tracks >= 2
    }

    // MARK: Text extraction

    private func isCallout(_ node: Node) -> Bool {
        if node.name == "blockquote" || node.name == "aside" { return true }
        if ["note", "alert", "status"].contains(node.attribute("role")?.lowercased() ?? "") { return true }
        return node.classes.contains { Self.calloutClasses.contains($0) || $0.hasPrefix("callout") }
    }

    private func containsStructure(_ node: Node, includingSelf: Bool = true) -> Bool {
        if includingSelf, Self.structural.contains(node.name) { return true }
        return node.children.contains { !$0.isText && containsStructure($0) }
    }

    private func label(of node: Node) -> String? {
        if let aria = node.attribute("aria-label").map(HTMLTree.collapse), !aria.isEmpty { return clip(aria, 120) }
        if let titled = node.children.first(where: { $0.name == "title" || $0.name == "figcaption" }) {
            let text = plain(titled)
            if !text.isEmpty { return clip(text, 120) }
        }
        return nil
    }

    /// Visible text with whitespace collapsed; scripts and styles excluded.
    private func plain(_ node: Node) -> String {
        var result = ""
        func walk(_ node: Node) {
            if let text = node.text { result += text; return }
            if Self.skipped.contains(node.name) { return }
            if node.name == "br" { result += " "; return }
            let block = !Self.inline.contains(node.name)
            if block { result += " " }
            var previousElement = false
            for child in node.children {
                if child.text == nil, previousElement { result += " " }
                walk(child)
                if let text = child.text { if !HTMLTree.collapse(text).isEmpty { previousElement = false } }
                else { previousElement = true }
            }
            if block { result += " " }
        }
        walk(node)
        return HTMLTree.collapse(result)
    }

    private func raw(_ node: Node) -> String {
        if let text = node.text { return text }
        if node.name == "br" { return "\n" }
        return node.children.map(raw).joined()
    }

    private func textRuns(_ node: Node) -> [String] {
        if let text = node.text {
            let value = HTMLTree.collapse(text)
            return value.isEmpty ? [] : [value]
        }
        if Self.skipped.contains(node.name) { return [] }
        return node.children.flatMap(textRuns)
    }

    /// Inline Markdown for a run of inline content; `\u{2028}` marks a hard break.
    private func inlineMarkdown(_ node: Node) -> String {
        if let text = node.text { return Self.markdownEscaped(text) }
        if Self.skipped.contains(node.name) { return "" }
        switch node.name {
        case "br": return "\u{2028}"
        case "code", "kbd", "samp", "tt":
            let text = plain(node)
            guard !text.isEmpty else { return "" }
            return text.contains("`") ? Self.markdownEscaped(text) : "`" + text + "`"
        default: break
        }
        if let label = badge(node) { return " **" + Self.markdownEscaped(label.text) + "** " }
        let inner = node.children.map(inlineMarkdown).joined()
        switch node.name {
        case "s", "del", "strike": return Self.wrap(inner, "~~")
        case "b", "strong": return Self.wrap(inner, "**")
        case "i", "em", "cite", "dfn": return Self.wrap(inner, "*")
        case "p", "div", "li", "section", "header", "footer", "h1", "h2", "h3", "h4", "h5", "h6", "tr", "dd", "dt":
            return "\u{2028}" + inner + "\u{2028}"
        case "td", "th": return " " + inner + " "
        default: return inner
        }
    }

    private func blockMarkdown(_ node: Node) -> String {
        let text = node.children.map(inlineMarkdown).joined()
        return text.components(separatedBy: "\u{2028}").map(HTMLTree.collapse).filter { !$0.isEmpty }.joined(separator: "\n")
    }

    private static func wrap(_ inner: String, _ marker: String) -> String {
        let trimmed = HTMLTree.collapse(inner.replacingOccurrences(of: "\u{2028}", with: " "))
        guard !trimmed.isEmpty else { return inner }
        let lead = inner.first.map { $0.isWhitespace } == true ? " " : ""
        let trail = inner.last.map { $0.isWhitespace } == true ? " " : ""
        return lead + marker + trimmed + marker + trail
    }

    static func markdownEscaped(_ text: String) -> String {
        var result = ""
        result.reserveCapacity(text.utf8.count)
        // Text that starts like a list item ("+ training", "- note", "3. ")
        // would turn into a bullet or number when it begins a line.
        let lead = text.prefix { $0.isWhitespace }
        let body = text.dropFirst(lead.count)
        var marker: String.Index?
        if let first = body.first, "+-".contains(first), body.dropFirst().first?.isWhitespace == true {
            marker = body.startIndex
        } else if let end = body.firstIndex(where: { !$0.isNumber }), end > body.startIndex, ".)".contains(body[end]),
                  body[body.index(after: end)...].first?.isWhitespace == true {
            marker = end
        }
        for index in text.indices {
            let character = text[index]
            if "\\`*_[]<>#|~".contains(character) || index == marker { result.append("\\") }
            result.append(character)
        }
        return result
    }

    private func clip(_ text: String, _ limit: Int) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.unicodeScalars.count > limit else { return trimmed }
        var scalars = String.UnicodeScalarView()
        scalars.append(contentsOf: trimmed.unicodeScalars.prefix(limit - 1))
        return String(scalars).trimmingCharacters(in: .whitespaces) + "…"
    }

    static func isFigure(_ text: String) -> Bool {
        text.unicodeScalars.count <= 16 && text.contains(where: \.isNumber)
    }

    /// "1,500", "~21.6k", "42%", "$3.99" → a non-negative finite value.
    static func number(_ text: String) -> Double? {
        var value = text.trimmingCharacters(in: .whitespaces)
        guard !value.isEmpty, value.count <= 24 else { return nil }
        for prefix in ["~", "≈", "$", "€", "£", "+"] where value.hasPrefix(prefix) { value.removeFirst() }
        value = value.replacingOccurrences(of: ",", with: "").replacingOccurrences(of: "\u{2009}", with: "")
        var scale = 1.0
        if value.hasSuffix("%") { value.removeLast() }
        else if let last = value.last, let factor = ["k": 1e3, "K": 1e3, "M": 1e6, "B": 1e9][last] {
            value.removeLast(); scale = factor
        }
        guard value.contains(where: \.isNumber), value.allSatisfy({ $0.isASCII && ($0.isNumber || $0 == ".") }),
              let number = Double(value), number.isFinite else { return nil }
        let result = number * scale
        return result <= 1_000_000_000 ? result : nil
    }
}

// MARK: - View

/// Lays out a prepared page; conversion already happened on the cache actor.
/// Rows are a lazy stack because a page can hold hundreds of blocks.
struct HTMLVisualPageView<Markdown: View>: View {
    let page: HTMLVisualPage
    var theme = NativeUITheme()
    var fonts = NativeUIFonts()
    /// Inside a conversation row the page is a bounded prefix in a plain
    /// stack: the conversation's own lazy stack must not hold another.
    var lazy = true
    /// Draws an SVG drawing as a picture; the app uses its sandboxed web
    /// view. Without one (the host snapshot package) the drawing is a framed
    /// placeholder naming it.
    var drawing: ((HTMLVisualPage.Drawing) -> AnyView)? = nil
    @ViewBuilder var markdown: (String) -> Markdown

    var body: some View {
        Group {
            if lazy {
                LazyVStack(alignment: .leading, spacing: 14) { content }
            } else {
                VStack(alignment: .leading, spacing: 14) { content }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("html-visual-page")
    }

    @ViewBuilder private var content: some View {
        if let title = page.title {
            Text(title).font(fonts.title).fontWeight(.bold).accessibilityAddTraits(.isHeader)
                .textSelection(.enabled)
        }
        ForEach(page.blocks.indices, id: \.self) { index in
            block(page.blocks[index])
        }
        if let note = page.partialNote {
            Text(note)
                .font(fonts.caption).foregroundStyle(theme.secondaryText)
                .accessibilityIdentifier("html-visual-omitted")
        }
    }

    @ViewBuilder private func block(_ block: HTMLVisualPage.Block) -> some View {
        switch block {
        case .labeled(let badges, let text):
            // The labels sit on their own line above the text they label. Side
            // by side in an HStack, the Markdown text's wrapping crashed the
            // conversation's self-sizing rows (simulator, Oct 23).
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    ForEach(badges.indices, id: \.self) { index in
                        NativeUIBadgeView(badge: badges[index], theme: theme, font: fonts.caption)
                    }
                }
                if !text.isEmpty { markdown(text) }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityElement(children: .combine)
        case .drawing(let sketch):
            Group {
                if let drawing { drawing(sketch) }
                else {
                    RoundedRectangle(cornerRadius: 8).fill(theme.surface)
                        .overlay { Image(systemName: "scribble").foregroundStyle(theme.secondaryText) }
                }
            }
            .aspectRatio(sketch.aspect, contentMode: .fit)
            .frame(maxWidth: .infinity)
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .accessibilityElement()
            .accessibilityLabel(sketch.label ?? "Drawing")
            .accessibilityAddTraits(.isImage)
        case .details(let summary, let inner):
            HTMLVisualDetails(summary: summary, fonts: fonts) {
                AnyView(VStack(alignment: .leading, spacing: 14) {
                    ForEach(inner.indices, id: \.self) { index in self.block(inner[index]) }
                })
            }
        case .rule:
            theme.separator.frame(height: 1).accessibilityHidden(true)
        case .heading(let text, let level):
            Text(text).font(level <= 2 ? fonts.section : fonts.heading).fontWeight(level <= 2 ? .semibold : nil)
                .padding(.top, level <= 2 ? 10 : 4)
                .accessibilityAddTraits(.isHeader)
        case .text(let text):
            markdown(text)
        case .callout(let text, let tone):
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: NativeUITheme.symbol(tone)).foregroundStyle(theme.color(tone))
                    .accessibilityLabel(NativeUITheme.spoken(tone) ?? "Note")
                markdown(text)
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(theme.color(tone).opacity(0.12), in: RoundedRectangle(cornerRadius: 8))
            .accessibilityElement(children: .combine)
        case .images(let images):
            let minimum: CGFloat = images.count == 1 ? 600 : 150
            let pictures = ForEach(images.indices, id: \.self) { index in
                HTMLVisualImageView(image: images[index], theme: theme, fonts: fonts)
            }
            // Only the full-page sheet's own lazy stack may hold a lazy grid.
            if lazy {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: minimum), spacing: 8, alignment: .top)],
                          alignment: .leading, spacing: 8) { pictures }
            } else {
                NativeUIAdaptiveGrid(minimum: minimum, spacing: 8, anchor: .top) { pictures }
            }
        case .code(let text):
            ScrollView(.horizontal) {
                Text(text).font(.system(.footnote, design: .monospaced)).textSelection(.enabled)
                    .fixedSize(horizontal: true, vertical: false).padding(12)
            }
            .background(theme.surface, in: RoundedRectangle(cornerRadius: 8))
        case .interface(let document) where document.components.allSatisfy({ $0.type == "mockup" }):
            // A sketch carries its own frame.
            NativeUIDocumentView(document: document, theme: theme, fonts: fonts)
        case .interface(let document):
            NativeUIDocumentView(document: document, theme: theme, fonts: fonts)
                .padding(12)
                .background(theme.surface, in: RoundedRectangle(cornerRadius: 8))
        }
    }
}

/// `<details>`: closed until opened, as on the web.
private struct HTMLVisualDetails: View {
    let summary: String
    let fonts: NativeUIFonts
    let content: () -> AnyView
    @State private var open = false
    var body: some View {
        DisclosureGroup(isExpanded: $open) {
            content().padding(.top, 8)
        } label: {
            Text(summary).font(fonts.heading)
        }
    }
}

/// A picture embedded in the page, decoded and downsampled off the main
/// actor when it appears and released when it leaves.
private struct HTMLVisualImageView: View {
    let image: HTMLVisualPage.Image
    let theme: NativeUITheme
    let fonts: NativeUIFonts
    @State private var picture: CGImage?
    @State private var failed = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Group {
                if let picture {
                    Image(decorative: picture, scale: 1).resizable().aspectRatio(contentMode: .fit)
                } else {
                    theme.surface.aspectRatio(4 / 3, contentMode: .fit)
                        .overlay { if failed { Image(systemName: "photo").foregroundStyle(theme.secondaryText) } else { ProgressView() } }
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .accessibilityElement()
            .accessibilityLabel(image.alt ?? image.caption ?? "Picture")
            .accessibilityAddTraits(.isImage)
            if let caption = image.caption {
                Text(caption).font(fonts.caption).foregroundStyle(theme.secondaryText).textSelection(.enabled)
            }
        }
        .task {
            let data = image.data
            let decoded = await Task.detached(priority: .userInitiated) { Self.thumbnail(data, maxPixels: 1_200) }.value
            guard !Task.isCancelled else { return }
            picture = decoded
            failed = decoded == nil
        }
        .onDisappear { picture = nil }
    }

    nonisolated static func thumbnail(_ data: Data, maxPixels: Int) -> CGImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary) else { return nil }
        return CGImageSourceCreateThumbnailAtIndex(source, 0, [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixels,
            kCGImageSourceShouldCacheImmediately: true,
        ] as CFDictionary)
    }
}
