import AVFoundation
import Foundation
import UniformTypeIdentifiers

/// Bridges AVFoundation's file-like reads to Wonder's authenticated, bounded
/// workspace ranges. Keep this object alive for the lifetime of its asset.
public final class WorkspaceMediaResourceLoader: NSObject, AVAssetResourceLoaderDelegate, @unchecked Sendable {
    private final class LoadingRequest: @unchecked Sendable {
        let value: AVAssetResourceLoadingRequest
        init(_ value: AVAssetResourceLoadingRequest) { self.value = value }
    }

    private let api: PairingAPI
    private let connection: SavedConnection
    private let path: String
    private let queue = DispatchQueue(label: "wonder.workspace.media.loader")
    private let lock = NSLock()
    private var tasks: [ObjectIdentifier: Task<Void, Never>] = [:]
    private var metadata: WorkspaceMediaChunk?
    private var closed = false
    private var stale = false

    /// A fenced range failed because the Mac file changed during this preview.
    public var fileChanged: Bool { lock.withLock { stale } }

    public init(api: PairingAPI, connection: SavedConnection, path: String) {
        self.api = api
        self.connection = connection
        self.path = path
    }

    public func asset(filename: String) throws -> AVURLAsset {
        var components = URLComponents()
        components.scheme = "wonder-media"
        components.host = "workspace"
        components.path = "/" + (filename.isEmpty ? "media" : filename)
        guard let url = components.url else { throw PairingFailure.invalidLink }
        let asset = AVURLAsset(url: url)
        asset.resourceLoader.setDelegate(self, queue: queue)
        return asset
    }

    public func resourceLoader(_ resourceLoader: AVAssetResourceLoader,
                               shouldWaitForLoadingOfRequestedResource loadingRequest: AVAssetResourceLoadingRequest) -> Bool {
        let request = LoadingRequest(loadingRequest)
        let id = ObjectIdentifier(loadingRequest)
        return lock.withLock {
            guard !closed else { return false }
            tasks[id] = Task { [self] in
                do {
                    try await serve(request.value)
                    if !Task.isCancelled { request.value.finishLoading() }
                } catch FileFailure.stale {
                    if !Task.isCancelled {
                        lock.withLock { stale = true }
                        if !request.value.isFinished { request.value.finishLoading(with: FileFailure.stale) }
                    }
                } catch {
                    if !Task.isCancelled && !request.value.isFinished {
                        request.value.finishLoading(with: error)
                    }
                }
                lock.withLock { _ = tasks.removeValue(forKey: id) }
            }
            return true
        }
    }

    public func resourceLoader(_ resourceLoader: AVAssetResourceLoader,
                               didCancel loadingRequest: AVAssetResourceLoadingRequest) {
        let task = lock.withLock { tasks.removeValue(forKey: ObjectIdentifier(loadingRequest)) }
        task?.cancel()
    }

    private func serve(_ request: AVAssetResourceLoadingRequest) async throws {
        // Each seek can produce several AVFoundation loading requests. Reuse the
        // first one-byte probe; every subsequent range is still revision-fenced
        // by the Mac, so a changed file cannot be spliced into this asset.
        let first: WorkspaceMediaChunk
        if let cached = lock.withLock({ metadata }) {
            first = cached
        } else {
            let fetched = try await api.downloadWorkspaceMediaRange(path, connection: connection,
                                                                    start: 0, end: 0, revision: nil)
            let established = lock.withLock { () -> WorkspaceMediaChunk in
                if let metadata { return metadata }
                if !closed { metadata = fetched }
                return fetched
            }
            guard fetched.revision == established.revision else { throw FileFailure.stale }
            first = established
        }
        try Task.checkCancellation()
        guard first.total <= UInt64(Int64.max) else { throw FileFailure.tooLarge }
        if let info = request.contentInformationRequest {
            info.contentType = UTType(mimeType: first.mimeType)?.identifier
            info.contentLength = Int64(first.total)
            info.isByteRangeAccessSupported = true
        }
        guard let dataRequest = request.dataRequest else { return }
        guard dataRequest.currentOffset >= 0, dataRequest.requestedOffset >= 0 else {
            throw FileFailure.integrity
        }
        let offset = max(dataRequest.currentOffset, dataRequest.requestedOffset)
        guard UInt64(offset) < first.total else { throw FileFailure.integrity }
        var position = UInt64(offset)
        let requestedEnd: UInt64
        if dataRequest.requestsAllDataToEndOfResource {
            requestedEnd = first.total
        } else {
            guard dataRequest.requestedLength > 0 else { throw FileFailure.integrity }
            let (end, overflow) = UInt64(dataRequest.requestedOffset)
                .addingReportingOverflow(UInt64(dataRequest.requestedLength))
            requestedEnd = min(first.total, overflow ? UInt64.max : end)
        }
        while position < requestedEnd {
            try Task.checkCancellation()
            let end = min(requestedEnd - 1, position + 1024 * 1024 - 1)
            let chunk = try await api.downloadWorkspaceMediaRange(path, connection: connection,
                                                                  start: position, end: end,
                                                                  revision: first.revision)
            try Task.checkCancellation()
            dataRequest.respond(with: chunk.bytes)
            position = chunk.end + 1
        }
    }

    /// Close the preview's unstructured AVFoundation reads when its owner goes
    /// away. A loading task retains this delegate, so deinit alone is too late.
    public func cancelAll() {
        let pending = lock.withLock {
            closed = true
            let pending = Array(tasks.values)
            tasks.removeAll()
            metadata = nil
            return pending
        }
        pending.forEach { $0.cancel() }
    }

    deinit { cancelAll() }
}
