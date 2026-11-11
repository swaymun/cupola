import Foundation
import CryptoKit
import Security
import UserNotifications

struct PushPreview: Codable, Equatable {
    let title: String
    let body: String
    var hostInstallationId: String? = nil
    var conversationId: String? = nil
    var approval: Approval? = nil
    // Separate from approval so older apps open option questions in the app.
    var question: Approval? = nil
    /// Where a finished reply sits in its computer's history. A read
    /// acknowledgement at or past it means the reply has been read.
    var readThrough: ReadThrough? = nil
    var action: Approval? { question ?? approval }

    struct ReadThrough: Codable, Equatable {
        let hostEpoch: String
        let sequence: UInt64
    }

    struct Approval: Codable, Equatable {
        let approvalId: String
        let actionNonce: String
        var supportsDenyMessage: Bool? = nil
        var questionId: String? = nil
        var questionOptions: [QuestionOption]? = nil
    }
    struct QuestionOption: Codable, Equatable {
        let label: String
        var description: String? = nil
    }
    static let commandCategory = "wonder.command-approval"
    static let commandFeedbackCategory = "wonder.command-approval-feedback"
    static let denyMessageAction = "wonder.deny-with-instruction"
    static let questionCategory = "wonder.question-reply"
    static let replyAction = "wonder.reply"
    static let optionActionPrefix = "wonder.question-option."
    static func optionAction(_ index: Int) -> String { optionActionPrefix + String(index) }
    static func optionIndex(_ action: String) -> Int? {
        guard action.hasPrefix(optionActionPrefix), let index = Int(action.dropFirst(optionActionPrefix.count)),
              (0..<32).contains(index), action == optionAction(index) else { return nil }
        return index
    }
    static var otherAnswerAction: UNTextInputNotificationAction {
        UNTextInputNotificationAction(identifier: replyAction, title: "Other answer…", options: [.authenticationRequired],
            textInputButtonTitle: "Send", textInputPlaceholder: "Your answer")
    }
    var questionActions: [UNNotificationAction] {
        guard action?.questionId != nil else { return [] }
        return (action?.questionOptions ?? []).prefix(4).enumerated().map {
            UNNotificationAction(identifier: Self.optionAction($0.offset), title: $0.element.label, options: [.authenticationRequired])
        } + [Self.otherAnswerAction]
    }
    var categoryIdentifier: String {
        guard let approval = action else { return "" }
        if approval.questionId != nil { return Self.questionCategory }
        return approval.supportsDenyMessage == true ? Self.commandFeedbackCategory : Self.commandCategory
    }
    static let allowAction = "wonder.allow-once"
    static let denyAction = "wonder.deny"
    var threadIdentifier: String {
        guard let hostInstallationId, let conversationId else { return "" }
        return SHA256.hash(data: Data("\(hostInstallationId)\n\(conversationId)".utf8)).map { String(format: "%02x", $0) }.joined()
    }

    /// Host-view content is emitted only for the Mac's counted dialog alert.
    /// The push route still uses the existing Project authorization boundary.
    var macDialogHost: String? {
        conversationId == "wonder-host-view" && action == nil ? hostInstallationId : nil
    }
    func applying(to original: UNNotificationContent) -> UNMutableNotificationContent {
        let content = original.mutableCopy() as! UNMutableNotificationContent
        content.title = title
        content.body = body
        content.threadIdentifier = threadIdentifier
        content.categoryIdentifier = categoryIdentifier
        if let host = macDialogHost {
            content.threadIdentifier = "mac-dialogs:" + host
            content.userInfo["wonderMacDialogs"] = ["host": host]
        }
        return content
    }

    static func decrypt(_ payload: [String: String], key: Data) throws -> Self {
        guard let registration = payload["registrationId"], UUID(uuidString: registration) != nil,
              let route = payload["routeId"], UUID(uuidString: route) != nil,
              let event = payload["eventId"], UUID(uuidString: event) != nil,
              let encoded = payload["preview"], encoded.utf8.count <= 3240,
              let combined = Data(pushBase64URL: encoded), key.count == 32 else { throw PreviewError.invalid }
        let aad = Data("wonder-push-v1\n\(registration)\n\(route)\n\(event)".utf8)
        let plain = try AES.GCM.open(AES.GCM.SealedBox(combined: combined), using: SymmetricKey(data: key), authenticating: aad)
        guard plain.count <= 2400 else { throw PreviewError.invalid }
        let preview = try JSONDecoder().decode(Self.self, from: plain)
        guard preview.question == nil || preview.approval == nil else { throw PreviewError.invalid }
        if let question = preview.question {
            guard question.questionId != nil, question.questionOptions != nil else { throw PreviewError.invalid }
        }
        guard !preview.title.isEmpty, !preview.body.isEmpty, preview.title.utf8.count <= 163, preview.body.utf8.count <= 1003 else { throw PreviewError.invalid }
        for value in [preview.hostInstallationId, preview.conversationId, preview.action?.approvalId, preview.action?.actionNonce, preview.action?.questionId, preview.readThrough?.hostEpoch].compactMap({ $0 }) {
            guard !value.isEmpty, value.utf8.count <= 256, !value.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else { throw PreviewError.invalid }
        }
        if preview.action != nil {
            guard preview.hostInstallationId != nil, preview.conversationId != nil, preview.body.utf8.count <= 382 else { throw PreviewError.invalid }
        }
        if let questionId = preview.action?.questionId {
            guard preview.action?.supportsDenyMessage != true,
                  !questionId.contains(where: \.isWhitespace) else { throw PreviewError.invalid }
        }
        if let options = preview.action?.questionOptions {
            guard preview.action?.questionId != nil, !options.isEmpty, options.count <= 32,
                  Set(options.map(\.label)).count == options.count else { throw PreviewError.invalid }
            for option in options {
                for (value, limit) in [(option.label, 120), (option.description ?? "", 160)] {
                    guard value.utf8.count <= limit,
                          !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control || (0x202a...0x202e).contains($0.value) || (0x2066...0x2069).contains($0.value) }) else { throw PreviewError.invalid }
                }
                guard !option.label.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw PreviewError.invalid }
            }
        }
        return preview
    }
}

