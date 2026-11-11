import SwiftUI
import LocalAuthentication
import Combine
import AVKit
import ImageIO
import PhotosUI
import WonderPairing
import WonderComputerView
import UniformTypeIdentifiers
import PDFKit
import WebKit
import VisionKit
import Photos

struct AttentionRow: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var model: ConnectionModel
    let request: AttentionRequest
    /// A review the agent opened by itself ignores decisions until this time, so a tap
    /// aimed at the timeline, keyboard or Send cannot land on Allow or Decline.
    var tapGuardUntil: Date? = nil
    @State private var answers: [String: String] = [:]
    @State private var questionIndex = 0
    @State private var choices: [PhoneApprovalChoice] = []
    @State private var compactChoices: [PhoneApprovalChoice] = []
    @State private var menuChoices: [PhoneApprovalChoice] = []
    @State private var fields: [PhoneApprovalField] = []
    @State private var permissionDetails: [String] = []
    @State private var unavailableReason: String?
    @State private var validChoices: Set<String> = []
    @State private var approvalTitle = "Permission requested"
    @State private var toolDetails: [String] = []
    @State private var approvalInput: String?
    @State private var inputPreview: String?
    @State private var hasLongInput = false
    @State private var computerDetail: String?
    @State private var inputExpanded = false
    /// The approval choice last sent, shown as selected like a question answer.
    @State private var sentChoice: String?
    var body: some View {
        if request.isSecretRequest { SecretRequestView(model: model, request: request, tapGuardUntil: tapGuardUntil) } else { reviewBody }
    }
    private var reviewBody: some View {
        GeometryReader { geometry in
            VStack(spacing: 0) {
                ScrollView {
                    requestContent
                        .modifier(ConversationColumn())
                        .padding(16)
                }
                .scrollDismissesKeyboard(.interactively)
                .accessibilityIdentifier("attention-review")
                if !request.isQuestion, model.savedDecisions[request.id] == nil {
                    VStack(alignment: .leading, spacing: 8) {
                        ViewThatFits(in: .vertical) {
                            approvalActions.fixedSize(horizontal: false, vertical: true)
                            compactApprovalActions.fixedSize(horizontal: false, vertical: true)
                        }
                        if model.resolving.contains(request.id) { ProgressView("Sending reply…") }
                    }
                    .modifier(ConversationColumn())
                    .padding(.horizontal, 16)
                    .padding(.vertical, 12)
                    // Leave a readable, scrollable request even at accessibility
                    // sizes. Extra choices move into a native menu when needed.
                    .frame(maxHeight: geometry.size.height * 0.7, alignment: .bottom)
                    .layoutPriority(1)
                    .background(theme.chrome)
                }
            }
        }
        .disabled(model.resolving.contains(request.id) || model.accessEnded)
        .task(id: request.id + request.actionNonce) {
            choices = request.phoneChoices
            let regular = choices.filter { !$0.isDestructive }
            if let first = regular.first(where: { ["accept", "allowTurn"].contains($0.id) }) ?? regular.first {
                let second = regular.first(where: { $0.id == "decline" && $0.id != first.id })
                    ?? regular.first(where: { $0.id != first.id })
                compactChoices = [first] + (second.map { [$0] } ?? [])
            } else { compactChoices = [] }
            let pinned = Set(compactChoices.map(\.id))
            menuChoices = choices.filter { !pinned.contains($0.id) }
            sentChoice = nil
            fields = request.elicitationFields
            permissionDetails = request.permissionDetails
            unavailableReason = request.unsupportedReason
            approvalTitle = request.toolActionTitle ?? request.approvalTitle
            toolDetails = request.toolApprovalDetails
            approvalInput = request.approvalInput
            if let approvalInput {
                let prefix = String(approvalInput.prefix(513))
                hasLongInput = prefix.count > 512 || prefix.split(separator: "\n", omittingEmptySubsequences: false).count > 4
                inputPreview = hasLongInput ? String(prefix.prefix(512)) + "…" : approvalInput
            } else {
                inputPreview = nil
                hasLongInput = false
            }
            computerDetail = request.computerAction?.detail
            inputExpanded = false
            answers = request.isQuestion ? model.answerDraft(request.id) : [:]
            for field in fields {
                if let value = field.defaultValue { answers[field.id] = value }
            }
            validateAnswers()
        }
        .onChange(of: answers) { _ in validateAnswers() }
    }

    private var requestContent: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let saved = model.savedDecisions[request.id] {
                Text(request.savedReplyNotice(saved)).fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("approval-saved-" + request.id)
                Button("Check saved reply") { guard !tapGuarded else { return }; Task { await model.resolve(request, decision: saved.decision, responseJSON: saved.responseJson, structuredDecision: saved.structuredDecision) } }
                    .buttonStyle(.bordered).tint(.primary).frame(minHeight: 44)
            } else if request.isQuestion {
                VStack(alignment: .leading, spacing: 8) {
                QuestionNavigation(index: $questionIndex, count: request.params.questions?.count ?? 0)
                ForEach(Array((request.params.questions ?? []).enumerated()).filter { $0.offset == questionIndex }, id: \.element.id) { _, question in
                    Text(question.question).font(.subheadline.weight(.semibold)).fixedSize(horizontal: false, vertical: true)
                    if question.multiSelect == true {
                        QuestionMultiChoice(question: question, draft: Binding(get: { answers[question.id] ?? "" }, set: { answers[question.id] = $0; save() }))
                    } else {
                    ForEach(question.options ?? [], id: \.label) { option in
                        ConversationChoice(title: option.label, detail: option.description, selected: answers[question.id] == option.label) {
                            answers[question.id] = option.label; save()
                        }.accessibilityIdentifier("question-option-\(question.id)-\(option.label)")
                    }
                    if question.isSecret == true {
                        SecureField("Type an answer", text: Binding(get: { answers[question.id] ?? "" }, set: { answers[question.id] = $0 }),
                                    prompt: Text("Type an answer").foregroundStyle(theme.secondaryText))
                            .textFieldStyle(.wonder).accessibilityLabel(question.question)
                    } else {
                        TextField("Type an answer", text: Binding(get: { answers[question.id] ?? "" }, set: { answers[question.id] = $0; save() }),
                                  prompt: Text("Type an answer").foregroundStyle(theme.secondaryText), axis: .vertical)
                            .textFieldStyle(.wonder).accessibilityLabel(question.question)
                    }
                    }
                }
                }.frame(maxWidth: .infinity, alignment: .leading)
                HStack {
                Button("Reply") { guard !tapGuarded else { return }; Task { await model.resolve(request, decision: "respond", answers: answers) } }
                    .buttonStyle(.wonderProminent)
                    .accessibilityIdentifier("question-reply")
                    .disabled(model.previewMode || (request.params.questions ?? []).contains { $0.answers(from: answers[$0.id] ?? "").isEmpty })
                if request.params.isBlocking == false {
                    Button("Skip") { guard !tapGuarded else { return }; Task { await model.resolve(request, decision: "skip", answers: [:]) } }.buttonStyle(.bordered).disabled(model.previewMode)
                }
                }.controlSize(.regular).frame(minHeight: 44)
            } else {
                if request.params.kind == "tool" {
                    Text(approvalTitle).font(.subheadline.weight(.semibold))
                        .accessibilityIdentifier("approval-title-" + request.id)
                    if let reason = request.params.reason { Text(reason) }
                    ForEach(Array(toolDetails.enumerated()), id: \.offset) { Text($0.element).font(.subheadline).textSelection(.enabled) }
                } else {
                    PreparedPermissionDetails(detail: computerDetail, reason: request.params.reason, command: inputPreview)
                }
                if let approvalInput {
                    if request.params.kind == "tool" || hasLongInput {
                        DisclosureGroup(request.params.kind == "tool" ? "Full details" : "Full input", isExpanded: $inputExpanded) {
                            Text(approvalInput).font(.subheadline.monospaced())
                                .frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled)
                        }
                        .accessibilityIdentifier("approval-input-" + request.id)
                    }
                }
                if let path = request.params.filePath ?? request.params.cwd {
                    Text(path).font(.caption).textSelection(.enabled)
                }
                if request.method == "item/fileChange/requestApproval" || !(request.params.changes ?? []).isEmpty {
                    if (request.params.changes ?? []).isEmpty {
                        Text("Changes not available").font(.subheadline).foregroundStyle(.secondary)
                            .accessibilityIdentifier("approval-changes-unavailable-" + request.id)
                    }
                    ForEach(request.params.changes ?? [], id: \.path) { change in
                        FileChangeDiffView(change: change, requestID: request.id)
                    }
                }
                if let message = request.params.message { Text(message) }
                ForEach(permissionDetails, id: \.self) { Text($0).font(.subheadline).textSelection(.enabled) }
                if let url = request.elicitationURL {
                    Link(destination: url) {
                        Label("Open " + (url.host ?? "service"), systemImage: "arrow.up.right.square")
                    }.buttonStyle(.bordered).tint(.primary).frame(minHeight: 44).accessibilityIdentifier("approval-service-link")
                    Text("Complete the request in your browser, then return here to continue.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                ForEach(fields) { field in
                    PhoneApprovalFieldView(field: field, answer: Binding(
                        get: { answers[field.id] ?? field.defaultValue ?? "" },
                        set: { answers[field.id] = $0 }
                    ))
                }
                if let reason = unavailableReason {
                    Text(reason).foregroundStyle(.secondary)
                }
            }
            if request.isQuestion || model.savedDecisions[request.id] != nil {
                deliveryFeedback
            } else if let error = model.attentionErrors[request.id] {
                FailureDetails(message: error, open: true)
            }
        }
    }

    private var approvalActions: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(choices.filter { !$0.isDestructive }) { choiceRow($0) }
            // Stop also ends the response, so it stays separate from the choices.
            ForEach(choices.filter(\.isDestructive)) { choice in
                VStack(alignment: .leading, spacing: 4) {
                    Button(role: .destructive) { send(choice) } label: { PreparedStopLabel(title: choice.title) }
                        .buttonStyle(.plain)
                        .disabled(model.previewMode || !validChoices.contains(choice.id))
                        .accessibilityHint(choice.detail ?? "")
                        .accessibilityIdentifier("approval-\(choice.id)-\(request.id)")
                    if let detail = choice.detail {
                        Text(detail).font(.caption).foregroundStyle(.secondary).accessibilityHidden(true)
                    }
                }
                .padding(.top, 8)
            }
        }
    }

    private var compactApprovalActions: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(compactChoices) { choiceRow($0) }
            if !menuChoices.isEmpty {
                Menu {
                    ForEach(menuChoices) { choice in
                        Button(role: choice.isDestructive ? .destructive : nil) { send(choice) } label: {
                            Text(choice.title)
                            if let detail = choice.detail { Text(detail) }
                        }
                        .disabled(model.previewMode || !validChoices.contains(choice.id))
                        .accessibilityIdentifier("approval-\(choice.id)-\(request.id)")
                    }
                } label: {
                    Label("More choices", systemImage: "ellipsis")
                        .font(.subheadline).frame(maxWidth: .infinity, minHeight: 44)
                }
                .accessibilityIdentifier("approval-more-" + request.id)
            }
        }
    }

    private func choiceRow(_ choice: PhoneApprovalChoice) -> some View {
        ConversationChoice(title: choice.title, detail: choice.detail, selected: sentChoice == choice.id ? true : nil,
                           emphasized: !request.isQuestion && choice.id == leadingChoiceID) {
            send(choice)
        }
        .disabled(model.previewMode || !validChoices.contains(choice.id))
        .accessibilityIdentifier("approval-\(choice.id)-\(request.id)")
    }

    @ViewBuilder private var deliveryFeedback: some View {
        if model.resolving.contains(request.id) { ProgressView("Sending reply…") }
        if let error = model.attentionErrors[request.id] { FailureDetails(message: error, open: true) }
    }

    /// Allow (once, or for this turn) leads; Decline, Cancel and the longer-lived grants stay quiet.
    private var leadingChoiceID: String? {
        choices.first { !$0.isDestructive && ["accept", "allowTurn"].contains($0.id) }?.id
    }

    private var tapGuarded: Bool { tapGuardUntil.map { Date() < $0 } ?? false }
    private func send(_ choice: PhoneApprovalChoice) {
        if tapGuarded { return }
        sentChoice = choice.id
        Task { await model.resolve(request, choice: choice, answers: answers) }
    }
    private func validateAnswers() {
        validChoices = Set(choices.filter { request.canSubmit(choice: $0, answers: answers) }.map(\.id))
    }
    /// Written after a short pause (`typeAnswerDraft`); a secret answer is never saved.
    private func save() { let secretIDs = Set((request.params.questions ?? []).filter { $0.isSecret == true }.map(\.id)); model.typeAnswerDraft(answers.filter { !secretIDs.contains($0.key) }, id: request.id) }
}

