import Darwin
import Foundation

public enum ControlPreferencesState: String, Equatable, Sendable {
    case disabled
    case enabled
    case unavailable
}

public enum ControlPreferencesStoreError: Error, Equatable, Sendable {
    case unsafeServiceDirectory
    case writeFailed(Int32)
    case encodeFailed
}

/// The only durable switch that permits a paired device to control this Mac.
/// Missing, malformed, or unsafe state is deliberately indistinguishable from
/// disabled to callers that ask whether control is allowed.
public struct ControlPreferencesStore: Sendable {
    public static let currentVersion = 1
    public static let fileName = "control-preferences.json"

    private static let maximumFileBytes = 4_096
    private let serviceDirectory: URL

    public init(serviceDirectory: URL) {
        self.serviceDirectory = serviceDirectory
    }

    public var state: ControlPreferencesState {
        switch readDocument() {
        case .missing:
            return .disabled
        case let .document(document):
            return document.allowControlFromPairedDevices ? .enabled : .disabled
        case .unavailable:
            return .unavailable
        }
    }

    public var isEnabled: Bool { state == .enabled }

    public func setAllowControlFromPairedDevices(_ enabled: Bool) throws {
        guard let directoryFD = openValidatedDirectory() else {
            throw ControlPreferencesStoreError.unsafeServiceDirectory
        }
        defer { close(directoryFD) }

        let document = ControlPreferencesDocument(
            version: Self.currentVersion,
            allowControlFromPairedDevices: enabled
        )
        let data: Data
        do {
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.sortedKeys]
            data = try encoder.encode(document)
        } catch {
            throw ControlPreferencesStoreError.encodeFailed
        }

        let temporaryName = ".\(Self.fileName).\(UUID().uuidString).tmp"
        let temporaryFD = temporaryName.withCString {
            openat(directoryFD, $0, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, mode_t(0o600))
        }
        guard temporaryFD >= 0 else {
            throw ControlPreferencesStoreError.writeFailed(errno)
        }

        var installed = false
        defer {
            close(temporaryFD)
            if !installed {
                _ = temporaryName.withCString { unlinkat(directoryFD, $0, 0) }
            }
        }

        do {
            try Self.writeAll(data, to: temporaryFD)
            guard fchmod(temporaryFD, mode_t(0o600)) == 0,
                  fsync(temporaryFD) == 0 else {
                throw ControlPreferencesStoreError.writeFailed(errno)
            }
            guard renameat(directoryFD, temporaryName, directoryFD, Self.fileName) == 0 else {
                throw ControlPreferencesStoreError.writeFailed(errno)
            }
            installed = true
            // Persist the directory entry as well as the file contents.
            guard fsync(directoryFD) == 0 else {
                throw ControlPreferencesStoreError.writeFailed(errno)
            }
        } catch let error as ControlPreferencesStoreError {
            throw error
        } catch {
            throw ControlPreferencesStoreError.writeFailed(errno)
        }
    }

    private enum ReadResult {
        case missing
        case document(ControlPreferencesDocument)
        case unavailable
    }

    private struct ControlPreferencesDocument: Codable, Sendable {
        let version: Int
        let allowControlFromPairedDevices: Bool
    }

    private func readDocument() -> ReadResult {
        guard let directoryFD = openValidatedDirectory() else {
            return .unavailable
        }
        defer { close(directoryFD) }

        let fileFD = Self.fileName.withCString {
            openat(directoryFD, $0, O_RDONLY | O_NOFOLLOW)
        }
        guard fileFD >= 0 else {
            return errno == ENOENT ? .missing : .unavailable
        }
        defer { close(fileFD) }

        var metadata = stat()
        guard fstat(fileFD, &metadata) == 0,
              (metadata.st_mode & S_IFMT) == S_IFREG,
              metadata.st_uid == geteuid(),
              (metadata.st_mode & mode_t(0o077)) == 0,
              (metadata.st_mode & mode_t(0o400)) != 0,
              metadata.st_size >= 0,
              metadata.st_size <= off_t(Self.maximumFileBytes) else {
            return .unavailable
        }

        guard let data = Self.readAll(from: fileFD, maximumBytes: Self.maximumFileBytes),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              object.keys.sorted() == ["allowControlFromPairedDevices", "version"],
              let version = object["version"] as? Int,
              version == Self.currentVersion,
              let allow = object["allowControlFromPairedDevices"] as? Bool,
              let document = try? JSONDecoder().decode(ControlPreferencesDocument.self, from: data),
              document.version == version,
              document.allowControlFromPairedDevices == allow else {
            return .unavailable
        }
        return .document(document)
    }

    private func openValidatedDirectory() -> Int32? {
        let path = serviceDirectory.standardizedFileURL.path
        guard path.hasPrefix("/"), Self.hasNoSymlinkComponents(path) else {
            return nil
        }
        let directoryFD = path.withCString {
            open($0, O_RDONLY | O_DIRECTORY | O_NOFOLLOW)
        }
        guard directoryFD >= 0 else { return nil }

        var metadata = stat()
        guard fstat(directoryFD, &metadata) == 0,
              (metadata.st_mode & S_IFMT) == S_IFDIR,
              metadata.st_uid == geteuid(),
              (metadata.st_mode & mode_t(0o077)) == 0,
              (metadata.st_mode & mode_t(0o700)) == mode_t(0o700) else {
            close(directoryFD)
            return nil
        }
        return directoryFD
    }

    private static func hasNoSymlinkComponents(_ path: String) -> Bool {
        var current = ""
        for component in path.split(separator: "/") {
            current += "/" + component
            var metadata = stat()
            guard lstat(current, &metadata) == 0,
                  (metadata.st_mode & S_IFMT) != S_IFLNK else {
                return false
            }
        }
        return true
    }

    private static func writeAll(_ data: Data, to fileDescriptor: Int32) throws {
        var offset = 0
        while offset < data.count {
            let written = data.withUnsafeBytes { bytes in
                write(
                    fileDescriptor,
                    bytes.baseAddress!.advanced(by: offset),
                    data.count - offset
                )
            }
            guard written > 0 else {
                throw ControlPreferencesStoreError.writeFailed(errno)
            }
            offset += written
        }
    }

    private static func readAll(from fileDescriptor: Int32, maximumBytes: Int) -> Data? {
        var data = Data()
        data.reserveCapacity(maximumBytes)
        while data.count < maximumBytes {
            var buffer = [UInt8](repeating: 0, count: min(512, maximumBytes - data.count))
            let count = buffer.withUnsafeMutableBytes {
                read(fileDescriptor, $0.baseAddress, $0.count)
            }
            if count < 0 || count == 0 { break }
            data.append(contentsOf: buffer.prefix(count))
        }
        return data
    }
}
