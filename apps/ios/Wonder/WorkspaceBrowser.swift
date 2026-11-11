import SwiftUI
import AVKit
import ImageIO
import PhotosUI
import WonderPairing
import UniformTypeIdentifiers
import PDFKit
import WebKit
import VisionKit
import Photos

struct WorkspacePreviewSelection: Identifiable {
    let id: String
    let name: String
    let mimeType: String
    let data: Data
    let sha256: String
    var rootID: String? = nil
    var path: String? = nil
    var scope: String? = nil
}

struct WorkspaceObservationPoint: Hashable {
    let hostEpoch: String
    let sequence: UInt64
}

/// Asks the computer for the open file again. `validator` is the change
/// token of the last read (the computer answers `.unchanged` while it is
/// current); `whole` asks for every byte rather than only what the preview
/// shows.
typealias WorkspaceRevisionRead = (_ validator: String?, _ whole: Bool) async throws -> WorkspaceFileReadResult

/// One selected file owns its verified revision across inline and full-screen
/// presentations. The open preview polls its own file while it is visible and
/// the app is active; closed files and chat events never trigger a read.
@MainActor final class WorkspaceRevisionState: ObservableObject {
    @Published var currentData: Data
    /// The hash of the displayed bytes. For a file shown only in part it
    /// names that start, never the file; comments need a whole read.
    @Published var currentSha256: String
    /// The file's length on the computer: longer than `currentData` while
    /// only the start of a long text file has been read.
    @Published private(set) var totalBytes: Int
    /// A newer revision held back while the reader is writing a comment on
    /// the displayed bytes. It is shown as soon as the comment is finished.
    @Published var offeredData: Data?
    @Published var offeredSha256: String?
    private var offeredTotalBytes = 0
    @Published var refreshFailure: String?
    @Published var preparedText: String?
    @Published var preparingText = true
    @Published var previewTruncated = false
    /// The reader asked for the whole of a file shown in part.
    @Published private(set) var loadingWhole = false
    @Published private(set) var wholeFailure: String?
    /// Set by the preview while a comment selection is in progress so the
    /// selected byte offsets keep pointing at the displayed revision.
    var holdsRevision = false
    private(set) var refreshing = false
    private var watchGeneration = UUID()
    private var lastCheckAt: TimeInterval
    private var preparedSha256: String?
    private var closed = false
    /// The computer's change token for the last read (not a content hash),
    /// and whether that read was only the file's start.
    private var validator: String?
    private var validatorIsPartial = false
    /// Once the reader loads the whole file, later reads stay whole.
    private var wantsWhole = false
    private var unchangedChecks = 0
    /// The hash of the last whole read, which draft comments are checked
    /// against; nil while only the file's start has been read.
    private var lastWholeSha256: String?

    var isPartial: Bool { currentData.count < totalBytes }

    /// Text shows at most this much of a file.
    nonisolated static let textPreviewLimit = 128 * 1024
    /// Text shows at most its first 128 KB, so a file that isn't a picture or
    /// a PDF is read only that far (a longer HTML page isn't shown at all, and
    /// a file that isn't text says so after 128 KB). Pictures and PDFs need
    /// every byte (an SVG is checked whole before it is drawn). A computer
    /// that can't send part of a file sends it whole.
    nonisolated static func previewPrefix(mimeType: String?) -> Int? {
        let mime = (mimeType ?? "").lowercased()
        if mime == "application/pdf" || mime.hasPrefix("image/") { return nil }
        return textPreviewLimit
    }

    /// While the computer can say a file is unchanged, a check sends no
    /// body, so it starts every 3 s. Otherwise each check downloads what is
    /// shown again, so passive reads stay near 128 KB/s at any size. Each
    /// check that finds nothing new doubles the wait, up to a minute (or the
    /// download's own interval when that is longer).
    static func pollInterval(byteCount: Int, conditional: Bool = false, unchangedChecks: Int = 0) -> TimeInterval {
        let base = conditional ? 3 : max(3, Double(byteCount) / (128 * 1024))
        return min(max(60, base), base * Double(1 << min(max(0, unchangedChecks), 5)))
    }
    /// Consecutive failed reads stay quiet until the Mac has been unreachable
    /// for a while; one dropped request is not worth an alert.
    static let failuresBeforeNotice = 3

    init(data: Data, sha256: String, totalBytes: Int? = nil, validator: String? = nil) {
        let total = max(totalBytes ?? data.count, data.count)
        currentData = data
        currentSha256 = sha256
        self.totalBytes = total
        self.validator = validator
        validatorIsPartial = data.count < total
        lastWholeSha256 = data.count < total ? nil : sha256
        lastCheckAt = ProcessInfo.processInfo.systemUptime
    }

    func prepareTextIfNeeded(mimeType: String) async {
        guard !closed, mimeType != "application/pdf", preparedSha256 != currentSha256 else { return }
        let data = currentData
        let sha256 = currentSha256
        let partial = isPartial
        preparingText = true
        let prepared = await Self.prepareText(data, mimeType: mimeType, partial: partial)
        guard !Task.isCancelled, !closed, currentSha256 == sha256 else { return }
        preparedText = prepared.0
        previewTruncated = prepared.1
        preparedSha256 = sha256
        preparingText = false
    }

    /// The previewable text of a verified file, prepared off the main thread:
    /// at most its first 128 KB (HTML only whole), and whether it was cut.
    /// Files and files opened from a reply both show text this way. `partial`
    /// data is only the file's start, so it is always cut.
    static func prepareText(_ data: Data, mimeType: String, partial: Bool = false) async -> (String?, Bool) {
        await Task.detached(priority: .userInitiated) { () -> (String?, Bool) in
            // TextKit must never lay out an entire multi-megabyte source on the
            // main thread. The preview is an exact prefix of the verified bytes,
            // so a selection still maps to the same UTF-8 source offsets.
            let limit = WorkspaceRevisionState.textPreviewLimit
            if partial {
                // The start of a longer file may end inside a character; a
                // file that isn't text fails without scanning back further.
                let head = data.prefix(limit)
                for drop in 0...min(3, head.count) {
                    if let text = String(data: head.dropLast(drop), encoding: .utf8) {
                        return (mimeType == "text/html" ? nil : text, true)
                    }
                }
                return (nil, false)
            }
            guard String(data: data, encoding: .utf8) != nil else { return (nil, false) }
            guard data.count > limit else { return (String(data: data, encoding: .utf8), false) }
            guard mimeType != "text/html" else { return (nil, true) }
            var end = limit
            while end > 0, String(data: data.prefix(end), encoding: .utf8) == nil { end -= 1 }
            return (String(data: data.prefix(end), encoding: .utf8), true)
        }.value
    }

    /// Polls the open file until the owning view's task is cancelled. The
    /// first read waits out the interval since the file (or last check) loaded.
    func watch(mimeType: String?, refresh: WorkspaceRevisionRead?,
               onRevision: ((String) async -> Void)?, failureMessage: String,
               interval: TimeInterval? = nil) async {
        guard let refresh, !closed else { return }
        let generation = UUID()
        watchGeneration = generation
        var failures = 0
        while !Task.isCancelled, !closed, generation == watchGeneration {
            let cadence = interval ?? Self.pollInterval(byteCount: max(currentData.count, offeredData?.count ?? 0),
                                                        conditional: validator != nil, unchangedChecks: unchangedChecks)
            // An offline Mac is retried progressively less often.
            let delay = failures == 0 ? cadence : min(60, cadence * Double(1 << min(failures, 4)))
            let wait = max(0, delay - (ProcessInfo.processInfo.systemUptime - lastCheckAt))
            if wait > 0 {
                do { try await Task.sleep(for: .milliseconds(Int64((wait * 1000).rounded(.up)))) } catch { return }
            }
            guard !Task.isCancelled, !closed, generation == watchGeneration else { return }
            // The whole file the reader asked for is already on its way.
            if loadingWhole { lastCheckAt = ProcessInfo.processInfo.systemUptime; continue }
            refreshing = true
            defer { refreshing = false }
            do {
                // A token from a partial read can't stand for the whole file.
                let whole = wantsWhole
                let result = try await refresh(validatorIsPartial && whole ? nil : validator, whole)
                guard !Task.isCancelled, !closed, generation == watchGeneration else { return }
                guard case .read(let read) = result else {
                    // Unchanged since the last read: no body, nothing to hash.
                    lastCheckAt = ProcessInfo.processInfo.systemUptime
                    failures = 0
                    refreshFailure = nil
                    unchangedChecks += 1
                    if let lastWholeSha256 { await onRevision?(lastWholeSha256) }
                    continue
                }
                let sha256 = await Task.detached(priority: .utility) { ConversationFile.digest(read.data) }.value
                guard !Task.isCancelled, !closed, generation == watchGeneration else { return }
                lastCheckAt = ProcessInfo.processInfo.systemUptime
                failures = 0
                refreshFailure = nil
                // The reader loaded the whole file while this partial read was out.
                if read.isPartial, wantsWhole { validator = nil; continue }
                validator = read.validator
                validatorIsPartial = read.isPartial
                lastWholeSha256 = read.isPartial ? nil : sha256
                if sha256 == currentSha256, read.totalBytes == totalBytes {
                    unchangedChecks += 1
                    offeredData = nil; offeredSha256 = nil
                } else {
                    unchangedChecks = 0
                    if holdsRevision {
                        if sha256 != offeredSha256 {
                            offeredData = read.data; offeredSha256 = sha256; offeredTotalBytes = read.totalBytes
                        }
                    } else {
                        await apply(data: read.data, sha256: sha256, totalBytes: read.totalBytes, mimeType: mimeType)
                        guard !Task.isCancelled, !closed, generation == watchGeneration else { return }
                    }
                }
                // Draft comments may have changed since the last check, or the
                // file may have reverted to the version a comment was made on.
                // Only a whole read says which version the file is.
                if !read.isPartial { await onRevision?(sha256) }
            } catch is CancellationError {
                return
            } catch {
                guard !Task.isCancelled, !closed, generation == watchGeneration else { return }
                // Timestamp the completed failure so a slow timeout does not
                // consume its own retry delay.
                lastCheckAt = ProcessInfo.processInfo.systemUptime
                failures = min(failures + 1, 5)
                if failures >= Self.failuresBeforeNotice { refreshFailure = failureMessage }
            }
        }
    }