private struct QuestionMultiChoice: View {
    @Environment(\.wonderTheme) private var theme
    let question: ChatQuestion
    @Binding var draft: String
    @State private var selected: Set<String> = []
    @State private var other = ""
    @State private var savedDraft: String?
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(question.options ?? [], id: \.label) { option in
                ConversationChoice(title: option.label, detail: option.description, selected: selected.contains(option.label)) {
                    if selected.contains(option.label) { selected.remove(option.label) } else { selected.insert(option.label) }
                    save()
                }.accessibilityIdentifier("question-option-\(question.id)-\(option.label)")
            }
            TextField("Another answer", text: $other,
                      prompt: Text("Another answer").foregroundStyle(theme.secondaryText), axis: .vertical)
                .textFieldStyle(.wonder)
                .accessibilityIdentifier("question-other-" + question.id)
                .onChange(of: other) { _, _ in save() }
        }.task(id: question.id) { restore() }
            .onChange(of: draft) { _, value in if value != savedDraft { restore() } }
    }
    private func restore() {
        let values = question.answers(from: draft)
        let labels = Set((question.options ?? []).map(\.label))
        selected = Set(values.filter { labels.contains($0) })
        other = values.filter { !labels.contains($0) }.joined(separator: "\n")
    }
    private func save() {
        var values = (question.options ?? []).map(\.label).filter { selected.contains($0) }
        let freeText = other.trimmingCharacters(in: .whitespacesAndNewlines)
        if !freeText.isEmpty { values.append(freeText) }
        if let data = try? JSONEncoder().encode(values) {
            let value = String(decoding: data, as: UTF8.self)
            savedDraft = value
            draft = value
        }
    }
}

