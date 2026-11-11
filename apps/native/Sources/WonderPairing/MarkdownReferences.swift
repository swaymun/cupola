import Foundation

/// Where a link or image in a Markdown file points, resolved against the file's
/// folder in its workspace root. A reference never leaves the root: `..` past
/// the root, home paths and non-web schemes are blocked.
public enum MarkdownReference: Equatable, Sendable {
    /// A root-relative path such as `docs/screenshot.png`.
    case local(String)
    case web(URL)
    /// A heading in the same file (`#install`).
    case anchor
    case blocked

    /// - Parameter documentPath: the Markdown file's root-relative path, such as `README.md`.
    public static func resolve(_ raw: String, documentPath: String) -> Self {
        var text = raw.trimmingCharacters(in: .whitespaces)
        if text.hasPrefix("<"), text.hasSuffix(">") { text = String(text.dropFirst().dropLast()) }
        guard !text.isEmpty, text.utf8.count <= 4096 else { return .blocked }
        if text.hasPrefix("#") { return .anchor }
        if let colon = text.firstIndex(of: ":"),
           text[..<colon].range(of: #"^[A-Za-z][A-Za-z0-9+.-]*$"#, options: .regularExpression) != nil {
            guard let url = URL(string: text), ["http", "https"].contains(url.scheme?.lowercased() ?? "") else { return .blocked }
            return .web(url)
        }
        // Query and fragment do not name a file.
        if let cut = text.firstIndex(where: { $0 == "?" || $0 == "#" }) { text = String(text[..<cut]) }
        guard !text.hasPrefix("~"), !text.contains("\\"),
              let decoded = text.removingPercentEncoding, !decoded.contains("\0"), !decoded.isEmpty else { return .blocked }
        // A leading slash means the workspace root, as on GitHub.
        var parts = decoded.hasPrefix("/") ? [] : documentPath.split(separator: "/").dropLast().map(String.init)
        for part in decoded.split(separator: "/", omittingEmptySubsequences: true) {
            switch part {
            case ".": continue
            case "..":
                guard !parts.isEmpty else { return .blocked }
                parts.removeLast()
            default: parts.append(String(part))
            }
        }
        return parts.isEmpty ? .blocked : .local(parts.joined(separator: "/"))
    }

    /// The parent folder and file name of a local path, for finding it in a listing.
    public static func split(_ path: String) -> (folder: String, name: String) {
        guard let slash = path.lastIndex(of: "/") else { return ("", path) }
        return (String(path[..<slash]), String(path[path.index(after: slash)...]))
    }
}

/// A Markdown file prepared for Preview: text runs for the message renderer,
/// and the images that stand on their own lines, which the viewer loads from
/// the workspace. Prepared off the main thread together with the chunks.
public enum MarkdownPreviewPiece: Equatable, Sendable {
    case markdown(String)
    case image(alt: String, source: MarkdownReference, link: MarkdownReference?)

    /// Splits each chunk at lines that hold only local images (Markdown
    /// `![alt](src)`, optionally linked, or an HTML `<img src>`). Lines with web
    /// images, such as badges, keep their current text rendering; a blocked path
    /// stays an image so the viewer can say why it is not shown.
    public static func pieces(_ chunk: String, documentPath: String) -> [Self] {
        var pieces: [Self] = [], text: [Substring] = []
        var fence: Character?
        func flush() {
            guard !text.isEmpty else { return }
            let joined = text.joined(separator: "\n")
            if !joined.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { pieces.append(.markdown(joined)) }
            text = []
        }
        for line in chunk.split(separator: "\n", omittingEmptySubsequences: false) {
            let body = line.drop(while: { $0 == " " })
            if let marker = body.first, marker == "`" || marker == "~", body.prefix(3).allSatisfy({ $0 == marker }) {
                fence = fence == nil ? marker : (fence == marker ? nil : fence)
            }
            if fence == nil, let images = images(in: body, documentPath: documentPath) {
                flush()
                pieces += images
            } else {
                text.append(line)
            }
        }
        flush()
        return pieces
    }

    private static func images(in line: Substring, documentPath: String) -> [Self]? {
        guard line.contains("![") || line.range(of: "<img", options: .caseInsensitive) != nil else { return nil }
        var rest = String(line), found: [Self] = []
        // [![alt](src)](href), ![alt](src "title"), <img ... src="src" ... alt="alt">
        let patterns = [
            #"\[!\[([^\]]*)\]\(\s*(<[^>]*>|[^)\s]+)(?:\s+"[^"]*")?\s*\)\]\(\s*(<[^>]*>|[^)\s]+)(?:\s+"[^"]*")?\s*\)"#,
            #"!\[([^\]]*)\]\(\s*(<[^>]*>|[^)\s]+)(?:\s+"[^"]*")?\s*\)"#,
            #"<img\b[^>]*>"#,
        ]
        for (index, pattern) in patterns.enumerated() {
            guard let regex = try? NSRegularExpression(pattern: pattern, options: .caseInsensitive) else { continue }
            let range = NSRange(rest.startIndex..., in: rest)
            for match in regex.matches(in: rest, range: range) {
                func group(_ n: Int) -> String {
                    Range(match.range(at: n), in: rest).map { String(rest[$0]) } ?? ""
                }
                switch index {
                case 0: found.append(.image(alt: group(1), source: .resolve(group(2), documentPath: documentPath),
                                            link: .resolve(group(3), documentPath: documentPath)))
                case 1: found.append(.image(alt: group(1), source: .resolve(group(2), documentPath: documentPath), link: nil))
                default:
                    let tag = group(0)
                    guard let src = attribute("src", in: tag) else { return nil }
                    found.append(.image(alt: attribute("alt", in: tag) ?? "", source: .resolve(src, documentPath: documentPath), link: nil))
                }
            }
            rest = regex.stringByReplacingMatches(in: rest, range: range, withTemplate: "")
        }
        // Only images (and wrapper tags such as <p align="center">) on this line.
        let leftover = rest.replacingOccurrences(of: #"</?(p|a|div|picture|br|center)\b[^>]*>"#, with: "",
                                                 options: [.regularExpression, .caseInsensitive])
        guard !found.isEmpty, leftover.trimmingCharacters(in: .whitespaces).isEmpty,
              !found.contains(where: { if case .image(_, .web, _) = $0 { return true } else { return false } }) else { return nil }
        return found
    }

    private static func attribute(_ name: String, in tag: String) -> String? {
        let pattern = #"\b"# + name + #"\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#
        guard let regex = try? NSRegularExpression(pattern: pattern, options: .caseInsensitive),
              let match = regex.firstMatch(in: tag, range: NSRange(tag.startIndex..., in: tag)) else { return nil }
        for group in 1...3 { if let range = Range(match.range(at: group), in: tag) { return String(tag[range]) } }
        return nil
    }
}
