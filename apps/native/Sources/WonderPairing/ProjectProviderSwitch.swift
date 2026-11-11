import Foundation

/// The model one message is sent with. For a Project thread it may belong to
/// the other provider; the host then moves the thread to that provider when the
/// message is delivered, in order and without stopping work that is running.
public struct ProjectMessageModel: Codable, Hashable, Sendable {
    public var family: AgentFamily
    public var model: String
    public var effort: String?
    public var serviceTier: String?

    public init(family: AgentFamily, model: String, effort: String? = nil, serviceTier: String? = nil) {
        self.family = family
        self.model = model
        self.effort = effort
        self.serviceTier = serviceTier
    }
}

/// How the composer treats a model chosen for a Project thread. The picker
/// is New chat's: the harness first, then that harness's models.
public enum ProjectModelPicker {
    /// Lists longer than this offer a search field.
    public static let searchThreshold = 8

    /// Models whose name contains every word of the query, ignoring case and
    /// accents; a provider's name ("Cursor") matches all of its models. IDs are
    /// not searched: Cursor's carry options such as "fast=true".
    public static func matching(_ models: [AgentOptions.Model], query: String) -> [AgentOptions.Model] {
        let terms = query.split(whereSeparator: \.isWhitespace).map(String.init)
        guard !terms.isEmpty else { return models }
        return models.filter { model in
            let fields = [model.displayName, model.family.title]
            return terms.allSatisfy { term in
                fields.contains { $0.range(of: term, options: [.caseInsensitive, .diacriticInsensitive]) != nil }
            }
        }
    }

    public enum Choice: Equatable, Sendable {
        /// Same provider: saved on the thread now, as before.
        case saveToThread
        /// Other provider: carried by the next message and applied on delivery.
        case sendWithNextMessage(ProjectMessageModel)
    }

    public static func choice(current: AgentFamily, option: AgentOptions.Model, effort: String?, serviceTier: String?) -> Choice {
        option.family == current
            ? .saveToThread
            : .sendWithNextMessage(ProjectMessageModel(family: option.family, model: option.id, effort: effort, serviceTier: serviceTier))
    }

    /// Choosing a harness in a thread. Its own harness drops a carried
    /// choice (nil); another one starts from the model last chosen for it,
    /// else its default, and travels with the next message. A harness with no
    /// listed models carries nothing.
    public static func carried(choosing family: AgentFamily, thread: AgentFamily,
                               models: [AgentOptions.Model], remembered: RememberedModel?) -> ProjectMessageModel? {
        guard family != thread else { return nil }
        let own = models.filter { $0.family == family && !$0.hidden }
        guard let option = own.first(where: { $0.id == remembered?.model }) ?? ModelDefaults.defaultModel(in: own) else { return nil }
        return ProjectMessageModel(family: family, model: option.id, effort: ModelDefaults.effort(remembered?.effort, for: option))
    }

    /// The access a thread will have once the next message moves it to
    /// another harness, as the host applies it: the access level where that
    /// harness offers access choices, plan mode where it has one, and that
    /// harness's own starting approval mode.
    public static func access(_ current: ProjectAccess, movingTo target: ProviderCapabilities) -> ProjectAccess {
        ProjectAccess(accessMode: target.accessChoices ? current.accessMode : .workspace, claudeApproval: nil,
                      planMode: current.planMode && target.plan != .unavailable)
    }

    /// Whether a choice from the next harness's access menu can be saved on
    /// the thread before it moves: only what the move keeps (access level and
    /// plan mode), and only what the current harness stores. An approval mode
    /// other than the next harness's starting one waits for the move.
    public static func canSaveBeforeMove(_ next: ProjectAccess, shown: ProjectAccess, current: ProviderCapabilities,
                                         target: ProviderCapabilities, supportsModes: Bool) -> Bool {
        if target.allowsApprovalChoice, (next.claudeApproval ?? .standard) != (shown.claudeApproval ?? .standard) { return false }
        if next.accessMode != shown.accessMode, !current.accessChoices || !target.accessChoices { return false }
        if next.planMode != shown.planMode, !supportsModes || current.plan == .unavailable || target.plan == .unavailable { return false }
        return true
    }

    /// What to save on the thread for a choice made before the move: the
    /// kept fields from `next`, and the thread's own approval mode unchanged.
    public static func savedBeforeMove(_ next: ProjectAccess, thread: ProjectAccess) -> ProjectAccess {
        ProjectAccess(accessMode: next.accessMode, claudeApproval: thread.claudeApproval, planMode: next.planMode)
    }

    /// The composer's model button: the model's name, else its harness's
    /// while the catalog has not listed the model. Never a bare "Model".
    public static func title(model: AgentOptions.Model?, family: AgentFamily?) -> String {
        model?.displayName ?? (family ?? .codex).title // provider identity: default provider
    }

    /// A carried choice ends once the host has applied it to the thread.
    public static func isApplied(_ pending: ProjectMessageModel, to detail: ProjectConversationDetail?) -> Bool {
        guard let detail else { return false }
        return detail.family == pending.family && detail.model == pending.model
    }
}
