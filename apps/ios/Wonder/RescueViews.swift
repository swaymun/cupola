import SwiftUI
import WonderPairing

/// "Cupola stopped on <computer>" with Restart, shown by the one place that
/// already says the computer can't be reached: the chat's composer line and
/// the computer's row in Chats. It replaces that line's Reconnect; it is not a
/// second banner.
struct RescueNoticeView: View {
    @ObservedObject var model: ConnectionModel
    let notice: RescueNotice
    /// `composer` or `sidebar`, so each place keeps its own identifiers.
    let place: String
    @State private var showingDiagnostics = false

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .center, spacing: 12) {
                message(wraps: false)
                Spacer(minLength: 8)
                actions
            }
            VStack(alignment: .leading, spacing: 0) {
                message(wraps: true)
                HStack(spacing: 20) { actions }
            }
        }
        .sheet(isPresented: $showingDiagnostics) { RescueDiagnosticsSheet(model: model) }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("\(place)-rescue")
    }

    private func message(wraps: Bool) -> some View {
        HStack(spacing: 8) {
            if notice.isRestarting {
                ProgressView().controlSize(.small).accessibilityHidden(true)
            } else {
                Image(systemName: "desktopcomputer.trianglebadge.exclamationmark").accessibilityHidden(true)
            }
            Text(notice.message(computer: model.macName))
                .lineLimit(wraps ? nil : 1)
                .fixedSize(horizontal: !wraps, vertical: wraps)
        }
        .font(.caption).foregroundStyle(.secondary)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("\(place)-rescue-message")
    }

    @ViewBuilder private var actions: some View {
        if notice.offersRestart {
            // Plain styles: in a List row (Chats) two default buttons both fire on one tap.
            Button { model.restartHost() } label: {
                Text(notice.restartTitle)
                    .font(.caption.weight(.semibold)).foregroundStyle(Color(uiColor: .systemBlue))
                    .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("\(place)-rescue-restart")
        }
        if !notice.isRestarting {
            Button { showingDiagnostics = true } label: {
                Text("Get diagnostics")
                    .font(.caption).foregroundStyle(.secondary)
                    .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("\(place)-rescue-diagnostics")
        }
    }
}

/// The open-chat screen when nothing of the chat is saved and the computer is away,
/// in place of "<computer> is offline" when its supervisor answers. Same words and
/// actions as the composer line; Try again stays for a computer that's merely slow.
struct RescueOfflineView: View {
    @ObservedObject var model: ConnectionModel
    let notice: RescueNotice
    let tryAgain: () -> Void
    @State private var showingDiagnostics = false

    var body: some View {
        ContentUnavailableView {
            HStack(spacing: 8) {
                if notice.isRestarting { ProgressView().accessibilityHidden(true) }
                Text(notice.message(computer: model.macName)).multilineTextAlignment(.center)
            }
            .font(.title3.bold())
            .accessibilityIdentifier("chat-rescue-message")
        } actions: {
            if notice.offersRestart {
                Button(notice.restartTitle) { model.restartHost() }
                    .buttonStyle(.wonderProminent)
                    .accessibilityIdentifier("chat-rescue-restart")
            }
            if !notice.isRestarting {
                Button("Get diagnostics") { showingDiagnostics = true }
                    .accessibilityIdentifier("chat-rescue-diagnostics")
            }
        }
        .sheet(isPresented: $showingDiagnostics) { RescueDiagnosticsSheet(model: model) }
    }
}

/// The host's redacted state and log, to read and to share with the system share sheet.
private struct RescueDiagnosticsSheet: View {
    @ObservedObject var model: ConnectionModel

    var body: some View {
        NavigationStack {
            content
                .wonderPage()
                .navigationTitle("Diagnostics")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    if case .ready(let report) = model.rescueDiagnostics {
                        ToolbarItem(placement: .primaryAction) {
                            ShareLink(item: report, subject: Text("Cupola diagnostics"))
                                .accessibilityIdentifier("rescue-diagnostics-share")
                        }
                    }
                }
        }
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
        .task { await model.loadRescueDiagnostics() }
    }

    @ViewBuilder private var content: some View {
        switch model.rescueDiagnostics {
        case .idle, .loading:
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
                .accessibilityLabel("Getting diagnostics")
                .accessibilityIdentifier("rescue-diagnostics-loading")
        case .ready(let report):
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    Text("Cupola removed secrets and personal details. Look it over before you share it.")
                        .font(.footnote).foregroundStyle(.secondary)
                    Text(report)
                        .font(.caption.monospaced()).textSelection(.enabled)
                        .accessibilityIdentifier("rescue-diagnostics-report")
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(16)
            }
        case .failed:
            ContentUnavailableView {
                Label("Couldn’t get diagnostics", systemImage: "exclamationmark.triangle")
            } description: {
                Text("\(model.macName) didn’t answer.")
            } actions: {
                Button("Try again") { Task { await model.loadRescueDiagnostics() } }
                    .accessibilityIdentifier("rescue-diagnostics-retry")
            }
        }
    }
}
