import Foundation
import WonderPairing

struct ProjectAccessChange {
    var desired: ProjectAccess
    var saving = true
    var reconciled = false
    var failure: String?
}

extension ConnectionModel {
    func approvalSettingsBlockSending(_ conversationID: String) -> Bool {
        guard let change = projectAccessChanges[conversationID] else { return false }
        return change.saving || !change.reconciled
    }

    func cancelApprovalSettings() {
        projectAccessTasks.values.forEach { $0.cancel() }
        projectAccessTasks = [:]
        projectAccessTokens = [:]
        projectAccessChanges = [:]
    }

    /// Access changes belong to the conversation, so leaving the picker never
    /// cancels a save or lets a message use settings the Mac has not confirmed.
    func setProjectAccess(_ access: ProjectAccess, conversationID: String) {
        guard connection != nil, !accessEnded, !previewMode,
              !savingComposerSettings.contains(conversationID),
              projects.details[conversationID] != nil else { return }
        projectAccessChanges[conversationID] = ProjectAccessChange(desired: access)
        guard projectAccessTasks[conversationID] == nil else { return }
        let token = UUID(), scope = assignmentScope
        projectAccessTokens[conversationID] = token
        projectAccessTasks[conversationID] = Task { [weak self] in
            await self?.saveProjectAccess(conversationID, scope: scope, token: token)
        }
    }

    private func accessSaveIsCurrent(_ id: String, scope: String, token: UUID) -> Bool {
        !Task.isCancelled && !accessEnded && assignmentScope == scope && projectAccessTokens[id] == token
    }

    private func saveProjectAccess(_ id: String, scope: String, token: UUID) async {
        defer {
            if projectAccessTokens[id] == token {
                projectAccessTasks[id] = nil
                projectAccessTokens[id] = nil
            }
        }
        while accessSaveIsCurrent(id, scope: scope, token: token), let change = projectAccessChanges[id] {
            let attempted = change.desired
            do {
                guard let detail = projects.details[id] else { throw PairingFailure.response(404) }
                let provider = projects.capabilities(detail.family)
                var fields: [String: Any] = [:]
                if provider.accessChoices { fields["accessMode"] = attempted.accessMode.rawValue }
                if projects.supportsModes {
                    fields["planMode"] = attempted.planMode
                    if provider.allowsApprovalChoice { fields["claudeApproval"] = (attempted.claudeApproval ?? .standard).rawValue }
                }
                try await projects.updateConversation(id, fields: fields)
                guard accessSaveIsCurrent(id, scope: scope, token: token) else { return }
                if projectAccessChanges[id]?.desired == attempted {
                    guard let confirmed = projects.details[id],
                          confirmed.access.changes(to: attempted, capabilities: projects.capabilities(confirmed.family), supportsModes: projects.supportsModes).isEmpty else {
                        throw PairingFailure.response(409)
                    }
                    projectAccessChanges[id] = nil
                    return
                }
            } catch {
                guard accessSaveIsCurrent(id, scope: scope, token: token) else { return }
                var reconciled = false
                do {
                    _ = try await projects.loadDetail(id)
                    reconciled = true
                } catch { /* Keep sending blocked until the Mac confirms access. */ }
                guard accessSaveIsCurrent(id, scope: scope, token: token), var latest = projectAccessChanges[id] else { return }
                if reconciled, let confirmed = projects.details[id],
                   confirmed.access.changes(to: latest.desired, capabilities: projects.capabilities(confirmed.family), supportsModes: projects.supportsModes).isEmpty {
                    projectAccessChanges[id] = nil
                    return
                }
                if latest.desired != attempted && reconciled { continue }
                latest.saving = false
                latest.reconciled = reconciled
                latest.failure = reconciled ? "Couldn’t save access. Try again." : "Couldn’t confirm access. Reconnect and try again."
                projectAccessChanges[id] = latest
                return
            }
        }
    }
}
