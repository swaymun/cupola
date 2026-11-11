import Foundation

/// The only data shared with WidgetKit: the last-used Mac, its most recently
/// active threads and its recent Projects as navigation labels and opaque IDs,
/// never message text, folder paths, credentials, or agent state.
struct ProjectWidgetSnapshot: Codable, Equatable, Sendable {
    static let version = 3
    static let widgetKind = "WonderProjectWidget"
    static let staleAfter: TimeInterval = 60 * 60
    static let maxBytes = 32 * 1024
    static let maxProjects = 8
    static let maxThreads = 8

    struct Project: Codable, Equatable, Sendable, Identifiable {
        let id: String
        let name: String
    }

    /// A Project thread: opened by its Wonder conversation when it has one,
    /// otherwise by its provider thread, which the app attaches on open.
    struct Thread: Codable, Equatable, Sendable, Identifiable {
        let projectID: String
        let family: String
        let nativeID: String?
        let conversationID: String?
        let title: String
        let projectName: String
        var id: String { conversationID ?? family + "-" + (nativeID ?? "") }
    }

    let schemaVersion: Int
    let savedAt: Date
    /// The Mac last used for a new chat; nil when none is paired.
    let hostID: String?
    let hostName: String
    /// That Mac's Projects, most recently used first.
    let projects: [Project]
    /// That Mac's Project threads, most recently active first.
    let threads: [Thread]

    init(savedAt: Date = Date(), hostID: String?, hostName: String,
         projects: [Project], threads: [Thread] = []) {
        self.schemaVersion = Self.version
        self.savedAt = savedAt
        self.hostID = hostID
        self.hostName = hostName
        self.projects = projects
        self.threads = threads
    }

    /// Validate after decoding as well as before writing: the shared container
    /// is a data boundary, not a source of trusted URLs or unbounded view text.
    func validated() -> Self? {
        guard schemaVersion == Self.version, savedAt.timeIntervalSince1970.isFinite else { return nil }
        guard let hostID, ProjectWidgetLink.validID(hostID) else {
            return Self(savedAt: savedAt, hostID: nil, hostName: "Computer", projects: [], threads: [])
        }
        var seen = Set<String>()
        var safe: [Project] = []
        for project in projects {
            if safe.count == Self.maxProjects { break }
            guard ProjectWidgetLink.validID(project.id), seen.insert(project.id).inserted else { continue }
            safe.append(Project(id: project.id, name: Self.safeLabel(project.name, fallback: "Project")))
        }
        var safeThreads: [Thread] = []
        var seenThreads = Set<String>()
        for thread in threads {
            if safeThreads.count == Self.maxThreads { break }
            guard ProjectWidgetLink.validID(thread.projectID), ProjectWidgetLink.canOpen(thread),
                  thread.conversationID.map(ProjectWidgetLink.validID) ?? true,
                  thread.nativeID.map(ProjectWidgetLink.validID) ?? true,
                  seenThreads.insert(thread.id).inserted else { continue }
            safeThreads.append(Thread(projectID: thread.projectID, family: thread.family, nativeID: thread.nativeID,
                                      conversationID: thread.conversationID,
                                      title: Self.safeLabel(thread.title, fallback: "Thread"),
                                      projectName: Self.safeLabel(thread.projectName, fallback: "Project")))
        }
        return Self(savedAt: savedAt, hostID: hostID, hostName: Self.safeLabel(hostName, fallback: "Computer"),
                    projects: safe, threads: safeThreads)
    }

    func isStale(at date: Date) -> Bool {
        let age = date.timeIntervalSince(savedAt)
        return age < -5 * 60 || age >= Self.staleAfter
    }

    private static func safeLabel(_ raw: String, fallback: String) -> String {
        let scalars = raw.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }
        let label = String(String.UnicodeScalarView(scalars.prefix(80)))
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return label.isEmpty ? fallback : label
    }
}

/// What VoiceOver reads for the Recent Threads widget. While iOS redacts
/// private widget content (a locked device), thread, Project and computer
/// names stay unspoken just as they stay hidden.
enum ProjectWidgetSpeech {
    static let stale = "Out of date. Open Cupola to refresh."

    static func destination(title: String, detail: String, redacted: Bool) -> String {
        redacted ? "Recent chat" : "\(title), \(detail)"
    }

    static func computer(hostName: String, redacted: Bool) -> String {
        redacted ? "Computer" : "Computer on \(hostName)"
    }

    /// The small widget is one tap target, so its label carries everything.
    static func small(destination: String?, hostName: String, stale: Bool, redacted: Bool) -> String {
        let main = destination.map { redacted ? $0 : "\($0) on \(hostName)" } ?? computer(hostName: hostName, redacted: redacted)
        return stale ? "\(main). \(Self.stale)" : main
    }
}

/// App and extension resolve their shared container from exact bundle IDs;
/// any other identity (a Debug build) gets no snapshot or links.
enum ProjectWidgetIdentity: String {
    case production = "com.swaymun.wonder"

