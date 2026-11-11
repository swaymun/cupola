import Foundation

/// A colour role for code shown in diffs. Views map roles to their palette.
public enum SyntaxRole: Sendable, Hashable {
    case keyword, string, comment, number, type, attribute, function, punctuation
}

/// A run of one line's text; `role` is nil for plain text.
public struct SyntaxSpan: Sendable, Equatable {
    public let text: String
    public let role: SyntaxRole?
}

/// A small line tokenizer for common source languages, chosen by file
/// extension. It colours keywords, strings, comments, numbers, type names
/// and attributes; it does not parse. Block comments and multi-line strings
/// (triple quotes, JavaScript and Go backticks, Rust strings) carry across
/// lines until `reset()`, which callers use at hunk boundaries. The
/// highlighter is a value: a copy holds that state, so a long file can be
/// tokenized in chunks that resume exactly where the previous one ended.
public struct SyntaxHighlighter: Sendable {
    struct Language: Sendable {
        var keywords: Set<String> = []
        var caseInsensitive = false
        var lineComments: [String] = []
        var block: (open: String, close: String)?
        var quotes: Set<Character> = ["\"", "'"]
        /// Strings that may span lines, opened and closed by the same token;
        /// `escapes` is false for raw strings (Go backticks).
        var multiline: [(token: String, escapes: Bool)] = []
        /// `'` delimits only short character literals (Rust lifetimes, C chars).
        var shortSingleQuote = false
        var capitalizedTypes = false
        var attributePrefixes: Set<Character> = []
        var markup = false
        /// A name followed by `(` is a function.
        var functions = true
        /// A string or name followed by `:` or `=` is a key (JSON, YAML, TOML).
        var keys = false
        var markdown = false
    }

    struct StringDelimiter: Sendable, Equatable {
        let token: [Unicode.Scalar]
        let escapes: Bool
    }

    let language: Language
    /// Delimiters as characters, so matching allocates nothing per character.
    private let lineCommentTokens: [[Unicode.Scalar]]
    private let blockTokens: (open: [Unicode.Scalar], close: [Unicode.Scalar])?
    private let multilineTokens: [StringDelimiter]
    private let quoteCharacters: [Unicode.Scalar]
    private let attributeCharacters: [Unicode.Scalar]
    private var inBlock = false
    private var openString: StringDelimiter?
    /// Marks brackets and operators `.punctuation`; off for diffs, which only
    /// colour words.
    public var emitsPunctuation = false

    public init?(path: String) {
        let name = (path as NSString).lastPathComponent.lowercased()
        let ext = (name as NSString).pathExtension
        guard let language = Self.language(name: name, ext: ext) else { return nil }
        self.init(language)
    }

    private init(_ language: Language) {
        self.language = language
        lineCommentTokens = language.lineComments.map { Array($0.unicodeScalars) }
        blockTokens = language.block.map { (Array($0.open.unicodeScalars), Array($0.close.unicodeScalars)) }
        // Longest first, so `"""` wins over `"`.
        multilineTokens = language.multiline.map { StringDelimiter(token: Array($0.token.unicodeScalars), escapes: $0.escapes) }
            .sorted { $0.token.count > $1.token.count }
        quoteCharacters = language.quotes.flatMap(\.unicodeScalars)
        attributeCharacters = language.attributePrefixes.flatMap(\.unicodeScalars)
    }

    /// A highlighter for a Markdown code fence's info string (`swift`, `ts`,
    /// `bash`, `yml`...); nil when the language is not one it knows.
    public init?(language tag: String) {
        let tag = tag.lowercased().trimmingCharacters(in: .whitespaces)
        let ext = Self.fenceExtensions[tag] ?? tag
        guard !ext.isEmpty, let language = Self.language(name: "code." + ext, ext: ext) else { return nil }
        self.init(language)
    }

