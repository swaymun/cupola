import UIKit
import UserNotifications
import UserNotificationsUI

final class NotificationViewController: UIViewController, UNNotificationContentExtension, UITableViewDataSource, UITableViewDelegate {
    private let prompt = UILabel()
    private let status = UILabel()
    private let promptHeader = UIStackView()
    private let table = UITableView(frame: .zero, style: .plain)
    private var payload: [String: String]?
    private var options: [PushPreview.QuestionOption] = []

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .secondarySystemBackground
        prompt.numberOfLines = 0
        prompt.font = .preferredFont(forTextStyle: .body)
        prompt.adjustsFontForContentSizeCategory = true
        status.numberOfLines = 0
        status.font = .preferredFont(forTextStyle: .footnote)
        status.adjustsFontForContentSizeCategory = true
        status.textColor = .secondaryLabel
        status.isHidden = true
        table.dataSource = self
        table.delegate = self
        table.backgroundColor = .clear
        table.rowHeight = UITableView.automaticDimension
        table.estimatedRowHeight = 64
        table.accessibilityIdentifier = "notification-question-options"
        promptHeader.axis = .vertical
        promptHeader.addArrangedSubview(prompt)
        promptHeader.isLayoutMarginsRelativeArrangement = true
        promptHeader.layoutMargins = UIEdgeInsets(top: 0, left: 0, bottom: 12, right: 0)
        table.tableHeaderView = promptHeader
        let stack = UIStackView(arrangedSubviews: [status, table])
        stack.axis = .vertical
        stack.spacing = 12
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -16),
            stack.topAnchor.constraint(equalTo: view.topAnchor, constant: 16),
            stack.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -8)
        ])
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        let width = table.bounds.width
        guard width > 0 else { return }
        let size = promptHeader.systemLayoutSizeFitting(
            CGSize(width: width, height: 0),
            withHorizontalFittingPriority: .required,
            verticalFittingPriority: .fittingSizeLevel
        )
        if promptHeader.frame.width != width || abs(promptHeader.frame.height - size.height) > 0.5 {
            promptHeader.frame = CGRect(x: 0, y: 0, width: width, height: size.height)
            table.tableHeaderView = promptHeader
        }
    }

    func didReceive(_ notification: UNNotification) {
        loadViewIfNeeded()
        payload = nil
        options = []
        // iOS may reuse this view; this view's title replaces the default one,
        // so every notification, including one that can't be read, sets its own.
        title = notification.request.content.title.isEmpty ? "Cupola" : notification.request.content.title
        table.isUserInteractionEnabled = true
        status.isHidden = true
        guard let payload = notification.request.content.userInfo["wonderPush"] as? [String: String],
              let registration = payload["registrationId"], let key = try? PushPreviewKeys.read(registration),
              let preview = try? PushPreview.decrypt(payload, key: key), preview.action?.questionId != nil,
              let options = preview.action?.questionOptions, !options.isEmpty else {
            prompt.text = "Open Cupola to review this question."
            extensionContext?.notificationActions = []
            table.reloadData()
            preferredContentSize = CGSize(width: 360, height: 100)
            view.setNeedsLayout()
            return
        }
        self.payload = payload
        self.options = options
        table.allowsSelection = options.count > 4
        title = preview.title
        prompt.text = preview.body
        extensionContext?.notificationActions = preview.questionActions
        table.reloadData()
        preferredContentSize = CGSize(width: 360, height: min(520, 100 + options.count * 72))
        view.setNeedsLayout()
    }

    func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { options.count }
    func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let cell = tableView.dequeueReusableCell(withIdentifier: "option") ?? UITableViewCell(style: .subtitle, reuseIdentifier: "option")
        let option = options[indexPath.row]
        var content = cell.defaultContentConfiguration()
        content.text = option.label
        content.secondaryText = option.description
        content.textProperties.font = .preferredFont(forTextStyle: .body)
        content.textProperties.numberOfLines = 0
        content.secondaryTextProperties.font = .preferredFont(forTextStyle: .footnote)
        content.secondaryTextProperties.numberOfLines = 0
        cell.contentConfiguration = content
        cell.backgroundColor = .clear
        cell.accessoryType = options.count > 4 ? .disclosureIndicator : .none
        cell.isAccessibilityElement = true
        cell.accessibilityLabel = [option.label, option.description].compactMap { $0 }.joined(separator: ". ")
        cell.accessibilityHint = options.count > 4 ? "Answer this question in Cupola." : nil
        cell.accessibilityTraits = options.count > 4 ? .button : .staticText
        cell.accessibilityIdentifier = "notification-question-option-\(indexPath.row)"
        return cell
    }
    func tableView(_ tableView: UITableView, didSelectRowAt indexPath: IndexPath) {
        guard let payload, options.count > 4, options.indices.contains(indexPath.row) else { return }
        do {
            try PushQuestionSelection.save(indexPath.row, payload: payload)
            table.isUserInteractionEnabled = false
            extensionContext?.performNotificationDefaultAction()
        } catch {
            table.deselectRow(at: indexPath, animated: false)
            status.text = "Your answer could not be saved. Open Cupola to reply."
            status.isHidden = false
            view.setNeedsLayout()
            UIAccessibility.post(notification: .announcement, argument: status.text)
        }
    }

    nonisolated func didReceive(_ response: UNNotificationResponse, completionHandler completion: @escaping (UNNotificationContentExtensionResponseOption) -> Void) {
        completion(.dismissAndForwardAction)
    }
}
