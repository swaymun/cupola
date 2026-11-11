import UserNotifications

final class NotificationService: UNNotificationServiceExtension {
    override func didReceive(_ request: UNNotificationRequest, withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void) {
        // All work is local and bounded. Never wait for a Mac/network connection.
        guard let payload = request.content.userInfo["wonderPush"] as? [String: String],
              let registration = payload["registrationId"],
              let key = try? PushPreviewKeys.read(registration),
              let preview = try? PushPreview.decrypt(payload, key: key) else {
            contentHandler(request.content); return
        }
        let content = preview.applying(to: request.content)
        guard preview.macDialogHost != nil else { contentHandler(content); return }
        // A newer count replaces the prior remote or local computer notice.
        // No network read or permission interaction happens in this extension.
        let center = UNUserNotificationCenter.current()
        center.getDeliveredNotifications { notifications in
            let previous = notifications.filter {
                $0.request.content.threadIdentifier == content.threadIdentifier
            }.map(\.request.identifier)
            center.removeDeliveredNotifications(withIdentifiers: previous)
            contentHandler(content)
        }
    }
}