    private static let fenceExtensions: [String: String] = [
        "javascript": "js", "node": "js", "typescript": "ts", "python": "py", "python3": "py", "rust": "rs",
        "golang": "go", "bash": "sh", "shell": "sh", "console": "sh", "shell-session": "sh", "zsh": "zsh",
        "markdown": "md", "postgres": "sql", "postgresql": "sql", "mysql": "sql", "sqlite": "sql",
        "c++": "cpp", "objc": "m", "objective-c": "m", "objectivec": "m", "kotlin": "kt", "ruby": "rb",
        "csharp": "cs", "c#": "cs", "yaml": "yml", "xml": "xml", "jsonc": "jsonc", "powershell": "sh",
    ]

    public mutating func reset() { inBlock = false; openString = nil }

    /// Whether the next line starts inside a block comment or a multi-line string.
    public var continuesConstruct: Bool { inBlock || openString != nil }

    public mutating func spans(_ line: String) -> [SyntaxSpan] {
        let scanned = ScannedLine(line)
        return tokens(scanned).map { SyntaxSpan(text: scanned.text($0.range), role: $0.role) }
    }

    /// Appends `line`'s coloured runs at UTF-16 `offset` without building
    /// span text, and returns the line's UTF-16 length.
    public mutating func appendRuns(_ line: String, at offset: Int, to runs: inout [SyntaxRun]) -> Int {
        let scanned = ScannedLine(line)
        var position = offset
        for token in tokens(scanned) {
            let length = scanned.utf16Length(token.range)
            if let role = token.role { runs.append(SyntaxRun(range: NSRange(location: position, length: length), role: role)) }
            position += length
        }
        return scanned.utf16Length(0..<scanned.scalars.count)
    }

    /// One line as Unicode scalars, not Characters: comparing scalars is an
    /// integer compare, which keeps a long file's tokenizing fast. Text is cut
    /// from the UTF-8 bytes, so spans still reassemble the exact line.
    struct ScannedLine {
        let line: String
        let scalars: [Unicode.Scalar]
        private let utf8: [UInt8]
        /// UTF-8 and UTF-16 offsets before each scalar, plus the end.
        private let utf8Offsets: [Int]
        private let utf16Offsets: [Int]

        init(_ line: String) {
            self.line = line
            scalars = Array(line.unicodeScalars)
            utf8 = Array(line.utf8)
            var utf8Offsets = [Int](), utf16Offsets = [Int]()
            utf8Offsets.reserveCapacity(scalars.count + 1); utf16Offsets.reserveCapacity(scalars.count + 1)
            var (u8, u16) = (0, 0)
            for scalar in scalars {
                utf8Offsets.append(u8); utf16Offsets.append(u16)
                u8 += UTF8.width(scalar); u16 += UTF16.width(scalar)
            }
            utf8Offsets.append(u8); utf16Offsets.append(u16)
            self.utf8Offsets = utf8Offsets; self.utf16Offsets = utf16Offsets
        }
        func text(_ range: Range<Int>) -> String {
            if range.lowerBound == 0, range.upperBound == scalars.count { return line }
            return String(decoding: utf8[utf8Offsets[range.lowerBound]..<utf8Offsets[range.upperBound]], as: UTF8.self)
        }
        func utf16Length(_ range: Range<Int>) -> Int { utf16Offsets[range.upperBound] - utf16Offsets[range.lowerBound] }
    }