/// Fields stay in the conversation's existing scroll surface and use native
/// controls, so keyboard, VoiceOver and Dynamic Type behave like other replies.
private struct PhoneApprovalFieldView: View {
    @Environment(\.wonderTheme) private var theme
    let field: PhoneApprovalField
    @Binding var answer: String
    @State private var selected: Set<String> = []
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(field.title + (field.required ? " (required)" : "")).font(.subheadline.weight(.semibold))
            if let detail = field.detail { Text(detail).font(.caption).foregroundStyle(.secondary) }
            switch field.kind {
            case .boolean:
                Picker(field.title, selection: $answer) {
                    Text("Choose").tag("")
                    Text("Yes").tag("true")
                    Text("No").tag("false")
                }.pickerStyle(.menu)
            case .choice:
                Picker(field.title, selection: $answer) {
                    Text("Choose").tag("")
                    ForEach(field.options, id: \.self) { option in Text(field.optionTitles[option] ?? option).tag(option) }
                }.pickerStyle(.menu)
            case .multiChoice:
                ForEach(field.options, id: \.self) { option in
                    Toggle(field.optionTitles[option] ?? option, isOn: Binding(
                        get: { selected.contains(option) },
                        set: { checked in
                            var values = selected
                            if checked { values.insert(option) } else { values.remove(option) }
                            selected = values
                            if let data = try? JSONEncoder().encode(values.sorted()) { answer = String(decoding: data, as: UTF8.self) }
                        }
                    )).systemSwitch()
                }
            case .text, .number, .integer:
                // The same field style as question answers.
                if field.secret {
                    SecureField(field.title, text: $answer, prompt: Text(field.title).foregroundStyle(theme.secondaryText))
                        .textFieldStyle(.wonder)
                } else {
                    TextField(field.title, text: $answer, prompt: Text(field.title).foregroundStyle(theme.secondaryText), axis: .vertical)
                        .textFieldStyle(.wonder)
                        .keyboardType(field.kind == .text ? .default : .numbersAndPunctuation)
                }
            }
        }.accessibilityIdentifier("approval-field-" + field.id)
            .task(id: answer) {
                if field.kind == .multiChoice {
                    selected = Set((try? JSONDecoder().decode([String].self, from: Data(answer.utf8))) ?? [])
                }
            }
    }
}

