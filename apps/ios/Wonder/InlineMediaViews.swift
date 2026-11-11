import SwiftUI
import AVKit
import WonderPairing

/// An SVG drawing a tool made, drawn in the HTML preview's sandbox: no
/// scripts, no network, loaded as an `<img>` from its verified bytes so its
/// own scripts and references never run. Tapping opens it larger.
struct InlineSVGPreview: View {
    /// Prepared pages by verified file (bounded; the system may evict them).
    @MainActor private static let pages: NSCache<NSString, NSString> = {
        let cache = NSCache<NSString, NSString>(); cache.totalCostLimit = 8 * 1_024 * 1_024; return cache
    }()
    let model: ConnectionModel
    let chat: ChatSummary
    let file: ConversationFile
    @Environment(\.wonderTheme) private var theme
    @State private var html: String?
    @State private var failed = false
    @State private var showingFull = false
    @State private var retry = 0

    var body: some View {
        Button { if html != nil { showingFull = true } } label: {
            theme.surface
                .frame(maxWidth: 360).frame(height: 220)
                .overlay {
                    if let html { ConstrainedHTML(html: html).allowsHitTesting(false) }
                    else if failed { Label("Preview unavailable", systemImage: "exclamationmark.triangle").font(.caption) }
                    else { ProgressView() }
                }
                .clipShape(RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Preview \(file.name)")
        .accessibilityIdentifier("tool-image:\(file.id)")
        .accessibilityValue(html != nil ? "Loaded" : failed ? "Unavailable" : "Loading")
        .overlay(alignment: .topLeading) {
            if failed {
                Button("Retry preview", systemImage: "arrow.clockwise") { retry += 1 }
                    .font(.caption).frame(minHeight: 44).padding(8)
            }
        }
        .task(id: "\(file.id):\(file.sha256 ?? ""):\(retry)") { await load() }
        .sheet(isPresented: $showingFull) {
            if let html { InlineSVGSheet(title: file.name, html: html) }
        }
    }

    private func load() async {
        failed = false
        let key = InlineMediaFiles.key(chat: chat, file: file)
        if let key, let cached = Self.pages.object(forKey: key as NSString) { html = cached as String; return }
        do {
            let data = try await model.download(file, chat: chat)
            guard ConversationFile.isSafeSVG(data) else { throw FileFailure.integrity }
            let encoded = data.base64EncodedString()
            let page = "<style>html,body{margin:0;height:100%;background:transparent}img{width:100%;height:100%;object-fit:contain}</style>"
                + "<img alt=\"\" src=\"data:image/svg+xml;base64,\(encoded)\">"
            if let key { Self.pages.setObject(page as NSString, forKey: key as NSString, cost: page.utf8.count) }
            html = page
        } catch {
            guard !Task.isCancelled else { return }
            failed = true
        }
    }
}

/// The drawing full screen, in the same sandbox, with the chrome of the other
/// full-screen viewers: its file name and Done. A swipe down still closes it.
private struct InlineSVGSheet: View {
    let title: String
    let html: String
    @Environment(\.dismiss) private var dismiss
    var body: some View {
        NavigationStack {
            ConstrainedHTML(html: html)
                .ignoresSafeArea(edges: .bottom)
                .wonderPage()
                .accessibilityIdentifier("svg-viewer")
                .navigationTitle(title)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done") { dismiss() }.accessibilityIdentifier("svg-viewer-done")
                    }
                }
        }
        .presentationDetents([.large])
    }
}

/// An SVG drawing from a converted page, shown as a picture: its markup is an
/// `<img>` in the sandboxed web view (no scripts, no network, no navigation),
/// so nothing in it runs or loads. The wrapper is built off the main actor.
struct HTMLDrawingView: View {
    let drawing: HTMLVisualPage.Drawing
    @State private var html: String?
    var body: some View {
        Group {
            if let html { ConstrainedHTML(html: html).allowsHitTesting(false) } else { Color.clear }
        }
        .task(id: drawing.svg) {
            let svg = drawing.svg
            html = await Task.detached(priority: .userInitiated) {
                "<style>html,body{margin:0;height:100%;background:transparent}img{width:100%;height:100%;object-fit:contain}</style>"
                    + "<img alt=\"\" src=\"data:image/svg+xml;base64,\(Data(svg.utf8).base64EncodedString())\">"
            }.value
        }
    }
}