    private mutating func tokens(_ scanned: ScannedLine) -> [(range: Range<Int>, role: SyntaxRole?)] {
        let line = scanned.line
        let chars = scanned.scalars
        func text(_ range: Range<Int>) -> String { scanned.text(range) }
        var spans: [(range: Range<Int>, role: SyntaxRole?)] = []
        // The run being built. Plain text and adjacent punctuation extend it.
        var pending: (start: Int, end: Int, role: SyntaxRole?)?
        func flush() {
            guard let run = pending else { return }
            spans.append((run.start..<run.end, run.role))
            pending = nil
        }
        func emit(_ range: Range<Int>, _ role: SyntaxRole?) {
            guard !range.isEmpty else { return }
            if let run = pending, run.role == role, role == nil || role == .punctuation, run.end == range.lowerBound {
                pending?.end = range.upperBound
                return
            }
            flush()
            pending = (range.lowerBound, range.upperBound, role)
        }
        func starts(_ token: [Unicode.Scalar], at index: Int) -> Bool {
            guard index + token.count <= chars.count else { return false }
            for (offset, character) in token.enumerated() where chars[index + offset] != character { return false }
            return true
        }
        /// Where a string whose body starts at `index` ends, and whether it closes on this line.
        func stringEnd(from index: Int, _ delimiter: StringDelimiter) -> (end: Int, closed: Bool) {
            var j = index
            while j < chars.count {
                if delimiter.escapes, chars[j] == "\\" { j += 2; continue }
                if starts(delimiter.token, at: j) { return (j + delimiter.token.count, true) }
                j += 1
            }
            return (chars.count, false)
        }
        func identifierChar(_ c: Unicode.Scalar) -> Bool { c.isLetter || c.isNumber || c == "_" || c == "$" }

        func nextNonSpace(after index: Int) -> Unicode.Scalar? {
            var k = index
            while k < chars.count, chars[k] == " " || chars[k] == "\t" { k += 1 }
            return k < chars.count ? chars[k] : nil
        }
        func isKeyMarker(after index: Int) -> Bool {
            guard let next = nextNonSpace(after: index) else { return false }
            if next == ":" { return true }
            guard next == "=" else { return false }
            var k = index
            while k < chars.count, chars[k] == " " { k += 1 }
            return k + 1 >= chars.count || chars[k + 1] != "="
        }

        if language.markdown {
            let trimmed = line.drop(while: { $0 == " " })
            if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
                return [(0..<chars.count, .comment)]
            }
            let hashes = trimmed.prefix(while: { $0 == "#" }).count
            if (1...6).contains(hashes), trimmed.dropFirst(hashes).first == " " {
                return [(0..<chars.count, .keyword)]
            }
        }
        var i = 0
        var inTag = false
        var expectTagName = false
        while i < chars.count {
            if let open = openString {
                let (end, closed) = stringEnd(from: i, open)
                if closed { openString = nil }
                emit(i..<end, .string); i = end; continue
            }
            if let block = blockTokens, inBlock || starts(block.open, at: i) {
                var j = inBlock ? i : i + block.open.count
                while j < chars.count && !starts(block.close, at: j) { j += 1 }
                inBlock = j >= chars.count
                if !inBlock { j += block.close.count }
                emit(i..<j, .comment); i = j; continue
            }
            let c = chars[i]
            if let comment = lineCommentTokens.first(where: { starts($0, at: i) }),
               comment != ["#"] || i == 0 || chars[i - 1].isWhitespace {
                emit(i..<chars.count, .comment); break
            }
            if language.markup {
                if c == "<" {
                    inTag = true; expectTagName = true
                    var j = i + 1
                    if j < chars.count, chars[j] == "/" || chars[j] == "!" || chars[j] == "?" { j += 1 }
                    emit(i..<j, nil); i = j; continue
                }
                if c == ">" { inTag = false; emit(i..<i + 1, nil); i += 1; continue }
                if !inTag { emit(i..<i + 1, nil); i += 1; continue }
            }
            if let delimiter = multilineTokens.first(where: { starts($0.token, at: i) }) {
                let (end, closed) = stringEnd(from: i + delimiter.token.count, delimiter)
                if !closed { openString = delimiter }
                emit(i..<end, .string); i = end; continue
            }
            if quoteCharacters.contains(c) {
                if c == "'", language.shortSingleQuote {
                    let limit = min(chars.count, i + 4)
                    var j = i + 1
                    if j < limit, chars[j] == "\\" { j += 2 } else { j += 1 }
                    if j < limit, chars[j] == "'" {
                        emit(i..<j + 1, .string); i = j + 1; continue
                    }
                    emit(i..<i + 1, nil); i += 1; continue
                }
                var j = i + 1
                while j < chars.count && chars[j] != c {
                    j += chars[j] == "\\" ? 2 : 1
                }
                j = min(j + 1, chars.count)
                emit(i..<j, language.keys && isKeyMarker(after: j) ? .type : .string); i = j; continue
            }
            if !language.markdown, c.isASCII, c.isNumber, i == 0 || !identifierChar(chars[i - 1]) {
                var j = i + 1
                while j < chars.count, chars[j].isHexDigit || "xXoObB_.".unicodeScalars.contains(chars[j]) {
                    if chars[j] == ".", j + 1 < chars.count, !chars[j + 1].isNumber { break }
                    j += 1
                }
                emit(i..<j, .number); i = j; continue
            }
            if attributeCharacters.contains(c),
               i + 1 < chars.count, chars[i + 1].isLetter {
                var j = i + 1
                while j < chars.count, identifierChar(chars[j]) || chars[j] == "." { j += 1 }
                emit(i..<j, .attribute); i = j; continue
            }
            if c.isLetter || c == "_" {
                var j = i + 1
                while j < chars.count, identifierChar(chars[j]) || (language.markup && (chars[j] == "-" || chars[j] == ":")) || (language.keys && chars[j] == "-") { j += 1 }
                let role: SyntaxRole?
                if language.markup {
                    role = expectTagName ? .keyword : .attribute
                    expectTagName = false
                } else {
                    let word = text(i..<j)
                    if language.keywords.contains(language.caseInsensitive ? word.lowercased() : word) {
                        role = .keyword
                    } else if language.keys, isKeyMarker(after: j),
                              chars[..<i].allSatisfy({ $0 == " " || $0 == "\t" || $0 == "-" }) {
                        role = .type
                    } else if language.capitalizedTypes, c.isUppercase, chars[i..<j].contains(where: \.isLowercase) {
                        role = .type
                    } else if language.functions, j < chars.count, chars[j] == "(" {
                        role = .function
                    } else {
                        role = nil
                    }
                }
                emit(i..<j, role); i = j; continue
            }
            let punctuation = emitsPunctuation && !language.markdown && !c.isWhitespace && !identifierChar(c)
            emit(i..<i + 1, punctuation ? .punctuation : nil); i += 1
        }
        flush()
        return spans
    }

    // MARK: Languages

    private static func language(name: String, ext: String) -> Language? {
        let cBlock = (open: "/*", close: "*/")
        switch ext {
        case "swift":
            return Language(keywords: swift, lineComments: ["//"], block: cBlock, quotes: ["\""],
                            multiline: [("\"\"\"", true)], capitalizedTypes: true, attributePrefixes: ["@"])
        case "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts":
            return Language(keywords: javascript, lineComments: ["//"], block: cBlock, quotes: ["\"", "'"],
                            multiline: [("`", true)], capitalizedTypes: true, attributePrefixes: ["@"])
        case "py", "pyi":
            return Language(keywords: python, lineComments: ["#"], multiline: [("\"\"\"", true), ("'''", true)],
                            capitalizedTypes: true, attributePrefixes: ["@"])
        case "go":
            return Language(keywords: go, lineComments: ["//"], block: cBlock, quotes: ["\"", "'"],
                            multiline: [("`", false)], shortSingleQuote: true, capitalizedTypes: true)
        case "rs":
            return Language(keywords: rust, lineComments: ["//"], block: cBlock, quotes: ["'"], multiline: [("\"", true)],
                            shortSingleQuote: true, capitalizedTypes: true, attributePrefixes: ["#"])
        case "java", "kt", "kts", "scala", "groovy", "gradle":
            return Language(keywords: jvm, lineComments: ["//"], block: cBlock, multiline: [("\"\"\"", true)],
                            shortSingleQuote: true, capitalizedTypes: true, attributePrefixes: ["@"])
        case "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "m", "mm":
            return Language(keywords: cFamily, lineComments: ["//"], block: cBlock,
                            shortSingleQuote: true, capitalizedTypes: true, attributePrefixes: ["#", "@"])
        case "cs":
            return Language(keywords: csharp, lineComments: ["//"], block: cBlock,
                            shortSingleQuote: true, capitalizedTypes: true)
        case "dart":
            return Language(keywords: dart, lineComments: ["//"], block: cBlock, multiline: [("\"\"\"", true), ("'''", true)],
                            capitalizedTypes: true, attributePrefixes: ["@"])
        case "rb", "rake", "gemspec":
            return Language(keywords: ruby, lineComments: ["#"], capitalizedTypes: true)
        case "php":
            return Language(keywords: php, lineComments: ["//", "#"], block: cBlock, capitalizedTypes: true)
        case "sh", "bash", "zsh", "fish", "command":
            return Language(keywords: shell, lineComments: ["#"])
        case "sql":
            return Language(keywords: sql, caseInsensitive: true, lineComments: ["--"], block: cBlock)
        case "json", "jsonc", "json5":
            return Language(keywords: literals, lineComments: ext == "json" ? [] : ["//"], block: ext == "json" ? nil : cBlock,
                            quotes: ["\""], functions: false, keys: true)
        case "yaml", "yml", "toml", "ini", "cfg", "conf", "env":
            return Language(keywords: literals.union(["yes", "no", "on", "off"]), lineComments: ["#", ";"].filter { $0 == "#" || ext == "ini" },
                            functions: false, keys: true)
        case "md", "markdown":
            return Language(quotes: ["`"], functions: false, markdown: true)
        case "css", "scss", "sass", "less":
            return Language(keywords: ["important", "media", "import", "keyframes"], lineComments: ext == "css" ? [] : ["//"],
                            block: cBlock, attributePrefixes: ["@"])
        case "html", "htm", "xml", "plist", "svg", "xib", "storyboard", "vue", "svelte":
            return Language(block: (open: "<!--", close: "-->"), markup: true)
        default:
            switch name {
            case "dockerfile", "makefile", "gemfile", "podfile", "brewfile", "fastfile", "appfile", "matchfile":
                return Language(keywords: shell.union(ruby), lineComments: ["#"], capitalizedTypes: name != "makefile")
            default:
                return nil
            }
        }
    }

    private static let literals: Set<String> = ["true", "false", "null", "nil", "none", "None", "True", "False"]
    private static let swift: Set<String> = [
        "actor", "any", "as", "associatedtype", "async", "await", "break", "case", "catch", "class", "continue",
        "convenience", "default", "defer", "deinit", "didSet", "do", "dynamic", "else", "enum", "extension",
        "fallthrough", "false", "fileprivate", "final", "for", "func", "get", "guard", "if", "import", "in",
        "indirect", "init", "inout", "internal", "is", "lazy", "let", "mutating", "nil", "nonisolated", "open",
        "operator", "override", "private", "protocol", "public", "repeat", "required", "rethrows", "return",
        "self", "Self", "set", "some", "static", "struct", "subscript", "super", "switch", "throw", "throws",
        "true", "try", "typealias", "var", "weak", "where", "while", "willSet", "unowned", "consuming", "borrowing",
    ]
    private static let javascript: Set<String> = [
        "abstract", "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
        "declare", "default", "delete", "do", "else", "enum", "export", "extends", "false", "finally", "for",
        "from", "function", "get", "if", "implements", "import", "in", "instanceof", "interface", "keyof", "let",
        "namespace", "new", "null", "of", "private", "protected", "public", "readonly", "return", "satisfies",
        "set", "static", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined", "var",
        "void", "while", "with", "yield", "number", "string", "boolean", "any", "unknown", "never",
    ]
    private static let python: Set<String> = [
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else",
        "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "match",
        "case", "None", "nonlocal", "not", "or", "pass", "raise", "return", "self", "True", "try", "while",
        "with", "yield",
    ]
    private static let go: Set<String> = [
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "false", "for",
        "func", "go", "goto", "if", "import", "interface", "iota", "map", "nil", "package", "range", "return",
        "select", "struct", "switch", "true", "type", "var",
    ]
    private static let rust: Set<String> = [
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false",
        "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
        "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
        "Some", "None", "Ok", "Err",
    ]
    private static let jvm: Set<String> = [
        "abstract", "as", "break", "case", "catch", "class", "companion", "const", "continue", "data", "def",
        "default", "do", "else", "enum", "extends", "false", "final", "finally", "for", "fun", "if", "implements",
        "import", "in", "interface", "internal", "is", "lateinit", "new", "null", "object", "open", "override",
        "package", "private", "protected", "public", "return", "sealed", "static", "super", "suspend", "switch",
        "synchronized", "this", "throw", "throws", "true", "try", "val", "var", "void", "when", "while",
        "int", "long", "boolean", "double", "float", "char", "byte", "short",
    ]
    private static let cFamily: Set<String> = [
        "auto", "bool", "break", "case", "catch", "char", "class", "const", "constexpr", "continue", "default",
        "delete", "do", "double", "else", "enum", "explicit", "extern", "false", "float", "for", "friend", "goto",
        "if", "inline", "int", "long", "namespace", "new", "nullptr", "NULL", "nil", "operator", "private",
        "protected", "public", "return", "self", "short", "signed", "sizeof", "static", "struct", "switch",
        "template", "this", "throw", "true", "try", "typedef", "typename", "union", "unsigned", "using",
        "virtual", "void", "volatile", "while", "YES", "NO", "id", "instancetype",
    ]
    private static let csharp: Set<String> = [
        "abstract", "as", "async", "await", "base", "bool", "break", "case", "catch", "class", "const",
        "continue", "default", "do", "double", "else", "enum", "false", "finally", "float", "for", "foreach",
        "get", "if", "in", "int", "interface", "internal", "is", "long", "namespace", "new", "null", "object",
        "out", "override", "private", "protected", "public", "readonly", "record", "ref", "return", "sealed",
        "set", "static", "string", "struct", "switch", "this", "throw", "true", "try", "using", "var",
        "virtual", "void", "while",
    ]
    private static let dart: Set<String> = [
        "abstract", "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "default",
        "do", "else", "enum", "extends", "false", "final", "finally", "for", "if", "import", "in", "is", "late",
        "mixin", "new", "null", "required", "return", "static", "super", "switch", "this", "throw", "true",
        "try", "var", "void", "while", "with", "yield",
    ]
    private static let ruby: Set<String> = [
        "alias", "and", "begin", "break", "case", "class", "def", "do", "else", "elsif", "end", "ensure",
        "false", "for", "if", "in", "module", "next", "nil", "not", "or", "redo", "require", "rescue",
        "retry", "return", "self", "super", "then", "true", "unless", "until", "when", "while", "yield",
        "lane", "private_lane",
    ]
    private static let php: Set<String> = [
        "abstract", "as", "break", "case", "catch", "class", "const", "continue", "default", "do", "echo",
        "else", "elseif", "extends", "false", "final", "finally", "fn", "for", "foreach", "function", "if",
        "implements", "interface", "namespace", "new", "null", "private", "protected", "public", "return",
        "static", "switch", "throw", "trait", "true", "try", "use", "while",
    ]
    private static let shell: Set<String> = [
        "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if", "in", "local",
        "readonly", "return", "set", "then", "unset", "until", "while", "end", "begin", "and", "or", "not",
        "FROM", "RUN", "COPY", "ADD", "WORKDIR", "ENV", "ARG", "CMD", "ENTRYPOINT", "EXPOSE", "USER", "LABEL",
    ]
    private static let sql: Set<String> = [
        "add", "alter", "and", "as", "asc", "begin", "between", "by", "case", "commit", "create", "default",
        "delete", "desc", "distinct", "drop", "else", "end", "exists", "foreign", "from", "group", "having",
        "if", "in", "index", "inner", "insert", "into", "is", "join", "key", "left", "like", "limit", "not",
        "null", "on", "or", "order", "outer", "primary", "references", "right", "select", "set", "table",
        "then", "transaction", "union", "unique", "update", "values", "view", "when", "where", "with",
        "integer", "text", "real", "blob", "varchar", "boolean",
    ]
}