struct NativeQuestionHistoryRow: View {
    let question: NativeQuestionPresentation
    let rowID: String
    let author: String
    @State private var expanded: Bool
    init(question: NativeQuestionPresentation, rowID: String, author: String) {
        self.question = question; self.rowID = rowID; self.author = author
        _expanded = State(initialValue: !question.isAnswered)
    }
    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            ForEach(question.questions.indices, id: \.self) { index in
                let prompt = question.questions[index]
                let answer = question.answers[index]
                VStack(alignment: .leading, spacing: 8) {
                    Text(prompt.title).font(.subheadline.weight(.semibold)).fixedSize(horizontal: false, vertical: true)
                    let options = prompt.options ?? []
                    ForEach(options.indices, id: \.self) { optionIndex in
                        let option = options[optionIndex]
                        Label(option, systemImage: answer == option ? "checkmark.circle.fill" : "circle")
                            .font(.subheadline)
                            .foregroundStyle(answer == option ? .primary : .secondary)
                            .accessibilityLabel(answer == option ? "Your answer: " + option + ", selected" : option)
                    }
                    if let answer, !(prompt.options ?? []).contains(answer) {
                        Text(answer).font(.subheadline).textSelection(.enabled).accessibilityLabel("Your answer: " + answer)
                    }
                }.padding(.vertical, 8)
            }
        } label: {
            let title = question.isAnswered ? "Answered question" : "Question"
            Text(title).accessibilityLabel(author + ": " + title)
        }
        .font(.footnote).padding(.horizontal, 12).padding(.vertical, 8)
        .accessibilityIdentifier("native-question-" + rowID)
        .accessibilityValue(expanded ? "Expanded" : "Collapsed")
    }
}

struct AsyncQuestionRow: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let question: AsyncQuestion
    /// A review the agent opened by itself ignores answers until this time.
    var tapGuardUntil: Date? = nil
    private var tapGuarded: Bool { tapGuardUntil.map { Date() < $0 } ?? false }
    @State private var answers: [String: String] = [:]
    @State private var questionIndex = 0
    @State private var showAnswer = false
    private var answerValidation: AsyncAnswerValidationError? {
        AsyncAnswerIntent(answers: question.questions.indices.map { answers[String($0)] ?? "" }, skip: false)
            .validationError(for: question)
    }
    var body: some View {
        // Only a pending question changes with time, once, at its deadline.
        TimelineView(AnswerDeadlineSchedule(deadlines: question.answerDeadline.map { [$0] } ?? [])) { context in
            VStack(alignment: .leading, spacing: 8) {
                if question.canAnswer(now: AnswerDeadlineSchedule.milliseconds(context.date)) {
                    VStack(alignment: .leading, spacing: 8) {
                    QuestionNavigation(index: $questionIndex, count: question.questions.count)
                    ForEach(Array(question.questions.enumerated()).filter { $0.offset == questionIndex }, id: \.offset) { index, prompt in
                        Text(prompt.title).font(.subheadline.weight(.semibold)).fixedSize(horizontal: false, vertical: true)
                        ForEach(prompt.options ?? [], id: \.self) { option in
                            ConversationChoice(title: option, selected: answers[String(index)] == option) { answers[String(index)] = option; save() }
                        }
                        TextField("Type an answer", text: Binding(get: { answers[String(index)] ?? "" }, set: { answers[String(index)] = $0; save() }),
                                  prompt: Text("Type an answer").foregroundStyle(theme.secondaryText), axis: .vertical)
                            .textFieldStyle(.wonder).accessibilityLabel(prompt.title)
                    }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                    if let error = answerValidation, error != .answerRequired {
                        Text(error.localizedDescription).font(.footnote).foregroundStyle(.secondary)
                    }
                    HStack {
                        Button("Reply") { guard !tapGuarded else { return }; Task { await model.replyAsync(question, chat: chat, answers: question.questions.indices.map { answers[String($0)] ?? "" }, skip: false) } }
                            .buttonStyle(.wonderProminent)
                            .disabled(model.previewMode || answerValidation != nil || model.savedAsyncReplies[question.id] != nil || model.retryableAsyncReplies.contains(question.id))
                        Button("Skip") { guard !tapGuarded else { return }; Task { await model.replyAsync(question, chat: chat, answers: [], skip: true) } }.buttonStyle(.bordered).disabled(model.previewMode || model.savedAsyncReplies[question.id] != nil || model.retryableAsyncReplies.contains(question.id))
                    }.controlSize(.regular).frame(minHeight: 44)
                } else {
                    DisclosureGroup(question.state == "answered" ? "Answered question" : question.state == "dismissed" ? "Skipped question" : "Expired question", isExpanded: $showAnswer) {
                        ForEach(Array(question.questions.enumerated()), id: \.offset) { index, prompt in
                            VStack(alignment: .leading, spacing: 8) {
                                Text(prompt.title).font(.subheadline.weight(.semibold))
                                let selected = question.response?.answers.indices.contains(index) == true ? question.response?.answers[index] : answers[String(index)]
                                ForEach(prompt.options ?? [], id: \.self) { option in
                                    ConversationChoice(title: option, selected: selected == option) {}.disabled(true)
                                }
                                if let selected, !(prompt.options ?? []).contains(selected) { Text(selected).font(.subheadline) }
                            }.padding(.vertical, 8)
                        }
                    }.font(.footnote).foregroundStyle(.secondary).padding(.horizontal, 12)

                }
                if let error = model.attentionErrors[question.id] {
                    FailureDetails(message: error)
                    if model.retryableAsyncReplies.contains(question.id) || model.savedAsyncReplies[question.id] != nil {
                        Button(model.retryableAsyncReplies.contains(question.id) ? "Check saved reply" : "Check status") {
                            guard !tapGuarded else { return }
                            Task { await model.replyAsync(question, chat: chat, answers: [], skip: true, retry: true) }
                        }.buttonStyle(.bordered).tint(.primary)
                    }
                }
                if let saved = model.savedAsyncReplies[question.id] {
                    DisclosureGroup("Saved reply") {
                        Text(saved.skip ? "Skip question" : saved.answers.joined(separator: "\n\n"))
                            .textSelection(.enabled)
                        if !saved.skip {
                            Button("Copy reply", systemImage: "doc.on.doc") {
                                UIPasteboard.general.string = saved.answers.joined(separator: "\n\n")
                            }
                        }
                    }
                }
                if model.resolving.contains(question.id) { ProgressView("Saving reply…") }
            }.frame(maxWidth: .infinity, alignment: .leading).padding(.vertical, 8).disabled(model.resolving.contains(question.id) || model.accessEnded)
        }.task { answers = model.answerDraft(question.id) }
    }
    private func save() { model.typeAnswerDraft(answers, id: question.id) }
}