/// A page an agent published for the reader (T3 Code's `html_render`), drawn
/// from its verified file in the reader's theme: a bounded preview
/// in the conversation, the whole page behind Expand.
/// Download and conversion run off the main actor; a page already prepared
/// (same SHA-256) shows at once when its row returns.
struct PublishedPageView: View {
    let model: ConnectionModel
    let chat: ChatSummary
    let file: ConversationFile
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @State private var prepared: HTMLVisualCache.Prepared?
    @State private var failure: String?
    @State private var expanded = false
    @State private var retry = 0
    /// Parts shown in the row; the rest opens with Expand.
    static let inlineBlocks = 16

    private var title: String { Self.title(of: file) }
    /// The page's name without its file extension.
    static func title(of file: ConversationFile) -> String {
        for suffix in [".html", ".json"] where file.name.lowercased().hasSuffix(suffix) {
            return String(file.name.dropLast(suffix.count))
        }
        return file.name
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Label(title, systemImage: "doc.richtext")
                    .font(typography.font(.headline))
                    .accessibilityAddTraits(.isHeader)
                Spacer(minLength: 8)
                Button { expanded = true } label: {
                    Label("Expand", systemImage: "arrow.up.left.and.arrow.down.right")
                        .font(.subheadline.weight(.medium)).frame(minHeight: 44)
                }
                .buttonStyle(.borderless)
                .disabled(prepared == nil)
                .accessibilityHint("Opens the whole page full screen.")
                .accessibilityIdentifier("html-render-expand")
            }
            content
        }
        .padding(12)
        .frame(maxWidth: 760, alignment: .leading)
        .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(theme.rule))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("html-render-page")
        .task(id: "\(file.id):\(file.sha256 ?? ""):\(retry)") { await load() }
        .sheet(isPresented: $expanded) {
            if let prepared { ExpandedMessageSheet(title: title, content: .page(prepared.html, fence: false)) }
        }
    }

    @ViewBuilder private var content: some View {
        if let prepared {
            if prepared.rendersHTML {
                HTMLArtifactView(html: prepared.html, inline: true)
            } else if prepared.page.isEmpty {
                Text("This page has nothing Cupola can show.")
                    .font(typography.font(.footnote)).foregroundStyle(theme.secondaryText)
            } else {
                let opening = prepared.page.opening(Self.inlineBlocks)
                HTMLVisualPageView(page: opening.page, theme: theme.nativeUI,
                                   fonts: typography.nativeUI,
                                   lazy: false, drawing: { AnyView(HTMLDrawingView(drawing: $0)) }) { text in
                    AgentMessageText(text: text)
                }
                if opening.remaining > 0 {
                    Button("Show the whole page (\(opening.remaining) more \(opening.remaining == 1 ? "part" : "parts"))") { expanded = true }
                        .font(.subheadline.weight(.medium)).frame(minHeight: 44)
                        .accessibilityIdentifier("html-render-more")
                }
            }
        } else if let failure {
            VStack(alignment: .leading, spacing: 4) {
                Label(failure, systemImage: "exclamationmark.triangle").font(typography.font(.footnote))
                Button("Try again") { retry += 1 }.frame(minHeight: 44)
            }
            .accessibilityIdentifier("html-render-failure")
        } else {
            ProgressView("Preparing page").accessibilityIdentifier("html-render-preparing")
        }
    }

    private func load() async {
        failure = nil
        if let sha256 = file.sha256, let hit = await HTMLVisualCache.shared.cached(sha256: sha256) {
            prepared = hit
            return
        }
        do {
            let data = try await model.download(file, chat: chat)
            guard let value = await HTMLVisualCache.shared.prepare(page: data)
            else { throw FileFailure.integrity }
            guard !Task.isCancelled else { return }
            prepared = value
        } catch {
            guard !Task.isCancelled else { return }
            failure = (error as? FileFailure) == .gone ? "This page is no longer on your computer."
                : "Cupola couldn’t load this page."
        }
    }
}

