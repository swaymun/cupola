import SwiftUI
import UIKit
import WidgetKit

/// The most recently active threads on the Mac last used for a new chat.
/// Tapping one opens that thread; Computer opens that Mac's screen. Before any
/// thread is known, recent Projects start a new chat instead. Its computer and
/// thread symbols repeat WonderPairing's ConceptSymbol, which it does not link.
struct WonderProjectWidgetEntry: TimelineEntry {
    let date: Date
    let snapshot: ProjectWidgetSnapshot?
    let identity: ProjectWidgetIdentity?

    var isStale: Bool { snapshot?.isStale(at: date) ?? true }
}

struct WonderProjectWidgetProvider: TimelineProvider {
    func placeholder(in context: Context) -> WonderProjectWidgetEntry {
        WonderProjectWidgetEntry(date: Date(), snapshot: ProjectWidgetSnapshot(hostID: "mac", hostName: "Computer", projects: [], threads: [
            .init(projectID: "one", family: "codex", nativeID: "one", conversationID: nil, title: "Chat 1", projectName: "Project"),
            .init(projectID: "one", family: "codex", nativeID: "two", conversationID: nil, title: "Chat 2", projectName: "Project"),
            .init(projectID: "one", family: "claude", nativeID: "three", conversationID: nil, title: "Chat 3", projectName: "Project"),
        ]), identity: nil)
    }

    func getSnapshot(in context: Context, completion: @escaping (WonderProjectWidgetEntry) -> Void) {
        completion(entry(at: Date()))
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<WonderProjectWidgetEntry>) -> Void) {
        let now = Date()
        let first = entry(at: now)
        var entries = [first]
        if let savedAt = first.snapshot?.savedAt {
            let staleAt = savedAt.addingTimeInterval(ProjectWidgetSnapshot.staleAfter)
            if staleAt > now { entries.append(entry(at: staleAt)) }
        }
        completion(Timeline(entries: entries, policy: .after(now.addingTimeInterval(60 * 60))))
    }

    private func entry(at date: Date) -> WonderProjectWidgetEntry {
        WonderProjectWidgetEntry(date: date,
                                 snapshot: ProjectWidgetSnapshotStore.load(bundleIdentifier: Bundle.main.bundleIdentifier),
                                 identity: ProjectWidgetIdentity(bundleIdentifier: Bundle.main.bundleIdentifier))
    }
}

struct WonderProjectWidgetView: View {
    @Environment(\.widgetFamily) private var family
    @Environment(\.redactionReasons) private var redactionReasons
    let entry: WonderProjectWidgetEntry

    /// Larger widgets show three recent threads (or Projects) and Computer as equal tiles.
    static let projectLimit = 3

    /// One tile: a thread to open, or a Project to start a chat in.
    private struct Destination: Identifiable {
        let id: String
        let title: String
        let detail: String
        let icon: String
        let url: URL?
        let accessibility: String
    }

    private func destinations(_ snapshot: ProjectWidgetSnapshot, hostID: String,
                              identity: ProjectWidgetIdentity) -> [Destination] {
        if !snapshot.threads.isEmpty {
            return snapshot.threads.prefix(Self.projectLimit).map { thread in
                Destination(id: "thread-" + thread.id, title: thread.title, detail: thread.projectName,
                            icon: "bubble.left.and.text.bubble.right",
                            url: ProjectWidgetLink.thread(hostID: hostID, thread: thread, identity: identity),
                            accessibility: ProjectWidgetSpeech.destination(title: thread.title, detail: thread.projectName, redacted: redacted))
            }
        }
        return snapshot.projects.prefix(Self.projectLimit).map { project in
            Destination(id: "project-" + project.id, title: project.name, detail: "New chat", icon: "square.and.pencil",
                        url: ProjectWidgetLink.newChat(hostID: hostID, projectID: project.id, identity: identity),
                        accessibility: ProjectWidgetSpeech.destination(title: project.name, detail: "new chat", redacted: redacted))
        }
    }
    private var columns: Int { family == .systemExtraLarge ? 4 : 2 }
    /// Large and extra-large tiles fill their space with a stacked layout.
    private var tall: Bool { family == .systemLarge || family == .systemExtraLarge }
    /// iOS hides private content (a locked device); spoken labels follow suit.
    private var redacted: Bool { redactionReasons.contains(.privacy) }

    var body: some View {
        Group {
            if let snapshot = entry.snapshot, let hostID = snapshot.hostID, let identity = entry.identity {
                if family == .systemSmall { small(snapshot, hostID: hostID, identity: identity) }
                else { grid(snapshot, hostID: hostID, identity: identity) }
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    Image(systemName: "desktopcomputer").foregroundStyle(.tint).widgetAccentable()
                    Spacer(minLength: 0)
                    Text("Open Cupola").font(.headline)
                    Text(entry.snapshot == nil ? "Your recent chats appear here." : "Pair a computer to see its chats.")
                        .font(.caption).foregroundStyle(.secondary).lineLimit(3)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
                .padding()
            }
        }
        .tint(.cupolaAccent)
        .containerBackground(.fill.tertiary, for: .widget)
    }

