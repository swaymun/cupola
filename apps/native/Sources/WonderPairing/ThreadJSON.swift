import Foundation

/// Types that carry `ThreadValue` payloads decode through `ThreadJSON`.
public protocol ThreadJSONDecodable: Decodable {}

/// Decodes JSON that carries free-form `ThreadValue` payloads in one pass.
///
/// `JSONDecoder` cannot say what kind of value it is looking at, so the old
/// `ThreadValue` decoder tried Bool, String, Double and Array in turn and let
/// every wrong guess throw (an object cost four thrown errors). `JSONSerialization`
/// parses the bytes once into typed values; this decoder walks that tree, so a
/// `ThreadValue` is read from its real type with no failed attempt. Everything
/// else decodes with the same rules as `JSONDecoder` for the types this app reads.
public enum ThreadJSON {
    public static func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        let root: Any
        do { root = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed]) }
        catch { throw DecodingError.dataCorrupted(.init(codingPath: [], debugDescription: "The given data was not valid JSON.", underlyingError: error)) }
        return try T(from: TreeDecoder(value: root, codingPath: []))
    }
}

/// Whether a parsed number is a JSON `true` or `false`; `1` and `0` are not.
private func isBoolean(_ number: NSNumber) -> Bool {
    #if canImport(Darwin)
    return CFGetTypeID(number) == CFBooleanGetTypeID()
    #else
    return number === (true as NSNumber) || number === (false as NSNumber)
    #endif
}

extension ThreadValue {
    /// The value for a parsed JSON node. Bool and number stay distinct.
    init(tree value: Any) {
        switch value {
        case let string as String: self = .string(string)
        case let number as NSNumber: self = isBoolean(number) ? .bool(number.boolValue) : .number(number.doubleValue)
        case let object as [String: Any]: self = .object(object.mapValues { ThreadValue(tree: $0) })
        case let array as [Any]: self = .array(array.map { ThreadValue(tree: $0) })
        default: self = .null
        }
    }
}

struct TreeDecoder: Decoder {
    let value: Any
    let codingPath: [CodingKey]
    var userInfo: [CodingUserInfoKey: Any] { [:] }

    func container<Key: CodingKey>(keyedBy type: Key.Type) throws -> KeyedDecodingContainer<Key> {
        guard let object = value as? [String: Any] else { throw mismatch([String: Any].self, codingPath, value) }
        return KeyedDecodingContainer(TreeKeyed<Key>(object: object, codingPath: codingPath))
    }
    func unkeyedContainer() throws -> UnkeyedDecodingContainer {
        guard let array = value as? [Any] else { throw mismatch([Any].self, codingPath, value) }
        return TreeUnkeyed(array: array, codingPath: codingPath)
    }
    func singleValueContainer() throws -> SingleValueDecodingContainer { TreeSingle(value: value, codingPath: codingPath) }
}

private func mismatch(_ type: Any.Type, _ path: [CodingKey], _ value: Any) -> DecodingError {
    .typeMismatch(type, .init(codingPath: path, debugDescription: "Expected \(type) but found \(Swift.type(of: value))."))
}

private enum Scalar {
    static func bool(_ value: Any, _ path: [CodingKey]) throws -> Bool {
        guard let number = value as? NSNumber, isBoolean(number) else { throw mismatch(Bool.self, path, value) }
        return number.boolValue
    }
    static func string(_ value: Any, _ path: [CodingKey]) throws -> String {
        guard let string = value as? String else { throw mismatch(String.self, path, value) }
        return string
    }
    static func double(_ value: Any, _ path: [CodingKey]) throws -> Double {
        guard let number = value as? NSNumber, !isBoolean(number) else { throw mismatch(Double.self, path, value) }
        return number.doubleValue
    }
    static func integer<T: FixedWidthInteger>(_ value: Any, _ path: [CodingKey]) throws -> T {
        guard let number = value as? NSNumber, !isBoolean(number) else { throw mismatch(T.self, path, value) }
        if NSNumber(value: number.int64Value) == number, let exact = T(exactly: number.int64Value) { return exact }
        if NSNumber(value: number.uint64Value) == number, let exact = T(exactly: number.uint64Value) { return exact }
        if let exact = T(exactly: number.doubleValue) { return exact }
        throw DecodingError.dataCorrupted(.init(codingPath: path, debugDescription: "Parsed JSON number \(number) does not fit in \(T.self)."))
    }
    static func decode<T: Decodable>(_ type: T.Type, _ value: Any, _ path: [CodingKey]) throws -> T {
        // Build the generic tree only for a ThreadValue; every other type decodes straight from the value.
        if T.self == ThreadValue.self, let thread = ThreadValue(tree: value) as? T { return thread }
        return try T(from: TreeDecoder(value: value, codingPath: path))
    }
}