/// A stable notice keeps questions reachable without replacing the message
/// draft. The sheet owns scrolling, including long forms at large text sizes.
/// A conversation's pending permission requests and questions, permissions
/// first because they block work. Subagent questions are included.
struct PendingAttention {
    let requests: [AttentionRequest]
    let questions: [(chat: ChatSummary, question: AsyncQuestion)]
    var count: Int { requests.count + questions.count }
    @MainActor init(model: ConnectionModel, chat: ChatSummary, now: Date) {
        let ms = UInt64(now.timeIntervalSince1970 * 1000)
        requests = model.requests(for: chat).sorted { !$0.isQuestion && $1.isQuestion }
        questions = (model.asyncQuestions[chat.id] ?? []).filter {
            $0.canAnswer(now: ms) || model.attentionErrors[$0.id] != nil
        }.map { (chat, $0) }
    }
    var isPermission: Bool { requests.first.map { !$0.isQuestion } ?? false }
    /// The pill's short name.
    private var isSecret: Bool { count == 1 && requests.first?.isSecretRequest == true }
    var label: String { count > 1 ? "\(count) requests" : isSecret ? "Secret" : isPermission ? "Permission" : "Question" }
    var symbol: String { isSecret ? "key" : isPermission ? "hand.raised" : "questionmark.bubble" }
}

/// Replaces the conversation, as Files does, while the owner answers pending
/// requests. It closes itself when nothing is left to answer.
struct AttentionReview: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    /// Room under a header that floats over the content (Duo, side-rail layouts),
    /// as the timeline leaves it.
    var headerClearance: CGFloat = 0
    /// Set when the agent opened this review by itself: decisions wait out this dwell.
    var tapGuardUntil: Date? = nil
    let close: () -> Void
    /// The request on screen, by id: arrivals and answers elsewhere never swap it under the finger.
    @State private var selectedID: String?
    @State private var lastIndex = 0
    var body: some View {
        TimelineView(AnswerDeadlineSchedule(questions: model.asyncQuestions[chat.id] ?? [])) { context in
            let pending = PendingAttention(model: model, chat: chat, now: AnswerDeadlineSchedule.now(context.date))
            let ids = pending.requests.map(\.id) + pending.questions.map(\.question.id)
            let index = selectedID.flatMap { ids.firstIndex(of: $0) } ?? min(lastIndex, max(0, pending.count - 1))
            let requestIndex = Binding<Int>(get: { index }, set: { selectedID = ids.indices.contains($0) ? ids[$0] : selectedID })
            VStack(spacing: 0) {
                Color.clear.frame(height: headerClearance)
                if pending.count > 1 {
                    QuestionNavigation(index: requestIndex, count: pending.count, noun: "request").padding(.horizontal, 16)
                }
                ForEach(Array(pending.requests.enumerated()).filter { $0.offset == index }, id: \.element.id) { _, request in
                    AttentionRow(model: model, request: request, tapGuardUntil: tapGuardUntil)
                }
                ForEach(Array(pending.questions.enumerated()).filter { $0.offset + pending.requests.count == index }, id: \.element.question.id) { _, item in
                    ScrollView {
                        AsyncQuestionRow(model: model, chat: item.chat, question: item.question, tapGuardUntil: tapGuardUntil)
                            .modifier(ConversationColumn())
                            .padding(16)
                    }
                    .scrollDismissesKeyboard(.interactively)
                    .accessibilityIdentifier("attention-review")
                }
            }
            .onChange(of: model.attentionRequestFocus, initial: true) { _, target in
                guard let target else { return }
                if ids.contains(target) { selectedID = target; model.attentionRequestFocus = nil }
            }
            .onChange(of: index, initial: true) { _, value in
                lastIndex = value
                if selectedID == nil || !ids.contains(selectedID!) { selectedID = ids.indices.contains(value) ? ids[value] : nil }
            }
            .onChange(of: pending.count, initial: true) { _, value in
                if value == 0 { close() }
            }
        }
    }
}