    /// Reads every byte of a file shown only in part, so a comment can be
    /// bound to the file's own revision. Later checks stay whole.
    func loadWhole(mimeType: String?, read: WorkspaceRevisionRead?, onRevision: ((String) async -> Void)?) async {
        guard let read, !closed, isPartial, !loadingWhole else { return }
        wantsWhole = true
        loadingWhole = true
        wholeFailure = nil
        defer { loadingWhole = false }
        do {
            guard case .read(let whole) = try await read(nil, true), !whole.isPartial else { throw URLError(.badServerResponse) }
            guard !closed else { return }
            let sha256 = await Task.detached(priority: .userInitiated) { ConversationFile.digest(whole.data) }.value
            guard !closed else { return }
            validator = whole.validator
            validatorIsPartial = false
            lastWholeSha256 = sha256
            lastCheckAt = ProcessInfo.processInfo.systemUptime
            unchangedChecks = 0
            await apply(data: whole.data, sha256: sha256, totalBytes: whole.totalBytes, mimeType: mimeType)
            await onRevision?(sha256)
        } catch {
            wantsWhole = false
            guard !closed, !(error is CancellationError) else { return }
            if case FileFailure.tooLarge = error {
                wholeFailure = "This file is too large to comment on here. Open it on your computer."
            } else {
                wholeFailure = "Couldn’t load the whole file. Check that your computer is connected and try again."
            }
        }
    }

    /// Prepares the replacement before publishing it, so text, offsets and
    /// bytes change together and the reader never sees an empty interim.
    private func apply(data: Data, sha256: String, totalBytes: Int, mimeType: String?) async {
        let displayed = currentSha256
        let prepared: (String?, Bool)?
        if let mimeType, mimeType != "application/pdf" {
            prepared = await Self.prepareText(data, mimeType: mimeType, partial: data.count < totalBytes)
        } else { prepared = nil }
        guard !Task.isCancelled, !closed else { return }
        guard currentSha256 == displayed else {
            // Another revision was shown meanwhile; read again rather than
            // let a later "unchanged" keep these bytes off screen.
            validator = nil
            return
        }
        if holdsRevision {
            // A comment began while the replacement was being prepared.
            offeredData = data; offeredSha256 = sha256; offeredTotalBytes = totalBytes
            return
        }
        if let prepared {
            preparedText = prepared.0; previewTruncated = prepared.1
            preparedSha256 = sha256; preparingText = false
        }
        currentData = data; currentSha256 = sha256; self.totalBytes = totalBytes
        if offeredSha256 != nil { offeredData = nil; offeredSha256 = nil }
    }

    /// Shows a revision that arrived while a comment was in progress.
    func showOfferedRevision(mimeType: String?) async {
        guard !closed, let data = offeredData, let sha256 = offeredSha256 else { return }
        await apply(data: data, sha256: sha256, totalBytes: max(offeredTotalBytes, data.count), mimeType: mimeType)
    }

    func close() {
        closed = true
        watchGeneration = UUID()
    }
}

@MainActor final class WorkspaceTextAnnotationDraft: ObservableObject {
    @Published var selectedRange: NSRange?
    @Published var selectionResetID = UUID()
    @Published var editingSelection = false
    @Published var note = ""
    @Published var noteSelection: NSRange?
    /// The reader is dragging a native text selection that may become a comment.
    @Published var selectingText = false
    var activeNoteFieldID: UUID?
    @Published var error: String?

    func clearSelection(discardNote: Bool) {
        selectedRange = nil
        selectingText = false
        selectionResetID = UUID()
        editingSelection = false
        activeNoteFieldID = nil
        noteSelection = nil
        error = nil
        if discardNote { note = "" }
    }

    func reset(note: String = "") {
        clearSelection(discardNote: true)
        self.note = note
        noteSelection = nil
    }
}

struct ArtifactPreviewContext {
    let projectID: String
    let conversationID: String
    let rootID: String
    let path: String
    let scope: String
}

/// New Chat has a Project workspace but no provider conversation yet. Its
/// comments are kept in the local draft until creation supplies one.
struct WorkspaceDraftAnnotationActions {
    let projectID: String
    let draftID: String
    let unavailableReason: String?
    let sketchUnavailableReason: String?
    let stage: (ArtifactAnnotation) throws -> String
    let stageSketch: (Data) async -> SketchAttachmentResult
    let contains: (String) -> Bool
    let noteRevision: (String, String, String) -> Void
}

/// Keep the destination with the sheet's identity so its first presentation
/// cannot capture a filename or attachment filter from an earlier render.
struct WorkspaceBrowserRequest: Identifiable {
    let id = UUID()
    var attachmentIDs: [String]? = nil
    var initialFilePath: String? = nil
    var preferredRootID: String? = nil
    var reanchor: ArtifactAnnotation? = nil
    var replacingAnnotationID: String? = nil
}

@MainActor final class WorkspaceMediaSelection: ObservableObject, Identifiable {
    let id: String
    let name: String
    let loader: WorkspaceMediaResourceLoader
    let asset: AVURLAsset
    @Published private(set) var player: AVPlayer?
    @Published private(set) var failure: String?
    /// Playing was refused because dictation or voice is using audio.
    @Published private(set) var blocked = false
    private var monitor: Task<Void, Never>?
    /// Sets up the phone's audio for playback while this preview plays.
    private var claim: MediaSessionClaim?
    private var closed = false

    init(id: String, name: String, loader: WorkspaceMediaResourceLoader) throws {
        self.id = id
        self.name = name
        self.loader = loader
        asset = try loader.asset(filename: name)
    }

    func start() {
        guard monitor == nil, !closed else { return }
        let asset = asset
        monitor = Task { [weak self] in
            do {
                guard try await asset.load(.isPlayable) else {
                    self?.failure = "This audio or video format cannot be played on this device."
                    return
                }
                guard !Task.isCancelled, self?.closed == false else { return }
                let item = AVPlayerItem(asset: asset)
                let player = AVPlayer(playerItem: item)
                let claim = MediaSessionClaim(player: player)
                claim.onBlocked = { [weak self] blocked in self?.blocked = blocked }
                self?.claim = claim
                self?.player = player
                while !Task.isCancelled {
                    guard self?.closed == false else { return }
                    if self?.reportFailureIfNeeded(for: item) == true { return }
                    try await Task.sleep(for: .milliseconds(500))
                }
            } catch is CancellationError {
            } catch {
                guard !Task.isCancelled, self?.closed == false else { return }
                self?.failure = self?.loader.fileChanged == true
                    ? "File changed. Close and reopen the preview."
                    : "This file could not be streamed. Check your computer connection and try again."
            }
        }
    }

    private func reportFailureIfNeeded(for item: AVPlayerItem) -> Bool {
        guard item.status == .failed else { return false }
        failure = loader.fileChanged
            ? "File changed. Close and reopen the preview."
            : "Playback stopped. Check your computer connection or try a different file."
        player?.pause()
        claim?.end(); claim = nil
        player = nil
        return true
    }

    func close() {
        guard !closed else { return }
        closed = true
        monitor?.cancel()
        monitor = nil
        player?.pause()
        claim?.end(); claim = nil
        player?.replaceCurrentItem(with: nil)
        player = nil
        loader.cancelAll()
    }

    deinit {
        monitor?.cancel()
        loader.cancelAll()
    }
}

struct WorkspaceFileLoad: Equatable {
    let id: UUID
    let name: String
    var received: Int
    var expected: Int?
}

