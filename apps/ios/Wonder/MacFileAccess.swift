import SwiftUI
import WonderPairing

/// This browser lists the paired Mac, independently of the device's Files app.
struct MacLocationBrowser: View {
    let model: ConnectionModel
    let title: String
    var foldersOnly = false
    var choose: (String, Bool) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var page: MacLocationPage?
    @State private var entries: [MacLocationPage.Entry] = []
    @State private var nextOffset: Int?
    @State private var requestedPath: String?
    @State private var showHidden = false
    @State private var busy = false
    @State private var failure: String?
    var body: some View {
        NavigationStack {
            WonderList {
                if let page {
                    Section {
                        if let parent = page.parentPath { Button("Up one folder", systemImage: "arrow.up") { Task { await load(parent) } } }
                        Text(page.path).font(.footnote).foregroundStyle(.secondary).textSelection(.enabled)
                    } header: { Text("On your computer") }
                    ForEach(entries) { entry in
                        if entry.isDirectory {
                            Button { Task { await load(entry.path) } } label: {
                                HStack { Label(entry.name, systemImage: "folder"); Spacer(); Image(systemName: "chevron.right").font(.caption).foregroundStyle(.secondary) }
                            }.foregroundStyle(.primary)
                        } else if !foldersOnly {
                            Button { guard !busy else { return }; choose(entry.path, false); dismiss() } label: { Label(entry.name, systemImage: "doc") }.foregroundStyle(.primary)
                        }
                    }
                    if nextOffset != nil { Button("Load more") { Task { await load(page.path, more: true) } } }
                    if entries.isEmpty && !busy && failure == nil { Text("This folder is empty").foregroundStyle(.secondary) }
                }
                if let failure { FailureDetails(message: failure); Button("Try again") { Task { await load(requestedPath) } } }
            }
            .overlay { if page == nil && busy { ProgressView("Loading folders…") } }
            .overlay(alignment: .topTrailing) {
                ProgressView().controlSize(.small).padding()
                    .opacity(busy && page != nil ? 1 : 0).allowsHitTesting(false)
                    .accessibilityLabel("Loading folders").accessibilityHidden(!busy || page == nil)
            }
            .navigationTitle(title).navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .primaryAction) { Menu { Button("Home", systemImage: "house") { Task { await load(nil) } }; Toggle("Show hidden files", isOn: $showHidden) } label: { Image(systemName: "ellipsis.circle").accessibilityLabel("View options") }.disabled(busy) }
            }
            .safeAreaInset(edge: .bottom) {
                if let page {
                    Button("Choose this folder") { choose(page.path, true); dismiss() }
                        .buttonStyle(.wonderProminent).controlSize(.large).frame(maxWidth: .infinity).padding()
                        .background(.bar).disabled(busy || failure != nil)
                }
            }
            .task { if page == nil { await load(nil) } }
            .onChange(of: showHidden) { _, _ in Task { await load(page?.path) } }
        }
    }
    private func load(_ path: String?, more: Bool = false) async {
        guard !busy else { return }; busy = true; failure = nil; requestedPath = path; defer { busy = false }
        var url = URLComponents(); url.path = "/api/v1/filesystem"
        url.queryItems = [URLQueryItem(name: "showHidden", value: String(showHidden)), URLQueryItem(name: "offset", value: String(more ? nextOffset ?? 0 : 0))]
        if let path { url.queryItems?.append(URLQueryItem(name: "path", value: path)) }
        do {
            let result: MacLocationPage = try await model.manage(url.string!)
            if more {
                guard result.path == page?.path else { return }
                let known = Set(entries.map(\.id)); entries += result.entries.filter { !known.contains($0.id) }
            } else { entries = result.entries }
            page = result
            let previous = more ? nextOffset : nil
            nextOffset = result.nextOffset == previous ? nil : result.nextOffset
        } catch is CancellationError {
            return
        } catch {
            guard !Task.isCancelled else { return }
            if case PairingFailure.response(let status) = error {
                switch status {
                case 401: failure = "This device’s access has ended. Reconnect in Settings."
                case 403: failure = "This location is protected or unavailable to Cupola. Choose another folder."
                case 404: failure = "This folder moved or was removed. Go up or return Home to choose another location."
                case 400, 422: failure = "This location cannot be opened as a folder. Choose another location."
                default: failure = "Your computer could not open this folder. Try again or choose another location."
                }
            } else { failure = "Your computer could not be reached. Check its connection and try again." }
        }
    }
}