private struct TreeKeyed<Key: CodingKey>: KeyedDecodingContainerProtocol {
    let object: [String: Any]
    let codingPath: [CodingKey]
    var allKeys: [Key] { object.keys.compactMap { Key(stringValue: $0) } }
    func contains(_ key: Key) -> Bool { object[key.stringValue] != nil }
    private func node(_ key: Key) throws -> Any {
        guard let value = object[key.stringValue] else {
            throw DecodingError.keyNotFound(key, .init(codingPath: codingPath, debugDescription: "No value associated with key \(key.stringValue)."))
        }
        return value
    }
    private func path(_ key: Key) -> [CodingKey] { codingPath + [key] }
    func decodeNil(forKey key: Key) throws -> Bool { try node(key) is NSNull }
    func decode(_ type: Bool.Type, forKey key: Key) throws -> Bool { try Scalar.bool(node(key), path(key)) }
    func decode(_ type: String.Type, forKey key: Key) throws -> String { try Scalar.string(node(key), path(key)) }
    func decode(_ type: Double.Type, forKey key: Key) throws -> Double { try Scalar.double(node(key), path(key)) }
    func decode(_ type: Float.Type, forKey key: Key) throws -> Float { Float(try Scalar.double(node(key), path(key))) }
    func decode(_ type: Int.Type, forKey key: Key) throws -> Int { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: Int8.Type, forKey key: Key) throws -> Int8 { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: Int16.Type, forKey key: Key) throws -> Int16 { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: Int32.Type, forKey key: Key) throws -> Int32 { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: Int64.Type, forKey key: Key) throws -> Int64 { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: UInt.Type, forKey key: Key) throws -> UInt { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: UInt8.Type, forKey key: Key) throws -> UInt8 { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: UInt16.Type, forKey key: Key) throws -> UInt16 { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: UInt32.Type, forKey key: Key) throws -> UInt32 { try Scalar.integer(node(key), path(key)) }
    func decode(_ type: UInt64.Type, forKey key: Key) throws -> UInt64 { try Scalar.integer(node(key), path(key)) }
    func decode<T: Decodable>(_ type: T.Type, forKey key: Key) throws -> T { try Scalar.decode(type, node(key), path(key)) }
    func nestedContainer<N: CodingKey>(keyedBy type: N.Type, forKey key: Key) throws -> KeyedDecodingContainer<N> {
        try TreeDecoder(value: node(key), codingPath: path(key)).container(keyedBy: type)
    }
    func nestedUnkeyedContainer(forKey key: Key) throws -> UnkeyedDecodingContainer {
        try TreeDecoder(value: node(key), codingPath: path(key)).unkeyedContainer()
    }
    func superDecoder() throws -> Decoder { TreeDecoder(value: object, codingPath: codingPath) }
    func superDecoder(forKey key: Key) throws -> Decoder { TreeDecoder(value: try node(key), codingPath: path(key)) }
}

private struct TreeIndex: CodingKey {
    var stringValue: String { String(intValue ?? 0) }
    let intValue: Int?
    init?(stringValue: String) { nil }
    init(_ index: Int) { intValue = index }
    init?(intValue: Int) { self.intValue = intValue }
}

private struct TreeUnkeyed: UnkeyedDecodingContainer {
    let array: [Any]
    let codingPath: [CodingKey]
    private(set) var currentIndex = 0
    var count: Int? { array.count }
    var isAtEnd: Bool { currentIndex >= array.count }
    init(array: [Any], codingPath: [CodingKey]) { self.array = array; self.codingPath = codingPath }