    init?(bundleIdentifier: String?) {
        switch bundleIdentifier {
        case Self.production.rawValue, Self.production.rawValue + ".Widget": self = .production
        default: return nil
        }
    }

    var appGroupID: String { "group.\(rawValue)" }
    var linkScheme: String { "wonder" }
}

enum ProjectWidgetLink {
    static func newChat(hostID: String, projectID: String, identity: ProjectWidgetIdentity) -> URL? {
        guard validID(hostID), validID(projectID) else { return nil }
        return url(path: "/hosts/\(hostID)/projects/\(projectID)/new", identity: identity)
    }

    static func computer(hostID: String, identity: ProjectWidgetIdentity) -> URL? {
        guard validID(hostID) else { return nil }
        return url(path: "/hosts/\(hostID)/computer", identity: identity)
    }

    static func chat(hostID: String, chatID: String, identity: ProjectWidgetIdentity) -> URL? {
        guard validID(hostID), validID(chatID) else { return nil }
        return url(path: "/hosts/\(hostID)/chats/\(chatID)", identity: identity)
    }

    /// A conversation, or a provider thread the app listed because its
    /// provider's sessions open from the Mac. Snapshot rows and links share
    /// this destination check.
    static func canOpen(_ thread: ProjectWidgetSnapshot.Thread) -> Bool {
        guard ["codex", "claude", "opencode", "cursor"].contains(thread.family) else { return false }
        if let conversation = thread.conversationID { return validID(conversation) }
        guard let native = thread.nativeID else { return false }
        return !native.isEmpty && native.utf8.count <= 128
            && native.utf8.allSatisfy { (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) || $0 == 45 }
    }

    /// Opens an existing conversation, or a supported native thread.
    static func thread(hostID: String, thread: ProjectWidgetSnapshot.Thread, identity: ProjectWidgetIdentity) -> URL? {
        guard validID(hostID), validID(thread.projectID), canOpen(thread) else { return nil }
        if let conversation = thread.conversationID { return chat(hostID: hostID, chatID: conversation, identity: identity) }
        guard let native = thread.nativeID else { return nil }
        return url(path: "/hosts/\(hostID)/projects/\(thread.projectID)/threads/\(thread.family)/\(native)", identity: identity)
    }

    static func validID(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 128 && value.utf8.allSatisfy {
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 95
        }
    }

    private static func url(path: String, identity: ProjectWidgetIdentity) -> URL? {
        var components = URLComponents()
        components.scheme = identity.linkScheme
        components.host = "v1"
        components.path = path
        guard let url = components.url, url.absoluteString.utf8.count <= 512 else { return nil }
        return url
    }
}

enum ProjectWidgetSnapshotStore {
    private static let fileName = "project-widget-snapshot-v3.json"

    static func containerURL(bundleIdentifier: String?) -> URL? {
        guard let identity = ProjectWidgetIdentity(bundleIdentifier: bundleIdentifier),
              let directory = FileManager.default.containerURL(
                forSecurityApplicationGroupIdentifier: identity.appGroupID) else { return nil }
        return directory
    }

    @discardableResult static func save(_ snapshot: ProjectWidgetSnapshot, bundleIdentifier: String?) -> Bool {
        guard let directory = containerURL(bundleIdentifier: bundleIdentifier) else { return false }
        return save(snapshot, in: directory)
    }

    /// Replaces the old file atomically, including when a previously corrupt
    /// snapshot needs to be recovered.
    @discardableResult static func save(_ snapshot: ProjectWidgetSnapshot, in directory: URL) -> Bool {
        guard let validated = snapshot.validated(),
              let data = try? JSONEncoder().encode(validated), data.count <= ProjectWidgetSnapshot.maxBytes else {
            return false
        }
        return (try? data.write(to: directory.appendingPathComponent(fileName), options: .atomic)) != nil
    }

    static func load(bundleIdentifier: String?) -> ProjectWidgetSnapshot? {
        guard let directory = containerURL(bundleIdentifier: bundleIdentifier) else { return nil }
        return load(from: directory)
    }

    static func load(from directory: URL) -> ProjectWidgetSnapshot? {
        let file = directory.appendingPathComponent(fileName)
        guard let properties = try? file.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey]),
              properties.isRegularFile == true, properties.isSymbolicLink != true,
              let handle = try? FileHandle(forReadingFrom: file) else { return nil }
        defer { try? handle.close() }
        var data = Data()
        while data.count <= ProjectWidgetSnapshot.maxBytes {
            let part: Data
            do {
                part = try handle.read(upToCount: min(4096, ProjectWidgetSnapshot.maxBytes + 1 - data.count)) ?? Data()
            } catch {
                return nil
            }
            if part.isEmpty { break }
            data.append(part)
        }
        guard data.count <= ProjectWidgetSnapshot.maxBytes,
              let snapshot = try? JSONDecoder().decode(ProjectWidgetSnapshot.self, from: data) else { return nil }
        return snapshot.validated()
    }
}