/// Highlights a whole code block for a chat message. Tokenizing is pure and
/// thread-safe: call it off the main thread and cache the result.
public enum CodeHighlighter {
    /// Longer blocks render plain rather than spending time on colour.
    public static let maxLines = 2000
    public static let maxBytes = 300_000

    /// Above this many lines highlight.js (about 35 ms per 2,000 lines without
    /// JIT) gives way to the faster line tokenizer.
    public static let highlightJSMaxLines = 1000

    public static func supports(language: String?) -> Bool {
        guard let language else { return false }
        return HighlightJS.shared.supports(language) || SyntaxHighlighter(language: language) != nil
    }

    /// One span list per line, or nil when the language is unknown or the block
    /// is too large to colour. highlight.js colours what it knows; the line
    /// tokenizer covers the rest and long blocks.
    /// highlight.js leaves calls and punctuation plain; take those, and numbers,
    /// from the line tokenizer where highlight.js gave no colour. Its type
    /// guesses (any capitalised word) are left out.
    static func filled(_ spans: [SyntaxSpan], from tokens: [SyntaxSpan]) -> [SyntaxSpan] {
        let characters = spans.flatMap { span in Array(span.text).map { ($0, span.role) } }
        let fill = tokens.flatMap { span in Array(span.text).map { _ in span.role } }
        guard characters.count == fill.count else { return spans }
        var result: [SyntaxSpan] = []
        var text = "", role: SyntaxRole?
        for (index, (character, own)) in characters.enumerated() {
            let next = own ?? fill[index].flatMap { [.function, .punctuation, .number].contains($0) ? $0 : nil }
            if next != role, !text.isEmpty { result.append(SyntaxSpan(text: text, role: role)); text = "" }
            role = next; text.append(character)
        }
        if !text.isEmpty { result.append(SyntaxSpan(text: text, role: role)) }
        return result
    }