/// The one conversation item that is playing. It keeps that item's player
/// while its row scrolls away, so audio keeps going and the row picks it up
/// again on return; paused items keep only their position (bounded). Playing
/// claims the shared audio session for playback, so media after dictation or
/// voice is heard, and one item plays at a time.
@MainActor final class InlineMediaPlayback {
    static let shared = InlineMediaPlayback()
    private struct Active {
        let key: String
        let conversationID: String
        /// The conversation screen (one per window) that started it; only that
        /// screen closing, leaving the foreground or losing its pairing stops it.
        let owner: UUID?
        let player: AVPlayer
        let url: URL?
        var lease: AudioSessionOwner.Lease?
        var shown: Bool
        var status: NSKeyValueObservation?
    }
    private var active: Active?
    private var positions = MediaResumePositions(limit: 32)

    /// The player still going for a row that left the screen, now back.
    func returning(_ key: String, owner: UUID? = nil) -> (player: AVPlayer, url: URL?)? {
        guard var active, active.key == key, active.owner == owner else { return nil }
        active.shown = true; self.active = active
        return (active.player, active.url)
    }
    func position(_ key: String) -> Double? { positions[key] }

    /// Plays `player` with the audio session set up for playback. Refused
    /// (false, and the item stays paused) while dictation or voice is on.
    func play(_ player: AVPlayer, key: String, conversationID: String, owner: UUID? = nil, url: URL?) -> Bool {
        if let active, active.player === player, AudioSessionOwner.shared.holds(active.lease) {
            if player.timeControlStatus == .paused { player.play() }
            return true
        }
        let lease: AudioSessionOwner.Lease
        do {
            lease = try AudioSessionOwner.shared.acquire(.media) { lease in
                Task { @MainActor in InlineMediaPlayback.shared.taken(lease) }
            }
        } catch {
            player.pause()
            return false
        }
        if let previous = active {
            if previous.player !== player { retire(previous) } else { previous.status?.invalidate() }
        }
        let id = ObjectIdentifier(player)
        // Delivered on any thread; only the player's identity crosses over.
        let status = player.observe(\.timeControlStatus) { @Sendable _, _ in
            Task { @MainActor in InlineMediaPlayback.shared.statusChanged(id) }
        }
        active = Active(key: key, conversationID: conversationID, owner: owner, player: player, url: url,
                        lease: lease, shown: true, status: status)
        if player.timeControlStatus == .paused { player.play() }
        return true
    }

    /// The row left the screen. A playing item keeps going here; a paused one
    /// keeps its position. Returns whether this owner kept the player (and its file).
    func rowLeft(_ key: String, player: AVPlayer, duration: Double) -> Bool {
        if var active, active.player === player, player.timeControlStatus != .paused {
            active.shown = false; self.active = active
            return true
        }
        if active?.player === player, let active { self.active = nil; retire(active, deleteFile: false) }
        player.pause()
        positions.keep(player.currentTime().seconds, for: key, duration: duration)
        return false
    }

    /// The conversation screen `owner` closed, left the foreground or lost its
    /// pairing: stop the item it started. Another window's item keeps playing.
    func stop(owner: UUID, forgetPositions: Bool = false) {
        if let active, active.owner == owner { self.active = nil; retire(active) }
        if forgetPositions { positions.removeAll() }
    }

    /// Whether `player` is the one item playback keeps (for tests and rows).
    func keeps(_ player: AVPlayer) -> Bool { active?.player === player }

    /// Stops whatever plays, from any window (a clean slate for tests).
    func stopAll(forgetPositions: Bool) {
        if let active { self.active = nil; retire(active) }
        if forgetPositions { positions.removeAll() }
    }

    private func taken(_ lease: AudioSessionOwner.Lease) {
        guard var active, active.lease == lease else { return }
        // A newer owner holds the session; this lease releases nothing now.
        active.lease = nil; self.active = active
        active.player.pause()
    }

