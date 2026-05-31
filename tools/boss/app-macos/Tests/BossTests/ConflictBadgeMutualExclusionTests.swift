import XCTest
@testable import Boss

/// Ensures the "Merge Conflict" blocked badge and the "conflict cleared"
/// badge are mutually exclusive. These are opposite states; showing both
/// simultaneously is always wrong (T795). The card view delegates the
/// decision to [[WorkBlockedBadge.conflictClearedVisible(forTask:cleared:isResolvingConflicts:)]];
/// these tests pin down every combination of (status, blockedReason, cleared,
/// isResolvingConflicts), including the contradictory/empty-signal case that
/// caused the live repro on T795.
final class ConflictBadgeMutualExclusionTests: XCTestCase {

    // MARK: - Core invariant: never both

    /// The live repro: status=blocked, blockedReason=merge_conflict, AND a
    /// recent cleared entry. Only the Merge Conflict badge should win.
    func testBlockedMergeConflictSuppressesConflictClearedBadge() {
        let task = makeTask(status: "blocked", blockedReason: "merge_conflict")
        XCTAssertFalse(
            WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false),
            "conflict cleared must be suppressed when the card shows an active Merge Conflict badge"
        )
    }

    /// Symmetric: cleared=true with no active conflict → badge shows.
    func testNoActiveConflictAllowsConflictClearedBadge() {
        let task = makeTask(status: "in_review", blockedReason: nil)
        XCTAssertTrue(
            WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false),
            "conflict cleared should show when no active merge-conflict block is present"
        )
    }

    // MARK: - cleared=false always hides the badge

    func testClearedFalseAlwaysHidesBadge_blockedMergeConflict() {
        let task = makeTask(status: "blocked", blockedReason: "merge_conflict")
        XCTAssertFalse(WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: false, isResolvingConflicts: false))
    }

    func testClearedFalseAlwaysHidesBadge_inReview() {
        let task = makeTask(status: "in_review", blockedReason: nil)
        XCTAssertFalse(WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: false, isResolvingConflicts: false))
    }

    // MARK: - isResolvingConflicts path

    /// While the engine is actively resolving the conflict the card shows
    /// "resolving conflicts" (not "Merge Conflict"), so the cleared badge
    /// is NOT suppressed by the active-conflict guard — but in practice
    /// the cleared badge is stale at that point and won't be set.
    func testResolvingConflictsDoesNotSuppressBadge() {
        let task = makeTask(status: "blocked", blockedReason: "merge_conflict")
        XCTAssertTrue(
            WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: true),
            "active resolution worker replaces the Merge Conflict badge, so suppression must not fire"
        )
    }

    // MARK: - Other blocked reasons do not suppress

    func testBlockedDependencyDoesNotSuppressConflictClearedBadge() {
        let task = makeTask(status: "blocked", blockedReason: "dependency")
        XCTAssertTrue(WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false))
    }

    func testBlockedCIFailureDoesNotSuppressConflictClearedBadge() {
        let task = makeTask(status: "blocked", blockedReason: "ci_failure")
        XCTAssertTrue(WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false))
    }

    func testBlockedNoReasonDoesNotSuppressConflictClearedBadge() {
        let task = makeTask(status: "blocked", blockedReason: nil)
        XCTAssertTrue(WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false))
    }

    // MARK: - Non-blocked statuses do not suppress

    func testInReviewWithMergeConflictReasonDoesNotSuppress() {
        // Stale blockedReason on a non-blocked task must not trigger suppression.
        let task = makeTask(status: "in_review", blockedReason: "merge_conflict")
        XCTAssertTrue(
            WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false),
            "stale blockedReason on non-blocked task must not suppress the cleared badge"
        )
    }

    func testActiveStatusDoesNotSuppress() {
        let task = makeTask(status: "active", blockedReason: "merge_conflict")
        XCTAssertTrue(WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false))
    }

    func testDoneStatusDoesNotSuppress() {
        let task = makeTask(status: "done", blockedReason: "merge_conflict")
        XCTAssertTrue(WorkBlockedBadge.conflictClearedVisible(forTask: task, cleared: true, isResolvingConflicts: false))
    }

    // MARK: - Helpers

    private func makeTask(
        status: String,
        blockedReason: String?,
        id: String = "task_\(UUID().uuidString)"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test chore",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/x/y/pull/795",
            deletedAt: nil,
            createdAt: "2026-05-31T00:00:00Z",
            updatedAt: "2026-05-31T00:00:00Z",
            blockedReason: blockedReason
        )
    }
}