struct ConversationChoice: View {
    @Environment(\.wonderTheme) private var theme
    let title: String
    var detail: String? = nil
    var selected: Bool? = nil
    /// A permission request's leading Allow: filled solid like Send; the rest stay quiet chips.
    var emphasized = false
    let choose: () -> Void
    var body: some View {
        Button(action: choose) {
            PreparedChoiceLabel(title: title, detail: detail, selected: selected,
                                accent: theme.accent, background: theme.chip,
                                emphasized: emphasized, emphasisFill: theme.text, onAccent: theme.page)
        }.buttonStyle(.plain).accessibilityAddTraits(selected == true ? [.isSelected] : [])
    }
}

struct QuestionNavigation: View {
    @Binding var index: Int
    let count: Int
    /// What VoiceOver calls each page: a form's questions, or the requests in review.
    var noun = "question"
    var body: some View {
        if count > 1 {
            HStack {
                Text("\(index + 1) of \(count)").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button { index -= 1 } label: { Image(systemName: "chevron.left").frame(width: 44, height: 44) }
                    .disabled(index == 0).accessibilityLabel("Previous " + noun)
                Button { index += 1 } label: { Image(systemName: "chevron.right").frame(width: 44, height: 44) }
                    .disabled(index + 1 >= count).accessibilityLabel("Next " + noun)
            }.buttonStyle(.plain)
        }
    }
}

/// Updates once now and once at each answer deadline still ahead, instead of
/// every second: settled questions need no clock, and a pending one changes
/// only when it stops being answerable.
struct AnswerDeadlineSchedule: TimelineSchedule {
    let deadlines: [Date]
    init(deadlines: [Date]) { self.deadlines = deadlines.sorted() }
    init(questions: [AsyncQuestion]) { self.init(deadlines: questions.compactMap(\.answerDeadline)) }
    func entries(from startDate: Date, mode: TimelineScheduleMode) -> [Date] {
        [startDate] + deadlines.filter { $0 > startDate }
    }
    /// A view refreshed for another reason reads the real time, never an
    /// earlier entry's date, so expiry is never stale.
    static func now(_ entry: Date) -> Date { max(entry, Date()) }
    static func milliseconds(_ entry: Date) -> UInt64 { UInt64(now(entry).timeIntervalSince1970 * 1000) }
}


/// A cached projection of request publishers only. Presence never starts a read.
@MainActor final class AttentionInboxPresenter: ObservableObject {
    struct Row: Identifiable {
        let item: AttentionInboxItem
        /// nil while the computer has not answered with the chat's details yet.
        let title: String?
        let project: String?
        let computer: String
        let loaded: Bool
        var id: String { item.id }
    }
    @Published private(set) var rows: [Row] = []
    private var models: [ConnectionModel] = []
    private var subscriptions: [AnyCancellable] = []
    private var expiry: Task<Void, Never>?
    private var scheduled = false
    deinit { expiry?.cancel() }

    func configure(_ models: [ConnectionModel]) {
        guard self.models.map(ObjectIdentifier.init) != models.map(ObjectIdentifier.init) else { return }
        self.models = models
        subscriptions = models.flatMap { model in
            let changes: [AnyPublisher<Void, Never>] = [
                model.$attention.map { _ in }.eraseToAnyPublisher(),
                model.$asyncQuestions.map { _ in }.eraseToAnyPublisher(),
                model.$attentionErrors.map { _ in }.eraseToAnyPublisher(),
                model.$connection.map { _ in }.eraseToAnyPublisher(),
                model.$accessEnded.map { _ in }.eraseToAnyPublisher(),
                model.projects.objectWillChange.eraseToAnyPublisher()
            ]
            return changes.map { $0.sink { [weak self] in self?.schedule() } }
        }
        rebuild()
    }
    private func schedule() {
        guard !scheduled else { return }; scheduled = true
        Task { @MainActor [weak self] in
            self?.scheduled = false; self?.rebuild()
        }
    }
    /// Titles are final once the project list and every read thread list answered,
    /// or once the computer reported it cannot (so a failed read shows "Chat", not a skeleton).
    private static func titlesSettled(_ projects: ProjectLibrary) -> Bool {
        if projects.supportsProjects == false || projects.failure != nil { return true }
        guard projects.supportsProjects == true, !projects.loadingProjects else { return false }
        return projects.threads.values.allSatisfy { !$0.isLoading && ($0.hasLoaded || $0.failure != nil) }
    }
    private func rebuild() {
        expiry?.cancel()
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        let sources = models.filter { !$0.accessEnded }.compactMap { model -> AttentionInboxProjection.Computer? in
            guard let host = model.connection?.credential.hostInstallationId else { return nil }
            return .init(host: host, requests: model.attention, questions: model.asyncQuestions.values.flatMap { $0 },
                         failedReplies: Set(model.attentionErrors.keys))
        }
        rows = AttentionInboxProjection.items(sources, now: now).compactMap { item in
            guard let model = models.first(where: { $0.connection?.credential.hostInstallationId == item.host }) else { return nil }
            let detail = model.projects.details[item.conversation]
            let thread = model.projects.threads.flatMap { project, state in
                state.threads.filter { $0.conversationId == item.conversation }.map { (project, $0) }
            }.first
            let projectID = detail?.projectId ?? thread?.0
            return Row(item: item, title: detail?.title ?? thread?.1.title,
                       project: projectID.flatMap { model.projects.project($0)?.name },
                       computer: model.macName, loaded: Self.titlesSettled(model.projects))
        }
        let deadlines = sources.flatMap { source in
            source.requests.compactMap { $0.params.expiresAtMs } + source.questions.filter { $0.state == "pending" }.map(\.expiresAtMs)
        }.filter { $0 > now }
        if let next = deadlines.min() {
            expiry = Task { [weak self] in
                do { try await Task.sleep(for: .milliseconds(next - now + 1)) } catch { return }
                self?.rebuild()
            }
        }
    }
}