/// A custom notification row can only forward the system's default action.
/// Share the encrypted-payload identity and option index, never credentials or
/// plaintext. The app clears this only after its normal durable receipt saves.
enum PushQuestionSelection {
    private struct Selection: Codable {
        let index: Int
        let created: Date
    }
    private static func file(_ payload: [String: String], directory: URL?) throws -> URL {
        let root: URL
        if let directory { root = directory }
        else {
            let group = Bundle.main.object(forInfoDictionaryKey: "WonderQuestionAppGroup") as? String
                ?? Bundle.main.bundleIdentifier.map { "group." + $0 }
            guard let group, let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group) else { throw PreviewError.invalid }
            root = container.appendingPathComponent("NotificationAnswers", isDirectory: true)
        }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let data = try JSONEncoder().encode(payload.sorted { $0.key < $1.key }.map { [$0.key, $0.value] })
        let id = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        return root.appendingPathComponent(id + ".json")
    }
    static func read(_ payload: [String: String], directory: URL? = nil) throws -> Int? {
        let path = try file(payload, directory: directory)
        guard FileManager.default.fileExists(atPath: path.path) else { return nil }
        let value = try JSONDecoder().decode(Selection.self, from: Data(contentsOf: path))
        guard (0..<32).contains(value.index), value.created.timeIntervalSinceNow > -86400,
              value.created.timeIntervalSinceNow < 60 else { throw PreviewError.invalid }
        return value.index
    }
    static func save(_ index: Int, payload: [String: String], directory: URL? = nil) throws {
        guard (0..<32).contains(index) else { throw PreviewError.invalid }
        if let saved = try read(payload, directory: directory) {
            guard saved == index else { throw PreviewError.invalid }
            return
        }
        let path = try file(payload, directory: directory)
        let pending = try FileManager.default.contentsOfDirectory(at: path.deletingLastPathComponent(), includingPropertiesForKeys: [.contentModificationDateKey])
            .filter { $0.pathExtension == "json" }
        for item in pending {
            if let modified = try item.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
               modified.timeIntervalSinceNow < -86400 { try FileManager.default.removeItem(at: item) }
        }
        guard pending.filter({ FileManager.default.fileExists(atPath: $0.path) }).count < 32 else { throw PreviewError.invalid }
        // Exclusive creation preserves the first choice across two extension
        // instances. Forward only after the complete write succeeds.
        do {
            try JSONEncoder().encode(Selection(index: index, created: Date())).write(to: path, options: [.withoutOverwriting, .completeFileProtectionUntilFirstUserAuthentication])
        } catch {
            guard let saved = try? read(payload, directory: directory), saved == index else { throw error }
        }
    }
    static func remove(_ payload: [String: String], directory: URL? = nil) throws {
        let path = try file(payload, directory: directory)
        if FileManager.default.fileExists(atPath: path.path) { try FileManager.default.removeItem(at: path) }
    }
}

enum PreviewError: Error { case invalid, keychain(OSStatus) }

/// This group contains only per-registration preview keys. Pairing and signing
/// credentials remain in the application's original, private Keychain group.
enum PushPreviewKeys {
    private static func query(_ registration: String) throws -> [String: Any] {
        guard UUID(uuidString: registration) != nil,
              let group = Bundle.main.object(forInfoDictionaryKey: "WonderPushKeychainGroup") as? String,
              !group.isEmpty, !group.contains("$(") else { throw PreviewError.invalid }
        return [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: "wonder.push.previews.v1",
                kSecAttrAccount as String: registration, kSecAttrAccessGroup as String: group]
    }
    static func read(_ registration: String) throws -> Data? {
        var query = try query(registration); query[kSecReturnData as String] = true
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data, data.count == 32 else { throw PreviewError.keychain(status) }
        return data
    }
    static func getOrCreate(_ registration: String) throws -> Data {
        if let key = try read(registration) { return key }
        var bytes = Data(count: 32)
        let status = bytes.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        guard status == errSecSuccess else { throw PreviewError.keychain(status) }
        var item = try query(registration); item[kSecValueData as String] = bytes
        // Available to the extension while locked after the first device unlock.
        // These keys never synchronize to iCloud or migrate to a different phone.
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let added = SecItemAdd(item as CFDictionary, nil)
        if added == errSecDuplicateItem, let existing = try read(registration) { return existing }
        guard added == errSecSuccess else { throw PreviewError.keychain(added) }
        return bytes
    }
    static func remove(_ registration: String) throws {
        let status = SecItemDelete(try query(registration) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw PreviewError.keychain(status) }
    }
}

extension Data {
    init?(pushBase64URL text: String) {
        let normalized = text.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        self.init(base64Encoded: normalized + String(repeating: "=", count: (4 - normalized.count % 4) % 4))
    }
}
