import XCTest
@testable import Boss

/// Covers the kanban lane rules for review-phase blocked tasks:
///
/// • `blocked: merge_conflict` / `ci_failure` / `ci_failure_exhausted` /
///   `review_feedback` → **Review** column (task has an open PR; block is
///   transient). Card shows the reason badge so the state is legible.
/// • When an active resolution/remediation worker is running → **Doing**
///   (the engine is actively touching it right now).
/// • `blocked: dependency` or `blocked` with no reason → **Backlog**.
///
/// Tests exercise both the `effectiveBoardColumn(for:)` routing helper and
/// the `workItems(in:)` integration so the card container and the column
/// list stay in sync.
@MainActor
final class ConflictResolutionKanbanTests: XCTestCase {

    // MARK: effectiveBoardColumn routing — active worker → Doing

    func testBlockedMergeConflictWithPendingResolutionRoutesToDoing() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "pending")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
    }

    func testBlockedMergeConflictWithRunningResolutionRoutesToDoing() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "running")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
    }

    // MARK: effectiveBoardColumn routing — no active worker → Review

    func testBlockedMergeConflictWithNoResolutionRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = []
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedMergeConflictWithSucceededResolutionRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "succeeded")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedMergeConflictWithFailedResolutionRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "failed")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedCIFailureRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "ci_failure", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedCIFailureExhaustedRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "ci_failure_exhausted", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedReviewFeedbackRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "review_feedback", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    // MARK: effectiveBoardColumn routing — non-review blocked → Backlog

    func testPlainBlockedRowStaysInBacklog() {
        let model = makeModel()
        // blocked without a reason
        let task = makeTask(blockedReason: nil, attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
    }

    func testBlockedDependencyStaysInBacklog() {
        let model = makeModel()
        let task = makeTask(blockedReason: "dependency", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
    }

    // MARK: activeConflictResolution lookup

    func testActiveConflictResolutionReturnsPendingAttempt() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_42", workItemID: "task_1", status: "pending")
        model.conflictResolutions = [resolution]
        XCTAssertEqual(model.activeConflictResolution(for: "task_1")?.id, "crz_42")
    }

    func testActiveConflictResolutionReturnsRunningAttempt() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_99", workItemID: "task_1", status: "running")
        model.conflictResolutions = [resolution]
        XCTAssertEqual(model.activeConflictResolution(for: "task_1")?.id, "crz_99")
    }

    func testActiveConflictResolutionNilForFinishedAttempt() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_7", workItemID: "task_1", status: "succeeded")
        model.conflictResolutions = [resolution]
        XCTAssertNil(model.activeConflictResolution(for: "task_1"))
    }

    func testActiveConflictResolutionNilForWrongTask() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_5", workItemID: "task_other", status: "running")
        model.conflictResolutions = [resolution]
        XCTAssertNil(model.activeConflictResolution(for: "task_1"))
    }

    // MARK: workItems(in:) integration

    func testTaskWithActiveResolutionAppearsInDoingColumn() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.choresByProductID = ["prod_test": [task]]
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "running")]

        let doingItems = model.workItems(in: .doing)
        XCTAssertTrue(doingItems.contains(where: { $0.id == task.id }),
                      "task with active resolution should appear in Doing")

        let backlogItems = model.workItems(in: .backlog)
        XCTAssertFalse(backlogItems.contains(where: { $0.id == task.id }),
                       "task with active resolution must NOT appear in Backlog")
    }

    func testTaskWithFinishedResolutionStaysInReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.choresByProductID = ["prod_test": [task]]
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "failed")]

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(reviewItems.contains(where: { $0.id == task.id }),
                      "task with finished resolution should be in Review (not Backlog)")

        let doingItems = model.workItems(in: .doing)
        XCTAssertFalse(doingItems.contains(where: { $0.id == task.id }),
                       "task with finished resolution must NOT be in Doing")

        let backlogItems = model.workItems(in: .backlog)
        XCTAssertFalse(backlogItems.contains(where: { $0.id == task.id }),
                       "task with finished resolution must NOT be in Backlog")
    }

    // MARK: - Helpers

    private func makeTask(
        id: String = "task_\(UUID().uuidString)",
        blockedReason: String?,
        attemptID: String?
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test conflict item",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/x/y/pull/42",
            deletedAt: nil,
            createdAt: "2026-05-13T00:00:00Z",
            updatedAt: "2026-05-13T00:00:00Z",
            blockedReason: blockedReason,
            blockedAttemptID: attemptID
        )
    }

    private func makeResolution(id: String, workItemID: String, status: String) -> WorkConflictResolution {
        WorkConflictResolution(
            id: id,
            productID: "prod_test",
            workItemID: workItemID,
            prURL: "https://github.com/x/y/pull/42",
            prNumber: 42,
            headBranch: "feature/test",
            baseBranch: "main",
            baseSHAAtTrigger: "abc123",
            headSHABefore: nil,
            headSHAAfter: nil,
            status: status,
            failureReason: nil,
            cubeLeaseID: nil,
            cubeWorkspaceID: nil,
            workerID: nil,
            conflictDiagnosis: nil,
            createdAt: "2026-05-13T00:00:00Z",
            startedAt: nil,
            finishedAt: nil
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-13T00:00:00Z",
                updatedAt: "2026-05-13T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