    private var path: [CodingKey] { codingPath + [TreeIndex(currentIndex)] }
    private mutating func next() throws -> Any {
        guard !isAtEnd else {
            throw DecodingError.valueNotFound(Any.self, .init(codingPath: path, debugDescription: "Unkeyed container is at end."))
        }
        defer { currentIndex += 1 }
        return array[currentIndex]
    }
    mutating func decodeNil() throws -> Bool {
        guard !isAtEnd else {
            throw DecodingError.valueNotFound(Any?.self, .init(codingPath: path, debugDescription: "Unkeyed container is at end."))
        }
        if array[currentIndex] is NSNull { currentIndex += 1; return true }
        return false
    }
    mutating func decode(_ type: Bool.Type) throws -> Bool { let p = path; return try Scalar.bool(next(), p) }
    mutating func decode(_ type: String.Type) throws -> String { let p = path; return try Scalar.string(next(), p) }
    mutating func decode(_ type: Double.Type) throws -> Double { let p = path; return try Scalar.double(next(), p) }
    mutating func decode(_ type: Float.Type) throws -> Float { let p = path; return Float(try Scalar.double(next(), p)) }
    mutating func decode(_ type: Int.Type) throws -> Int { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: Int8.Type) throws -> Int8 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: Int16.Type) throws -> Int16 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: Int32.Type) throws -> Int32 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: Int64.Type) throws -> Int64 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: UInt.Type) throws -> UInt { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: UInt8.Type) throws -> UInt8 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: UInt16.Type) throws -> UInt16 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: UInt32.Type) throws -> UInt32 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode(_ type: UInt64.Type) throws -> UInt64 { let p = path; return try Scalar.integer(next(), p) }
    mutating func decode<T: Decodable>(_ type: T.Type) throws -> T { let p = path; return try Scalar.decode(type, next(), p) }
    mutating func nestedContainer<N: CodingKey>(keyedBy type: N.Type) throws -> KeyedDecodingContainer<N> {
        let p = path; return try TreeDecoder(value: next(), codingPath: p).container(keyedBy: type)
    }
    mutating func nestedUnkeyedContainer() throws -> UnkeyedDecodingContainer {
        let p = path; return try TreeDecoder(value: next(), codingPath: p).unkeyedContainer()
    }
    mutating func superDecoder() throws -> Decoder { let p = path; return TreeDecoder(value: try next(), codingPath: p) }
}

private struct TreeSingle: SingleValueDecodingContainer {
    let value: Any
    let codingPath: [CodingKey]
    func decodeNil() -> Bool { value is NSNull }
    func decode(_ type: Bool.Type) throws -> Bool { try Scalar.bool(value, codingPath) }
    func decode(_ type: String.Type) throws -> String { try Scalar.string(value, codingPath) }
    func decode(_ type: Double.Type) throws -> Double { try Scalar.double(value, codingPath) }
    func decode(_ type: Float.Type) throws -> Float { Float(try Scalar.double(value, codingPath)) }
    func decode(_ type: Int.Type) throws -> Int { try Scalar.integer(value, codingPath) }
    func decode(_ type: Int8.Type) throws -> Int8 { try Scalar.integer(value, codingPath) }
    func decode(_ type: Int16.Type) throws -> Int16 { try Scalar.integer(value, codingPath) }
    func decode(_ type: Int32.Type) throws -> Int32 { try Scalar.integer(value, codingPath) }
    func decode(_ type: Int64.Type) throws -> Int64 { try Scalar.integer(value, codingPath) }
    func decode(_ type: UInt.Type) throws -> UInt { try Scalar.integer(value, codingPath) }
    func decode(_ type: UInt8.Type) throws -> UInt8 { try Scalar.integer(value, codingPath) }
    func decode(_ type: UInt16.Type) throws -> UInt16 { try Scalar.integer(value, codingPath) }
    func decode(_ type: UInt32.Type) throws -> UInt32 { try Scalar.integer(value, codingPath) }
    func decode(_ type: UInt64.Type) throws -> UInt64 { try Scalar.integer(value, codingPath) }
    func decode<T: Decodable>(_ type: T.Type) throws -> T { try Scalar.decode(type, value, codingPath) }
}
