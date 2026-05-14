import AppKit
import UserNotifications

/// Manages macOS system notifications for tasks that transition into the
/// Review lane. Fires a banner notification when a `WorkTask` reaches
/// `status == "in_review"` while the app is backgrounded, allowing the
/// user to see the event without keeping Boss.app in the foreground.
///
/// Authorization is requested lazily on the first qualifying transition
/// so the permission dialog appears in a meaningful context. The OS
/// persists the user's choice; subsequent launches skip the dialog.
///
/// Deduplication: each task gets one notification identifier
/// (`"boss.review:<id>"`). The OS replaces any pending notification for
/// the same identifier on re-delivery, so repeated engine events for the
/// same task don't stack up. The ChatViewModel additionally tracks which
/// task IDs are already known-in-review so the notification fires only on
/// genuine entry transitions.
@MainActor
final class ReviewNotificationCenter: NSObject {

    /// Called when the user taps a notification. Receives the work item id
    /// from the notification's userInfo. Set before any notifications fire.
    var onSelectWorkItem: ((String) -> Void)?

    private var hasRequestedAuthorization = false

    func configure() {
        // UNUserNotificationCenter.current() requires a proper app bundle
        // with a bundle proxy. It crashes in xctest and swift-run contexts
        // even when Bundle.main.bundleIdentifier is non-nil. Use the
        // well-known XCTestCase class presence as the guard.
        guard NSClassFromString("XCTestCase") == nil else { return }
        UNUserNotificationCenter.current().delegate = self
    }

    func notifyReadyForReview(task: WorkTask) {
        guard NSClassFromString("XCTestCase") == nil else { return }
        guard !NSApplication.shared.isActive else { return }

        let center = UNUserNotificationCenter.current()

        if !hasRequestedAuthorization {
            hasRequestedAuthorization = true
            Task {
                _ = try? await center.requestAuthorization(options: [.alert, .sound])
            }
        }

        let content = UNMutableNotificationContent()
        content.title = "Ready for Review"
        if let shortID = task.shortID {
            content.body = "#\(shortID): \(task.name)"
        } else {
            content.body = task.name
        }
        content.sound = .default
        content.userInfo = ["work_item_id": task.id]

        let identifier = "boss.review:\(task.id)"
        let request = UNNotificationRequest(identifier: identifier, content: content, trigger: nil)
        center.add(request) { _ in }
    }
}

extension ReviewNotificationCenter: UNUserNotificationCenterDelegate {
    /// Suppress banners when Boss.app is already in the foreground — the
    /// kanban card is visible and a system banner would be redundant.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([])
    }

    /// User tapped the notification: activate the app and navigate to the
    /// relevant work item in the kanban.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let workItemID = response.notification.request.content.userInfo["work_item_id"] as? String
        Task { @MainActor in
            NSApplication.shared.activate(ignoringOtherApps: true)
            if let workItemID {
                self.onSelectWorkItem?(workItemID)
            }
        }
        completionHandler()
    }
}
