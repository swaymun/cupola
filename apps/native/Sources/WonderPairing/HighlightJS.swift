import Foundation
#if canImport(JavaScriptCore)
import JavaScriptCore
#endif

/// Code block colouring with highlight.js (BSD-3-Clause, vendored subset of 41
/// languages; `scripts/build-highlightjs`). One JavaScriptCore context, loaded
/// on first use and used by one caller at a time. Callers run it off the main
/// thread and cache the result (`CodeBlockView`). Measured against the regex
/// highlighter (A8, 2026-10-15): about 3.6x slower without JIT, as in an iOS
/// app (0.3 ms for 20 lines, 3.3 ms for 200), and it adds TSX tags, TOML
/// tables, YAML values, Markdown and 13 more languages.
public final class HighlightJS: @unchecked Sendable {
    public static let shared = HighlightJS()
    private let lock = NSLock()
    #if canImport(JavaScriptCore)
    private var context: JSContext?
    #endif
    private var failed = false
    private var supported: [String: Bool] = [:]

    /// The vendored highlight.js licence, for Acknowledgements.
    public static var licenseText: String? {
        Bundle.module.url(forResource: "Highlight", withExtension: nil)
            .flatMap { try? String(contentsOf: $0.appendingPathComponent("LICENSE"), encoding: .utf8) }
    }

    /// Whether the fence tag names a language highlight.js colours here.
    public func supports(_ tag: String) -> Bool {
        let tag = tag.lowercased().trimmingCharacters(in: .whitespaces)
        guard !tag.isEmpty else { return false }
        return lock.withLock {
            if let known = supported[tag] { return known }
            #if canImport(JavaScriptCore)
            let answer = loadedContext()?.objectForKeyedSubscript("wonderSupports")?.call(withArguments: [tag])?.toBool() ?? false
            #else
            let answer = false
            #endif
            supported[tag] = answer
            return answer
        }
    }

    /// One span list per line, or nil when the language is unknown here or the
    /// script failed (callers then use `SyntaxHighlighter`).
    public func lines(_ code: String, language: String) -> [[SyntaxSpan]]? {
        guard supports(language) else { return nil }
        #if canImport(JavaScriptCore)
        let flat: String? = lock.withLock {
            guard let context = loadedContext(),
                  let result = context.objectForKeyedSubscript("wonderSpans")?.call(withArguments: [code, language.lowercased()]),
                  context.exception == nil, result.isString else { context?.exception = nil; return nil }
            return result.toString()
        }
        guard let flat else { return nil }
        return Self.lines(code, spans: flat)
        #else
        return nil
        #endif
    }

    /// Splits `code` into lines of spans from `start,length,role,...` UTF-16 runs.
    static func lines(_ code: String, spans flat: String) -> [[SyntaxSpan]]? {
        let numbers = flat.isEmpty ? [] : flat.split(separator: ",").compactMap { Int($0) }
        guard numbers.count % 3 == 0 else { return nil }
        let utf16 = Array(code.utf16)
        var roleAt = [SyntaxRole?](repeating: nil, count: utf16.count)
        let roles: [SyntaxRole] = [.keyword, .string, .comment, .number, .type, .attribute, .function, .punctuation]
        for index in stride(from: 0, to: numbers.count, by: 3) {
            let (start, length, role) = (numbers[index], numbers[index + 1], numbers[index + 2])
            guard start >= 0, length >= 0, start + length <= utf16.count, roles.indices.contains(role) else { return nil }
            for offset in start..<(start + length) { roleAt[offset] = roles[role] }
        }
        var result: [[SyntaxSpan]] = [[]]
        var run: [UInt16] = [], runRole: SyntaxRole?
        func flush() {
            guard !run.isEmpty else { return }
            result[result.count - 1].append(SyntaxSpan(text: String(decoding: run, as: UTF16.self), role: runRole))
            run = []
        }
        for (offset, unit) in utf16.enumerated() {
            if unit == 10 { flush(); result.append([]); continue }
            let role = roleAt[offset]
            if role != runRole, !run.isEmpty, !(UTF16.isTrailSurrogate(unit)) { flush() }
            if run.isEmpty { runRole = role }
            run.append(unit)
        }
        flush()
        return result
    }

    #if canImport(JavaScriptCore)
    /// Called with `lock` held.
    private func loadedContext() -> JSContext? {
        if let context { return context }
        guard !failed else { return nil }
        failed = true
        guard let directory = Bundle.module.url(forResource: "Highlight", withExtension: nil),
              let library = try? String(contentsOf: directory.appendingPathComponent("highlight.min.js"), encoding: .utf8),
              let adapter = try? String(contentsOf: directory.appendingPathComponent("wonder-spans.js"), encoding: .utf8),
              let context = JSContext() else { return nil }
        context.evaluateScript(library)
        context.evaluateScript(adapter)
        guard context.exception == nil, context.objectForKeyedSubscript("wonderSpans")?.isObject == true else { return nil }
        failed = false
        self.context = context
        return context
    }
    #endif
}