    private func statusChanged(_ id: ObjectIdentifier) {
        guard let active, ObjectIdentifier(active.player) == id, active.player.timeControlStatus == .paused else { return }
        // Paused or finished: give the session back so other apps' audio resumes.
        AudioSessionOwner.shared.release(active.lease)
        if active.shown {
            self.active?.lease = nil
        } else {
            self.active = nil; retire(active)
        }
    }

    private func retire(_ item: Active, deleteFile: Bool = true) {
        item.status?.invalidate()
        item.player.pause()
        AudioSessionOwner.shared.release(item.lease)
        let duration = item.player.currentItem?.duration.seconds ?? 0
        positions.keep(item.player.currentTime().seconds, for: item.key, duration: duration.isFinite ? duration : 0)
        if deleteFile, !item.shown, let url = item.url, !InlineMediaFiles.isKept(url) { try? FileManager.default.removeItem(at: url) }
    }
}

/// Claims the shared audio session for playback while a player outside the
/// conversation (a Files preview) plays, and gives it back when it pauses,
/// ends or closes. Playing during dictation or voice is paused and `blocked`.
@MainActor final class MediaSessionClaim {
    private let player: AVPlayer
    private var lease: AudioSessionOwner.Lease?
    private var status: NSKeyValueObservation?
    var onBlocked: ((Bool) -> Void)?

    init(player: AVPlayer) {
        self.player = player
        status = player.observe(\.timeControlStatus) { @Sendable [weak self] _, _ in
            Task { @MainActor [weak self] in self?.statusChanged() }
        }
    }

    private func statusChanged() {
        guard status != nil else { return }
        if player.timeControlStatus == .paused {
            AudioSessionOwner.shared.release(lease); lease = nil
            return
        }
        guard !AudioSessionOwner.shared.holds(lease) else { return }
        do {
            lease = try AudioSessionOwner.shared.acquire(.media) { [weak self] lease in
                Task { @MainActor [weak self] in
                    guard let self, self.lease == lease else { return }
                    self.lease = nil; self.player.pause()
                }
            }
            onBlocked?(false)
        } catch {
            player.pause()
            onBlocked?(true)
        }
    }

    func end() {
        status?.invalidate(); status = nil
        AudioSessionOwner.shared.release(lease); lease = nil
    }
    deinit { AudioSessionOwner.shared.release(lease) }
}

/// Verified copies of tool audio and video in this app's temporary folder,
/// kept for rows that leave the screen and return so they play without
/// downloading the file again. Bounded by size; the oldest copies go first.
/// Each launch writes its own folder and removes earlier launches' at
/// startup, so the bound holds on disk, not only for this launch's list.
@MainActor enum InlineMediaFiles {
    private static var copies: [(key: String, url: URL, bytes: Int)] = []
    private static let limit = 48 * 1_024 * 1_024
    nonisolated static let root = FileManager.default.temporaryDirectory.appendingPathComponent("inline-media", isDirectory: true)
    nonisolated static let launch = UUID().uuidString
    /// Removes copies earlier launches left (they are neither reused nor
    /// counted), including loose ones older builds kept in the temporary
    /// folder. Runs off the main thread at launch.
    nonisolated static func removeEarlierLaunches(root: URL = root, temporary: URL = FileManager.default.temporaryDirectory,
                                                  keeping launch: String = launch) {
        let files = FileManager.default
        for url in (try? files.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)) ?? [] where url.lastPathComponent != launch {
            try? files.removeItem(at: url)
        }
        for url in (try? files.contentsOfDirectory(at: temporary, includingPropertiesForKeys: nil)) ?? []
        where url.lastPathComponent.hasPrefix("inline-") && url.standardizedFileURL != root.standardizedFileURL {
            try? files.removeItem(at: url)
        }
    }
    /// Only a file with a digest has a stable identity to keep it under.
    static func key(chat: ChatSummary, file: ConversationFile) -> String? {
        file.sha256.map { [chat.id, file.id, $0].joined(separator: "/") }
    }
    static func url(_ key: String) -> URL? {
        guard let index = copies.firstIndex(where: { $0.key == key }) else { return nil }
        let copy = copies.remove(at: index)
        guard FileManager.default.fileExists(atPath: copy.url.path) else { return nil }
        copies.append(copy)
        return copy.url
    }
    static func keep(_ data: Data, key: String?, extension ext: String) throws -> URL {
        let folder = root.appendingPathComponent(launch, isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let url = folder.appendingPathComponent("\(UUID().uuidString).\(ext)")
        try data.write(to: url, options: [.atomic, .completeFileProtection])
        guard let key else { return url }
        copies.append((key, url, data.count))
        while copies.count > 1, copies.reduce(0, { $0 + $1.bytes }) > limit {
            try? FileManager.default.removeItem(at: copies.removeFirst().url)
        }
        return url
    }
    static func isKept(_ url: URL) -> Bool { copies.contains { $0.url == url } }
}