/// Previews arrive from the Mac over the network. The download takes over the
/// Files pane with its progress; the X in the ring cancels it.
private struct WorkspaceFileLoadOverlay: View {
    @Environment(\.wonderTheme) private var theme
    let load: WorkspaceFileLoad
    let cancel: () -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    private func size(_ bytes: Int) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
    private var fraction: Double? {
        guard let expected = load.expected, expected > 0 else { return nil }
        return min(1, Double(load.received) / Double(expected))
    }
    private var detail: String {
        guard let expected = load.expected, expected > 0 else { return size(load.received) }
        return "\(size(load.received)) of \(size(expected))"
    }
    var body: some View {
        VStack(spacing: 16) {
            Button(action: cancel) {
                ZStack {
                    Circle().stroke(Color.secondary.opacity(0.25), lineWidth: 5)
                    if let fraction {
                        Circle().trim(from: 0, to: max(0.02, fraction))
                            .stroke(theme.accent, style: StrokeStyle(lineWidth: 5, lineCap: .round))
                            .rotationEffect(.degrees(-90))
                            .animation(reduceMotion ? nil : .linear(duration: 0.2), value: fraction)
                    } else {
                        ProgressView().controlSize(.large)
                    }
                    Image(systemName: "xmark").font(.title3.weight(.semibold)).foregroundStyle(.primary)
                }
                .frame(width: 72, height: 72)
                .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Cancel download")
            .accessibilityIdentifier("workspace-file-load-cancel")
            VStack(spacing: 4) {
                Text(load.name).font(.headline).lineLimit(2).multilineTextAlignment(.center)
                Text(detail).font(.subheadline.monospacedDigit()).foregroundStyle(.secondary)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Opening \(load.name), \(detail)")
            .accessibilityIdentifier("workspace-file-loading")
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(theme.page)
        .transition(.opacity)
    }
}

/// Files inside a conversation close from the composer's Files button. A sheet
/// has no composer, so it carries its own Done control.
struct WorkspaceSheet<Content: View>: View {
    let close: () -> Void
    @ViewBuilder let content: Content
    @Environment(\.companionPane) private var pane
    @State private var verticalBar = false
    private var inlineHeader: Bool { pane != nil }
    var body: some View {
        NavigationStack {
            content
                .wonderPage()
                // Where the pane draws a tool's own header row, the browser
                // draws it with its location and actions in that one row.
                .environment(\.workspacePaneHeader, inlineHeader ? WorkspacePaneHeaderContext(close: close) : nil)
                .navigationTitle(inlineHeader ? "" : "Files").navigationBarTitleDisplayMode(.inline)
                .toolbar(inlineHeader ? .hidden : .automatic, for: .navigationBar)
                .toolbar {
                    if !inlineHeader {
                        ToolbarItem(placement: .confirmationAction) {
                            HStack {
                                CompanionExpandButton()
                                WorkspaceDoneControl(close: close)
                            }
                        }
                    }
                }
        }
        .readsVerticalBar($verticalBar)
    }
}

struct WorkspaceDoneControl: View {
    let close: () -> Void
    var body: some View {
        ToolCloseControl { Button("Done", action: close).accessibilityIdentifier("workspace-sheet-done") }
    }
}

/// Set by `WorkspaceSheet` where the companion pane draws a tool's own header
/// row (the Duo); nil in a sheet and where the pane uses the toolbar.
struct WorkspacePaneHeaderContext {
    let close: () -> Void
}

extension EnvironmentValues {
    @Entry var workspacePaneHeader: WorkspacePaneHeaderContext?
}

/// A computer's home folder shown as ~, the way a terminal shows it: /Users/maya/Code is
/// ~/Code, C:\Users\Maya\Code is ~\Code. The computer sends only absolute paths, so the
/// home folder is recognised by where the system keeps them.
enum HomeFolder {
    static func abbreviated(_ path: String) -> String {
        for root in ["/Users/", "/home/", "C:\\Users\\", "C:/Users/"] where path.hasPrefix(root) {
            let rest = path.dropFirst(root.count)
            if rest.hasPrefix("Shared") { return path }
            guard let separator = rest.firstIndex(where: { $0 == "/" || $0 == "\\" }) else { return rest.isEmpty ? path : "~" }
            return "~" + rest[separator...]
        }
        if path == "/root" { return "~" }
        if path.hasPrefix("/root/") { return "~" + path.dropFirst("/root".count) }
        return path
    }
}

/// A Files row: its icon starts 16 pt in from the pane's edge, left-aligned in one
/// slot width so folders, documents and pictures line up with each other and
/// with the location row above.
struct FilesRowLabel: View {
    static let iconWidth: CGFloat = 28
    static let spacing: CGFloat = 12
    static let insets = EdgeInsets(top: 8, leading: 16, bottom: 8, trailing: 16)
    let name: String
    let symbol: String

    var body: some View {
        HStack(spacing: Self.spacing) {
            Image(systemName: symbol).frame(width: Self.iconWidth, alignment: .leading)
            Text(name).fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .combine)
    }
}

/// The Files list's one location row, with a full path in its folder menu.
/// Actions move into ⋯ at accessibility text sizes rather than being cut short.
struct WorkspacePaneHeader: Equatable {
    let location: String
    let showsBack: Bool
    let picksRoot: Bool
    let showsHiddenToggle: Bool
    let actionsInMenu: Bool

    init(modified: Bool, directoryPath: String, rootLabel: String?, rootCount: Int,
         rootIsDirectory: Bool, accessibilitySize: Bool) {
        let folder = rootLabel ?? ""
        location = modified ? "Modified" : HomeFolder.abbreviated(directoryPath.isEmpty ? folder : (folder as NSString).appendingPathComponent(directoryPath))
        showsBack = !modified && rootIsDirectory && !directoryPath.isEmpty
        picksRoot = rootCount > 0
        showsHiddenToggle = !modified && rootIsDirectory
        actionsInMenu = accessibilitySize
    }
}

struct WorkspaceBrowser: View {
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.wonderTheme) private var theme
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.workspacePaneHeader) private var paneHeader
    @Environment(\.companionPane) private var companionPane
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let attachmentIDs: [String]?
    var initialFilePath: String? = nil
    var preferredRootID: String? = nil
    var reanchor: ArtifactAnnotation? = nil
    var replacingAnnotationID: String? = nil
    var draftAnnotationActions: WorkspaceDraftAnnotationActions? = nil
    @State private var openedInitialFile = false
    @State private var response: WorkspaceRootsResponse?
    @State private var selectedRootID: String?
    @State private var directoryPath = ""
    @State private var directory: WorkspaceDirectoryPage?
    @State private var missingDirectory = false
    @State private var git: WorkspaceGitStatusResponse?
    @State private var viewMode = "all"
    @State private var showHidden = false
    @State private var loadingRequests: Set<UUID> = []
    @State private var rootsLoadingID: UUID?
    @State private var locationGeneration = UUID()
    @State private var rootsRequestID = UUID()
    @State private var loadedRootsScope: String?
    @State private var directoryRequestID = UUID()
    @State private var gitRequestID = UUID()
    @State private var listingRefreshTask: Task<Void, Never>?
    @State private var listingRefreshID: UUID?
    @State private var listedPoint: WorkspaceObservationPoint?
    @State private var listingRetryCount = 0
    @State private var lastListingAttemptAt: TimeInterval?
    @State private var diffRequestID = UUID()
    @State private var fileRequestID = UUID()
    @State private var attachmentRequestID = UUID()
    @State private var failure: String?
    /// An audio or video file in a format Cupola can't play, named in an alert.
    @State private var unsupportedMediaName: String?
    /// The computer's reason the thread's folder, a worktree its Project
    /// leaves out, can't be read. It outlasts a failed read of the Project's
    /// own folders: adding the folder replaces Try again, which could not help.
    @State private var worktreeRefusal: String?
    private var outsideProject: Bool { worktreeRefusal != nil }
    @StateObject private var folderAdder = WorkspaceFolderAdder()
    /// While the worktree is left out, the Project's own folders, read-only
    /// through the Project's Files scope: what the computer still lets this
    /// thread read. Nil reads the thread's own folder.
    @State private var projectFolders: ChatSummary?
    @State private var addFolderError: String?
    private var source: ChatSummary { projectFolders ?? chat }
    @State private var fileLoad: WorkspaceFileLoad?
    @State private var fileLoadTask: Task<Void, Never>?
    /// Closing a preview rebuilds the list; return to the row that was opened.
    @State private var returnEntryPath: String?
    @State private var selection: WorkspacePreviewSelection?
    @State private var documentSelection: WorkspacePreviewSelection?
    /// Images a Markdown Preview showed from this root, bounded and dropped on
    /// memory pressure or when the preview closes.
    @State private var markdownImages: NSCache<NSString, UIImage> = {
        let cache = NSCache<NSString, UIImage>()
        cache.countLimit = 24
        cache.totalCostLimit = 48 * 1024 * 1024
        return cache
    }()
    @State private var previewRevision: WorkspaceRevisionState?
    @State private var mediaSelection: WorkspaceMediaSelection?
    @State private var attachmentPhoto: ConversationFile?
    @State private var attachmentSelection: ConversationFile?
    @State private var attachmentData: Data?
    @State private var attachmentDigest: String?
    @State private var selectedDiff: WorkspaceGitChange?
    @State private var selectedDiffStaged = false
    @State private var diffChoice: WorkspaceGitChange?
    @State private var diffText: String?
    @State private var expandedPreview = false
    @State private var collapsingPreview = false
    @State private var addedAnnotationID: String?
    @State private var pdfSession = PDFPreviewSession()
    @State private var htmlSession = WorkspaceHTMLPreviewSession()
    @StateObject private var textAnnotationDraft = WorkspaceTextAnnotationDraft()

    private var loading: Bool { !loadingRequests.isEmpty }
    private var selectedRoot: WorkspaceRoot? { response?.roots.first(where: { $0.id == selectedRootID }) }
    /// Files lists the project folder. Message attachments appear only when a
    /// message's attachment chip opened this browser for those files.
    private var visibleAttachments: [ConversationFile] {
        guard let attachmentIDs else { return [] }
        return (response?.attachments ?? []).filter { attachmentIDs.contains($0.id) }
    }
    // Hidden-file changes are explicitly reloaded by the action below. Keeping
    // this task identity scoped to navigation prevents one tap from starting
    // both the implicit task and the explicit refresh.
    private var directoryTaskID: String {
        "\(model.assignmentScope):\(source.id):\(selectedRootID ?? ""):\(selectedRoot?.path ?? ""):\(directoryPath)"
    }
    private var hasPreview: Bool {
        selection != nil || documentSelection != nil || mediaSelection != nil ||
        attachmentPhoto != nil || (attachmentSelection != nil && attachmentData != nil && attachmentDigest != nil) ||
        (selectedDiff != nil && diffText != nil)
    }
    private var shouldPollListing: Bool { scenePhase == .active && !hasPreview }
    private var previewName: String {
        selection?.name ?? documentSelection?.name ?? mediaSelection?.name ?? attachmentPhoto?.name ??
        attachmentSelection?.name ?? selectedDiff?.path ?? "Preview"
    }
    private var observationPoint: WorkspaceObservationPoint? {
        model.snapshots[chat.id].map { WorkspaceObservationPoint(hostEpoch: $0.hostEpoch, sequence: $0.lastSequence) }
    }
    private var annotationAdded: Bool {
        guard let addedAnnotationID else { return false }
        if let draftAnnotationActions { return draftAnnotationActions.contains(addedAnnotationID) }
        return model.composers[chat.id]?.attachmentIDs.contains(addedAnnotationID) == true
    }

    /// Files use the same compact, separated rows in sheets and companion panes.
    private var plainRows: Bool { true }

    @ViewBuilder private var workspaceActions: some View {
        if viewMode == "all", selectedRoot?.isDirectory == true {
            Button(action: toggleHiddenFiles) {
                Image(systemName: showHidden ? "eye" : "eye.slash")
                    .frame(minWidth: 44, minHeight: 44)
            }
            .accessibilityLabel(showHidden ? "Hide hidden files" : "Show hidden files")
            .accessibilityValue(showHidden ? "On" : "Off")
            .accessibilityIdentifier("workspace-hidden-toggle")
        }
        Button(viewMode == "all" ? "Modified" : "All files", action: toggleViewMode)
        .font(.subheadline)
        .lineLimit(1)
        .frame(minHeight: 44)
        .accessibilityIdentifier("workspace-view-toggle")
    }

    private var workspaceControls: some View {
        let layout = paneHeaderLayout
        return HStack(spacing: 8) {
            if layout.showsBack {
                Button { navigate(to: directory?.parentPath ?? "") } label: {
                    Image(systemName: "chevron.left").frame(minWidth: 44, minHeight: 44)
                }
                .accessibilityLabel("Back")
                .accessibilityIdentifier("workspace-back")
            }
            paneTitle(layout)
            Spacer(minLength: 0)
            if layout.actionsInMenu { paneActionsMenu(layout) }
            else { workspaceActions }
        }
        .padding(.horizontal, 16)
    }

    private func toggleViewMode() {
        viewMode = viewMode == "all" ? "modified" : "all"
    }

    private var paneHeaderLayout: WorkspacePaneHeader {
        WorkspacePaneHeader(modified: viewMode != "all", directoryPath: directoryPath,
                            rootLabel: selectedRoot?.path, rootCount: response?.roots.count ?? 0,
                            rootIsDirectory: selectedRoot?.isDirectory == true,
                            accessibilitySize: typeSize.isAccessibilitySize)
    }

    /// Lists show their path and actions in one row. An open file replaces that
    /// row with its own Back/title/expand row rather than stacking both headers.
    @ViewBuilder private func paneHeaderRow(_ context: WorkspacePaneHeaderContext) -> some View {
        if !hasPreview {
            let layout = paneHeaderLayout
            CompanionInlineHeader {
                HStack(spacing: 2) {
                    if layout.showsBack {
                        Button { navigate(to: directory?.parentPath ?? "") } label: {
                            Image(systemName: "chevron.left")
                                .frame(minWidth: 44, minHeight: 44)
                                .contentShape(Rectangle())
                        }
                        .accessibilityLabel("Back")
                        .accessibilityIdentifier("workspace-back")
                    }
                    if !layout.location.isEmpty || layout.picksRoot { paneTitle(layout) }
                }
            } controls: {
                if layout.actionsInMenu {
                    paneActionsMenu(layout)
                } else {
                    HStack(spacing: 12) { workspaceActions }.fixedSize()
                }
                WorkspaceDoneControl(close: context.close).fixedSize()
            }
        }
    }

    @ViewBuilder private func paneTitle(_ layout: WorkspacePaneHeader) -> some View {
        let label = VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 3) {
                Text(layout.location)
                    .lineLimit(layout.actionsInMenu ? 2 : 1)
                    .truncationMode(.middle)
                if layout.picksRoot {
                    Image(systemName: "chevron.up.chevron.down").imageScale(.small)
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .frame(minHeight: 44, alignment: .leading)
        .contentShape(Rectangle())
        if layout.picksRoot, let response {
            Menu {
                ForEach(response.roots) { root in
                    WorkspaceRootMenuButton(root: root) { selectRoot($0) }
                }
            } label: { label }
            .buttonStyle(.plain)
            .accessibilityLabel("Files, \(layout.location)")
            .accessibilityHint("Chooses another folder")
            .accessibilityIdentifier("workspace-root-picker")
        } else {
            label
                .accessibilityElement(children: .combine)
                .accessibilityAddTraits(.isHeader)
                .accessibilityIdentifier("workspace-pane-title")
        }
    }

    /// At accessibility text sizes the actions sit in ⋯ instead of squeezing
    /// the location and each other down to a few letters.
    private func paneActionsMenu(_ layout: WorkspacePaneHeader) -> some View {
        Menu {
            if layout.showsHiddenToggle {
                Button(showHidden ? "Hide hidden files" : "Show hidden files",
                       systemImage: showHidden ? "eye.slash" : "eye", action: toggleHiddenFiles)
                    .accessibilityIdentifier("workspace-hidden-toggle")
            }
            Button(viewMode == "all" ? "Modified" : "All files",
                   systemImage: viewMode == "all" ? "arrow.triangle.branch" : "folder", action: toggleViewMode)
                .accessibilityIdentifier("workspace-view-toggle")
        } label: {
            Image(systemName: "ellipsis.circle")
                .frame(minWidth: 44, minHeight: 44)
                .contentShape(Rectangle())
        }
        .accessibilityLabel("Files options")
        .accessibilityIdentifier("workspace-header-menu")
    }

    @ViewBuilder private var addFolderButton: some View {
        if folderAdder.adding { ProgressView().accessibilityLabel("Adding folder") }
        else {
            Button("Add folder", systemImage: "folder.badge.plus") { addFolder() }
                .buttonStyle(.bordered)
                .accessibilityIdentifier("workspace-add-folder")
        }
    }

    private var fileBrowser: some View {
        VStack(spacing: 0) {
            // In the pane's own header row these join the title (Duo).
            if paneHeader == nil { workspaceControls }
            if outsideProject, failure == nil {
                HStack(alignment: .firstTextBaseline, spacing: 12) {
                    Text(addFolderError ?? "This chat’s worktree isn’t one of the Project’s folders. Showing the Project’s folders, read-only.")
                        .font(.footnote)
                        .foregroundStyle(addFolderError == nil ? .secondary : theme.text)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("workspace-read-only-notice")
                    Spacer(minLength: 0)
                    addFolderButton
                }
                .padding(.horizontal)
                .padding(.vertical, 6)
                .accessibilityElement(children: .contain)
            }
            if let failure {
                VStack(spacing: 12) {
                    let title = outsideProject ? "Folder not in this Project" : "Files unavailable"
                    ContentUnavailableView(title, systemImage: outsideProject ? "folder.badge.questionmark" : "externaldrive.badge.xmark",
                                           description: Text(failure))
                        .accessibilityElement(children: .ignore)
                        .accessibilityLabel("\(title). \(failure)")
                        .accessibilityIdentifier("workspace-unavailable")
                    if outsideProject {
                        addFolderButton
                    } else {
                        Button("Try again") { Task { await retryWorkspace() } }
                            .buttonStyle(.bordered)
                            .accessibilityIdentifier("workspace-retry")
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if viewMode == "modified" {
                modifiedView
            } else {
                allFilesView
            }
        }
        .overlay { fileLoadOverlay }
        .alert(unsupportedMediaName.map { "Can’t play \($0)" } ?? "",
               isPresented: Binding(get: { unsupportedMediaName != nil },
                                    set: { if !$0 { unsupportedMediaName = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(PlayableMedia.unsupportedMessage)
        }
    }

    var body: some View {
        Group {
            if hasPreview {
                inlinePreview
            } else {
                fileBrowser
            }
        }
            .safeAreaInset(edge: .top, spacing: 0) {
                if let paneHeader { paneHeaderRow(paneHeader) }
            }
            .foregroundStyle(theme.text)
            .toolbar(hasPreview || paneHeader != nil ? .hidden : .automatic, for: .navigationBar)
            #if WONDER_DIAGNOSTICS
            .task {
                guard ProcessInfo.processInfo.arguments.contains("-diagnostics-ux3-markdown") ||
                      ProcessInfo.processInfo.arguments.contains("-diagnostics-ux3-source") else { return }
                var consumed: Set<String> = []
                while !Task.isCancelled {
                    for name in ["ux3-expand", "ux3-collapse", "ux3-back"] where !consumed.contains(name) && DiagnosticSubagentFixture.signalled(name) {
                        consumed.insert(name)
                        switch name {
                        case "ux3-expand": pdfSession.capture(); htmlSession.capture(); expandedPreview = true
                        case "ux3-collapse": collapseExpandedPreview()
                        default: closePreview()
                        }
                    }
                    try? await Task.sleep(for: .milliseconds(100))
                }
            }
            #endif
            .task(id: model.assignmentScope) {
                let scope = model.assignmentScope
                // Switching between the list and an inline preview can remount
                // this task. Only a pairing change should reset the selection.
                guard loadedRootsScope != scope || response == nil else { return }
                let startingPoint = observationPoint
                invalidateLocation()
                response = nil; selectedRootID = nil; directoryPath = ""; directory = nil; git = nil
                projectFolders = nil; worktreeRefusal = nil; addFolderError = nil
                folderAdder.cancel()
                if await loadRoots() {
                    loadedRootsScope = scope
                    if viewMode == "modified" { await loadGit() }
                    listedPoint = startingPoint
                    scheduleListingRefresh()
                }
            }
            .task(id: directoryTaskID) {
                guard viewMode == "all", selectedRoot?.isDirectory == true else { return }
                guard await loadDirectory(reset: true) else { return }
                await openInitialFileIfNeeded()
            }
            .onChange(of: viewMode) { _, next in
                // The first roots request belongs to Files, not either tab.
                // Keep it alive when Modified is selected before roots arrive.
                invalidateLocation(preserveRoots: rootsLoadingID != nil)
                if response != nil && !missingDirectory { failure = nil }
                if next == "modified" { git = nil; Task { await loadGit() } }
                else if selectedRoot?.isDirectory == true { Task { await loadDirectory(reset: true) } }
                scheduleListingRefresh()
            }
            .onChange(of: directoryTaskID) { _, _ in scheduleListingRefresh() }
            .onChange(of: observationPoint) { _, _ in scheduleListingRefresh() }
            .onChange(of: hasPreview) { _, preview in
                if !preview { scheduleListingRefresh() }
            }
            .onChange(of: scenePhase) { _, phase in
                if phase == .active { scheduleListingRefresh(force: true) }
                else {
                    listingRefreshTask?.cancel()
                    listingRefreshTask = nil
                    listingRefreshID = nil
                    folderAdder.cancel()
                }
            }
            .onAppear { scheduleListingRefresh() }
            .task(id: shouldPollListing) {
                guard shouldPollListing else { return }
                while !Task.isCancelled {
                    // Re-reading every loaded page every 12 seconds can flood
                    // large folders. Keep the first pages responsive and slow
                    // the poll as the visible listing grows.
                    let pageCount = max(1, ((directory?.entries.count ?? 0) + 199) / 200)
                    let interval = max(12, pageCount * 6)
                    do { try await Task.sleep(for: .seconds(interval)) } catch { return }
                    scheduleListingRefresh(force: true)
                }
            }
            .onDisappear {
                listingRefreshTask?.cancel()
                listingRefreshTask = nil
                listingRefreshID = nil
                // A full-screen cover temporarily hides the browser; closing
                // Files or leaving the conversation must release its work.
                if !expandedPreview {
                    folderAdder.cancel()
                    mediaSelection?.close()
                    previewRevision?.close()
                }
            }
            .fullScreenCover(isPresented: $expandedPreview, onDismiss: {
                // A nested region editor also hides this cover temporarily.
                // Release its source only when the outer cover actually closes.
                if !collapsingPreview { closePreview() }
                collapsingPreview = false
            }) {
                VStack(spacing: 0) {
                    previewHeader(expanded: true)
                    previewContent
                }
                .foregroundStyle(theme.text)
                .wonderPage()
            }
            .confirmationDialog("Choose diff", isPresented: Binding(get: { diffChoice != nil }, set: { if !$0 { diffChoice = nil } })) {
                if let change = diffChoice {
                    Button("Staged changes") { loadDiff(change, staged: true) }
                    Button("Unstaged changes") { loadDiff(change, staged: false) }
                }
            } message: {
                Text("This file has both staged and unstaged changes.")
            }
    }

    private var inlinePreview: some View {
        VStack(spacing: 0) {
            previewHeader(expanded: false)
            // A preview has one owner while expanded, including its scroll position.
            if !expandedPreview { previewContent }
        }
    }

    private func previewHeader(expanded: Bool) -> some View {
        ZStack {
            HStack(spacing: 6) {
                Text(previewName)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
                    .truncationMode(.middle)
                    .accessibilityIdentifier(expanded ? "workspace-preview-expanded-title" : "workspace-preview-title")
                if annotationAdded {
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(.secondary)
                        .accessibilityLabel("Added to message")
                        .accessibilityIdentifier("workspace-annotation-added")
                }
            }
            .padding(.horizontal, 52)
            .frame(maxWidth: .infinity)
            HStack(spacing: 0) {
                if !expanded {
                    Button { closePreview() } label: {
                        Image(systemName: "chevron.left").frame(minWidth: 44, minHeight: 44)
                    }
                    .accessibilityLabel("Back to Files")
                    .accessibilityIdentifier("workspace-preview-back")
                }
                Spacer(minLength: 0)
                if expanded {
                    Button { collapseExpandedPreview() } label: {
                        Image(systemName: "arrow.down.right.and.arrow.up.left")
                            .frame(minWidth: 44, minHeight: 44)
                    }
                    .accessibilityLabel("Show in chat")
                    .accessibilityIdentifier("workspace-preview-collapse")
                } else {
                    previewExpandButton
                }
            }
        }
        .frame(minHeight: 44)
        .padding(.horizontal, 12)
        .modifier(PreviewWindowControlsLayout())
        .background(theme.page)
    }

    private func collapseExpandedPreview() {
        pdfSession.capture()
        htmlSession.capture()
        collapsingPreview = true
        expandedPreview = false
    }

    private var previewExpandButton: some View {
        Button("Full screen", systemImage: "arrow.up.left.and.arrow.down.right") {
            pdfSession.capture()
            htmlSession.capture()
            expandedPreview = true
        }
        .labelStyle(.iconOnly)
        .frame(minWidth: 44, minHeight: 44)
        .accessibilityIdentifier("workspace-preview-expand")
        #if WONDER_DIAGNOSTICS
        .accessibilityValue(DiagnosticSubagentFixture.projectFilesSendFixture &&
            model.composers[chat.id]?.pending?.receipt != nil ? "Host received test message" : "")
        #endif
    }

    @ViewBuilder private var previewContent: some View {
        if let item = selection {
            if let previewRevision {
                let scope = model.assignmentScope
                WorkspaceImagePreview(model: model, chat: chat, item: item,
                                      sketchUnavailableReason: sketchUnavailableReason,
                                      revision: previewRevision,
                                      refresh: revisionRefresh(for: item),
                                      onRevision: revisionNotice(for: item), onClose: closePreview) { png in
                    if let draftAnnotationActions { return await draftAnnotationActions.stageSketch(png) }
                    return await model.stageSketch(png, chat: chat, scope: scope)
                }
            } else {
                PhotoViewer(model: model, chat: chat, name: item.name, mimeType: item.mimeType,
                            sourceID: item.id, localData: item.data, file: nil,
                            onClose: closePreview, showsHeader: false)
            }
        } else if let file = attachmentPhoto {
            PhotoViewer(model: model, chat: chat, file: file,
                        galleryFiles: ConversationAttachmentGallery.imageFiles(visibleAttachments),
                        initialFileID: file.id, onClose: closePreview, showsHeader: false)
        } else if let item = documentSelection {
            if let previewRevision {
                let context = annotationContext(for: item)
                let replacement = reanchorDraft(for: item)
                let annotationReason = context == nil ? nil : annotationUnavailableReason(for: item)
                WorkspaceDocumentPreview(name: item.name, mimeType: item.mimeType,
                                         revision: previewRevision, draft: textAnnotationDraft,
                                         pdfSession: pdfSession,
                                         htmlSession: htmlSession,
                                         annotationContext: context,
                                         annotationUnavailableReason: annotationReason,
                                         onAnnotation: { annotation in
                    guard let context else { throw FileFailure.integrity }
                    try stagePreviewAnnotation(annotation, context: context, replacing: replacement?.id)
                }, refresh: revisionRefresh(for: item), onRevision: revisionNotice(for: item),
                                         onClose: closePreview, markdownReferences: markdownReferences(for: item))
            }
        } else if let item = mediaSelection {
            WorkspaceMediaPreview(selection: item, onClose: closePreview)
        } else if let file = attachmentSelection, attachmentData != nil, attachmentDigest != nil {
            if let previewRevision {
                WorkspaceDocumentPreview(name: file.name, mimeType: file.mimeType ?? "application/octet-stream",
                                         revision: previewRevision, draft: textAnnotationDraft,
                                         pdfSession: pdfSession,
                                         htmlSession: htmlSession,
                                         onClose: closePreview)
            }
        } else if let change = selectedDiff, let diffText {
            WorkspaceDiffPreview(path: change.path, state: selectedDiffStaged ? "Staged changes" : "Unstaged changes", diff: diffText,
                                 onClose: closePreview)
        }
    }

    private func closePreview() {
        mediaSelection?.close()
        previewRevision?.close()
        pdfSession = PDFPreviewSession()
        htmlSession = WorkspaceHTMLPreviewSession()
        expandedPreview = false
        fileRequestID = UUID(); attachmentRequestID = UUID()
        textAnnotationDraft.reset()
        selection = nil; documentSelection = nil; previewRevision = nil; mediaSelection = nil
        attachmentPhoto = nil; attachmentSelection = nil; attachmentData = nil; attachmentDigest = nil
        diffRequestID = UUID(); selectedDiff = nil; diffText = nil
        addedAnnotationID = nil
        markdownImages.removeAllObjects()
    }

    private func stagePreviewAnnotation(_ annotation: ArtifactAnnotation, context: ArtifactPreviewContext,
                                        replacing oldID: String?) throws {
        if let draftAnnotationActions {
            guard draftAnnotationActions.unavailableReason == nil,
                  context.projectID == draftAnnotationActions.projectID,
                  context.conversationID == "new-chat:" + draftAnnotationActions.draftID,
                  context.scope == model.assignmentScope else { throw FileFailure.integrity }
            addedAnnotationID = try draftAnnotationActions.stage(annotation)
            if let offered = previewRevision?.offeredSha256 {
                draftAnnotationActions.noteRevision(context.rootID, context.path, offered)
            }
            return
        }
        try model.stageAnnotation(annotation, chat: chat, expectedScope: context.scope, replacing: oldID)
        addedAnnotationID = model.composers[chat.id]?.stagedFiles?.last?.id
        if let offered = previewRevision?.offeredSha256 {
            Task {
                guard context.scope == model.assignmentScope else { return }
                await model.noteWorkspaceRevision(chat, rootID: context.rootID, path: context.path,
                                                  currentSha256: offered)
            }
        }
    }

    @ViewBuilder private var fileLoadOverlay: some View {
        if let fileLoad {
            WorkspaceFileLoadOverlay(load: fileLoad) { cancelFileLoad() }
                .id(fileLoad.id)
        }
    }
    @ViewBuilder private var allFilesView: some View {
        if loading && directory == nil && visibleAttachments.isEmpty { ProgressView("Loading files…").frame(maxWidth: .infinity, maxHeight: .infinity) }
        else {
            ScrollViewReader { proxy in
            WonderList(overBackdrop: true, plainRows: plainRows) {
                if let root = selectedRoot {
                    if !directoryPath.isEmpty, paneHeader == nil {
                        Button { navigate(to: directory?.parentPath ?? "") } label: { Label("Back", systemImage: "chevron.left") }
                            .accessibilityIdentifier("workspace-back")
                    }
                    Section {
                        if root.isDirectory {
                            ForEach(directory?.entries ?? []) { entry in
                                Button { open(entry) } label: {
                                    FilesRowLabel(name: entry.name,
                                                  symbol: entry.isDirectory ? "folder" : (entry.mimeType?.hasPrefix("image/") == true ? "photo" : (Self.isMedia(entry) ? "play.rectangle" : "doc.text")))
                                }
                                .listRowInsets(FilesRowLabel.insets)
                                .id("workspace-row:" + entry.path)
                                .accessibilityIdentifier("workspace-\(entry.isDirectory ? "directory" : "file")-entry:\(entry.path)")
                            }
                            if let next = directory?.nextOffset {
                                Button("Load more", systemImage: "ellipsis") {
                                    // User pagination takes priority over a
                                    // refresh already waiting on the host.
                                    listingRefreshTask?.cancel()
                                    listingRefreshTask = nil
                                    listingRefreshID = nil
                                    Task { await loadDirectory(reset: false, offset: next) }
                                }
                                .accessibilityIdentifier("workspace-next-page")
                            }
                        } else {
                            Button { openRootFile(root) } label: { FilesRowLabel(name: URL(fileURLWithPath: root.path).lastPathComponent, symbol: "doc.text") }
                                .listRowInsets(FilesRowLabel.insets)
                                .accessibilityIdentifier("workspace-file-entry:\(root.id)")
                        }
                    }
                }
                if !visibleAttachments.isEmpty {
                    Section("Attachments") {
                        ForEach(visibleAttachments) { file in
                            Button { openAttachment(file) } label: { FilesRowLabel(name: file.name, symbol: PhotoViewerRouting.isImage(file) ? "photo" : "paperclip") }
                                .listRowInsets(FilesRowLabel.insets)
                                .accessibilityIdentifier("workspace-attachment:\(file.id)")
                        }
                    }
                }
                if let failure { Section { FailureDetails(message: failure) } }
            }
            .accessibilityIdentifier("workspace-file-list")
            .onAppear {
                if let path = returnEntryPath { proxy.scrollTo("workspace-row:" + path, anchor: .center) }
            }
            }
        }
    }

    @ViewBuilder private var modifiedView: some View {
        if loading && git == nil {
            ProgressView("Checking changes…").frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let git, !git.available {
            ContentUnavailableView("Modified unavailable", systemImage: "arrow.triangle.branch", description: Text(git.detail ?? "Git status is unavailable on this host."))
                .accessibilityIdentifier("workspace-git-unavailable")
        } else if let git, git.changes.isEmpty {
            ContentUnavailableView("No modified files", systemImage: "checkmark.circle")
        } else if let git {
            WonderList(overBackdrop: true, plainRows: plainRows) {
                if selectedDiff != nil && diffText == nil {
                    ProgressView("Loading diff…")
                        .accessibilityIdentifier("workspace-diff-loading")
                }
                ForEach(git.changes) { change in
                    Button { openDiff(change) } label: {
                        HStack(spacing: FilesRowLabel.spacing) {
                            Image(systemName: change.state == "renamed" ? "arrow.triangle.2.circlepath" : change.state == "conflicted" ? "exclamationmark.triangle" : "doc.text")
                                .frame(width: FilesRowLabel.iconWidth, alignment: .leading)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(change.path).fixedSize(horizontal: false, vertical: true)
                                Text(change.originalPath.map { "Renamed from \($0)" } ?? (change.indexStatus != " " && change.indexStatus != "?" && change.worktreeStatus != " " ? "Staged and unstaged" : change.state.capitalized))
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            Spacer()
                            Image(systemName: "chevron.right").font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .listRowInsets(FilesRowLabel.insets)
                    .accessibilityIdentifier("workspace-modified-entry:\(change.path)")
                }
            }
            .accessibilityIdentifier("workspace-modified-list")
        } else {
            ProgressView("Checking changes…").frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private struct Location: Equatable {
        let scope: String
        let generation: UUID
        let rootID: String?
        let path: String
        let mode: String
        let hidden: Bool
    }
    private var location: Location {
        Location(scope: model.assignmentScope, generation: locationGeneration,
                 rootID: selectedRootID, path: directoryPath, mode: viewMode, hidden: showHidden)
    }
    private func isCurrent(_ captured: Location) -> Bool {
        !Task.isCancelled && !model.accessEnded && captured == location
    }
    private func isUnavailableWorkspace(_ error: Error) -> Bool {
        switch error {
        case PairingFailure.worktreeNotInProject, PairingFailure.hostMessage(403, _): return true
        case PairingFailure.response(let code): return code == 401 || code == 403 || code == 404
        default: return false
        }
    }
    private func clearUnavailableWorkspace(_ error: Error) {
        listingRefreshTask?.cancel()
        listingRefreshTask = nil
        listingRefreshID = nil
        invalidateLocation()
        response = nil
        selectedRootID = nil
        directoryPath = ""
        directory = nil
        git = nil
        let unavailable = WorkspaceUnavailable(error, worktreeRefusal: worktreeRefusal)
        failure = unavailable.message
        worktreeRefusal = unavailable.worktreeRefusal
        projectFolders = nil
        addFolderError = nil
    }
    private func scheduleListingRefresh(force: Bool = false) {
        guard scenePhase == .active, response != nil, !hasPreview,
              !loading, (force || (observationPoint != nil && observationPoint != listedPoint)),
              listingRefreshTask == nil else { return }
        let requestID = UUID()
        listingRefreshID = requestID
        let minimumInterval = min(30.0, 2.0 * pow(2.0, Double(listingRetryCount)))
        let elapsed = lastListingAttemptAt.map { ProcessInfo.processInfo.systemUptime - $0 } ?? minimumInterval
        let delay = max(0, minimumInterval - elapsed)
        listingRefreshTask = Task {
            if delay > 0 { try? await Task.sleep(for: .seconds(delay)) }
            guard listingRefreshID == requestID else { return }
            if Task.isCancelled || hasPreview || loading {
                listingRefreshTask = nil
                listingRefreshID = nil
                return
            }
            if !force && observationPoint == listedPoint {
                listingRefreshTask = nil
                listingRefreshID = nil
                return
            }
            lastListingAttemptAt = ProcessInfo.processInfo.systemUptime
            let loaded = await refreshListing(point: observationPoint)
            guard listingRefreshID == requestID else { return }
            listingRefreshTask = nil
            listingRefreshID = nil
            guard !Task.isCancelled, !hasPreview else { return }
            listingRetryCount = loaded ? 0 : min(listingRetryCount + 1, 4)
            if failure == nil { scheduleListingRefresh() }
        }
    }
    private func refreshListing(point: WorkspaceObservationPoint?) async -> Bool {
        guard !hasPreview, response != nil else { return false }
        let requestedPoint = point
        let captured = location
        let requestID = UUID()
        rootsRequestID = requestID
        let loadingID = beginLoading()
        defer { loadingRequests.remove(loadingID) }
        do {
            let next = try await model.loadWorkspaceRoots(source)
            guard isCurrent(captured), requestID == rootsRequestID, !hasPreview else { return false }
            let oldRoot = selectedRoot
            let nextRoot = next.roots.first(where: { $0.id == selectedRootID })
            let rootChanged = nextRoot == nil || oldRoot?.path != nextRoot?.path ||
                oldRoot?.kind != nextRoot?.kind || oldRoot?.isDirectory != nextRoot?.isDirectory
            if rootChanged {
                invalidateLocation()
                selectedRootID = nextRoot?.id ?? next.roots.first?.id
                directoryPath = ""
                directory = nil
                git = nil
            }
            response = next
            guard selectedRoot != nil else {
                failure = next.detail ?? "No verified workspace locations are available."
                return false
            }
            // Changing roots remounts the directory task. Let that one load
            // the new path, avoiding two requests that can cancel each other.
            if rootChanged && viewMode == "all" {
                listedPoint = requestedPoint
                return true
            }
            let loaded: Bool
            if viewMode == "modified" {
                loaded = await loadGit(backgroundRefresh: true)
            } else if selectedRoot?.isDirectory == true {
                loaded = await loadDirectory(reset: true, preserveUntilLoaded: true,
                                             backgroundRefresh: true)
            } else { loaded = true }
            guard loaded, !Task.isCancelled else { return false }
            listedPoint = requestedPoint
            return true
        } catch {
            guard isCurrent(captured), requestID == rootsRequestID, !hasPreview else { return false }
            if isUnavailableWorkspace(error) {
                clearUnavailableWorkspace(error)
                return false
            }
            return false
        }
    }
    private func beginLoading() -> UUID {
        let id = UUID()
        loadingRequests.insert(id)
        return id
    }
    private func invalidateLocation(preserveRoots: Bool = false) {
        locationGeneration = UUID()
        mediaSelection?.close()
        previewRevision?.close()
        pdfSession = PDFPreviewSession()
        expandedPreview = false
        if preserveRoots, let rootsLoadingID { loadingRequests = [rootsLoadingID] }
        else { rootsRequestID = UUID(); rootsLoadingID = nil; loadingRequests.removeAll() }
        directoryRequestID = UUID()
        gitRequestID = UUID()
        diffRequestID = UUID()
        fileRequestID = UUID()
        attachmentRequestID = UUID()
        textAnnotationDraft.reset()
        addedAnnotationID = nil
        selectedDiff = nil; diffChoice = nil; diffText = nil
        selection = nil; documentSelection = nil; previewRevision = nil; mediaSelection = nil
        attachmentPhoto = nil; attachmentSelection = nil; attachmentData = nil; attachmentDigest = nil
    }
    private func navigate(to path: String) {
        guard directoryPath != path else { return }
        invalidateLocation()
        directoryPath = path
        directory = nil
        missingDirectory = false
        failure = nil
    }
    private func selectRoot(_ root: WorkspaceRoot) {
        // A single-root menu also discloses the full path. Selecting the
        // already-open root must not clear its list without changing task ID.
        guard selectedRootID != root.id || !directoryPath.isEmpty || viewMode != "all" else { return }
        invalidateLocation()
        selectedRootID = root.id
        directoryPath = ""
        directory = nil
        missingDirectory = false
        git = nil
        failure = nil
        viewMode = "all"
    }
    @discardableResult private func loadRoots() async -> Bool {
        let scope = model.assignmentScope
        let requestID = UUID()
        rootsRequestID = requestID
        let loadingID = beginLoading()
        rootsLoadingID = loadingID
        failure = nil
        defer {
            loadingRequests.remove(loadingID)
            if rootsLoadingID == loadingID { rootsLoadingID = nil }
        }
        do {
            let next = try await model.loadWorkspaceRoots(source)
            guard !Task.isCancelled, !model.accessEnded, scope == model.assignmentScope,
                  requestID == rootsRequestID else { return false }
            let previousRootID = selectedRootID
            let previousPath = directoryPath
            invalidateLocation()
            directoryPath = ""
            directory = nil
            git = nil
            response = next
            if !openedInitialFile, projectFolders == nil, let path = FileChangeSummary.relativePath(initialFilePath) {
                let root: WorkspaceRoot?
                if let preferredRootID {
                    root = next.roots.first(where: { $0.id == preferredRootID && $0.isDirectory })
                } else {
                    root = next.roots.first(where: { ["workingDirectory", "childWorkingDirectory"].contains($0.kind) && $0.isDirectory })
                }
                guard let root else {
                    failure = "This chat’s working folder is unavailable. Check its project or workspace settings on your computer."
                    return true
                }
                selectedRootID = root.id
                directoryPath = path.split(separator: "/").dropLast().joined(separator: "/")
            } else if let previousRootID, next.roots.contains(where: { $0.id == previousRootID }) {
                selectedRootID = previousRootID
                directoryPath = previousPath
            } else { selectedRootID = next.roots.first?.id }
            if next.roots.isEmpty { failure = next.detail ?? "No verified workspace locations are available." }
            return true
        } catch {
            guard !Task.isCancelled, !model.accessEnded, scope == model.assignmentScope,
                  requestID == rootsRequestID else { return false }
            if case PairingFailure.worktreeNotInProject(let reason) = error, projectFolders == nil,
               let folders = await projectFoldersScope() {
                guard !Task.isCancelled, scope == model.assignmentScope, requestID == rootsRequestID else { return false }
                projectFolders = folders
                worktreeRefusal = reason
                return await loadRoots()
            }
            if isUnavailableWorkspace(error) { clearUnavailableWorkspace(error) }
            else { failure = workspaceBrowserError(error) }
            return false
        }
    }
    /// The Project's own folders for a thread whose worktree it leaves out.
    private func projectFoldersScope() async -> ChatSummary? {
        guard draftAnnotationActions == nil, !chat.id.hasPrefix("project-files:") else { return nil }
        let detail: ProjectConversationDetail
        if let known = model.projects.details[chat.id] { detail = known }
        else if let loaded = try? await model.projects.loadDetail(chat.id) { detail = loaded }
        else { return nil }
        if model.projects.project(detail.projectId) == nil { await model.projects.refresh() }
        guard let project = model.projects.project(detail.projectId),
              let folder = project.primaryFolder ?? project.folders.first else { return nil }
        return ChatSummary(conversationId: "project-files:\(project.id):\(folder.id)",
            title: project.name, lastMessagePreview: nil, lastMessageAt: nil, messageCount: 0,
            deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false)
    }
    private func addFolder() {
        addFolderError = nil
        let conversationID = chat.id
        folderAdder.start(add: { [model] in try await model.addWorkingFolderToProject(conversationID) },
                          added: { await retryWorkspace() },
                          failed: { error in
            // Reading the Project's folders keeps working; say why beside them.
            if projectFolders != nil && failure == nil { addFolderError = ConnectionModel.addFolderFailure(error) }
            else { failure = ConnectionModel.addFolderFailure(error) }
        })
    }
    private func retryWorkspace() async {
        failure = nil
        worktreeRefusal = nil
        projectFolders = nil
        addFolderError = nil
        if missingDirectory {
            navigate(to: "")
        }
        guard await loadRoots(), failure == nil else { return }
        if viewMode == "modified" { await loadGit() }
        else if selectedRoot?.isDirectory == true {
            guard await loadDirectory(reset: true) else { return }
            await openInitialFileIfNeeded()
        }
    }
    private func openInitialFileIfNeeded() async {
        var initialPath = initialFilePath
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux3-markdown") { initialPath = "guide.md" }
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux3-source") { initialPath = "trip.rs" }
        #endif
        guard !Task.isCancelled, !openedInitialFile, failure == nil, projectFolders == nil,
              let path = FileChangeSummary.relativePath(initialPath) else { return }
        let name = FileChangeSummary.filename(path)
        let entry = directory?.entries.first(where: { $0.path == path && !$0.isDirectory })
            ?? WorkspaceEntry(name: name, path: path, isDirectory: false, byteSize: nil,
                              mimeType: UTType(filenameExtension: URL(fileURLWithPath: name).pathExtension)?.preferredMIMEType)
        #if WONDER_DIAGNOSTICS
        // Seed only when the fixture first opens its file. Later mode changes
        // use the production preference across inline/expanded presentations.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux3-markdown") {
            TextViewerPreference.remember(.rendered, for: .markdown)
        }
        #endif
        let captured = location
        if await loadWorkspaceFile(entry), isCurrent(captured) {
            openedInitialFile = true
            // A file the chat names opens as a link does: nearly full screen,
            // not in the sheet or pane it was asked from (Show in chat returns it).
            pdfSession.capture(); htmlSession.capture()
            expandedPreview = true
        }
    }
    private func toggleHiddenFiles() {
        invalidateLocation()
        showHidden.toggle()
        Task { await loadDirectory(reset: true, preserveUntilLoaded: true) }
    }
    @discardableResult private func loadDirectory(reset: Bool, offset: Int = 0,
                                                   preserveUntilLoaded: Bool = false,
                                                   backgroundRefresh: Bool = false) async -> Bool {
        guard let root = selectedRoot, root.isDirectory else { return false }
        let captured = location
        let requestID = UUID()
        directoryRequestID = requestID
        let loadedCount = backgroundRefresh ? (directory?.entries.count ?? 0) : 0
        if reset && !preserveUntilLoaded { directory = nil }
        let loadingID = beginLoading()
        defer { loadingRequests.remove(loadingID) }
        do {
            var page = try await model.loadWorkspaceDirectory(source, root: root, path: captured.path,
                                                              showHidden: captured.hidden, offset: offset)
            var seenOffsets: Set<Int> = []
            while backgroundRefresh, page.entries.count < loadedCount,
                  let next = page.nextOffset, seenOffsets.insert(next).inserted {
                guard isCurrent(captured), requestID == directoryRequestID else { return false }
                let more = try await model.loadWorkspaceDirectory(source, root: root, path: captured.path,
                                                                  showHidden: captured.hidden, offset: next)
                guard more.rootId == page.rootId, more.path == page.path else { throw FileFailure.integrity }
                page = WorkspaceDirectoryPage(rootId: page.rootId, path: page.path, parentPath: page.parentPath,
                                              entries: page.entries + more.entries, nextOffset: more.nextOffset)
            }
            guard isCurrent(captured), requestID == directoryRequestID else { return false }
            failure = nil
            missingDirectory = false
            if reset || directory == nil { directory = page }
            else if let current = directory { directory = WorkspaceDirectoryPage(rootId: page.rootId, path: page.path, parentPath: page.parentPath, entries: current.entries + page.entries, nextOffset: page.nextOffset) }
            return true
        } catch {
            guard isCurrent(captured), requestID == directoryRequestID else { return false }
            if case PairingFailure.response(404) = error, !captured.path.isEmpty {
                missingDirectory = true
                directory = nil
                failure = "This folder moved or was deleted. Try again to return to the workspace root."
            } else if isUnavailableWorkspace(error) {
                clearUnavailableWorkspace(error)
            } else if !backgroundRefresh { failure = workspaceBrowserError(error) }
            return false
        }
    }
    @discardableResult private func loadGit(backgroundRefresh: Bool = false) async -> Bool {
        guard let root = selectedRoot else { return false }
        let captured = location
        let requestID = UUID()
        gitRequestID = requestID
        let loadingID = beginLoading()
        defer { loadingRequests.remove(loadingID) }
        do {
            let next = try await model.loadWorkspaceGitStatus(source, root: root)
            guard isCurrent(captured), requestID == gitRequestID else { return false }
            git = next; failure = nil
            return true
        } catch {
            guard isCurrent(captured), requestID == gitRequestID else { return false }
            if isUnavailableWorkspace(error) { clearUnavailableWorkspace(error) }
            else if !backgroundRefresh { failure = workspaceBrowserError(error) }
            return false
        }
    }
    private func open(_ entry: WorkspaceEntry) {
        if entry.isDirectory { returnEntryPath = nil; navigate(to: entry.path) }
        else { returnEntryPath = entry.path; startFileLoad(entry) }
    }
    /// A Markdown file in a project root shows its local images and follows its
    /// relative links inside the same root.
    private func markdownReferences(for item: WorkspacePreviewSelection) -> MarkdownReferenceContext? {
        guard TextViewerKind.detect(name: item.name, mimeType: item.mimeType) == .markdown,
              let rootID = item.rootID, let path = item.path, !path.isEmpty,
              let root = selectedRoot, root.id == rootID, item.scope == model.assignmentScope else { return nil }
        let images = markdownImages
        return MarkdownReferenceContext(documentPath: path, loadImage: { target in
            let key = "\(model.assignmentScope):\(source.id):\(root.id):\(target)"
            if let hit = images.object(forKey: key as NSString) { return hit }
            guard let entry = try await workspaceEntry(target, in: root), !entry.isDirectory else { throw MarkdownImageFailure.missing }
            guard let mime = entry.mimeType?.lowercased(), PhotoViewerRouting.isImage(mimeType: mime) else { throw MarkdownImageFailure.notImage }
            guard let size = entry.byteSize, size <= 8 * 1024 * 1024 else { throw MarkdownImageFailure.tooLarge }
            let data = try await model.downloadWorkspaceFile(source, root: root, entry: entry, countsAsOpen: false)
            guard data.count <= 8 * 1024 * 1024 else { throw MarkdownImageFailure.tooLarge }
            do { try ConversationFile.validateContent(data, mime: mime) } catch { throw MarkdownImageFailure.unreadable }
            guard let image = await Task.detached(priority: .utility, operation: { ToolPreviewImage.decode(data) }).value else {
                throw MarkdownImageFailure.unreadable
            }
            images.setObject(image, forKey: key as NSString, cost: image.cgImage.map { $0.bytesPerRow * $0.height } ?? 0)
            return image
        }, open: { target in
            do {
                guard let entry = try await workspaceEntry(target, in: root), !entry.isDirectory else {
                    return "\(target) isn't in this project."
                }
                open(entry)
                return nil
            } catch {
                return "Can't reach your computer to open \(target)."
            }
        })
    }
    /// Finds a root-relative path in its folder's listing, which also supplies
    /// the size and type the download is checked against.
    private func workspaceEntry(_ path: String, in root: WorkspaceRoot) async throws -> WorkspaceEntry? {
        let (folder, name) = MarkdownReference.split(path)
        var offset = 0
        for _ in 0..<10 {
            let page: WorkspaceDirectoryPage
            do { page = try await model.loadWorkspaceDirectory(source, root: root, path: folder, showHidden: true, offset: offset) }
            catch PairingFailure.response(404) { return nil }
            if let entry = page.entries.first(where: { $0.name == name }) { return entry }
            guard let next = page.nextOffset, next > offset else { return nil }
            offset = next
        }
        return nil
    }
    private func openRootFile(_ root: WorkspaceRoot) {
        let name = URL(fileURLWithPath: root.path).lastPathComponent
        let mime: String
        if let advertised = root.mimeType {
            mime = advertised
        } else if let inferred = UTType(filenameExtension: URL(fileURLWithPath: name).pathExtension)?.preferredMIMEType {
            mime = inferred
        } else {
            mime = "application/octet-stream"
        }
        startFileLoad(WorkspaceEntry(name: name, path: "", isDirectory: false, byteSize: root.byteSize, mimeType: mime))
    }
    private func startFileLoad(_ entry: WorkspaceEntry) {
        fileLoadTask?.cancel()
        fileLoadTask = Task { await loadWorkspaceFile(entry) }
    }
    private func cancelFileLoad() {
        fileLoadTask?.cancel()
        fileLoadTask = nil
        fileRequestID = UUID() // A late result is discarded.
        fileLoad = nil
    }
    @discardableResult private func loadWorkspaceFile(_ entry: WorkspaceEntry) async -> Bool {
        guard let root = selectedRoot else { return false }
        let captured = location
        let requestID = UUID()
        fileRequestID = requestID
        let loadingID = beginLoading()
        failure = nil
        defer { loadingRequests.remove(loadingID) }
        // Nothing could show it, so don't download it to say so.
        if PlayableMedia.isUnsupported(fileName: entry.name, mimeType: entry.mimeType) {
            unsupportedMediaName = entry.name
            return true
        }
        do {
            if Self.isMedia(entry) {
                // Media streams in ranges the relay can't carry; say so instead of a stalled player.
                if model.usingRelay { throw RelayUnsupported.workspacePreview }
                let loader = try model.workspaceMediaLoader(source, root: root, entry: entry)
                guard isCurrent(captured), requestID == fileRequestID else { loader.cancelAll(); return false }
                mediaSelection = try WorkspaceMediaSelection(id: root.id + ":" + entry.path,
                                                              name: entry.name, loader: loader)
                return true
            }
            fileLoad = WorkspaceFileLoad(id: requestID, name: entry.name, received: 0, expected: entry.byteSize.map(Int.init))
            defer { if fileLoad?.id == requestID { fileLoad = nil } }
            // Text shows only its start, so only that much is read.
            let result = try await model.readWorkspaceFile(source, root: root, entry: entry,
                                                           prefix: WorkspaceRevisionState.previewPrefix(mimeType: entry.mimeType)) { received, expected in
                Task { @MainActor in
                    guard fileLoad?.id == requestID else { return }
                    fileLoad?.received = received
                    if let expected { fileLoad?.expected = expected }
                }
            }
            guard case .read(let read) = result else { throw URLError(.badServerResponse) }
            let data = read.data
            let sha256 = await Task.detached(priority: .userInitiated) { ConversationFile.digest(data) }.value
            guard isCurrent(captured), requestID == fileRequestID else { return false }
            let carriedNote: String
            if let reanchor, replacingAnnotationID != nil,
               reanchor.conversationId == chat.id,
               reanchor.projectId == model.projects.details[chat.id]?.projectId,
               reanchor.rootId == root.id, reanchor.path == entry.path {
                carriedNote = reanchor.note
            } else { carriedNote = "" }
            textAnnotationDraft.reset(note: carriedNote)
            addedAnnotationID = nil
            pdfSession = PDFPreviewSession()
            previewRevision = WorkspaceRevisionState(data: data, sha256: sha256, totalBytes: read.totalBytes,
                                                     validator: read.validator)
            if PhotoViewerRouting.isImage(mimeType: entry.mimeType) { selection = WorkspacePreviewSelection(id: root.id + ":" + entry.path, name: entry.name, mimeType: entry.mimeType ?? "image/*", data: data, sha256: sha256, rootID: root.id, path: entry.path, scope: captured.scope) }
            else { attachmentSelection = nil; attachmentData = nil; attachmentDigest = nil; selection = nil; selectedDiff = nil; diffText = nil; documentSelection = WorkspacePreviewSelection(id: root.id + ":" + entry.path, name: entry.name, mimeType: entry.mimeType ?? "application/octet-stream", data: data, sha256: sha256, rootID: root.id, path: entry.path, scope: captured.scope) }
            return true
        } catch {
            guard isCurrent(captured), requestID == fileRequestID, !Task.isCancelled,
                  !(error is CancellationError) else { return false }
            if case FileFailure.tooLarge = error {
                failure = "This file is too large to preview on this \(UIDevice.current.model). Files up to \(PairingAPI.workspacePreviewLimit / (1024 * 1024)) MB open here; open larger files on your computer."
            } else {
                failure = WorkspaceUnavailable.fileMessage(error)
            }
            return false
        }
    }
    private static func isMedia(_ entry: WorkspaceEntry) -> Bool {
        PlayableMedia.isPlayable(fileName: entry.name)
    }
    private func annotationContext(for item: WorkspacePreviewSelection) -> ArtifactPreviewContext? {
        // The Project's folders shown for a left-out worktree are read-only.
        guard projectFolders == nil, let rootID = item.rootID, let path = item.path, !path.isEmpty,
              let scope = item.scope, scope == model.assignmentScope else { return nil }
        if let draftAnnotationActions {
            return ArtifactPreviewContext(projectID: draftAnnotationActions.projectID,
                                          conversationID: "new-chat:" + draftAnnotationActions.draftID,
                                          rootID: rootID, path: path, scope: scope)
        }
        guard let projectID = model.projects.details[chat.id]?.projectId else { return nil }
        return ArtifactPreviewContext(projectID: projectID, conversationID: chat.id,
                                      rootID: rootID, path: path, scope: scope)
    }
    private var sketchUnavailableReason: String? {
        if let draftAnnotationActions { return draftAnnotationActions.sketchUnavailableReason }
        if projectFolders != nil { return "Add this chat’s folder to the Project to sketch on its files." }
        return model.sketchUnavailableReason(chat)
    }

    private func annotationUnavailableReason(for item: WorkspacePreviewSelection) -> String? {
        if let draftAnnotationActions { return draftAnnotationActions.unavailableReason }
        guard !model.canAnnotate(chat, replacing: reanchorDraft(for: item)?.id) else { return nil }
        if chat.isArchived || model.projects.details[chat.id]?.isArchived == true {
            return "Restore this chat to add a comment."
        }
        if model.accessEnded || model.macConnected != true {
            return "Reconnect to your computer to add a comment."
        }
        if model.preparingSends.contains(chat.id) || model.sending.contains(chat.id) ||
            model.composers[chat.id]?.pending != nil {
            return "Wait for the current message to complete before saving this comment."
        }
        if model.uploading.contains(chat.id) {
            return "Wait for the current attachment to finish uploading."
        }
        if (model.composers[chat.id]?.attachmentCount ?? 0) >= 4 {
            return "Remove an attachment to add a comment."
        }
        return "Comments are unavailable right now."
    }
    private func reanchorDraft(for item: WorkspacePreviewSelection) -> (id: String, note: String)? {
        guard let reanchor, let replacingAnnotationID,
              reanchor.conversationId == chat.id,
              reanchor.projectId == model.projects.details[chat.id]?.projectId,
              reanchor.rootId == item.rootID, reanchor.path == item.path else { return nil }
        return (replacingAnnotationID, reanchor.note)
    }
    private func revisionRefresh(for item: WorkspacePreviewSelection) -> WorkspaceRevisionRead? {
        guard let rootID = item.rootID, let path = item.path,
              let scope = item.scope, scope == model.assignmentScope,
              let root = response?.roots.first(where: { $0.id == rootID }) else { return nil }
        let entry = WorkspaceEntry(name: item.name, path: path, isDirectory: false,
                                   byteSize: nil, mimeType: item.mimeType)
        let prefix = WorkspaceRevisionState.previewPrefix(mimeType: item.mimeType)
        return { validator, whole in
            guard scope == model.assignmentScope, !model.accessEnded else { throw CancellationError() }
            let result = try await model.refreshWorkspaceFile(source, root: root, entry: entry,
                                                              prefix: whole ? nil : prefix, validator: validator)
            guard scope == model.assignmentScope, !model.accessEnded else { throw CancellationError() }
            return result
        }
    }
    private func revisionNotice(for item: WorkspacePreviewSelection) -> ((String) async -> Void)? {
        guard let rootID = item.rootID, let path = item.path, let scope = item.scope else { return nil }
        if let draftAnnotationActions {
            return { sha256 in
                guard scope == model.assignmentScope else { return }
                draftAnnotationActions.noteRevision(rootID, path, sha256)
            }
        }
        return { sha256 in
            guard scope == model.assignmentScope else { return }
            await model.noteWorkspaceRevision(chat, rootID: rootID, path: path, currentSha256: sha256)
        }
    }
    private func openAttachment(_ file: ConversationFile) {
        if PhotoViewerRouting.isImage(file) { attachmentPhoto = file }
        else { Task { await loadAttachment(file) } }
    }
    private func loadAttachment(_ file: ConversationFile) async {
        let captured = location
        let requestID = UUID()
        attachmentRequestID = requestID
        let loadingID = beginLoading()
        failure = nil
        defer { loadingRequests.remove(loadingID) }
        do {
            let data = try await model.download(file, chat: chat)
            let digest = await Task.detached(priority: .userInitiated) { ConversationFile.digest(data) }.value
            guard isCurrent(captured), requestID == attachmentRequestID,
                  visibleAttachments.contains(where: { $0.id == file.id }) else { return }
            textAnnotationDraft.reset()
            pdfSession = PDFPreviewSession()
            previewRevision = WorkspaceRevisionState(data: data, sha256: digest)
            attachmentSelection = file; attachmentData = data; attachmentDigest = digest
        }
        catch {
            guard isCurrent(captured), requestID == attachmentRequestID else { return }
            failure = workspaceBrowserError(error)
        }
    }
    private func openDiff(_ change: WorkspaceGitChange) {
        let staged = change.indexStatus != " " && change.indexStatus != "?"
        let unstaged = change.worktreeStatus != " " || change.indexStatus == "?"
        if staged && unstaged { diffChoice = change } else { loadDiff(change, staged: staged) }
    }
    private func loadDiff(_ change: WorkspaceGitChange, staged: Bool) {
        guard let root = selectedRoot else { return }
        let captured = location
        let requestID = UUID()
        diffRequestID = requestID
        selectedDiff = change
        diffText = nil
        failure = nil
        Task {
            do {
                let response = try await model.loadWorkspaceGitDiff(source, root: root, path: change.path, staged: staged)
                guard isCurrent(captured), requestID == diffRequestID,
                      selectedDiff?.id == change.id else { return }
                selectedDiffStaged = response.staged
                diffText = response.diff
            } catch {
                guard isCurrent(captured), requestID == diffRequestID,
                      selectedDiff?.id == change.id else { return }
                failure = workspaceBrowserError(error)
                selectedDiff = nil
            }
        }
    }
    private func workspaceBrowserError(_ error: Error) -> String { WorkspaceUnavailable.message(error) }
}

/// What Files says when a location can't be read. Once the computer has
/// refused the thread's worktree, that stays the reason (with Add folder) even
/// if reading the Project's own folders in its place fails as well.
struct WorkspaceUnavailable: Equatable {
    let message: String
    let worktreeRefusal: String?

    init(_ error: Error, worktreeRefusal earlier: String?) {
        switch error {
        case PairingFailure.worktreeNotInProject(let reason): worktreeRefusal = reason
        // An ended connection is its own reason; adding a folder can't help.
        case PairingFailure.response(401): worktreeRefusal = nil
        default: worktreeRefusal = earlier
        }
        message = worktreeRefusal ?? Self.message(error)
    }

    static func message(_ error: Error) -> String {
        switch error {
        case PairingFailure.hostMessage(_, let message), PairingFailure.worktreeNotInProject(let message): return message
        default: break
        }
        if case PairingFailure.response(let code) = error {
            switch code {
            case 401: return "Your computer connection expired. Reconnect, then try again."
            case 403: return "Access to this folder changed. Review the Project's folders on your computer."
            case 404: return "This chat or Project folder is unavailable. Check it on your computer."
            case 409: return "These files changed while you were browsing. Close Files, then open it again."
            case 503: return "Your computer couldn't verify this folder. Check the connection."
            default: break
            }
        }
        return "Files could not be loaded. Check your computer."
    }

    /// Why one file didn't open: a dropped connection, lost access, a file
    /// that is really gone and a failed check each name their own fix.
    static func fileMessage(_ error: Error) -> String {
        switch error {
        case PairingFailure.hostMessage(_, let message): return message
        case let relay as RelayUnsupported: return relay.errorDescription ?? "Open this file on your home network or over Tailscale."
        case is URLError:
            return "Your computer couldn’t be reached. Check the connection, then try again."
        case FileFailure.integrity, FileFailure.stale:
            return "This file changed or arrived incomplete. Try again."
        case PairingFailure.response(401):
            return "Your computer connection expired. Reconnect, then try again."
        case PairingFailure.response(403):
            return "Cupola no longer has access to this file. Review the Project’s folders on your computer."
        case PairingFailure.response(404), FileFailure.gone:
            return "This file is no longer there. It may have moved or been deleted."
        case PairingFailure.response(let code) where code >= 500:
            return "Your computer couldn’t read this file. Check your computer, then try again."
        default:
            return "This file could not be opened. Check your computer, then try again."
        }
    }
}

/// Owns Files' Add folder request. Closing or backgrounding Files cancels it,
/// so a late answer neither rereads the workspace nor changes what it shows.
@MainActor final class WorkspaceFolderAdder: ObservableObject {
    @Published private(set) var adding = false
    private var task: Task<Void, Never>?
    private var lifetime = UUID()

    func start(add: @escaping @MainActor () async throws -> Void,
               added: @escaping @MainActor () async -> Void,
               failed: @escaping @MainActor (Error) -> Void) {
        guard !adding else { return }
        adding = true
        let owner = lifetime
        task = Task { [weak self] in
            do {
                try await add()
                guard let self, !Task.isCancelled, lifetime == owner else { return }
                adding = false
                // Stay owned through the reread so closing Files still cancels it.
                await added()
                if lifetime == owner { task = nil }
            } catch {
                guard let self, !Task.isCancelled, lifetime == owner else { return }
                adding = false; task = nil
                if !(error is CancellationError) { failed(error) }
            }
        }
    }

    func cancel() {
        lifetime = UUID()
        task?.cancel(); task = nil
        adding = false
    }
}
