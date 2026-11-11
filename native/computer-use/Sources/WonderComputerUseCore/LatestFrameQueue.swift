import Foundation

public final class LatestFrameQueue<Element>: @unchecked Sendable {
    public struct Statistics: Equatable, Sendable {
        public let count: Int
        public let droppedCount: UInt64
    }

    private let capacity: Int
    private var elements: [Element] = []
    private var droppedCount: UInt64 = 0
    private let lock = NSLock()

    public init(capacity: Int = 2) {
        self.capacity = max(1, min(capacity, 8))
    }

    @discardableResult
    public func offer(_ element: Element) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        let dropped = elements.count >= capacity
        if dropped {
            elements.removeFirst()
            droppedCount &+= 1
        }
        elements.append(element)
        return dropped
    }

    public func takeLatest() -> Element? {
        lock.lock()
        defer { lock.unlock() }
        guard let element = elements.last else { return nil }
        elements.removeAll(keepingCapacity: true)
        return element
    }

    public func statistics() -> Statistics {
        lock.lock()
        defer { lock.unlock() }
        return Statistics(count: elements.count, droppedCount: droppedCount)
    }

    public func clear() {
        lock.lock()
        elements.removeAll(keepingCapacity: true)
        lock.unlock()
    }

    public func reset() {
        lock.lock()
        elements.removeAll(keepingCapacity: true)
        droppedCount = 0
        lock.unlock()
    }
}