/// Audio or video a tool made, played inline from its verified bytes (at most
/// 8 MB, so no streaming). A recent copy stays for the row's return (see
/// `InlineMediaFiles`); any other temporary copy is removed when the row leaves.
extension EnvironmentValues {
    /// The conversation screen that owns chat audio and video started here.
    @Entry var inlineMediaOwner: UUID?
}

struct InlineMediaPlayer: View {
    let model: ConnectionModel
    let chat: ChatSummary
    let file: ConversationFile
    @Environment(\.wonderTheme) private var theme
    @Environment(\.inlineMediaOwner) private var mediaOwner
    @State private var player: AVPlayer?
    @State private var localURL: URL?
    @State private var failed = false
    @State private var playing = false
    @State private var position: Double = 0
    @State private var duration: Double = 0
    @State private var retry = 0
    @State private var observer: Any?
    /// Playing was refused because dictation or voice is using audio.
    @State private var blocked = false

    private var isVideo: Bool { file.mimeType?.lowercased().hasPrefix("video/") == true }
    /// Identity across the row leaving and returning.
    private var playbackKey: String { InlineMediaFiles.key(chat: chat, file: file) ?? "\(chat.id)/\(file.id)" }

    var body: some View {
        Group {
            if isVideo { video } else { audio }
        }
        .accessibilityIdentifier("tool-media:\(file.id)")
        .task(id: "\(file.id):\(file.sha256 ?? ""):\(retry)") { await load() }
        .onDisappear { release() }
    }

    private var video: some View {
        theme.surface
            .frame(maxWidth: 360).frame(height: 220)
            .overlay {
                if let player { VideoPlayer(player: player) }
                else if failed { unavailable }
                else { ProgressView() }
            }
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .overlay(alignment: .bottomLeading) { if blocked { blockedNote.padding(8) } }
            .accessibilityLabel("Video \(file.name)")
    }

    private var blockedNote: some View {
        Text("Finish dictation or voice to play this.")
            .font(.caption).foregroundStyle(.secondary)
            .accessibilityIdentifier("tool-media-blocked:\(file.id)")
    }

    private var audio: some View {
        HStack(spacing: 10) {
            Button { toggle() } label: {
                Image(systemName: playing ? "pause.fill" : "play.fill")
                    .frame(width: 44, height: 44)
            }
            .disabled(player == nil)
            .accessibilityLabel(playing ? "Pause \(file.name)" : "Play \(file.name)")
            .accessibilityIdentifier("tool-media-play:\(file.id)")
            VStack(alignment: .leading, spacing: 2) {
                Text(file.name).font(.subheadline).lineLimit(1)
                if failed { unavailable }
                else {
                    Slider(value: Binding(get: { position }, set: { seek(to: $0) }), in: 0...max(duration, 0.1))
                        .disabled(player == nil)
                        .accessibilityLabel("Position")
                        .accessibilityValue(Self.time(position))
                    Text("\(Self.time(position)) / \(Self.time(duration))")
                        .font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                    if blocked { blockedNote }
                }
            }
        }
        .padding(.horizontal, 8).padding(.vertical, 6)
        .frame(maxWidth: 360, alignment: .leading)
        .background(theme.surface, in: RoundedRectangle(cornerRadius: 8))
    }