/// The sidebar bell. It appears only while requests wait (a dimmed bell that did nothing read as
/// broken) and opens `AttentionInboxView` in the sidebar, under the icon row.
struct AttentionInboxEntry: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var shell: ShellState
    @StateObject private var presenter = AttentionInboxPresenter()
    var body: some View {
        let count = presenter.rows.count
        // A stack, not a Group: the presenter loads from `.task`, which never runs on an empty Group.
        VStack(spacing: 0) {
            if count > 0 {
                Button { shell.attentionPresented.toggle() } label: {
                    Image(systemName: "bell").font(.body.weight(.medium)).frame(minWidth: 44, minHeight: 44)
                        .overlay(alignment: .topTrailing) {
                            // A small accent count, not an alarm: the bell already says something waits.
                            Text(count > 99 ? "99+" : "\(count)").monospacedDigit()
                                .font(.system(size: 10, weight: .semibold)).foregroundStyle(.white)
                                .padding(.horizontal, 4).frame(minWidth: 14, minHeight: 14)
                                .background(theme.accent, in: Capsule())
                                .offset(x: -4, y: 6)
                        }
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Needs you")
                .accessibilityIdentifier("sidebar-needs-you")
                .accessibilityValue("\(count) " + (count == 1 ? "request" : "requests"))
            }
        }
        .task(id: library.saved.connections.map { $0.credential.hostInstallationId }) {
            presenter.configure(library.saved.connections.map { library.model(for: $0) })
        }
    }
}

/// The waiting requests, in the sidebar surface under the bell and above Search. Empty while nothing waits.
struct AttentionInboxView: View {
    @Environment(\.dynamicTypeSize) private var typeSize
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var shell: ShellState
    @ScaledMetric(relativeTo: .body) private var rowHeight = 96
    @StateObject private var presenter = AttentionInboxPresenter()
    var body: some View {
        // A stack, not a Group: modifiers on an empty Group never run, and the presenter loads from `.task`.
        VStack(spacing: 0) {
            if !presenter.rows.isEmpty { requestList }
        }
        .task(id: library.saved.connections.map { $0.credential.hostInstallationId }) {
            presenter.configure(library.saved.connections.map { library.model(for: $0) })
        }
        // The last request being answered closes the list; a list not yet loaded stays pending.
        .onChange(of: presenter.rows.isEmpty) { _, empty in
            if empty { shell.attentionPresented = false }
        }
        // Voice or a link can open it with nothing waiting, which never changes `rows`: after the
        // lists had a moment to load, an empty one closes again instead of opening later by itself.
        .task(id: shell.attentionPresented) {
            guard shell.attentionPresented, presenter.rows.isEmpty else { return }
            try? await Task.sleep(for: .seconds(2))
            if !Task.isCancelled, presenter.rows.isEmpty { shell.attentionPresented = false }
        }
    }
    private func action(_ item: AttentionInboxItem) -> String {
        switch item.kind {
        case .approval: "Approve an action"
        case .question: "Answer a question"
        case .failedReply: "Reply didn’t send"
        case .finished: "Review finished work"
        case .failed: "Review a failed reply"
        }
    }
    private func icon(_ item: AttentionInboxItem) -> String {
        switch item.kind {
        case .approval: "hand.raised"
        case .failedReply, .failed: "exclamationmark.bubble"
        default: "questionmark.bubble"
        }
    }
    private var requestList: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(presenter.rows) { row in
                    Button { open(row.item) } label: {
                        let waiting = row.title == nil && !row.loaded
                        VStack(alignment: .leading, spacing: 4) {
                            Label(action(row.item), systemImage: icon(row.item))
                                .font(.body.weight(.semibold)).foregroundStyle(.primary).lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                            Text(row.title ?? (waiting ? "Loading chat title" : "Chat"))
                                .font(.subheadline).foregroundStyle(.primary).lineLimit(2)
                                .redacted(reason: waiting ? .placeholder : [])
                            Text([row.project, row.computer].compactMap { $0 }.joined(separator: " · "))
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                        .padding(.vertical, 8).padding(.horizontal, 20)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("needs-you:" + row.item.host + ":" + row.item.request)
                    Divider().padding(.leading, 20)
                }
            }
        }
        // As tall as its rows, scrolling past about four.
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxHeight: rowHeight * 3.5)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("needs-you-list")
    }
    private func open(_ item: AttentionInboxItem) {
        guard let saved = library.saved.connections.first(where: { $0.credential.hostInstallationId == item.host }) else { return }
        let model = library.model(for: saved)
        model.attentionRequestFocus = item.request
        model.attentionFocus = item.conversation
        shell.attentionPresented = false
        shell.open(host: item.host, conversation: item.conversation)
    }
}


