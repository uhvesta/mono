import XCTest
@testable import Boss

/// Phase 5 #15 of the merge-conflict design: the
/// `"✓ conflict cleared"` PR-card chip appears on cards whose PR was
/// the target of a successful conflict-resolution attempt in the last
/// 24 hours and ages out beyond that window. The view-model owns the
/// state map; the kanban card view binds to
/// `ChatViewModel.showsConflictClearedBadge(forPR:)` for the chip
/// decision. These tests exercise the model in isolation (the chip
/// rendering is a thin presentational wrapper) — the design's
/// acceptance criterion ("visual regression test on a card with a
/// 6h-old vs 36h-old successful attempt") is met by feeding both
/// timestamps through the model and asserting the boolean flips.
@MainActor
final class ConflictClearedBadgeTests: XCTestCase {
    func testBadgeAbsentByDefault() {
        let model = makeModel()
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: "https://github.com/x/y/pull/1"))
    }

    func testSucceededEventRecordsRecentlyClearedPR() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/42"

        model.applyEventForTest(.conflictResolutionSucceeded(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_1",
            prURL: prURL
        ))

        XCTAssertTrue(model.showsConflictClearedBadge(forPR: prURL))
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: "https://github.com/x/y/pull/99"))
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: nil))
    }

    /// 6h-old success: chip stays visible. 36h-old success: chip ages
    /// out. Together these are the visual-regression cases the design
    /// pins down for #15.
    func testBadgeAgesOutAfterFreshnessWindow() {
        let model = makeModel()
        let recentPR = "https://github.com/x/y/pull/6h"
        let stalePR = "https://github.com/x/y/pull/36h"
        let now = Date()

        // 6 hours ago — well inside the 24h window.
        model.recentlyClearedConflictPRs[recentPR] = now.addingTimeInterval(-6 * 60 * 60)
        // 36 hours ago — outside.
        model.recentlyClearedConflictPRs[stalePR] = now.addingTimeInterval(-36 * 60 * 60)

        XCTAssertTrue(model.showsConflictClearedBadge(forPR: recentPR), "6h-old success should still show the chip")
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: stalePR), "36h-old success should have aged out")
    }

    /// Other lifecycle events (start, failure, abandon) don't move the
    /// badge state — only `succeeded` registers a clear.
    func testFailureAndAbandonDoNotSetBadge() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/77"

        model.applyEventForTest(.conflictResolutionStarted(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_a",
            prURL: prURL
        ))
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: prURL))

        model.applyEventForTest(.conflictResolutionFailed(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_a",
            prURL: prURL,
            failureReason: "architectural_mismatch"
        ))
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: prURL))

        model.applyEventForTest(.conflictResolutionAbandoned(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_a",
            prURL: prURL,
            failureReason: "pr_closed"
        ))
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: prURL))
    }

    /// A subsequent success on the same PR refreshes the timestamp so
    /// the chip's 24h window restarts from the latest clear.
    func testSecondSuccessRefreshesTimestamp() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/123"

        // Plant an old, aged-out clear.
        model.recentlyClearedConflictPRs[prURL] = Date().addingTimeInterval(-48 * 60 * 60)
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: prURL))

        // A fresh success on the same PR brings the chip back.
        model.applyEventForTest(.conflictResolutionSucceeded(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_b",
            prURL: prURL
        ))
        XCTAssertTrue(model.showsConflictClearedBadge(forPR: prURL))
    }

    /// T778: once a PR is cleared, a fresh `conflictResolutionStarted`
    /// (re-conflict / resolver re-dispatched) must remove the badge so
    /// the card shows "resolving", not "cleared".
    func testReConflictClearsBadge() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/778"

        // 1. First resolution succeeds — badge should appear.
        model.applyEventForTest(.conflictResolutionSucceeded(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_1",
            prURL: prURL
        ))
        XCTAssertTrue(model.showsConflictClearedBadge(forPR: prURL), "badge should show after first success")

        // 2. PR re-conflicts and a new resolver is dispatched.
        model.applyEventForTest(.conflictResolutionStarted(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_2",
            prURL: prURL
        ))
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: prURL), "badge must be cleared when resolution restarts (T778)")
    }

    /// T778 corollary: after the badge is cleared by a re-conflict start,
    /// a new successful resolution brings it back.
    func testBadgeReappearsAfterNewSuccessFollowingReConflict() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/778b"

        model.applyEventForTest(.conflictResolutionSucceeded(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_1",
            prURL: prURL
        ))
        model.applyEventForTest(.conflictResolutionStarted(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_2",
            prURL: prURL
        ))
        XCTAssertFalse(model.showsConflictClearedBadge(forPR: prURL), "badge cleared by re-conflict")

        // New resolution succeeds — badge should reappear.
        model.applyEventForTest(.conflictResolutionSucceeded(
            productID: "prd_1",
            workItemID: "task_1",
            attemptID: "crz_2",
            prURL: prURL
        ))
        XCTAssertTrue(model.showsConflictClearedBadge(forPR: prURL), "badge must reappear after new successful resolution")
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }
}