    private var unavailable: some View {
        HStack {
            Text("Can’t play this file").font(.caption).foregroundStyle(.secondary)
            Button("Try again") { retry += 1 }.font(.caption)
        }
    }

    private func load() async {
        release()
        failed = false
        if let returning = InlineMediaPlayback.shared.returning(playbackKey, owner: mediaOwner) {
            let seconds = returning.player.currentItem?.duration.seconds ?? 0
            duration = seconds.isFinite ? seconds : 0
            adopt(returning.player, url: returning.url)
            return
        }
        do {
            let key = InlineMediaFiles.key(chat: chat, file: file)
            let url: URL
            if let key, let kept = InlineMediaFiles.url(key) {
                url = kept
            } else {
                let data = try await model.download(file, chat: chat)
                let ext = (file.name as NSString).pathExtension.isEmpty ? "media" : (file.name as NSString).pathExtension
                url = try InlineMediaFiles.keep(data, key: key, extension: ext)
            }
            guard !Task.isCancelled else { if !InlineMediaFiles.isKept(url) { try? FileManager.default.removeItem(at: url) }; return }
            let asset = AVURLAsset(url: url)
            let seconds = try await asset.load(.duration).seconds
            guard !Task.isCancelled else { if !InlineMediaFiles.isKept(url) { try? FileManager.default.removeItem(at: url) }; return }
            let next = AVPlayer(playerItem: AVPlayerItem(asset: asset))
            duration = seconds.isFinite ? seconds : 0
            adopt(next, url: url)
            // A paused item returns where it stopped.
            if let saved = InlineMediaPlayback.shared.position(playbackKey) { seek(to: saved) }
        } catch {
            guard !Task.isCancelled else { return }
            failed = true
        }
    }

    /// Shows `next` in this row. The time observer also runs when playback
    /// starts or stops, so playing started from the video's own controls
    /// goes through the same audio-session claim as the play button.
    private func adopt(_ next: AVPlayer, url: URL?) {
        localURL = url
        position = next.currentTime().seconds.isFinite ? next.currentTime().seconds : 0
        playing = next.timeControlStatus != .paused
        let key = playbackKey, conversationID = chat.id, owner = mediaOwner
        observer = next.addPeriodicTimeObserver(forInterval: CMTime(seconds: 0.5, preferredTimescale: 600), queue: .main) { time in
            MainActor.assumeIsolated {
                position = time.seconds.isFinite ? time.seconds : 0
                playing = next.timeControlStatus != .paused
                if playing {
                    blocked = !InlineMediaPlayback.shared.play(next, key: key, conversationID: conversationID, owner: owner, url: url)
                    playing = !blocked
                }
            }
        }
        player = next
    }

    private func toggle() {
        guard let player else { return }
        if playing { player.pause(); playing = false; return }
        if duration > 0, position >= duration - 0.05 { player.seek(to: .zero) }
        blocked = !InlineMediaPlayback.shared.play(player, key: playbackKey, conversationID: chat.id, owner: mediaOwner, url: localURL)
        playing = !blocked
    }

    private func seek(to seconds: Double) {
        position = seconds
        player?.seek(to: CMTime(seconds: seconds, preferredTimescale: 600))
    }

    /// The row leaves (or reloads). A playing item stays with
    /// `InlineMediaPlayback`, file included; a paused one keeps its position.
    private func release() {
        if let observer { player?.removeTimeObserver(observer) }
        observer = nil
        let kept = player.map { InlineMediaPlayback.shared.rowLeft(playbackKey, player: $0, duration: duration) } ?? false
        player = nil
        playing = false; blocked = false
        if !kept, let localURL, !InlineMediaFiles.isKept(localURL) { try? FileManager.default.removeItem(at: localURL) }
        localURL = nil
    }

    static func time(_ seconds: Double) -> String {
        guard seconds.isFinite, seconds >= 0 else { return "0:00" }
        let total = Int(seconds.rounded(.down))
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}
