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

    /// `UNUserNotificationCenter.current()` throws
    /// `NSInternalInconsistencyException` ("bundleProxyForCurrentProcess
    /// is nil") when the current process is not running inside a real
    /// `.app` bundle. Two no-bundle contexts hit this:
    ///
    /// 1. xctest, detected via the well-known `XCTestCase` class.
    /// 2. `swift run` builds, whose `Bundle.main.bundleURL` points at
    ///    `…/.build/<triple>/debug/` — a plain directory, not a
    ///    `.app` (so `pathExtension != "app"`).
    ///
    /// Returning false skips notification wiring on both.
    private var isBundleContextSafe: Bool {
        guard NSClassFromString("XCTestCase") == nil else { return false }
        return Bundle.main.bundleURL.pathExtension == "app"
    }

    func configure() {
        guard isBundleContextSafe else { return }
        UNUserNotificationCenter.current().delegate = self
    }

    func notifyReadyForReview(task: WorkTask) {
        guard isBundleContextSafe else { return }
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
