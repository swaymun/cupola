import SwiftUI
import WonderPairing

struct DiffCommentContext {
    let projectID: String
    let conversationID: String
    let assignmentScope: String
}

/// The same immutable annotation attachment used by Files, with an explicit
/// patch anchor. This sheet never sends a message or rewrites selected text.
struct DiffCommentEditor: View {
    let source: DiffAnnotationSource
    let context: DiffCommentContext
    let path: String
    let comparison: WorkspaceChangeScope
    let originalPath: String?
    let stage: (ArtifactAnnotation) throws -> Void
    @State private var selection: ClosedRange<Int>?
    @State private var note = ""
    @State private var failure: String?
    @Environment(\.dismiss) private var dismiss
    @Environment(\.colorScheme) private var scheme

    private var canAdd: Bool {
        selection.map(source.validSelection) == true && !note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && note.utf8.count <= 4096
    }
    var body: some View {
        NavigationStack {
            WonderList {
                Section {
                    TextField("Tell the agent what to change", text: $note, axis: .vertical)
                        .lineLimit(2...5).accessibilityIdentifier("diff-comment-note")
                    if note.utf8.count > 4096 { Text("Keep the comment under 4,096 bytes.").foregroundStyle(.red) }
                    if let failure { Text(failure).foregroundStyle(.red).accessibilityIdentifier("diff-comment-error") }
                }
                Section {
                    ForEach(source.lines) { line in
                        Button { select(line.id) } label: {
                            HStack(alignment: .top, spacing: 8) {
                                Image(systemName: selection?.contains(line.id) == true ? "checkmark.circle.fill" : "circle")
                                    .foregroundStyle(selection?.contains(line.id) == true ? Color.accentColor : Color.secondary)
                                VStack(alignment: .leading, spacing: 3) {
                                    Text(line.label).font(.caption).foregroundStyle(.secondary)
                                    Text(String(line.sign) + " " + line.preview).font(.system(.footnote, design: .monospaced))
                                        .foregroundStyle(line.sign == "+" ? DiffColors.added(scheme) : line.sign == "-" ? DiffColors.removed(scheme) : .primary)
                                        .frame(maxWidth: .infinity, alignment: .leading)
                                }
                            }
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel(line.label + ", " + line.preview)
                        .accessibilityValue(selection?.contains(line.id) == true ? "Selected" : "Not selected")
                        .accessibilityIdentifier("diff-comment-line:\(line.id)")
                    }
                } header: { Text(path).textCase(nil) }
                footer: { Text("Tap the first line, then the last to select a range. Up to 100 lines in one group.") }
            }
            .navigationTitle("Comment on changes")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add") { add() }.disabled(!canAdd).accessibilityIdentifier("diff-comment-add")
                }
                ToolbarItem(placement: .bottomBar) {
                    Button("Clear selection") { selection = nil; failure = nil }
                        .disabled(selection == nil).accessibilityIdentifier("diff-comment-clear")
                }
            }
        }
    }
    private func select(_ line: Int) {
        failure = nil
        guard source.validSelection(line...line) else {
            failure = "This line is too long to comment on here. Choose a shorter line."; return
        }
        if let selected = selection, selected.lowerBound == selected.upperBound, selected.lowerBound != line {
            let range = min(line, selected.lowerBound)...max(line, selected.upperBound)
            guard source.validSelection(range) else {
                failure = "Choose up to 100 neighboring lines from the same group."; return
            }
            selection = range
        } else { selection = line...line }
    }
    private func add() {
        guard let selection else { return }
        do {
            let annotation = try ArtifactAnnotation(projectId: context.projectID, conversationId: context.conversationID,
                rootId: "workspace", path: path, patch: source, scope: comparison, originalPath: originalPath,
                startLine: selection.lowerBound, endLine: selection.upperBound, note: note)
            try stage(annotation)
            dismiss()
        } catch {
            failure = "This comment could not be added. Check that the chat is connected and has room for another attachment."
        }
    }
}