/// An agent asked for a token. The value goes from this field to the owner's
/// computer and nowhere else: it is not drafted, saved, logged or kept after Save.
struct SecretRequestView: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var model: ConnectionModel
    let request: AttentionRequest
    var tapGuardUntil: Date? = nil
    @State private var value = ""
    @State private var expired = false
    @State private var authenticating = false
    @State private var authentication: LAContext?
    @State private var authenticationError: String?
    @Environment(\.scenePhase) private var scenePhase
    private var summary: SecretRequestSummary? { request.params.secretRequest }
    private var expiryMs: UInt64? { summary?.expiresAtMs }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let summary {
                Text(summary.label).font(.headline).accessibilityIdentifier("secret-label")
                Text(summary.purpose).fixedSize(horizontal: false, vertical: true)
                SecureField(summary.label, text: $value, prompt: Text(summary.label).foregroundStyle(theme.secondaryText))
                    .textFieldStyle(.wonder).autocorrectionDisabled().textInputAutocapitalization(.never)
                    .accessibilityIdentifier("secret-field")
                Text("Saved in this Project’s Keychain vault on your computer. The agent receives a private reference.")
                    .font(.caption).foregroundStyle(theme.secondaryText).fixedSize(horizontal: false, vertical: true)
            }
            if let authenticationError { Text(authenticationError).font(.caption).foregroundStyle(theme.secondaryText).accessibilityIdentifier("secret-auth-error") }
            if expired { Text("This request expired. The agent can ask again.").font(.caption).foregroundStyle(theme.secondaryText) }
            if let error = model.attentionErrors[request.id] { FailureDetails(message: error) }
            HStack {
                Button(expired ? "Dismiss" : "Cancel") { send(nil) }
                    .buttonStyle(.bordered).accessibilityIdentifier("secret-cancel").disabled(model.previewMode)
                Spacer(minLength: 12)
                if authenticating || model.resolving.contains(request.id) { ProgressView().accessibilityLabel("Saving secret") }
                Button("Save") { send(value) }
                    .buttonStyle(.wonderProminent).accessibilityIdentifier("secret-save")
                    .disabled(model.previewMode || expired || authenticating || value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }.controlSize(.regular).frame(minHeight: 44)
        }
        .modifier(ConversationColumn()).padding(16)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .disabled(model.resolving.contains(request.id) || model.accessEnded)
        .onDisappear { value = ""; authentication?.invalidate(); authentication = nil }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background { value = ""; authentication?.invalidate() }
        }
        .task(id: request.id) {
            guard let expiryMs else { return }
            while !Task.isCancelled {
                let now = UInt64(Date().timeIntervalSince1970 * 1000)
                if now >= expiryMs { expired = true; value = ""; return }
                try? await Task.sleep(for: .milliseconds(min(expiryMs - now, 30_000)))
            }
        }
    }
    private func send(_ sent: String?) {
        if let until = tapGuardUntil, Date() < until { return }
        value = ""; authenticationError = nil
        guard let sent else { authentication?.invalidate(); Task { await model.answerSecret(request, value: nil) }; return }
        let context = LAContext()
        authentication = context; authenticating = true
        Task { @MainActor in
            defer { authenticating = false; context.invalidate(); authentication = nil }
            do {
                guard try await context.evaluatePolicy(.deviceOwnerAuthentication,
                    localizedReason: "Save this secret for your Project’s agent."), !Task.isCancelled, !expired,
                    scenePhase != .background else { return }
                await model.answerSecret(request, value: sent)
            } catch {
                authenticationError = "Confirm with Face ID, Touch ID or your passcode to save. Enter the value again to retry."
            }
        }
    }
}


/// One proposed file change: its path and the diff the host sent, so an edit is never approved blind.
private struct FileChangeDiffView: View {
    let change: RequestedFileChange
    let requestID: String
    @State private var expanded = false
    private static let collapsedLines = 12

    private var lines: [Substring] { (change.diff ?? "").split(separator: "\n", omittingEmptySubsequences: false) }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(change.path).font(.caption).textSelection(.enabled)
            if let diff = change.diff, !diff.isEmpty {
                let long = lines.count > Self.collapsedLines
                let shown = expanded || !long ? lines : Array(lines.prefix(Self.collapsedLines))
                Text(Self.attributed(shown))
                    .font(.caption.monospaced())
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
                    .accessibilityIdentifier("approval-diff-" + requestID)
                if long {
                    Button(expanded ? "Show less" : "Show all \(lines.count) lines") { expanded.toggle() }
                        .font(.caption).frame(minHeight: 44, alignment: .leading)
                        .accessibilityIdentifier("approval-diff-toggle-" + requestID)
                }
            } else {
                Text(change.truncated == true ? "Too large to review here" : "Changes not available")
                    .font(.subheadline).foregroundStyle(.secondary)
                    .accessibilityIdentifier("approval-changes-unavailable-" + requestID)
            }
        }
    }

    private static func attributed(_ lines: [Substring]) -> AttributedString {
        var out = AttributedString()
        for (i, line) in lines.enumerated() {
            var part = AttributedString(String(line) + (i < lines.count - 1 ? "\n" : ""))
            part.foregroundColor = color(line)
            out += part
        }
        return out
    }

    private static func color(_ line: Substring) -> Color {
        if line.hasPrefix("+") && !line.hasPrefix("+++") { return .green }
        if line.hasPrefix("-") && !line.hasPrefix("---") { return .red }
        return .secondary
    }
}
