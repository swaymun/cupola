import Foundation

/// Pure presentation state for the outer Working disclosure.
///
/// The policy deliberately consumes lifecycle authority rather than item or
/// receipt state. A turn that is merely unknown is neither opened automatically
/// nor treated as terminal; search and an explicit user tap can still reveal it.
public struct ActivityDisclosurePolicy: Equatable, Sendable {
    public enum Lifecycle: String, Hashable, Sendable {
        case active
        case completed
        case interrupted
        case failed
        case unknown

        public var isActive: Bool { self == .active }
        public var isTerminal: Bool {
            switch self {
            case .completed, .interrupted, .failed: true
            case .active, .unknown: false
            }
        }
    }

    public struct Key: Hashable, Sendable {
        public let conversationID: String
        public let turnID: String
        public let entryID: String

        public init(conversationID: String, turnID: String, entryID: String) {
            self.conversationID = conversationID
            self.turnID = turnID
            self.entryID = entryID
        }
    }

    public struct Entry: Hashable, Sendable {
        public let key: Key
        public let lifecycle: Lifecycle
        /// Whether an active lifecycle is the current actionable turn, derived
        /// from ConversationSnapshot.
        public let autoOpenWhileActive: Bool

        public init(
            conversationID: String,
            turnID: String,
            entryID: String,
            lifecycle: Lifecycle,
            autoOpenWhileActive: Bool
        ) {
            self.key = Key(conversationID: conversationID, turnID: turnID, entryID: entryID)
            self.lifecycle = lifecycle
            self.autoOpenWhileActive = autoOpenWhileActive
        }
    }

    public struct State: Equatable, Sendable {
        fileprivate var collapsedWhileActive: Set<Key>
        fileprivate var expandedWhileHistorical: Set<Key>
        fileprivate var reopenedAfterTerminal: Set<Key>

        public init() {
            collapsedWhileActive = []
            expandedWhileHistorical = []
            reopenedAfterTerminal = []
        }
    }

    public static func lifecycle(for turn: ReadTurn?) -> Lifecycle {
        lifecycle(status: turn?.status)
    }

    public static func lifecycle(status: String?) -> Lifecycle {
        switch status {
        case "inProgress": return .active
        case "completed": return .completed
        case "interrupted": return .interrupted
        case "failed": return .failed
        default: return .unknown
        }
    }

    /// Remove overrides only for conversations/turns that the presentation
    /// owner no longer retains. A paged-out activity entry does not by itself
    /// discard an override for its still-retained turn.
    public static func reconciled(
        _ state: State,
        entries: [Entry],
        retainedConversationID: String? = nil,
        retainedTurnIDs: Set<String>? = nil
    ) -> State {
        var result = state
        func retained(_ key: Key) -> Bool {
            if let retainedConversationID, key.conversationID != retainedConversationID { return false }
            if let retainedTurnIDs, !retainedTurnIDs.contains(key.turnID) { return false }
            return true
        }
        result.collapsedWhileActive = result.collapsedWhileActive.filter(retained)
        result.expandedWhileHistorical = result.expandedWhileHistorical.filter(retained)
        result.reopenedAfterTerminal = result.reopenedAfterTerminal.filter(retained)

        // A terminal transition invalidates overrides made while the turn was
        // running. Reopened terminal work is kept so a deliberate inspection
        // remains open across later projections of the same completed turn.
        for entry in entries {
            if entry.lifecycle.isActive {
                if entry.autoOpenWhileActive {
                    result.expandedWhileHistorical.remove(entry.key)
                }
                result.reopenedAfterTerminal.remove(entry.key)
            } else if entry.lifecycle.isTerminal {
                result.collapsedWhileActive.remove(entry.key)
                result.expandedWhileHistorical.remove(entry.key)
            }
        }
        return result
    }

    public static func expandedEntryIDs(
        entries: [Entry],
        state: State,
        searchReveal: Set<Key> = []
    ) -> Set<String> {
        Set(entries.compactMap { entry in
            if searchReveal.contains(entry.key) { return entry.key.entryID }
            if entry.lifecycle.isActive {
                if entry.autoOpenWhileActive {
                    return state.collapsedWhileActive.contains(entry.key) ? nil : entry.key.entryID
                }
                return state.expandedWhileHistorical.contains(entry.key) ? entry.key.entryID : nil
            }
            return state.reopenedAfterTerminal.contains(entry.key) ? entry.key.entryID : nil
        })
    }

    public static func toggled(
        _ state: State,
        entry: Entry,
        isExpanded: Bool
    ) -> State {
        var result = state
        if entry.lifecycle.isActive {
            result.reopenedAfterTerminal.remove(entry.key)
            if entry.autoOpenWhileActive {
                result.expandedWhileHistorical.remove(entry.key)
                if isExpanded { result.collapsedWhileActive.insert(entry.key) }
                else { result.collapsedWhileActive.remove(entry.key) }
            } else {
                result.collapsedWhileActive.remove(entry.key)
                if isExpanded { result.expandedWhileHistorical.remove(entry.key) }
                else { result.expandedWhileHistorical.insert(entry.key) }
            }
        } else {
            result.collapsedWhileActive.remove(entry.key)
            result.expandedWhileHistorical.remove(entry.key)
            if isExpanded { result.reopenedAfterTerminal.remove(entry.key) }
            else { result.reopenedAfterTerminal.insert(entry.key) }
        }
        return result
    }
}
