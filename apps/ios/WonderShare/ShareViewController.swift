import UIKit
import SwiftUI
import WonderPairing

/// Share sheet entry point. It sends with the paired phone's saved session over
/// the app's routes (pinned home address, Tailscale, the relay's sealed
/// connection) and never renews sessions or rewrites the app's saved
/// connections. Unconfirmed shares wait in the extension's own journal.
final class ShareViewController: UIViewController {
    override func viewDidLoad() {
        super.viewDidLoad()
        let items = extensionContext?.inputItems.compactMap { $0 as? NSExtensionItem } ?? []
        let model = ShareModel { [weak self] sent in
            guard let context = self?.extensionContext else { return }
            if sent { context.completeRequest(returningItems: nil) }
            else { context.cancelRequest(withError: CocoaError(.userCancelled)) }
        }
        // The app shares its theme choice through the Keychain group both use.
        let themeID = ((try? PhoneIdentity().read(ShareComposition.themeKeychainAccount)) ?? nil).flatMap { String(data: $0, encoding: .utf8) }
        let host = UIHostingController(rootView: ShareView(model: model, themeID: themeID))
        addChild(host)
        host.view.frame = view.bounds
        host.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(host.view)
        host.didMove(toParent: self)
        model.start(providers: items.flatMap { $0.attachments ?? [] })
    }
}