    /// One tap target: the most recent thread (or Project), or the computer.
    private func small(_ snapshot: ProjectWidgetSnapshot, hostID: String, identity: ProjectWidgetIdentity) -> some View {
        let first = destinations(snapshot, hostID: hostID, identity: identity).first
        let url = first?.url ?? ProjectWidgetLink.computer(hostID: hostID, identity: identity)
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 4) {
                hostLabel(snapshot)
                // The grid has room to say this in words; here a mark beside
                // the computer keeps the destination's lines intact.
                if entry.isStale {
                    Spacer(minLength: 0)
                    Image(systemName: "clock.badge.exclamationmark")
                        .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
            if let first {
                Image(systemName: first.icon).font(.title3).foregroundStyle(.tint).widgetAccentable()
                Text(first.title).font(.headline).lineLimit(2).privacySensitive()
                Text(first.detail).font(.caption).foregroundStyle(.secondary).lineLimit(1).privacySensitive()
            } else {
                Image(systemName: "desktopcomputer").font(.title3).foregroundStyle(.tint).widgetAccentable()
                Text("Computer").font(.headline)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
        .padding()
        .widgetURL(url)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(ProjectWidgetSpeech.small(destination: first?.accessibility, hostName: snapshot.hostName,
                                                      stale: entry.isStale, redacted: redacted))
    }

    private func grid(_ snapshot: ProjectWidgetSnapshot, hostID: String, identity: ProjectWidgetIdentity) -> some View {
        let projects = destinations(snapshot, hostID: hostID, identity: identity)
        let rows = (projects.count + 1 + columns - 1) / columns
        return VStack(alignment: .leading, spacing: 8) {
            hostLabel(snapshot)
            if projects.isEmpty {
                Text("Choose Projects in Cupola to start chats from here.")
                    .font(.subheadline).foregroundStyle(.secondary)
            }
            Grid(horizontalSpacing: 8, verticalSpacing: 8) {
                ForEach(0..<rows, id: \.self) { row in
                    GridRow {
                        ForEach(0..<columns, id: \.self) { column in
                            let index = row * columns + column
                            if index < projects.count {
                                destinationLink(projects[index])
                            } else if index == projects.count {
                                computerLink(snapshot, hostID: hostID, identity: identity)
                            } else {
                                Color.clear
                            }
                        }
                    }
                }
            }
            .frame(maxHeight: tall ? .infinity : nil)
            if !tall { Spacer(minLength: 0) }
            if entry.isStale {
                Text("Open Cupola to refresh").font(.caption2).foregroundStyle(.secondary)
                    .accessibilityLabel(ProjectWidgetSpeech.stale)
            }
        }
        .padding()
    }

    @ViewBuilder private func destinationLink(_ destination: Destination) -> some View {
        if let url = destination.url {
            Link(destination: url) {
                tile(icon: destination.icon, title: destination.title, private: true, detail: destination.detail)
            }
            .accessibilityLabel(destination.accessibility)
        }
    }

    @ViewBuilder private func computerLink(_ snapshot: ProjectWidgetSnapshot, hostID: String,
                                           identity: ProjectWidgetIdentity) -> some View {
        if let url = ProjectWidgetLink.computer(hostID: hostID, identity: identity) {
            Link(destination: url) {
                tile(icon: "desktopcomputer", title: "Computer", private: false, detail: nil)
            }
            .accessibilityLabel(ProjectWidgetSpeech.computer(hostName: snapshot.hostName, redacted: redacted))
        }
    }

    private func tile(icon: String, title: String, private isPrivate: Bool, detail: String?) -> some View {
        Group {
            if tall {
                VStack(alignment: .leading, spacing: 4) {
                    Image(systemName: icon).font(.title3).foregroundStyle(.tint).widgetAccentable()
                    Spacer(minLength: 0)
                    Text(title).font(.subheadline.weight(.semibold)).lineLimit(2).minimumScaleFactor(0.8)
                        .privacySensitive(isPrivate)
                    if let detail {
                        Text(detail).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            .privacySensitive(isPrivate)
                    }
                }
                .padding(12)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
            } else {
                HStack(spacing: 6) {
                    Image(systemName: icon).foregroundStyle(.tint).widgetAccentable()
                    Text(title).font(.subheadline).lineLimit(1).minimumScaleFactor(0.8)
                        .privacySensitive(isPrivate)
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 10)
                .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
            }
        }
        .background(.background.opacity(0.6), in: RoundedRectangle(cornerRadius: 10))
        .contentShape(Rectangle())
    }

    private func hostLabel(_ snapshot: ProjectWidgetSnapshot) -> some View {
        Label(snapshot.hostName, systemImage: "desktopcomputer")
            .font(.caption.weight(.semibold)).foregroundStyle(.secondary).lineLimit(1)
            .privacySensitive()
    }
}

struct WonderProjectWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: ProjectWidgetSnapshot.widgetKind, provider: WonderProjectWidgetProvider()) { entry in
            WonderProjectWidgetView(entry: entry)
        }
        .configurationDisplayName("Recent chats")
        .description("Open the chats you worked on most recently on your computer, or view its screen.")
        .supportedFamilies([.systemSmall, .systemMedium, .systemLarge, .systemExtraLarge])
    }
}

extension Color {
    /// Widgets and Live Activities show outside Cupola, where the chosen theme
    /// cannot reach, so they use Cupola's own amber (the System theme's accent)
    /// rather than the system blue an extension without an accent falls back
    /// to. Tinted Home Screens recolour it through `widgetAccentable()`.
    static let cupolaAccent = Color(UIColor { traits in
        traits.userInterfaceStyle == .dark
            ? UIColor(red: 0xF2 / 255, green: 0xB8 / 255, blue: 0x4B / 255, alpha: 1)
            : UIColor(red: 0x9A / 255, green: 0x5A / 255, blue: 0x00 / 255, alpha: 1)
    })
}

@main struct WonderProjectWidgetBundle: WidgetBundle {
    var body: some Widget {
        WonderProjectWidget()
        AgentLiveActivityWidget()
        VoiceLiveActivityWidget()
    }
}
