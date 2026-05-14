import XCTest
@testable import Boss

/// Pins the deduplication logic for Review-lane notifications. Tests
/// operate on `ChatViewModel.knownReviewTaskIDs` and the
/// `applyEventForTest` entry point so they exercise the real dispatch
/// path without ever touching the OS notification centre.
@MainActor
final class ReviewNotificationTests: XCTestCase {

    // MARK: - seedReviewTaskIDs (workTree handler)

    /// A task that is already `in_review` when the first work-tree
    /// snapshot arrives must be seeded into `knownReviewTaskIDs`
    /// immediately — so a subsequent update for the same task arriving
    /// while still in review is treated as a no-op (not a new transition).
    func testSeedingFromWorkTreePreventsSpuriousNotification() {
        let model = makeModel()
        let task = makeTask(id: "t_seeded", status: "in_review")

        // Deliver a workTree event that already contains the in-review task.
        model.applyEventForTest(makeWorkTreeEvent(tasks: [task]))

        // The model should now know about this task.
        XCTAssertTrue(model.knownReviewTaskIDs.contains("t_seeded"))
    }

    /// A task that is NOT in review when the work-tree arrives should
    /// not be in the seeded set — transitions from that task to
    /// `in_review` later should be treated as genuine new entries.
    func testNonReviewTaskNotSeeded() {
        let model = makeModel()
        let task = makeTask(id: "t_active", status: "active")

        model.applyEventForTest(makeWorkTreeEvent(tasks: [task]))

        XCTAssertFalse(model.knownReviewTaskIDs.contains("t_active"))
    }

    // MARK: - maybeFireReviewNotification (workItemUpdated handler)

    /// First `in_review` transition for a task adds it to the known set.
    func testFirstInReviewTransitionIsRecorded() {
        let model = makeModel()
        let task = makeTask(id: "t_new", status: "in_review")

        model.applyEventForTest(.workItemUpdated(item: .task(task)))

        XCTAssertTrue(model.knownReviewTaskIDs.contains("t_new"))
    }

    /// If the task is already in `knownReviewTaskIDs` (e.g. seeded at
    /// startup), a repeated `workItemUpdated` while still `in_review`
    /// must NOT add it again (idempotent). The set size stays 1.
    func testRepeatedInReviewEventIsIdempotent() {
        let model = makeModel()
        var task = makeTask(id: "t_repeat", status: "in_review")

        // Seed as if the task was already in review at startup.
        model.knownReviewTaskIDs.insert("t_repeat")

        model.applyEventForTest(.workItemUpdated(item: .task(task)))

        // Still exactly one entry — the event was a no-op.
        XCTAssertTrue(model.knownReviewTaskIDs.contains("t_repeat"))
        XCTAssertEqual(model.knownReviewTaskIDs.count, 1)
    }

    /// When a task leaves `in_review` it is removed from the set so a
    /// future re-entry (worker revises and re-opens the PR) can fire
    /// again.
    func testLeavingReviewClearsEntry() {
        let model = makeModel()
        model.knownReviewTaskIDs.insert("t_back")

        let taskBackToActive = makeTask(id: "t_back", status: "active")
        model.applyEventForTest(.workItemUpdated(item: .task(taskBackToActive)))

        XCTAssertFalse(model.knownReviewTaskIDs.contains("t_back"))
    }

    /// After leaving review and then re-entering, the task is added
    /// back to the set (enabling a second notification for the revised PR).
    func testReEntryAfterLeaveIsRecorded() {
        let model = makeModel()

        // Enter review.
        model.applyEventForTest(.workItemUpdated(item: .task(makeTask(id: "t_cycle", status: "in_review"))))
        XCTAssertTrue(model.knownReviewTaskIDs.contains("t_cycle"))

        // Leave review.
        model.applyEventForTest(.workItemUpdated(item: .task(makeTask(id: "t_cycle", status: "active"))))
        XCTAssertFalse(model.knownReviewTaskIDs.contains("t_cycle"))

        // Re-enter review.
        model.applyEventForTest(.workItemUpdated(item: .task(makeTask(id: "t_cycle", status: "in_review"))))
        XCTAssertTrue(model.knownReviewTaskIDs.contains("t_cycle"))
    }

    // MARK: - seedReviewTaskIDs clears stale entries

    /// When a product's work-tree snapshot arrives and a task that was
    /// previously in review is now `done`, the seeding pass removes the
    /// stale entry from the set so a re-dispatch to `in_review` later
    /// fires correctly.
    func testSeedingRemovesStaleEntry() {
        let model = makeModel()

        // Manually seed a task as known-in-review.
        model.knownReviewTaskIDs.insert("t_done")

        // Deliver a work tree where that task is now `done`.
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "t_done", status: "done")]))

        XCTAssertFalse(model.knownReviewTaskIDs.contains("t_done"))
    }

    /// Chores follow the same seeding logic as tasks.
    func testChoreInReviewIsSeeded() {
        let model = makeModel()
        let chore = makeChore(id: "c_review", status: "in_review")

        model.applyEventForTest(makeWorkTreeEvent(chores: [chore]))

        XCTAssertTrue(model.knownReviewTaskIDs.contains("c_review"))
    }

    // MARK: - Fixtures

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-review-test-\(UUID().uuidString).sock")
    }

    private func makeTask(id: String, status: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: "proj_test",
            kind: "task",
            name: "Task \(id)",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-14T00:00:00Z",
            updatedAt: "2026-05-14T00:00:00Z"
        )
    }

    private func makeChore(id: String, status: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Chore \(id)",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-14T00:00:00Z",
            updatedAt: "2026-05-14T00:00:00Z"
        )
    }

    private func makeWorkTreeEvent(
        tasks: [WorkTask] = [],
        chores: [WorkTask] = []
    ) -> EngineEvent {
        .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-14T00:00:00Z",
                updatedAt: "2026-05-14T00:00:00Z"
            ),
            projects: [],
            tasks: tasks,
            chores: chores,
            taskRuntimes: [],
            dependencies: []
        )
    }
}