    public static func lines(_ code: String, language: String?) -> [[SyntaxSpan]]? {
        guard let language, code.utf8.count <= maxBytes else { return nil }
        let rows = code.split(separator: "\n", omittingEmptySubsequences: false)
        guard rows.count <= maxLines else { return nil }
        let tokenizer = SyntaxHighlighter(language: language)
        if rows.count <= highlightJSMaxLines, let lines = HighlightJS.shared.lines(code, language: language) {
            guard var tokenizer else { return lines }
            tokenizer.emitsPunctuation = true
            return zip(lines, rows).map { spans, row in filled(spans, from: tokenizer.spans(String(row))) }
        }
        guard var highlighter = tokenizer else { return nil }
        highlighter.emitsPunctuation = true
        var result: [[SyntaxSpan]] = []
        result.reserveCapacity(rows.count)
        for row in rows { result.append(highlighter.spans(String(row))) }
        return result
    }
}

/// The character classes the tokenizer needs, with an ASCII fast path.
fileprivate extension Unicode.Scalar {
    var isLetter: Bool {
        isASCII ? (value | 0x20) >= 0x61 && (value | 0x20) <= 0x7A : properties.isAlphabetic
    }
    var isNumber: Bool { isASCII ? value >= 0x30 && value <= 0x39 : properties.numericType != nil }
    var isWhitespace: Bool { isASCII ? value == 0x20 || (value >= 0x09 && value <= 0x0D) : properties.isWhitespace }
    var isUppercase: Bool { isASCII ? value >= 0x41 && value <= 0x5A : properties.isUppercase }
    var isLowercase: Bool { isASCII ? value >= 0x61 && value <= 0x7A : properties.isLowercase }
    var isHexDigit: Bool { isNumber && isASCII || (value | 0x20) >= 0x61 && (value | 0x20) <= 0x66 }
}
